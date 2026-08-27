use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::{stream as future_stream, Stream};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::VecDeque, convert::Infallible, fs, net::SocketAddr, path::PathBuf};
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

use crate::boot_page::with_link_start_boot;
use crate::cli::{DaemonRecord, StartArgs};
use crate::daemon::process_alive;
use crate::store::Store;
use crate::util::{canonical, hash, now, output, token, MAX_BODY, PROTOCOL, VERSION};

#[derive(Clone)]
pub(crate) struct AppState {
    store: Store,
    host: String,
}

#[derive(Deserialize)]
pub(crate) struct RegisterConnection {
    #[serde(rename = "protocolMajor")]
    protocol_major: String,
    callsign: Option<String>,
}
#[derive(Serialize)]
pub(crate) struct ConnectionCreated {
    #[serde(rename = "connectionId")]
    connection_id: String,
    callsign: String,
    #[serde(rename = "capability")]
    capability: String,
    status: String,
}
#[derive(Deserialize)]
pub(crate) struct RegisterApp {
    #[serde(rename = "protocolMajor")]
    protocol_major: String,
    #[serde(rename = "connectionId")]
    connection_id: String,
    manifest: Manifest,
    page: Option<LaunchPage>,
}
#[derive(Deserialize)]
pub(crate) struct LaunchPage {
    #[serde(rename = "htmlPath")]
    html_path: PathBuf,
}
#[derive(Deserialize, Serialize)]
pub(crate) struct Manifest {
    #[serde(rename = "appId")]
    app_id: String,
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "originPolicy")]
    origin_policy: OriginPolicy,
}
#[derive(Deserialize, Serialize)]
pub(crate) struct OriginPolicy {
    #[serde(rename = "exactOrigin")]
    exact_origin: String,
}
#[derive(Serialize)]
pub(crate) struct AppCreated {
    #[serde(rename = "instanceId")]
    instance_id: String,
    capability: String,
    origin: String,
    status: String,
}
#[derive(Serialize)]
pub(crate) struct GrantCreated {
    grant: String,
    expires_at: i64,
    #[serde(rename = "pageId")]
    page_id: String,
    #[serde(rename = "launchUrl")]
    launch_url: String,
}
#[derive(Deserialize)]
pub(crate) struct EventInput {
    #[serde(rename = "eventId")]
    event_id: String,
    payload: Value,
}
#[derive(Deserialize)]
pub(crate) struct AckInput {
    #[serde(rename = "eventId")]
    event_id: String,
}
#[derive(Deserialize)]
pub(crate) struct FailInput {
    #[serde(rename = "eventId")]
    event_id: String,
    reason: String,
}
#[derive(Deserialize)]
pub(crate) struct FeedbackInput {
    #[serde(rename = "feedbackId")]
    feedback_id: String,
    #[serde(rename = "inReplyToEventId")]
    in_reply_to: Option<String>,
    payload: Value,
}
#[derive(Deserialize)]
pub(crate) struct StreamQuery {
    cursor: Option<i64>,
}
pub(crate) fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
pub(crate) fn reject(code: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::FORBIDDEN, Json(json!({"error":code})))
}
pub(crate) fn origin_gate(
    headers: &HeaderMap,
    expected_host: &str,
    expected_origin: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    if headers.get(header::HOST).and_then(|h| h.to_str().ok()) != Some(expected_host) {
        return Err(reject("host_mismatch"));
    };
    let actual_origin = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok());
    let daemon_same_origin = expected_origin == format!("http://{expected_host}");
    if actual_origin != Some(expected_origin) && !(daemon_same_origin && actual_origin.is_none()) {
        return Err(reject("origin_mismatch"));
    };
    Ok(())
}
pub(crate) fn protocol(v: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if v == PROTOCOL {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"protocol_mismatch","expected":PROTOCOL})),
        ))
    }
}
pub(crate) async fn serve(a: StartArgs) -> Result<()> {
    if a.host != "127.0.0.1" {
        bail!("loopback_only: host must be 127.0.0.1")
    }
    let record_path = a.state.state_dir.join("daemon.json");
    if let Ok(bytes) = fs::read(&record_path) {
        let record: DaemonRecord =
            serde_json::from_slice(&bytes).context("invalid daemon discovery record")?;
        if process_alive(record.pid) {
            if record.version != VERSION || record.protocol_major != PROTOCOL {
                bail!("daemon_version_or_protocol_conflict: explicit drain/restart required")
            }
            output(
                a.state.json,
                json!({"reused":true,"version":VERSION,"protocolMajor":PROTOCOL}),
                "相容 daemon 已在執行，安全重用",
            );
            return Ok(());
        }
    }
    let store = Store::open(&a.state.state_dir)?;
    let addr: SocketAddr = format!("{}:{}", a.host, a.port).parse()?;
    let host = addr.to_string();
    fs::write(
        &record_path,
        serde_json::to_vec(
            &json!({"address":host,"pid":std::process::id(),"version":VERSION,"protocolMajor":PROTOCOL}),
        )?,
    )?;
    let state = AppState { store, host };
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/connections", post(register_connection))
        .route("/v1/apps", post(register_app))
        .route("/v1/launch-grants", post(create_grant))
        .route("/v1/launch-pages/:locator", get(serve_launch_page))
        .route(
            "/v1/launch-grants/redeem",
            post(redeem_grant).options(preflight_redeem),
        )
        .route(
            "/v1/apps/:id/events",
            post(receive_event).options(preflight_app_post),
        )
        .route(
            "/v1/apps/:id/cancel",
            post(cancel_event).options(preflight_app_post),
        )
        .route(
            "/v1/apps/:id/stream",
            get(stream).options(preflight_app_get),
        )
        .route("/v1/connections/:id/ack", post(ack))
        .route("/v1/connections/:id/fail", post(fail))
        .route("/v1/connections/:id/feedback/:app", post(feedback))
        .layer(axum::middleware::from_fn_with_state(state.clone(), cors))
        .layer(RequestBodyLimitLayer::new(MAX_BODY))
        .with_state(state);
    println!("LinkStart daemon listening on http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}
pub(crate) async fn health(State(_s): State<AppState>) -> Json<Value> {
    Json(json!({"version":VERSION,"protocolMajor":PROTOCOL,"status":"ready"}))
}
pub(crate) async fn cors(
    State(s): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        == Some(&s.host);
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = req.uri().path();
    let app_id = path
        .strip_prefix("/v1/apps/")
        .and_then(|p| p.split('/').next());
    let allowed = host_ok
        && match (app_id, origin.as_deref()) {
            (Some(id), Some(o)) => app_origin(&s.store, id).ok().flatten().as_deref() == Some(o),
            (None, Some(o)) if path == "/v1/launch-grants/redeem" => {
                o == format!("http://{}", s.host)
            }
            _ => false,
        };
    let mut response = next.run(req).await;
    if allowed {
        if let Some(origin) = origin {
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_str(&origin).unwrap(),
            );
            response.headers_mut().insert(
                header::VARY,
                HeaderValue::from_static("Origin, Access-Control-Request-Private-Network"),
            );
        }
    }
    response
}
pub(crate) fn app_origin(store: &Store, id: &str) -> Result<Option<String>> {
    store.with(|c| {
        Ok(c.query_row(
            "SELECT origin FROM apps WHERE id=? AND status='connected'",
            [id],
            |r| r.get(0),
        )
        .optional()?)
    })
}
pub(crate) fn preflight_headers(
    headers: &HeaderMap,
    method: &str,
    required: &[&str],
) -> Result<HeaderMap, StatusCode> {
    if headers
        .get("access-control-request-method")
        .and_then(|v| v.to_str().ok())
        != Some(method)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let requested = headers
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|x| x.trim().to_ascii_lowercase())
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>();
    if requested
        .iter()
        .any(|x| x != "authorization" && x != "content-type")
        || required.iter().any(|x| !requested.iter().any(|v| v == x))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut out = HeaderMap::new();
    out.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_str(&required.join(", ")).unwrap(),
    );
    out.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_str(method).unwrap(),
    );
    out.insert(
        header::VARY,
        HeaderValue::from_static("Origin, Access-Control-Request-Private-Network"),
    );
    if headers
        .get("access-control-request-private-network")
        .and_then(|v| v.to_str().ok())
        == Some("true")
    {
        out.insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
    }
    Ok(out)
}
pub(crate) fn exact_preflight(
    headers: &HeaderMap,
    host: &str,
    origin: &str,
    method: &str,
    required: &[&str],
) -> Result<HeaderMap, StatusCode> {
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some(host)
        || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(origin)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut out = preflight_headers(headers, method, required)?;
    out.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(origin).map_err(|_| StatusCode::FORBIDDEN)?,
    );
    Ok(out)
}
pub(crate) async fn preflight_app_post(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap), StatusCode> {
    let origin = app_origin(&s.store, &id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::FORBIDDEN)?;
    Ok((
        StatusCode::NO_CONTENT,
        exact_preflight(
            &headers,
            &s.host,
            &origin,
            "POST",
            &["authorization", "content-type"],
        )?,
    ))
}
pub(crate) async fn preflight_app_get(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap), StatusCode> {
    let origin = app_origin(&s.store, &id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::FORBIDDEN)?;
    Ok((
        StatusCode::NO_CONTENT,
        exact_preflight(&headers, &s.host, &origin, "GET", &["authorization"])?,
    ))
}
pub(crate) async fn preflight_redeem(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap), StatusCode> {
    Ok((
        StatusCode::NO_CONTENT,
        exact_preflight(
            &headers,
            &s.host,
            &format!("http://{}", s.host),
            "POST",
            &["authorization"],
        )?,
    ))
}
pub(crate) async fn register_connection(
    State(s): State<AppState>,
    Json(x): Json<RegisterConnection>,
) -> Result<Json<ConnectionCreated>, (StatusCode, Json<Value>)> {
    protocol(&x.protocol_major)?;
    let id = Uuid::new_v4().to_string();
    let cap = token();
    let callsign = x.callsign.unwrap_or_else(|| format!("LS-{}", &id[..6]));
    s.store
        .with(|c| {
            c.execute(
                "INSERT INTO connections VALUES(?,?,?,?,?)",
                params![id, hash(&cap), callsign, "online", now()],
            )?;
            Ok(())
        })
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(ConnectionCreated {
        connection_id: id,
        callsign,
        capability: cap,
        status: "online".into(),
    }))
}
pub(crate) async fn register_app(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(x): Json<RegisterApp>,
) -> Result<Json<AppCreated>, (StatusCode, Json<Value>)> {
    protocol(&x.protocol_major)?;
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&x.connection_id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    };
    if x.manifest.origin_policy.exact_origin == "null" {
        return Err(reject("opaque_origin_requires_launch_grant"));
    };
    let id = Uuid::new_v4().to_string();
    let app_cap = token();
    s.store
        .with(|c| {
            c.execute(
                "INSERT INTO apps VALUES(?,?,?,?,?,?,?,?)",
                params![
                    id,
                    x.connection_id,
                    hash(&app_cap),
                    x.manifest.origin_policy.exact_origin,
                    x.manifest.app_id,
                    x.manifest.display_name,
                    "connected",
                    now()
                ],
            )?;
            Ok(())
        })
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(AppCreated {
        instance_id: id,
        capability: app_cap,
        origin: x.manifest.origin_policy.exact_origin,
        status: "connected".into(),
    }))
}
pub(crate) fn host_gate(
    headers: &HeaderMap,
    expected: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    if headers.get(header::HOST).and_then(|h| h.to_str().ok()) == Some(expected) {
        Ok(())
    } else {
        Err(reject("host_mismatch"))
    }
}
pub(crate) async fn create_grant(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(x): Json<RegisterApp>,
) -> Result<Json<GrantCreated>, (StatusCode, Json<Value>)> {
    protocol(&x.protocol_major)?;
    host_gate(&headers, &s.host)?;
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&x.connection_id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    }
    if x.manifest.origin_policy.exact_origin != "null" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"launch_grant_requires_opaque_manifest"})),
        ));
    }
    let page = x.page.as_ref().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"launch_page_required"})),
        )
    })?;
    let html = fs::read_to_string(&page.html_path).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"launch_page_unreadable"})),
        )
    })?;
    if html.len() > 2 * 1024 * 1024 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error":"launch_page_too_large"})),
        ));
    }
    let grant = token();
    let expires = now() + 300;
    let grant_id = Uuid::new_v4().to_string();
    let page_id = Uuid::new_v4().to_string();
    let manifest = serde_json::to_string(&x.manifest).map_err(|_| reject("storage_error"))?;
    s.store
        .with(|c| {
            c.execute(
                "INSERT INTO launch_grants VALUES(?,?,?,?,?,NULL)",
                params![grant_id, hash(&grant), x.connection_id, manifest, expires],
            )?;
            c.execute(
                "INSERT INTO launch_pages VALUES(?,?,?,?)",
                params![page_id, grant_id, html, now()],
            )?;
            Ok(())
        })
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(GrantCreated {
        launch_url: format!(
            "http://{}/v1/launch-pages/{}#daemon=http%3A%2F%2F{}&grant={}",
            s.host, page_id, s.host, grant
        ),
        page_id,
        expires_at: expires,
        grant,
    }))
}

