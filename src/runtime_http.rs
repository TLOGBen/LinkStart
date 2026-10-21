//! HTTP adapter for the same local daemon. Authentication belongs to core dispatch.
use crate::runtime_agent::{AgentRunner, CodexProcess, JobRequest};
use crate::runtime_control_bridge::OwnedControlBridge;
use crate::runtime_core::{Engine, ToolRequest};
use crate::runtime_history::{HistoryStore, Retention};
use crate::runtime_source_jobs::SourceJobs;
use crate::runtime_terminal::TerminalManager;
use crate::runtime_watch::WatchManager;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, path::Path, time::Duration};
#[derive(Clone)]
struct HttpState {
    owned: Arc<OwnedControlBridge>,
    engine: Engine,
    host: String,
    gate: Arc<Mutex<NativeState>>,
    terminals: TerminalManager,
    history: HistoryStore,
    intent_dir: std::path::PathBuf,
    stop: tokio::sync::watch::Sender<bool>,
    settings: Value,
    startup_candidates: Value,
    agents: AgentRunner,
    sources: SourceJobs,
    watches: WatchManager,
}
#[derive(Default)]
struct NativeState {
    sessions: HashMap<String, String>,
    receipts: HashMap<String, (String, Value)>,
    uncertain: std::collections::HashSet<String>,
    epoch: u64,
    ticket: Option<(String, u64, u64, std::time::Instant)>,
    draining: bool,
}

