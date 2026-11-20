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
    // Mirrors NativeState.draining so health never waits behind the gate.
    draining: Arc<std::sync::atomic::AtomicBool>,
    // Endpoint -> current owned session. Its own lock lets observe skip the gate;
    // lock order is always gate before sessions.
    sessions: Arc<Mutex<HashMap<String, String>>>,
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
/// What a request_id did, so a resend never acts twice: `Some(reply)` once it
/// succeeded (a resend gets the same reply), `None` while its effect is unknown
/// (a resend answers UnknownEffect). Bounded (K2): each identity keeps its newest
/// `RECEIPT_CAP_PER_IDENTITY` receipts for `RECEIPT_TTL`; an older one becomes a
/// tombstone that keeps only the request's digest and answers UnknownEffect.
/// Only a request whose tombstone is among the oldest beyond `TOMBSTONE_CAP`
/// (shared by all identities) is forgotten, and a resend of it would run again.
#[derive(Default)]
struct Receipts {
    live: HashMap<String, (String, Option<Value>, std::time::Instant)>,
    queues: HashMap<String, std::collections::VecDeque<(std::time::Instant, String)>>,
    tombstones: HashMap<String, String>,
    tombstone_order: std::collections::VecDeque<String>,
}
const RECEIPT_TTL: Duration = Duration::from_secs(3600);
const RECEIPT_CAP_PER_IDENTITY: usize = 2_000;
const TOMBSTONE_CAP: usize = 200_000;
/// A request is remembered by its digest, never its (possibly large) body.
fn request_digest(request: &str) -> String {
    format!("{:x}", Sha256::digest(request.as_bytes()))
}
impl Receipts {
    /// `Ok(Some(reply))` replays, `Ok(None)` means never seen.
    fn lookup(
        &self,
        key: &str,
        digest: &str,
    ) -> Result<Option<Value>, crate::runtime_core::ApiError> {
        let known = self
            .live
            .get(key)
            .map(|(d, reply, _)| (d, reply.as_ref()))
            .or_else(|| self.tombstones.get(key).map(|d| (d, None)));
        match known {
            None => Ok(None),
            Some((old, _)) if old != digest => {
                Err(error("RequestConflict", "request id content differs"))
            }
            Some((_, Some(reply))) => Ok(Some(reply.clone())),
            Some((_, None)) => Err(error(
                "UnknownEffect",
                "the earlier call with this request_id may have acted, or its receipt expired; inspect before issuing a new request_id",
            )),
        }
    }
    /// Marks the request as in flight: until `finish`, a resend is UnknownEffect.
    fn begin(&mut self, key: &str, digest: &str, now: std::time::Instant) {
        let bucket = key.split(':').next().unwrap_or_default().to_string();
        self.live.insert(key.into(), (digest.into(), None, now));
        let queue = self.queues.entry(bucket.clone()).or_default();
        queue.push_back((now, key.into()));
        self.prune(&bucket, now);
    }
    fn finish(&mut self, key: &str, reply: Value) {
        if let Some(entry) = self.live.get_mut(key) {
            entry.1 = Some(reply);
        }
    }
    /// A completed request that had no in-flight mark (create, restart).
    fn record(&mut self, key: &str, digest: &str, reply: Value) {
        self.begin(key, digest, std::time::Instant::now());
        self.finish(key, reply);
    }
    fn prune(&mut self, bucket: &str, now: std::time::Instant) {
        let Some(queue) = self.queues.get_mut(bucket) else {
            return;
        };
        while let Some((at, key)) = queue.front() {
            let old = now.saturating_duration_since(*at) > RECEIPT_TTL;
            if !old && queue.len() <= RECEIPT_CAP_PER_IDENTITY {
                break;
            }
            if self
                .live
                .get(key)
                .is_some_and(|(_, _, stored)| stored == at)
            {
                let (digest, _, _) = self.live.remove(key).expect("checked above");
                if self.tombstones.insert(key.clone(), digest).is_none() {
                    self.tombstone_order.push_back(key.clone());
                }
                while self.tombstone_order.len() > TOMBSTONE_CAP {
                    if let Some(oldest) = self.tombstone_order.pop_front() {
                        self.tombstones.remove(&oldest);
                    }
                }
            }
            queue.pop_front();
        }
    }
}
/// Takes the dispatch gate, recovering it if a call panicked while holding it
/// (K17, K18). The state is conservative: a terminal call marks its receipt in
/// flight before any native effect, and terminal create/restart persist an
/// intent marker first, so a retry answers UnknownEffect either way. But the
/// effect, such as a process it spawned, may have changed the shutdown impact,
/// so the impact epoch advances and an earlier shutdown ticket goes stale.
/// A shutdown ticket admits only the impact it was prepared for, for a minute.
fn ticket_current(n: &NativeState, ticket: &str) -> bool {
    n.ticket.as_ref().is_some_and(|(t, epoch, _, issued)| {
        t == ticket && *epoch == n.epoch && issued.elapsed() <= Duration::from_secs(60)
    })
}
fn lock_gate(gate: &Mutex<NativeState>) -> std::sync::MutexGuard<'_, NativeState> {
    match gate.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            gate.clear_poison();
            let mut guard = poisoned.into_inner();
            guard.epoch += 1;
            guard
        }
    }
}
#[derive(Default)]
struct NativeState {
    receipts: Receipts,
    epoch: u64,
    ticket: Option<(String, u64, u64, std::time::Instant)>,
    draining: bool,
}