pub(crate) async fn serve_launch_page(
    State(s): State<AppState>,
    Path(locator): Path<String>,
    headers: HeaderMap,
) -> Result<(HeaderMap, String), (StatusCode, Json<Value>)> {
    host_gate(&headers, &s.host)?;
    let html = s
        .store
        .with(|c| {
            Ok(c.query_row(
                "SELECT p.html FROM launch_pages p JOIN launch_grants g ON g.id=p.grant_id WHERE p.locator=? AND g.expires_at>=?",
                params![locator, now()],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
        .map_err(|_| reject("storage_error"))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"launch_page_not_found"})),
            )
        })?;
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response_headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok((response_headers, with_link_start_boot(html)))
}
pub(crate) async fn redeem_grant(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AppCreated>, (StatusCode, Json<Value>)> {
    host_gate(&headers, &s.host)?;
    let launch_origin = format!("http://{}", s.host);
    if headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) != Some(&launch_origin) {
        return Err(reject("origin_mismatch"));
    }
    let grant = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    let app_cap = token();
    let app_id = Uuid::new_v4().to_string();
    let result=s.store.with(|c|{let row:Option<(String,String,i64,Option<i64>)>=c.query_row("SELECT connection_id,manifest,expires_at,redeemed_at FROM launch_grants WHERE capability_hash=?",[hash(grant)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;let(conn,manifest,expires,redeemed)=row.context("launch_grant_invalid")?;if redeemed.is_some()||expires<now(){bail!("launch_grant_expired_or_redeemed")};let m:Manifest=serde_json::from_str(&manifest)?;let status:String=c.query_row("SELECT status FROM connections WHERE id=?",[&conn],|r|r.get(0))?;if status!="online"{bail!("origin_offline")};c.execute("INSERT INTO apps VALUES(?,?,?,?,?,?,?,?)",params![app_id,conn,hash(&app_cap),launch_origin,m.app_id,m.display_name,"connected",now()])?;c.execute("UPDATE launch_grants SET redeemed_at=? WHERE capability_hash=? AND redeemed_at IS NULL",params![now(),hash(grant)])?;Ok(AppCreated{instance_id:app_id.clone(),capability:app_cap.clone(),origin:launch_origin.clone(),status:"connected".into()})}).map_err(|_|reject("launch_grant_invalid_or_expired"))?;
    Ok(Json(result))
}
pub(crate) async fn receive_event(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(x): Json<EventInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    let (_, origin) = s
        .store
        .auth_app(&id, cap)
        .map_err(|_| reject("storage_error"))?
        .ok_or_else(|| reject("capability_invalid"))?;
    origin_gate(&headers, &s.host, &origin)?;
    if x.event_id.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_event_id"})),
        ));
    };
    let payload = canonical(x.payload);
    let payload_s = serde_json::to_string(&payload).unwrap();
    let h = hash(&payload_s);
    let reply=s.store.with(|c|{let conn_status: String=c.query_row("SELECT c.status FROM connections c JOIN apps a ON a.connection_id=c.id WHERE a.id=?",[&id],|r|r.get(0))?;if conn_status!="online"{return Ok(json!({"error":"origin_offline"}))};let old:Option<(String,String,String,i64)>=c.query_row("SELECT receipt_id,payload_hash,status,sequence FROM events WHERE app_id=? AND id=?",params![id,x.event_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;if let Some((receipt,old_hash,status,seq))=old{return Ok(if old_hash==h {json!({"receiptId":receipt,"eventId":x.event_id,"sequence":seq,"status":status,"duplicate":true})}else{json!({"error":"event_id_conflict"})})};let seq:i64=c.query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM events WHERE app_id=?",[&id],|r|r.get(0))?;let receipt=format!("rcpt-{}",Uuid::new_v4());c.execute("INSERT INTO events VALUES(?,?,?,?,?,?,?,?,?)",params![x.event_id,id,seq,payload_s,h,"received",Option::<String>::None,receipt,now()])?;Ok(json!({"receiptId":receipt,"eventId":x.event_id,"sequence":seq,"status":"received","untrustedInput":true}))}).map_err(|_|reject("storage_error"))?;
    if reply.get("error").is_none() {
        s.store
            .with(|c| Store::insert_notification(c, &id, "receipt", &reply))
            .map_err(|_| reject("storage_error"))?;
    }
    Ok(Json(reply))
}
pub(crate) async fn cancel_event(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(x): Json<AckInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    let (_, o) = s
        .store
        .auth_app(&id, cap)
        .map_err(|_| reject("storage_error"))?
        .ok_or_else(|| reject("capability_invalid"))?;
    origin_gate(&headers, &s.host, &o)?;
    let n=s.store.with(|c|Ok(c.execute("UPDATE events SET status='cancelled' WHERE app_id=? AND id=? AND status='received'",params![id,x.event_id])?)).map_err(|_|reject("storage_error"))?;
    if n == 0 {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error":"event_not_received"})),
        ));
    };
    let d = json!({"eventId":x.event_id,"status":"cancelled"});
    s.store
        .with(|c| Store::insert_notification(c, &id, "event", &d))
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(d))
}
pub(crate) async fn ack(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(x): Json<AckInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    };
    let out = s.store.ack_event(&id, &x.event_id).map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":"event_not_received_or_not_inflight"})),
        )
    })?;
    Ok(Json(out))
}
pub(crate) async fn fail(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(x): Json<FailInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    };
    let n=s.store.with(|c|Ok(c.execute("UPDATE events SET status='failed',reason=? WHERE id=? AND status='received' AND app_id IN (SELECT id FROM apps WHERE connection_id=?)",params![x.reason,x.event_id,id])?)).map_err(|_|reject("storage_error"))?;
    if n == 0 {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error":"event_not_received"})),
        ));
    };
    Ok(Json(json!({"eventId":x.event_id,"status":"failed"})))
}
pub(crate) async fn feedback(
    State(s): State<AppState>,
    Path((id, app)): Path<(String, String)>,
    headers: HeaderMap,
    Json(x): Json<FeedbackInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    };
    let data = canonical(x.payload);
    let out = s
        .store
        .insert_feedback(&id, &app, &x.feedback_id, x.in_reply_to.as_deref(), data)
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(out))
}
pub(crate) async fn stream(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    let (_, o) = s
        .store
        .auth_app(&id, cap)
        .map_err(|_| reject("storage_error"))?
        .ok_or_else(|| reject("capability_invalid"))?;
    origin_gate(&headers, &s.host, &o)?;
    let cursor = query.cursor.unwrap_or(0);
    let replay = s
        .store
        .replay(&id, cursor)
        .map_err(|_| reject("storage_error"))?;
    let durable = future_stream::unfold(
        (s.store.clone(), id, cursor, VecDeque::from(replay)),
        |(store, app_id, mut cursor, mut pending)| async move {
            loop {
                if let Some(notification) = pending.pop_front() {
                    cursor = notification.cursor;
                    let event = Event::default()
                        .id(notification.cursor.to_string())
                        .event(notification.kind)
                        .data(notification.data.to_string());
                    return Some((Ok(event), (store, app_id, cursor, pending)));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                pending = VecDeque::from(store.replay(&app_id, cursor).unwrap_or_default());
            }
        },
    );
    let st: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(durable);
    let mut response = Sse::new(st)
        .keep_alive(KeepAlive::default())
        .into_response();
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(&o).unwrap(),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Origin"));
    Ok(response)
}