pub fn router(
    dir: &Path,
    host: &str,
) -> anyhow::Result<(Router, tokio::sync::watch::Receiver<bool>)> {
    router_inner(dir, host, None)
}
/// Trusted in-process injection used by embedders; no HTTP plan installation.
pub fn router_with_bridge(
    dir: &Path,
    host: &str,
    bridge: Arc<OwnedControlBridge>,
) -> anyhow::Result<(Router, tokio::sync::watch::Receiver<bool>)> {
    router_inner(dir, host, Some(bridge))
}
fn router_inner(
    dir: &Path,
    host: &str,
    bridge: Option<Arc<OwnedControlBridge>>,
) -> anyhow::Result<(Router, tokio::sync::watch::Receiver<bool>)> {
    let engine = Engine::open(dir)?;
    let owned = match bridge {
        Some(bridge) => bridge,
        None => {
            let registry =
                crate::runtime_codex_host::OwnedCodexRegistry::open(dir, engine.clone())?;
            let supervisor = Arc::new(crate::runtime_supervisor::OwnedSessionSupervisor::new(
                engine.clone(),
                registry.clone(),
                Default::default(),
            )?);
            let participants = Arc::new(crate::runtime_participants::ParticipantService::open(
                dir,
                engine.clone(),
                Arc::new(registry.clone()),
            )?);
            let plans = Arc::new(crate::runtime_launch_policy::LaunchPolicyStore::open(
                dir,
                engine.clone(),
                registry.clone(),
            )?);
            Arc::new(OwnedControlBridge::open(
                dir,
                engine.clone(),
                supervisor,
                registry,
                participants,
                plans,
            )?)
        }
    };
    let owner = std::fs::read_to_string(crate::runtime_core::owner_token_path(dir))?;
    let settings = engine
        .execute(
            &owner,
            &ToolRequest {
                request_id: "startup:settings".into(),
                tool: "settings.get".into(),
                space_id: None,
                args: json!({}),
            },
        )
        .map_err(|e| anyhow::anyhow!(e.message))?["settings"]
        .clone();
    let terminals = TerminalManager::with_capacity(
        settings["history_limit"].as_u64().unwrap_or(10000) as usize,
    );
    let startup_candidates = if settings["discovery_enabled"] == true {
        discover_commands()
    } else {
        json!([])
    };
    let agents = AgentRunner::open(
        &dir.join("agent-jobs"),
        engine.clone(),
        Arc::new(CodexProcess::default()),
    )?;
    let sources = SourceJobs::open(&dir.join("source-jobs"), engine.clone())?;
    let watches = WatchManager::open(&dir.join("source-watches"), engine.clone(), sources.clone())?;
    let intent_dir = dir.join("native-intents");
    std::fs::create_dir_all(&intent_dir)?;
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let history = HistoryStore::open(dir, Retention::default()).map_err(anyhow::Error::msg)?;
    let state = HttpState {
        owned,
        engine,
        host: host.into(),
        gate: Arc::new(Mutex::new(NativeState::default())),
        terminals,
        history,
        intent_dir,
        stop,
        settings,
        startup_candidates,
        agents,
        sources,
        watches,
    };
    Ok((
        Router::new()
            .route("/v2/health", get(health))
            .route("/v2/tools", post(tools))
            .route("/console", get(console))
            .route("/console/", get(console))
            .route("/console/live.js", get(script))
            .route("/console/live.css", get(style))
            .layer(tower_http::limit::RequestBodyLimitLayer::new(256 * 1024))
            .with_state(state),
        receiver,
    ))
}
fn asset(kind: &'static str, body: &'static str) -> impl axum::response::IntoResponse {
    ([("Content-Type",kind),("Cache-Control","no-store"),("X-Content-Type-Options","nosniff"),("Content-Security-Policy","default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'")],body)
}
async fn console() -> impl axum::response::IntoResponse {
    asset(
        "text/html; charset=utf-8",
        include_str!("../web/runtime-console/live.html"),
    )
}
async fn script() -> impl axum::response::IntoResponse {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../web/runtime-console/live.js"),
    )
}
async fn style() -> impl axum::response::IntoResponse {
    asset(
        "text/css; charset=utf-8",
        include_str!("../web/runtime-console/live.css"),
    )
}
async fn health(State(s): State<HttpState>) -> Json<Value> {
    Json(
        json!({"status":if s.gate.lock().map(|n|n.draining).unwrap_or(true){"draining"}else{"ready"},"protocol":"v2"}),
    )
}
type Reply = (StatusCode, Json<Value>);
fn failure(status: StatusCode, code: &str, message: &str) -> Reply {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
}
async fn tools(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<ToolRequest>,
) -> Reply {
    if headers.get("host").and_then(|h| h.to_str().ok()) != Some(state.host.as_str()) {
        return failure(StatusCode::FORBIDDEN, "Denied", "unexpected Host");
    }
    if let Some(origin) = headers.get("origin") {
        if origin.to_str().ok() != Some(format!("http://{}", state.host).as_str()) {
            return failure(StatusCode::FORBIDDEN, "Denied", "unexpected Origin");
        }
    }
    let Some(token) = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    else {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Unauthenticated",
            "credential required",
        );
    };
    if request.tool.starts_with("owned.")
        || (request.tool == "operations.get"
            && matches!(
                request.args["source_kind"].as_str(),
                Some("owned_session" | "participant")
            ))
    {
        let actor = token.to_string();
        let bridge = state.owned.clone();
        return match tokio::task::spawn_blocking(move || bridge.execute(&actor, &request)).await {
            Ok(result) => reply(result),
            Err(_) => failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                "owned dispatch failed",
            ),
        };
    }
    if matches!(
        request.tool.as_str(),
        "runtime.confirm_shutdown" | "runtime.force_shutdown"
    ) {
        return shutdown_tools(state.clone(), token.to_string(), request).await;
    }
    let Ok(mut guard) = state.gate.lock() else {
        return failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            "dispatch lock poisoned",
        );
    };
    let read = request.tool.ends_with(".list")
        || request.tool.ends_with(".read")
        || request.tool.ends_with(".get")
        || request.tool == "history.checkpoint"
        || request.tool == "terminal.observe"
        || request.tool == "endpoint.authorize"
        || request.tool == "identity.current";
    if guard.draining
        && !read
        && !request.tool.starts_with("runtime.")
        && !matches!(
            request.tool.as_str(),
            "terminal.stop" | "terminal.force" | "terminal.release"
        )
    {
        return failure(
            StatusCode::CONFLICT,
            "Draining",
            "runtime is draining; new mutations disabled",
        );
    }
    let result = dispatch(&state, &mut guard, token, &request);
    if result.is_ok() && !read && !request.tool.starts_with("runtime.") {
        guard.epoch += 1;
    }
    reply(result)
}
fn reply(result: Result<Value, crate::runtime_core::ApiError>) -> Reply {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(e) => failure(
            match e.code.as_str() {
                "Unauthenticated" => StatusCode::UNAUTHORIZED,
                "Denied" => StatusCode::FORBIDDEN,
                "NotFound" => StatusCode::NOT_FOUND,
                "Invalid" => StatusCode::BAD_REQUEST,
                "QueueFull" => StatusCode::SERVICE_UNAVAILABLE,
                "Internal" => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::CONFLICT,
            },
            &e.code,
            &e.message,
        ),
    }
}