pub fn router(
    dir: &Path,
    host: &str,
) -> anyhow::Result<(Router, tokio::sync::watch::Receiver<bool>)> {
    router_inner(dir, host, None).map(|(r, s, _)| (r, s))
}
/// Like [`router`], plus the flag that turns true once shutdown is admitted, so
/// other health endpoints of the same daemon never report a draining runtime ready.
pub fn router_with_draining(
    dir: &Path,
    host: &str,
) -> anyhow::Result<(
    Router,
    tokio::sync::watch::Receiver<bool>,
    Arc<std::sync::atomic::AtomicBool>,
)> {
    router_inner(dir, host, None)
}
/// Trusted in-process injection used by embedders; no HTTP plan installation.
pub fn router_with_bridge(
    dir: &Path,
    host: &str,
    bridge: Arc<OwnedControlBridge>,
) -> anyhow::Result<(Router, tokio::sync::watch::Receiver<bool>)> {
    router_inner(dir, host, Some(bridge)).map(|(r, s, _)| (r, s))
}
fn router_inner(
    dir: &Path,
    host: &str,
    bridge: Option<Arc<OwnedControlBridge>>,
) -> anyhow::Result<(
    Router,
    tokio::sync::watch::Receiver<bool>,
    Arc<std::sync::atomic::AtomicBool>,
)> {
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
    let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let state = HttpState {
        owned,
        engine,
        host: host.into(),
        gate: Arc::new(Mutex::new(NativeState::default())),
        draining: draining.clone(),
        sessions: Default::default(),
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
            .route("/v2/prove", post(prove))
            .route("/console", get(console))
            .route("/console/", get(console))
            .route("/console/live.js", get(script))
            .route("/console/live.css", get(style))
            .layer(tower_http::limit::RequestBodyLimitLayer::new(256 * 1024))
            .with_state(state),
        receiver,
        draining,
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
        json!({"status":if s.draining.load(std::sync::atomic::Ordering::SeqCst)||s.gate.is_poisoned(){"draining"}else{"ready"},"protocol":"v2"}),
    )
}
/// K8: proves to a bare-address caller, before it sends its credential, that
/// this Runtime already knows that credential. Needs no credential itself.
async fn prove(State(s): State<HttpState>, headers: HeaderMap, Json(body): Json<Value>) -> Reply {
    if headers.get("host").and_then(|h| h.to_str().ok()) != Some(s.host.as_str()) {
        return failure(StatusCode::FORBIDDEN, "Denied", "unexpected Host");
    }
    if let Some(origin) = headers.get("origin") {
        if origin.to_str().ok() != Some(format!("http://{}", s.host).as_str()) {
            return failure(StatusCode::FORBIDDEN, "Denied", "unexpected Origin");
        }
    }
    let hex = |v: &Value, len: usize| {
        v.as_str()
            .filter(|t| t.len() == len && t.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_string)
    };
    let (Some(key_id), Some(nonce)) = (hex(&body["key_id"], 32), hex(&body["nonce"], 64)) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "Invalid",
            "key_id and nonce required",
        );
    };
    let engine = s.engine.clone();
    let host = s.host.clone();
    match tokio::task::spawn_blocking(move || engine.prove_credential(&key_id, &host, &nonce)).await
    {
        Ok(Ok(Some(proof))) => (StatusCode::OK, Json(json!({"proof": proof}))),
        Ok(Ok(None)) => failure(StatusCode::NOT_FOUND, "NotFound", "unknown credential"),
        Ok(Err(e)) => reply(Err(e)),
        Err(_) => panicked(true, "prove failed"),
    }
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
        let read = read_only(&request.tool);
        return match tokio::task::spawn_blocking(move || bridge.execute(&actor, &request)).await {
            Ok(result) => reply(result),
            Err(_) => panicked(read, "owned dispatch failed"),
        };
    }
    if matches!(
        request.tool.as_str(),
        "runtime.confirm_shutdown" | "runtime.force_shutdown"
    ) {
        return shutdown_tools(state.clone(), token.to_string(), request).await;
    }
    // Observing one terminal must not wait behind another agent's slow native call.
    if request.tool == "terminal.observe" {
        let token = token.to_string();
        return match tokio::task::spawn_blocking(move || observe_unlocked(&state, &token, &request))
            .await
        {
            Ok(result) => reply(result),
            Err(_) => failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                "observe failed",
            ),
        };
    }
    let token = token.to_string();
    let read = read_only(&request.tool);
    let gate = state.gate.clone();
    match tokio::task::spawn_blocking(move || gated(&state, &token, &request)).await {
        Ok(reply) => reply,
        Err(_) => {
            // The interrupted call marked itself uncertain before any native effect
            // and records receipts only after success, so the gate's state stays
            // conservative: a same-ID retry answers UnknownEffect. Keep the runtime
            // usable (and stoppable) instead of failing every later call; recover
            // now so health stops reporting draining and the epoch advances.
            let _ = tokio::task::spawn_blocking(move || drop(lock_gate(&gate))).await;
            panicked(read, "dispatch failed")
        }
    }
}
fn read_only(tool: &str) -> bool {
    tool.ends_with(".list")
        || tool.ends_with(".read")
        || tool.ends_with(".get")
        || tool == "history.checkpoint"
        || tool == "terminal.observe"
        || tool == "endpoint.authorize"
        || tool == "identity.current"
}
/// A panic part-way through a mutation may already have changed state or a
/// native process; the caller must inspect, never assume it failed.
fn panicked(read: bool, what: &str) -> Reply {
    if read {
        failure(StatusCode::INTERNAL_SERVER_ERROR, "Internal", what)
    } else {
        failure(
            StatusCode::CONFLICT,
            "UnknownEffect",
            &format!(
                "{what}: the operation may have taken effect; inspect before issuing a new request"
            ),
        )
    }
}
fn observe_unlocked(
    s: &HttpState,
    token: &str,
    r: &ToolRequest,
) -> Result<Value, crate::runtime_core::ApiError> {
    if r.request_id.trim().is_empty() {
        return Err(error("Invalid", "request_id required"));
    }
    let eid = field(&r.args, "endpoint_id")?;
    let authorize = || {
        s.engine.execute(
            token,
            &ToolRequest {
                request_id: format!("auth:{}", uuid::Uuid::new_v4()),
                tool: "endpoint.authorize".into(),
                space_id: r.space_id.clone(),
                args: json!({"endpoint_id":eid,"operation":"observe"}),
            },
        )
    };
    authorize()?;
    let sid = owned_sessions(s)?.get(eid).cloned().ok_or_else(|| {
        error(
            "Interrupted",
            "terminal is not owned by this runtime; explicit new session required",
        )
    })?;
    let view = s
        .terminals
        .observe(&sid, r.args["cursor"].as_u64().unwrap_or(0))
        .map_err(|e| error("NativeError", e))?;
    // Outside the gate a revocation can commit while observe waits for the
    // session; re-check so output produced after it is never returned.
    authorize()?;
    Ok(json!(view))
}
fn owned_sessions(
    s: &HttpState,
) -> Result<std::sync::MutexGuard<'_, HashMap<String, String>>, crate::runtime_core::ApiError> {
    // Only inserts and lookups happen under this lock, so a panic leaves it
    // consistent; recovering keeps terminals and the revocation fence working
    // instead of failing every later call (K16).
    Ok(s.sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}
fn gated(state: &HttpState, token: &str, request: &ToolRequest) -> Reply {
    let mut guard = lock_gate(&state.gate);
    let read = read_only(&request.tool);
    if guard.draining
        && !read
        && !request.tool.starts_with("runtime.")
        && !matches!(
            request.tool.as_str(),
            // Revoking a credential only removes authority, so it stays possible
            // while a stuck shutdown keeps the runtime draining.
            "terminal.stop" | "terminal.force" | "terminal.release" | "members.revoke_credential"
        )
    {
        return failure(
            StatusCode::CONFLICT,
            "Draining",
            "runtime is draining; new mutations disabled",
        );
    }
    // A resent rotation only replays its reply: the lease the member took since
    // with the new credential must not be fenced as if it were the old one (K14).
    // An unreadable state fails toward fencing.
    let replayed_rotation = request.tool == "members.rotate_credential"
        && state
            .engine
            .has_receipt(token, &request.request_id)
            .unwrap_or(false);
    let result = dispatch(state, &mut guard, token, request);
    if result.is_ok() && !read && !request.tool.starts_with("runtime.") {
        guard.epoch += 1;
    }
    if result.is_ok()
        && matches!(
            request.tool.as_str(),
            "endpoints.grant"
                | "members.set_roles"
                | "members.revoke_credential"
                | "members.rotate_credential"
        )
    {
        // Still under the gate, so no acquire or write can interleave. A
        // rotation cuts off the old credential, so the member's leases end too.
        let rotated = (request.tool == "members.rotate_credential" && !replayed_rotation)
            .then(|| request.args["member_id"].as_str())
            .flatten();
        fence_revoked_holders(state, None, rotated);
    }
    reply(result)
}
/// Ends every held lease whose holder no longer permits control (or is `rotated`).
/// `only` limits the check to one endpoint (the lazy fence at acquire, which also
/// covers time-based credential expiry).
fn fence_revoked_holders(s: &HttpState, only: Option<&str>, rotated: Option<&str>) {
    // Copy the map so observe's fast path is not held behind state reads.
    let sessions: Vec<(String, String)> = match owned_sessions(s) {
        Ok(map) => map
            .iter()
            .filter(|(eid, _)| only.is_none_or(|o| o == eid.as_str()))
            .map(|(e, sid)| (e.clone(), sid.clone()))
            .collect(),
        Err(_) => return,
    };
    for (eid, sid) in sessions {
        if let Some(holder) = s.terminals.holder(&sid) {
            // An unreadable state fails closed: the holder must re-acquire.
            if rotated == Some(holder.as_str())
                || !s
                    .engine
                    .identity_permits(&holder, &eid, "control")
                    .unwrap_or(false)
            {
                s.terminals.revoke_control(&sid, &holder);
            }
        }
    }
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
    // The gate is a blocking lock: take it on a blocking thread, never on the
    // async runtime, which would stall every other request while it waits.
    let admitting = state.clone();
    let args = request.args.clone();
    let admission = tokio::task::spawn_blocking(move || {
        let state = admitting;
        let ticket = field(&args, "ticket")?;
        let mut guard = lock_gate(&state.gate);
        if !ticket_current(&guard, ticket) {
            return Err(error(
                "StaleImpact",
                "impact changed or ticket expired; prepare shutdown again",
            ));
        }
        state
            .owned
            .begin_shutdown(guard.ticket.as_ref().unwrap().2)?;
        guard.draining = true;
        state
            .draining
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    })
    .await
    .unwrap_or_else(|_| {
        // A panic here may already have begun the shutdown; the next caller
        // recovers the gate (lock_gate) with a new epoch, so prepare again.
        Err(error(
            "UnknownEffect",
            "shutdown admission failed; inspect runtime health, then prepare shutdown again",
        ))
    });
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
        // Cleanup may already have stopped terminals or cancelled work.
        Err(_) => panicked(false, "shutdown cleanup failed"),
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
// Raw terminal input: whitespace-only keys (Enter, Tab, Space) are real keystrokes.
fn raw_input(args: &Value) -> Result<&str, crate::runtime_core::ApiError> {
    args["input"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error("Invalid", "input required"))
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
                let owned = owned_sessions(s)?.clone();
                for ep in items {
                    if ep["kind"] == "terminal" {
                        let active = ep["id"]
                            .as_str()
                            .and_then(|eid| owned.get(eid))
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
    // Bounded like the owned bridge's: the request_id is kept in every receipt (K2).
    if r.request_id.trim().is_empty() || r.request_id.len() > 256 || r.request_id.contains('\0') {
        return Err(error("Invalid", "bounded request_id required"));
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
        if let Some(value) = n.receipts.lookup(&key, "")? {
            return Ok(value);
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
        owned_sessions(s)?.insert(eid, session.id.clone());
        let value = json!({"endpoint":ep,"session":session});
        n.receipts.record(&key, "", value.clone());
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
    let digest = request_digest(&fingerprint);
    if r.tool != "terminal.observe" {
        if let Some(value) = n.receipts.lookup(&key, &digest)? {
            return Ok(value);
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
        let current = owned_sessions(s)?.get(eid).cloned();
        let latest = current
            .as_deref()
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
        let Some(approver) = authorization["endpoint"]["launch_approval"]["approved_by"].as_str()
        else {
            return Err(error(
                "Denied",
                "terminal launch lacks a human administrator's approval; a human administrator can approve it with endpoints.approve_launch",
            ));
        };
        let space = authorization["endpoint"]["space_id"]
            .as_str()
            .unwrap_or_default();
        // Approvals made before generations were recorded count as generation 0.
        let generation = authorization["endpoint"]["launch_approval"]["approver_generation"]
            .as_u64()
            .unwrap_or(0);
        // For terminal.restart only: HTTP revocations are serialized behind the
        // same gate, so none lands between this check and the spawn; only a
        // writer outside this Runtime could, within milliseconds, and revocation
        // never stops a live process. (Owned launches recheck in approve().)
        if !s
            .engine
            .launch_approver_trusted(approver, space, generation)?
        {
            return Err(error(
                "Denied",
                "the administrator who approved this launch is no longer trusted; a current human administrator can approve it again with endpoints.approve_launch",
            ));
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
        owned_sessions(s)?.insert(eid.into(), session.id.clone());
        let value = json!({"endpoint":authorization["endpoint"],"session":session,"previous_session_id":expected,"process_resume":"unsupported"});
        n.receipts.record(&key, &digest, value.clone());
        return Ok(value);
    }
    let sid = &owned_sessions(s)?.get(eid).cloned().ok_or_else(|| {
        error(
            "Interrupted",
            "terminal is not owned by this runtime; explicit new session required",
        )
    })?;
    // An in-flight or failed call is already answered UnknownEffect by lookup().
    if r.tool != "terminal.observe" {
        n.receipts.begin(&key, &digest, std::time::Instant::now());
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
        "terminal.acquire" => {
            fence_revoked_holders(s, Some(eid), None);
            json!(s.terminals.acquire(sid, identity).map_err(native)?)
        }
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
                    raw_input(&r.args)?.as_bytes(),
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
        n.receipts.finish(&key, value.clone());
    }
    Ok(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    fn code(r: Result<Option<Value>, crate::runtime_core::ApiError>) -> String {
        match r {
            Ok(Some(_)) => "replay".into(),
            Ok(None) => "new".into(),
            Err(e) => e.code,
        }
    }
    #[test]
    fn resend_replays_and_unfinished_or_failed_call_is_unknown() {
        let mut receipts = Receipts::default();
        let now = Instant::now();
        assert_eq!(code(receipts.lookup("a:1", "d")), "new");
        receipts.begin("a:1", "d", now);
        // In flight, or failed after possibly acting: never run again.
        assert_eq!(code(receipts.lookup("a:1", "d")), "UnknownEffect");
        receipts.finish("a:1", json!({"ok":true}));
        assert_eq!(
            receipts.lookup("a:1", "d").unwrap(),
            Some(json!({"ok":true}))
        );
        // Same ID, different request.
        assert_eq!(code(receipts.lookup("a:1", "other")), "RequestConflict");
    }
    #[test]
    fn expired_receipt_becomes_tombstone_and_is_never_rerun() {
        let mut receipts = Receipts::default();
        let start = Instant::now();
        receipts.begin("a:stop", "d", start);
        receipts.finish("a:stop", json!({"stopped":true}));
        receipts.begin("a:next", "e", start + RECEIPT_TTL + Duration::from_secs(1));
        assert!(!receipts.live.contains_key("a:stop"));
        // The reply is gone, but a resend still cannot act a second time.
        assert_eq!(code(receipts.lookup("a:stop", "d")), "UnknownEffect");
        assert_eq!(code(receipts.lookup("a:stop", "x")), "RequestConflict");
    }
    #[test]
    fn one_identity_cannot_evict_another_and_each_stays_bounded() {
        let mut receipts = Receipts::default();
        let now = Instant::now();
        receipts.begin("victim:write", "d", now); // failed: effect unknown
        for i in 0..RECEIPT_CAP_PER_IDENTITY + 50 {
            let key = format!("noisy:{i}");
            receipts.begin(&key, "d", now);
            receipts.finish(&key, json!(i));
        }
        assert_eq!(code(receipts.lookup("victim:write", "d")), "UnknownEffect");
        assert!(receipts.live.contains_key("victim:write"));
        assert_eq!(receipts.queues["noisy"].len(), RECEIPT_CAP_PER_IDENTITY);
        // A successful call takes one slot, not two.
        assert_eq!(
            receipts
                .live
                .keys()
                .filter(|k| k.starts_with("noisy:"))
                .count(),
            RECEIPT_CAP_PER_IDENTITY
        );
        // The evicted ones are tombstones, still refusing a rerun.
        assert_eq!(code(receipts.lookup("noisy:0", "d")), "UnknownEffect");
    }
    #[test]
    fn tombstones_are_bounded_oldest_first() {
        let mut receipts = Receipts::default();
        let now = Instant::now();
        for i in 0..TOMBSTONE_CAP + RECEIPT_CAP_PER_IDENTITY + 5 {
            receipts.begin(&format!("a:{i}"), "d", now);
        }
        assert_eq!(receipts.tombstones.len(), TOMBSTONE_CAP);
        assert_eq!(receipts.tombstone_order.len(), TOMBSTONE_CAP);
        assert_eq!(code(receipts.lookup("a:0", "d")), "new");
        assert_eq!(code(receipts.lookup("a:5", "d")), "UnknownEffect");
    }
    #[test]
    fn request_digest_is_small_and_stable() {
        let big = "x".repeat(100_000);
        assert_eq!(request_digest(&big).len(), 64);
        assert_eq!(request_digest(&big), request_digest(&big));
        assert_ne!(request_digest(&big), request_digest("y"));
    }
    #[test]
    fn poisoned_gate_recovers_and_stales_earlier_shutdown_tickets() {
        // K17, K18: a call that panicked while holding the gate.
        let gate = Arc::new(Mutex::new(NativeState::default()));
        {
            let mut n = gate.lock().unwrap();
            n.epoch = 5;
            n.ticket = Some(("t".into(), 5, 0, Instant::now()));
            assert!(ticket_current(&n, "t"));
        }
        let holder = gate.clone();
        let _ = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            panic!("panic while holding the gate");
        })
        .join();
        assert!(gate.is_poisoned());
        {
            let n = lock_gate(&gate);
            assert_eq!(n.epoch, 6);
            // The panicked call may have changed the impact: the earlier ticket
            // no longer admits a shutdown.
            assert!(!ticket_current(&n, "t"));
        }
        assert!(
            !gate.is_poisoned(),
            "recovered once, for every later caller"
        );
        assert_eq!(lock_gate(&gate).epoch, 6);
    }
}