async fn shutdown_tools(state: HttpState, actor: String, request: ToolRequest) -> Reply {
    if !state.engine.is_owner(&actor) {
        return failure(
            StatusCode::FORBIDDEN,
            "Denied",
            "runtime owner credential required",
        );
    }
    let admission = (|| {
        let ticket = field(&request.args, "ticket")?;
        let mut guard = state
            .gate
            .lock()
            .map_err(|_| error("Internal", "dispatch lock poisoned"))?;
        if guard.ticket.as_ref().is_none_or(|(t, epoch, _, issued)| {
            t != ticket || *epoch != guard.epoch || issued.elapsed() > Duration::from_secs(60)
        }) {
            return Err(error(
                "StaleImpact",
                "impact changed or ticket expired; prepare shutdown again",
            ));
        }
        state
            .owned
            .begin_shutdown(guard.ticket.as_ref().unwrap().2)?;
        guard.draining = true;
        Ok(())
    })();
    if let Err(e) = admission {
        return reply(Err(e));
    }
    // Neither terminal cleanup nor history EOF/flush may hold the global gate.
    let cleanup_state = state.clone();
    let force = request.tool == "runtime.force_shutdown";
    let result = tokio::task::spawn_blocking(move || shutdown_cleanup(&cleanup_state, force)).await;
    match result {
        Ok(Ok(value)) => {
            if value["status"] == "stopping" {
                let stop = state.stop.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let _ = stop.send(true);
                });
            }
            reply(Ok(value))
        }
        Ok(Err(e)) => reply(Err(e)),
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            "shutdown cleanup failed",
        ),
    }
}
fn shutdown_cleanup(s: &HttpState, force: bool) -> Result<Value, crate::runtime_core::ApiError> {
    s.agents.cancel_all();
    s.watches.cancel_all();
    s.sources.cancel_all();
    let remaining = if force {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        for session in s.terminals.list() {
            s.terminals
                .force_owned(
                    &session.id,
                    deadline.saturating_duration_since(std::time::Instant::now()),
                )
                .map_err(|e| error("NativeError", e))?;
        }
        s.terminals
            .list()
            .into_iter()
            .filter(|v| v.status != "exited")
            .collect::<Vec<_>>()
    } else {
        s.terminals.shutdown(Duration::from_millis(500))
    };
    let owned = s.owned.inventory();
    let history_error = if owned.active_workers == 0
        && owned.launch_reservations == 0
        && remaining.is_empty()
        && s.agents.active_count() == 0
        && s.sources.active_count() == 0
        && s.watches.active_count() == 0
    {
        s.history.shutdown_flush(Duration::from_millis(750)).err()
    } else {
        Some("owned processes or workers are still active".into())
    };
    if let Some(reason) = history_error {
        Ok(
            json!({"status":"pending","owned_sessions":owned,"remaining":remaining,"history_error":reason,"active_agent_jobs":s.agents.active_count(),"active_source_jobs":s.sources.active_count(),"active_source_watches":s.watches.active_count()}),
        )
    } else {
        Ok(json!({"status":"stopping","remaining":remaining,"history":"durable"}))
    }
}
fn error(code: &str, message: impl ToString) -> crate::runtime_core::ApiError {
    crate::runtime_core::ApiError {
        code: code.into(),
        message: message.to_string(),
    }
}
fn discover_commands() -> Value {
    let mut candidates = Vec::new();
    let path = std::env::var_os("PATH").unwrap_or_default();
    for name in ["codex", "claude", "claude-local", "ollama"] {
        let suffixes = if cfg!(windows) {
            vec![".exe", ".cmd", ".bat", ""]
        } else {
            vec![""]
        };
        'search: for dir in std::env::split_paths(&path) {
            for suffix in &suffixes {
                let file = dir.join(format!("{name}{suffix}"));
                if file.is_file() {
                    candidates.push(json!({"name":name,"command":file,"status":"available_command","connected":false,"capabilities":["raw_terminal"],"attachment":"explicit new owned session required; external process attachment unsupported"}));
                    break 'search;
                }
            }
        }
    }
    json!(candidates)
}
fn field<'a>(args: &'a Value, name: &str) -> Result<&'a str, crate::runtime_core::ApiError> {
    args[name]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error("Invalid", format!("{name} required")))
}
fn dispatch(
    s: &HttpState,
    n: &mut NativeState,
    token: &str,
    r: &ToolRequest,
) -> Result<Value, crate::runtime_core::ApiError> {
    if r.tool == "operations.get" {
        return crate::runtime_operations::OperationService::new(
            s.engine.clone(),
            s.sources.clone(),
        )
        .execute(token, r);
    }
    match r.tool.as_str() {
        "sources.watch.start" => return Ok(json!(s.watches.start(token, r)?)),
        "sources.watch.get" => {
            return Ok(json!(s.watches.get(token, field(&r.args, "watch_id")?)?))
        }
        "sources.watch.stop" => {
            return Ok(json!(s.watches.stop(token, field(&r.args, "watch_id")?)?))
        }
        "sources.watch.list" => {
            return Ok(
                json!({"items":s.watches.list(token,r.space_id.as_deref().ok_or_else(||error("Invalid","space_id required"))?)?}),
            )
        }
        _ => {}
    }
    if r.tool == "sources.get" {
        return Ok(json!(s.sources.get(token, field(&r.args, "job_id")?)?));
    }
    if r.tool.starts_with("sources.") {
        return Ok(json!(s.sources.start(token, r)?));
    }
    if matches!(r.tool.as_str(), "agent.run" | "agent.recover" | "agent.get") {
        let args = r
            .args
            .as_object()
            .ok_or_else(|| error("Invalid", "agent args must be an object"))?;
        if args
            .keys()
            .any(|key| !matches!(key.as_str(), "job_id" | "mission_topic_id"))
        {
            return Err(error("Invalid", "unsupported agent argument"));
        }
        let request = JobRequest {
            job_id: field(&r.args, "job_id")?.into(),
            space_id: r
                .space_id
                .clone()
                .ok_or_else(|| error("Invalid", "space_id required"))?,
            mission_topic_id: field(&r.args, "mission_topic_id")?.into(),
        };
        return Ok(json!(match r.tool.as_str() {
            "agent.recover" => s.agents.recover(token, &request, &r.request_id)?,
            "agent.get" => s.agents.get_scoped(token, &request)?,
            _ => s.agents.start(token, &request)?,
        }));
    }
    if r.tool.starts_with("runtime.") {
        if !s.engine.is_owner(token) {
            return Err(error("Denied", "runtime owner credential required"));
        }
        match r.tool.as_str() {
            "runtime.discover" => {
                return Ok(
                    json!({"candidates":discover_commands(),"startup_candidates":s.startup_candidates,"startup_enabled":s.settings["discovery_enabled"],"scope":"explicit known command names in PATH; no process or port scan"}),
                )
            }
            "runtime.settings_effective" => {
                return Ok(
                    json!({"settings":s.settings,"applied":["history_limit as terminal output bytes","discovery_enabled for startup command candidates"],"unsupported":["external endpoint autojoin","provider reconnect policy"]}),
                )
            }
            "runtime.prepare_shutdown" => {
                let mut spaces = Vec::new();
                let mut endpoints = Vec::new();
                let mut cursor = Value::Null;
                loop {
                    let page = s.engine.execute(
                        token,
                        &ToolRequest {
                            request_id: "shutdown:spaces".into(),
                            tool: "spaces.list".into(),
                            space_id: None,
                            args: json!({"limit":100,"cursor":cursor}),
                        },
                    )?;
                    for space in page["items"]
                        .as_array()
                        .ok_or_else(|| error("Internal", "spaces response invalid"))?
                    {
                        let mut ecursor = Value::Null;
                        loop {
                            let eps = s.engine.execute(
                                token,
                                &ToolRequest {
                                    request_id: "shutdown:endpoints".into(),
                                    tool: "endpoints.list".into(),
                                    space_id: space["id"].as_str().map(str::to_string),
                                    args: json!({"limit":100,"cursor":ecursor}),
                                },
                            )?;
                            endpoints.extend(
                                eps["items"]
                                    .as_array()
                                    .ok_or_else(|| error("Internal", "endpoints response invalid"))?
                                    .clone(),
                            );
                            ecursor = eps["next_cursor"].clone();
                            if ecursor.is_null() {
                                break;
                            }
                        }
                        spaces.push(space.clone());
                    }
                    cursor = page["next_cursor"].clone();
                    if cursor.is_null() {
                        break;
                    }
                }
                let ticket = uuid::Uuid::new_v4().to_string();
                let owned = s.owned.inventory();
                n.ticket = Some((
                    ticket.clone(),
                    n.epoch,
                    owned.epoch,
                    std::time::Instant::now(),
                ));
                return Ok(
                    json!({"ticket":ticket,"expires_in_seconds":60,"impact":{"spaces":spaces,"endpoints":endpoints,"owned_terminals":s.terminals.list(),"owned_sessions":owned,"active_agent_jobs":s.agents.active_count(),"active_source_jobs":s.sources.active_count(),"active_source_watches":s.watches.active_count(),"scope":"this runtime; other applications excluded","epoch":n.epoch},"status":if n.draining{"pending"}else{"prepared"}}),
                );
            }
            _ => return Err(error("Unsupported", "unknown runtime tool")),
        }
    }
    if r.tool.starts_with("history.") {
        let eid = field(&r.args, "endpoint_id")?;
        s.engine.execute(
            token,
            &ToolRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                tool: "endpoint.authorize".into(),
                space_id: r.space_id.clone(),
                args: json!({"endpoint_id":eid,"operation":"observe"}),
            },
        )?;
        let storage = |e: String| error("HistoryError", e);
        return match r.tool.as_str() {
            "history.list" => Ok(json!({"items":s.history.list(eid).map_err(storage)?})),
            "history.checkpoint" => Ok(json!(s
                .history
                .checkpoint(eid, field(&r.args, "session_id")?)
                .map_err(storage)?)),
            "history.read" => {
                let cursor = r
                    .args
                    .get("cursor")
                    .map(|v| {
                        v.as_u64()
                            .ok_or_else(|| error("Invalid", "cursor must be unsigned integer"))
                    })
                    .transpose()?
                    .unwrap_or(0);
                let limit = r
                    .args
                    .get("limit")
                    .map(|v| {
                        v.as_u64()
                            .filter(|v| *v > 0 && *v <= 1000)
                            .ok_or_else(|| error("Invalid", "limit must be 1..1000"))
                    })
                    .transpose()?
                    .unwrap_or(100);
                Ok(json!(s
                    .history
                    .read(eid, field(&r.args, "session_id")?, cursor, limit as usize)
                    .map_err(storage)?))
            }
            _ => Err(error("Unsupported", "unknown history tool")),
        };
    }
    if !r.tool.starts_with("terminal.") {
        let mut value = s.engine.execute(token, r)?;
        if r.tool == "endpoints.list" {
            if let Some(items) = value["items"].as_array_mut() {
                let sessions = s.terminals.list();
                for ep in items {
                    if ep["kind"] == "terminal" {
                        let active = ep["id"]
                            .as_str()
                            .and_then(|eid| n.sessions.get(eid))
                            .and_then(|sid| sessions.iter().find(|v| &v.id == sid));
                        ep["session"] = json!(active);
                        ep["status"] =
                            json!(active.map(|v| v.status.as_str()).unwrap_or("interrupted"));
                        ep["capabilities"] =
                            json!(["raw_terminal", "bounded_output", "exclusive_control"]);
                    }
                }
            }
        }
        return Ok(value);
    }
    if r.request_id.trim().is_empty() {
        return Err(error("Invalid", "request_id required"));
    }
    if r.tool == "terminal.create" {
        let command = field(&r.args, "command")?.to_string();
        let args = r.args.get("args").cloned().unwrap_or(json!([]));
        let argv: Vec<String> =
            serde_json::from_value(args.clone()).map_err(|e| error("Invalid", e))?;
        let cwd = r
            .args
            .get("cwd")
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_str()
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| error("Invalid", "cwd must be string"))
            })
            .transpose()?;
        let name = r
            .args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&command);
        // The persisted endpoint is the intent. A replay from another runtime never spawns again.
        let reg = ToolRequest {
            request_id: r.request_id.clone(),
            tool: "endpoints.register".into(),
            space_id: r.space_id.clone(),
            args: json!({"name":name,"kind":"terminal","config":{"command":command,"args":args,"cwd":cwd}}),
        };
        let ep = s.engine.execute(token, &reg)?;
        let eid = ep["id"]
            .as_str()
            .ok_or_else(|| error("Internal", "endpoint id missing"))?
            .to_string();
        let key = format!("create:{eid}");
        if let Some((_, value)) = n.receipts.get(&key) {
            return Ok(value.clone());
        }
        // Reject an endpoint that predated this create attempt (including crash recovery).
        // A runtime marker is persisted before any native side effect below.
        let marker = s.intent_dir.join(&eid);
        let intent=std::fs::OpenOptions::new().write(true).create_new(true).open(marker).map_err(|e|error(if e.kind()==std::io::ErrorKind::AlreadyExists{"UnknownEffect"}else{"Internal"},"native create already attempted or intent could not be stored; explicit new request required"))?;
        intent.sync_all().map_err(|e| error("Internal", e))?;
        let session = s
            .terminals
            .create_recorded(&eid, &s.history, command, argv, cwd)
            .map_err(|e| error("NativeError", e))?;
        n.sessions.insert(eid, session.id.clone());
        let value = json!({"endpoint":ep,"session":session});
        n.receipts.insert(key, (String::new(), value.clone()));
        return Ok(value);
    }
    let eid = field(&r.args, "endpoint_id")?;
    let operation = match r.tool.as_str() {
        "terminal.observe" => "observe",
        "terminal.write" => "send",
        "terminal.acquire" | "terminal.release" | "terminal.renew" | "terminal.resize" => "control",
        "terminal.stop" | "terminal.force" | "terminal.restart" => "manage",
        _ => return Err(error("Unsupported", "unknown terminal tool")),
    };
    let authorization = s.engine.execute(
        token,
        &ToolRequest {
            request_id: format!("auth:{}", uuid::Uuid::new_v4()),
            tool: "endpoint.authorize".into(),
            space_id: r.space_id.clone(),
            args: json!({"endpoint_id":eid,"operation":operation}),
        },
    )?;
    let identity = authorization["identity_id"]
        .as_str()
        .ok_or_else(|| error("Internal", "missing identity"))?;
    let fingerprint = serde_json::to_string(r).map_err(|e| error("Invalid", e))?;
    let key = format!("{}:{}", identity, r.request_id);
    if r.tool != "terminal.observe" {
        if let Some((old, value)) = n.receipts.get(&key) {
            if old != &fingerprint {
                return Err(error("RequestConflict", "request id content differs"));
            }
            return Ok(value.clone());
        }
    }
    if r.tool == "terminal.restart" {
        let marker = s
            .intent_dir
            .join(format!("restart-{:x}", Sha256::digest(key.as_bytes())));
        if marker.exists() {
            return Err(error(
                "UnknownEffect",
                "restart was already attempted; inspect history before issuing a new request",
            ));
        }
        let expected = field(&r.args, "expected_session_id")?;
        let history = s.history.list(eid).map_err(|e| error("HistoryError", e))?;
        let latest = n
            .sessions
            .get(eid)
            .map(String::as_str)
            .or_else(|| history.last().map(|h| h.session.id.as_str()))
            .ok_or_else(|| error("NotFound", "no prior recorded session"))?;
        if latest != expected {
            return Err(error(
                "StaleSession",
                "expected_session_id is not the latest session",
            ));
        }
        if s.terminals
            .list()
            .iter()
            .any(|session| session.id == expected && session.status != "exited")
        {
            return Err(error(
                "SessionAlive",
                "original owned process tree is still alive",
            ));
        }
        let config = &authorization["endpoint"]["config"];
        if authorization["endpoint"]["kind"] != "terminal" {
            return Err(error("Invalid", "endpoint is not terminal"));
        }
        let command = field(config, "command")?.to_string();
        let args: Vec<String> =
            serde_json::from_value(config["args"].clone()).map_err(|e| error("Invalid", e))?;
        let cwd = config["cwd"].as_str().map(std::path::PathBuf::from);
        let mut intent = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .map_err(|e| error("UnknownEffect", e))?;
        use std::io::Write;
        intent
            .write_all(fingerprint.as_bytes())
            .and_then(|_| intent.sync_all())
            .map_err(|e| error("Internal", e))?;
        let session = s
            .terminals
            .create_recorded(eid, &s.history, command, args, cwd)
            .map_err(|e| error("NativeError", e))?;
        n.sessions.insert(eid.into(), session.id.clone());
        let value = json!({"endpoint":authorization["endpoint"],"session":session,"previous_session_id":expected,"process_resume":"unsupported"});
        n.receipts.insert(key, (fingerprint, value.clone()));
        return Ok(value);
    }
    let sid = n.sessions.get(eid).ok_or_else(|| {
        error(
            "Interrupted",
            "terminal is not owned by this runtime; explicit new session required",
        )
    })?;
    if n.uncertain.contains(&key) {
        return Err(error("UnknownEffect","previous native call did not finish successfully; inspect before issuing a new request"));
    }
    if r.tool != "terminal.observe" {
        n.uncertain.insert(key.clone());
    }
    let generation = || {
        r.args["generation"]
            .as_u64()
            .ok_or_else(|| error("Invalid", "generation required"))
    };
    let native = |e: String| error("NativeError", e);
    let value = match r.tool.as_str() {
        "terminal.observe" => json!(s
            .terminals
            .observe(sid, r.args["cursor"].as_u64().unwrap_or(0))
            .map_err(native)?),
        "terminal.acquire" => json!(s.terminals.acquire(sid, identity).map_err(native)?),
        "terminal.renew" => json!(s
            .terminals
            .renew(sid, identity, field(&r.args, "lease")?, generation()?)
            .map_err(native)?),
        "terminal.release" => {
            s.terminals
                .release(sid, identity, field(&r.args, "lease")?)
                .map_err(native)?;
            json!({"released":true})
        }
        "terminal.write" => {
            s.terminals
                .write(
                    sid,
                    identity,
                    field(&r.args, "lease")?,
                    generation()?,
                    field(&r.args, "input")?.as_bytes(),
                )
                .map_err(native)?;
            json!({"accepted":true})
        }
        "terminal.resize" => {
            let cols = r.args["cols"]
                .as_u64()
                .filter(|v| *v > 0 && *v <= 1000)
                .ok_or_else(|| error("Invalid", "cols 1..1000"))?;
            let rows = r.args["rows"]
                .as_u64()
                .filter(|v| *v > 0 && *v <= 1000)
                .ok_or_else(|| error("Invalid", "rows 1..1000"))?;
            s.terminals
                .resize(
                    sid,
                    identity,
                    field(&r.args, "lease")?,
                    generation()?,
                    cols as u16,
                    rows as u16,
                )
                .map_err(native)?;
            json!({"resized":true})
        }
        "terminal.stop" => json!(s
            .terminals
            .stop_owned(sid, Duration::from_millis(300))
            .map_err(native)?),
        "terminal.force" => json!(s
            .terminals
            .force_owned(sid, Duration::from_millis(500))
            .map_err(native)?),
        _ => unreachable!(),
    };
    if r.tool != "terminal.observe" {
        n.uncertain.remove(&key);
        n.receipts.insert(key, (fingerprint, value.clone()));
    }
    Ok(value)
}
