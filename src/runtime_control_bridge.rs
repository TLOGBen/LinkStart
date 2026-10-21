//! Shared local/HTTP owned controls. A durable intent is never permission to retry.
use crate::{
    runtime_codex_host::{HostError, HostScope, OwnedCodexRegistry},
    runtime_core::{ApiError, Engine, ToolRequest},
    runtime_launch_policy::LaunchPolicyStore,
    runtime_participants::ParticipantService,
    runtime_supervisor::{
        OperationPhase, OwnedSessionSupervisor, SupervisorError, ThreadAdmission,
    },
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

fn error(code: &str, message: &str) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
    }
}

fn storage(_: impl std::fmt::Display) -> ApiError {
    error("Internal", "owned control storage unavailable")
}
fn id(v: &Value, key: &str) -> Result<String, ApiError> {
    v[key]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 256 && !s.contains('\0'))
        .map(Into::into)
        .ok_or_else(|| error("Invalid", "nonempty bounded ID required"))
}
fn positive(v: &Value, key: &str) -> Result<u64, ApiError> {
    v[key]
        .as_u64()
        .filter(|n| *n > 0 && *n <= i64::MAX as u64)
        .ok_or_else(|| error("Invalid", "positive generation/revision required"))
}
fn host_error(e: SupervisorError) -> ApiError {
    match e {
        SupervisorError::Denied | SupervisorError::Host(HostError::Denied(_)) => {
            error("Denied", "current authority or exact binding denied")
        }
        SupervisorError::QueueFull => error("QueueFull", "owned command queue full"),
        SupervisorError::Invalid => error("Invalid", "invalid owned command"),
        SupervisorError::Pending | SupervisorError::Host(HostError::Pending { .. }) => {
            error("Pending", "provider observation pending")
        }
        SupervisorError::Host(HostError::Unsupported(_)) => {
            error("Unsupported", "provider capability unsupported")
        }
        SupervisorError::Host(HostError::Unknown { .. }) => {
            error("Unknown", "provider outcome unknown; do not resend")
        }
        SupervisorError::Closed | SupervisorError::Conflict => {
            error("Conflict", "owned session command unavailable")
        }
        _ => error("Unknown", "provider outcome unavailable; do not resend"),
    }
}
#[derive(Default)]
struct Admission {
    draining: bool,
    reservations: usize,
    epoch: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct OwnedInventory {
    pub epoch: u64,
    pub launch_reservations: usize,
    pub active_workers: usize,
    pub draining: bool,
}
/// Source provenance is internal, not a caller-supplied phase or priority.
#[derive(Clone, Copy)]
enum ProjectionSource {
    Mutation,
    Observation,
    NoObservation,
}
fn definite_receipt(tool: &str, state: &str) -> bool {
    state == "rejected"
        || match tool {
            "owned.turn" | "owned.interrupt" | "owned.launch" => state == "confirmed",
            "owned.stop" => state == "exited",
            _ => false,
        }
}
fn resolves_unknown(source: ProjectionSource, tool: &str, state: &str, body: &Value) -> bool {
    if !matches!(source, ProjectionSource::Observation) {
        return false;
    }
    // Only a current exact provider ACK/InProgress witness clears uncertainty.
    // Queued, Dispatching, launch Starting and stop Pending are not completion.
    tool == "owned.turn"
        && ((state == "accepted"
            && body["snapshot"]["phase"] == "Accepted"
            && body["snapshot"]["provider_turn_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty()))
            || (state == "pending" && body["observation"]["outcome"] == "in_progress"))
}

fn terminal_outcome(value: &Value) -> bool {
    matches!(value.as_str(), Some("completed" | "interrupted" | "failed"))
}

/// Compare verified native facts, never just equal JSON envelope states.
fn same_turn_fact(retained: &Value, observation: &Value, operation: &str) -> bool {
    if retained["state"] != "confirmed" || retained["operation_id"] != operation {
        return false;
    }
    if let Some(existing) = retained.get("observation") {
        return existing == observation;
    }
    let snapshot = &retained["snapshot"];
    snapshot["operation_id"] == operation
        && snapshot["phase"] == "Confirmed"
        && terminal_outcome(&snapshot["provider_outcome"])
        && snapshot["provider_outcome"] == observation["outcome"]
        && snapshot["provider_turn_id"] == observation["turn_id"]
}
pub struct OwnedControlBridge {
    path: PathBuf,
    engine: Engine,
    supervisor: Arc<OwnedSessionSupervisor>,
    registry: OwnedCodexRegistry,
    participants: Arc<ParticipantService>,
    plans: Arc<LaunchPolicyStore>,
    admission: Mutex<Admission>,
    #[cfg(test)]
    projection_test_hook: Mutex<Option<Arc<dyn Fn(&str, &str) + Send + Sync>>>,
}
struct Reservation<'a>(&'a OwnedControlBridge);
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let draining = {
            let mut a = self.0.admission.lock().unwrap_or_else(|e| e.into_inner());
            a.reservations -= 1;
            a.epoch += 1;
            a.draining
        };
        // A launch admitted before draining can finish registration afterwards.
        // The reservation stays in shutdown inventory until this cleanup request.
        if draining {
            self.0.supervisor.request_shutdown();
        }
    }
}
impl OwnedControlBridge {
    pub fn open(
        dir: &Path,
        engine: Engine,
        supervisor: Arc<OwnedSessionSupervisor>,
        registry: OwnedCodexRegistry,
        participants: Arc<ParticipantService>,
        plans: Arc<LaunchPolicyStore>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            dir.canonicalize()? == engine.private_storage_directory()?,
            "bridge storage must match Engine"
        );
        crate::runtime_core::protect(dir)?;
        let path = dir.join("v2-owned-control.sqlite");
        if path.exists() {
            crate::runtime_core::protect(&path)?;
        }
        let mut db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS requests(space TEXT NOT NULL,actor TEXT NOT NULL,request TEXT NOT NULL,fingerprint TEXT NOT NULL,operation TEXT UNIQUE NOT NULL,session TEXT NOT NULL,tool TEXT NOT NULL,state TEXT NOT NULL,body TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(space,actor,request));
          CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,scope TEXT NOT NULL,profile TEXT NOT NULL,status TEXT);")?;
        let columns = tx
            .prepare("PRAGMA table_info(requests)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !columns.iter().any(|name| name == "revision") {
            tx.execute_batch(
                "ALTER TABLE requests ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        // Recovery is part of the same durable owner transaction. Fence every
        // unresolved pre-open observation, including an already-Unknown row.
        tx.execute_batch("UPDATE requests SET state='unknown',body=json_set(body,'$.state','unknown','$.evidence','runtime reopened; original side effect is not retried'),revision=revision+1 WHERE state IN ('intent','accepted','pending','unknown');")?;
        tx.commit()?;
        crate::runtime_core::protect(&path)?;
        Ok(Self {
            path,
            engine,
            supervisor,
            registry,
            participants,
            plans,
            admission: Mutex::new(Admission::default()),
            #[cfg(test)]
            projection_test_hook: Mutex::new(None),
        })
    }
    fn db(&self) -> Result<Connection, ApiError> {
        let db = Connection::open(&self.path).map_err(storage)?;
        db.busy_timeout(Duration::from_secs(5)).map_err(storage)?;
        Ok(db)
    }
    fn reserve(&self) -> Result<Reservation<'_>, ApiError> {
        let mut a = self.admission.lock().map_err(storage)?;
        if a.draining {
            return Err(error("Draining", "runtime is draining"));
        }
        a.reservations += 1;
        a.epoch += 1;
        Ok(Reservation(self))
    }
    pub fn inventory(&self) -> OwnedInventory {
        let a = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        OwnedInventory {
            epoch: a.epoch,
            launch_reservations: a.reservations,
            active_workers: self.supervisor.active_count(),
            draining: a.draining,
        }
    }
    /// Linearization point: closes admission before requesting worker cleanup.
    pub fn begin_shutdown(&self, expected_epoch: u64) -> Result<OwnedInventory, ApiError> {
        {
            let mut a = self.admission.lock().map_err(storage)?;
            if a.epoch != expected_epoch {
                return Err(error(
                    "StaleImpact",
                    "owned inventory changed; prepare shutdown again",
                ));
            }
            a.draining = true;
        }
        self.supervisor.request_shutdown();
        Ok(self.inventory())
    }
    fn authorize(
        &self,
        actor: &str,
        space: &str,
        endpoint: &str,
        grant: &str,
    ) -> Result<String, ApiError> {
        self.engine
            .with_endpoint_authorization(actor, space, endpoint, grant, |auth| {
                auth["identity_id"]
                    .as_str()
                    .map(Into::into)
                    .ok_or_else(|| error("Internal", "identity unavailable"))
            })
    }
    fn scope(
        &self,
        space: &str,
        endpoint: &str,
        session: &str,
        generation: u64,
    ) -> Result<HostScope, ApiError> {
        let encoded: Option<String> = self
            .db()?
            .query_row("SELECT scope FROM sessions WHERE id=?1", [session], |r| {
                r.get(0)
            })
            .optional()
            .map_err(storage)?;
        let scope: HostScope = serde_json::from_str(
            &encoded.ok_or_else(|| error("NotFound", "owned session not found"))?,
        )
        .map_err(storage)?;
        if scope.space_id != space || scope.endpoint_id != endpoint {
            return Err(error("Denied", "session differs from authorized target"));
        }
        if scope.binding_generation != generation {
            return Err(error(
                "StaleGeneration",
                "exact current generation required",
            ));
        }
        Ok(scope)
    }
    /// The sole projection owner: merge under IMMEDIATE, retain irreversible
    /// evidence, fence stale non-final snapshots, and return what was committed.
    fn save(
        &self,
        operation: &str,
        observed_revision: u64,
        source: ProjectionSource,
        state: &str,
        body: &Value,
    ) -> Result<Value, ApiError> {
        #[cfg(test)]
        {
            let hook = self.projection_test_hook.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook(operation, state);
            }
        }
        if body["operation_id"] != operation || body["state"] != state {
            return Err(error(
                "Internal",
                "receipt projection identity/state mismatch",
            ));
        }
        let mut db = self.db()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let (tool, current_state, encoded, revision, session): (
            String,
            String,
            String,
            u64,
            String,
        ) = tx
            .query_row(
                "SELECT tool,state,body,revision,session FROM requests WHERE operation=?1",
                [operation],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .map_err(storage)?;
        let retained: Value = serde_json::from_str(&encoded).map_err(storage)?;
        let mut enriched = None;
        if tool == "owned.turn" && state == "confirmed" && body.get("observation").is_some() {
            let observation = &body["observation"];
            let scope: String = tx
                .query_row("SELECT scope FROM sessions WHERE id=?1", [&session], |r| {
                    r.get(0)
                })
                .map_err(storage)?;
            let scope: HostScope = serde_json::from_str(&scope).map_err(storage)?;
            let metadata = self
                .registry
                .durable_thread_metadata(&scope)
                .map_err(|_| error("Conflict", "exact durable thread evidence unavailable"))?
                .ok_or_else(|| error("Conflict", "exact durable thread evidence unavailable"))?;
            if observation["operation_id"] != operation
                || observation["thread_id"] != metadata.provider_thread_id
                || !observation["turn_id"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                || !terminal_outcome(&observation["outcome"])
            {
                return Err(error(
                    "Conflict",
                    "typed observation differs from exact operation binding",
                ));
            }
            if definite_receipt(&tool, &current_state) {
                if !same_turn_fact(&retained, observation, operation) {
                    return Err(error(
                        "Conflict",
                        "typed observation contradicts retained operation evidence",
                    ));
                }
                let mut merged = retained.clone();
                // Preserve every original field/result; only add absent evidence.
                if merged.get("observation").is_none() {
                    merged["observation"] = observation.clone();
                }
                enriched = Some(merged);
            }
        }
        // These are operation-specific historical facts, not a ranking between
        // Completed/Interrupted/Failed or between rejection and confirmation.
        // A contradictory definite observation never silently replaces one.
        let keep = definite_receipt(&tool, &current_state)
            || matches!(source, ProjectionSource::NoObservation)
            || (!definite_receipt(&tool, state)
                && (revision != observed_revision
                    || (current_state == "unknown"
                        && state != "unknown"
                        && !resolves_unknown(source, &tool, state, body))));
        let proposed = enriched.as_ref().unwrap_or(body);
        let result = if (keep && enriched.is_none()) || retained == *proposed {
            retained
        } else {
            let next = revision
                .checked_add(1)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| error("Internal", "receipt revision exhausted"))?;
            let changed=tx.execute("UPDATE requests SET state=?1,body=?2,revision=?3 WHERE operation=?4 AND revision=?5",params![state,proposed.to_string(),next,operation,revision]).map_err(storage)?;
            if changed != 1 {
                return Err(error("Internal", "receipt owner revision changed"));
            }
            proposed.clone()
        };
        tx.commit().map_err(storage)?;
        Ok(result)
    }
    fn status(&self, actor: &str, scope: &HostScope) -> Result<Value, ApiError> {
        let db = self.db()?;
        let profile: String = db
            .query_row(
                "SELECT profile FROM sessions WHERE id=?1",
                [&scope.session_id],
                |r| r.get(0),
            )
            .map_err(storage)?;
        let mut result = match self.supervisor.get_as(actor, scope) {
            Ok(status) => {
                let value = json!(status);
                db.execute(
                    "UPDATE sessions SET status=?1 WHERE id=?2",
                    params![value.to_string(), scope.session_id],
                )
                .map_err(storage)?;
                value
            }
            Err(SupervisorError::Denied) => {
                let persisted: Option<String> = db
                    .query_row(
                        "SELECT status FROM sessions WHERE id=?1",
                        [&scope.session_id],
                        |r| r.get(0),
                    )
                    .map_err(storage)?;
                let prior: Value = persisted
                    .and_then(|v| serde_json::from_str(&v).ok())
                    .unwrap_or(Value::Null);
                if prior["worker_exited"] == true {
                    prior
                } else {
                    json!({"scope":scope,"phase":"Unknown","worker_exited":false,"evidence":"no live supervisor instance after runtime reopen"})
                }
            }
            Err(e) => return Err(host_error(e)),
        };
        result["profile"] = serde_json::from_str(&profile).map_err(storage)?;
        if let Some(metadata) =
            self.registry
                .durable_thread_metadata(scope)
                .map_err(|e| match e {
                    HostError::Denied(_) => error(
                        "Conflict",
                        "durable owned thread identity or generation differs",
                    ),
                    _ => error("Internal", "durable owned thread metadata unavailable"),
                })?
        {
            if metadata.profile != result["profile"]
                || (result["provider_thread_id"].is_string()
                    && result["provider_thread_id"] != metadata.provider_thread_id)
            {
                return Err(error(
                    "Conflict",
                    "durable owned thread profile or identity differs",
                ));
            }
            result["provider_thread_id"] = json!(metadata.provider_thread_id);
            result["model"] = json!(metadata.model);
        }
        result["capabilities"] = json!(["owned_stdio", "exact_thread", "precise_interrupt"]);
        self.authorize(actor, &scope.space_id, &scope.endpoint_id, "observe")?;
        Ok(result)
    }
    pub fn execute(&self, actor: &str, r: &ToolRequest) -> Result<Value, ApiError> {
        if r.tool == "operations.get" {
            return match r.args["source_kind"].as_str() {
                Some("owned_session") => {
                    let mut mapped = r.clone();
                    mapped.tool = "owned.operation.get".into();
                    mapped
                        .args
                        .as_object_mut()
                        .ok_or_else(|| error("Invalid", "args object required"))?
                        .remove("source_kind");
                    self.execute(actor, &mapped)
                }
                Some("participant") => {
                    let mut mapped = r.clone();
                    mapped
                        .args
                        .as_object_mut()
                        .ok_or_else(|| error("Invalid", "args object required"))?
                        .remove("source_kind");
                    self.participants.execute(actor, &mapped)
                }
                _ => Err(error(
                    "Unsupported",
                    "explicit supported source_kind required",
                )),
            };
        }
        if r.request_id.trim().is_empty() || r.request_id.len() > 256 || r.request_id.contains('\0')
        {
            return Err(error("Invalid", "bounded request_id required"));
        }
        let space = r
            .space_id
            .as_deref()
            .filter(|s| !s.trim().is_empty() && s.len() <= 256)
            .ok_or_else(|| error("Invalid", "space_id required"))?;
        let args = r
            .args
            .as_object()
            .ok_or_else(|| error("Invalid", "args object required"))?;
        let extra: &[&str] = match r.tool.as_str() {
            "owned.launch" => &["plan_id", "expected_plan_revision"],
            "owned.list" => &["limit", "cursor"],
            "owned.get" => &[],
            "owned.read" | "owned.stop" => &["wait_ms"],
            "owned.turn" => &["text"],
            "owned.operation.get" => &["operation_id"],
            "owned.operation.reconcile" => &["operation_id", "wait_ms"],
            "owned.interrupt" => &["provider_turn_id", "wait_ms"],
            "owned.pause" | "owned.resume" => {
                return Err(error(
                    "Unsupported",
                    "owned pause/resume/rebind require the lifecycle slice",
                ))
            }
            _ => return Err(error("Unsupported", "unknown owned tool")),
        };
        let common = if r.tool == "owned.launch" {
            vec!["endpoint_id"]
        } else if r.tool == "owned.list" {
            vec![]
        } else {
            vec!["endpoint_id", "session_id", "expected_generation"]
        };
        if args
            .keys()
            .any(|k| !common.contains(&k.as_str()) && !extra.contains(&k.as_str()))
        {
            return Err(error("Invalid", "unknown owned argument"));
        }
        let wait = match args.get("wait_ms") {
            None => 0,
            Some(v) => v
                .as_u64()
                .filter(|n| *n <= 1000)
                .ok_or_else(|| error("Invalid", "wait_ms must be 0..1000"))?,
        };
        let wait = Duration::from_millis(wait);
        if r.tool == "owned.list" {
            return self.list(actor, space, &r.args);
        }
        let endpoint = id(&r.args, "endpoint_id")?;
        let grant = match r.tool.as_str() {
            "owned.launch" | "owned.stop" => "manage",
            "owned.turn" => "send",
            "owned.interrupt" => "control",
            _ => "observe",
        };
        let identity = self.authorize(actor, space, &endpoint, grant)?;
        let launch = r.tool == "owned.launch";
        let scope = if launch {
            None
        } else {
            let session = id(&r.args, "session_id")?;
            let generation = positive(&r.args, "expected_generation")?;
            let resolved = self.scope(space, &endpoint, &session, generation);
            if matches!(&resolved,Err(e) if e.code=="NotFound")
                && matches!(
                    r.tool.as_str(),
                    "owned.operation.get" | "owned.operation.reconcile"
                )
            {
                // A crash can precede policy resolution and exact worker binding
                // publication. Its launch intent still has an authorized lookup.
                let operation = id(&r.args, "operation_id")?;
                let row:Option<String>=self.db()?.query_row("SELECT body FROM requests WHERE operation=?1 AND space=?2 AND session=?3 AND tool='owned.launch'",params![operation,space,session],|row|row.get(0)).optional().map_err(storage)?;
                if let Some(encoded) = row {
                    let body: Value = serde_json::from_str(&encoded).map_err(storage)?;
                    if body["endpoint_id"] != endpoint {
                        return Err(error(
                            "Denied",
                            "launch intent differs from authorized target",
                        ));
                    }
                    if body["binding_generation"] != generation {
                        return Err(error("StaleGeneration", "exact launch generation required"));
                    }
                    if r.tool == "owned.operation.reconcile" {
                        return Err(error(
                            "Unsupported",
                            "launch intent has no provider turn to reconcile",
                        ));
                    }
                    self.authorize(actor, space, &endpoint, "observe")?;
                    return Ok(body);
                }
            }
            Some(resolved?)
        };
        if r.tool == "owned.get" {
            return self.status(actor, scope.as_ref().unwrap());
        }
        if r.tool == "owned.read" {
            return self.authorized_observation(
                actor,
                scope.as_ref().unwrap(),
                self.supervisor
                    .read_exact_thread_as(actor, scope.as_ref().unwrap(), wait)
                    .map_err(host_error),
            );
        }
        if r.tool == "owned.operation.get" || r.tool == "owned.operation.reconcile" {
            let scope = scope.as_ref().unwrap();
            let operation = id(&r.args, "operation_id")?;
            let row:Option<(String,String,u64)>=self.db()?.query_row("SELECT body,tool,revision FROM requests WHERE operation=?1 AND space=?2 AND session=?3",params![operation,space,scope.session_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(storage)?;
            let (body, tool, revision) =
                row.ok_or_else(|| error("NotFound", "owned operation not found"))?;
            let mut value: Value = serde_json::from_str(&body).map_err(storage)?;
            if r.tool == "owned.operation.reconcile" {
                if tool != "owned.turn" {
                    return Err(error(
                        "Unsupported",
                        "only a turn has a provider reconciliation observation",
                    ));
                }
                let observation = self
                    .supervisor
                    .reconcile_as(actor, scope, operation.clone(), wait)
                    .map_err(host_error);
                value=self.authorized_observation(actor,scope,observation.map(|observed|json!({"state":if observed.outcome==crate::runtime_codex_host::ProviderTurnOutcome::InProgress{"pending"}else{"confirmed"},"operation_id":operation,"observation":observed})))?;
                value["operation_id"] = json!(operation);
                let source = if value["state"] == "pending" && value.get("observation").is_none() {
                    ProjectionSource::NoObservation
                } else {
                    ProjectionSource::Observation
                };
                value = self.save(
                    &operation,
                    revision,
                    source,
                    value["state"].as_str().unwrap_or("unknown"),
                    &value,
                )?;
            } else {
                if let Ok(snapshot) = self.supervisor.operation_as(actor, scope, &operation) {
                    let state = match snapshot.phase {
                        OperationPhase::Confirmed => "confirmed",
                        OperationPhase::Rejected => "rejected",
                        OperationPhase::Unknown => "unknown",
                        _ => "accepted",
                    };
                    value = json!({"state":state,"operation_id":operation,"snapshot":snapshot});
                    value = self.save(
                        &operation,
                        revision,
                        ProjectionSource::Observation,
                        state,
                        &value,
                    )?;
                } else if tool == "owned.launch" || tool == "owned.stop" {
                    if let Ok(status) = self.supervisor.get_as(actor, scope) {
                        let state = if tool == "owned.stop" && status.worker_exited {
                            "exited"
                        } else if tool == "owned.launch"
                            && status.phase == crate::runtime_supervisor::SessionPhase::Running
                        {
                            "confirmed"
                        } else if status.phase == crate::runtime_supervisor::SessionPhase::Unknown {
                            "unknown"
                        } else {
                            "pending"
                        };
                        value = json!({"state":state,"operation_id":operation,"status":status});
                        value = self.save(
                            &operation,
                            revision,
                            ProjectionSource::Observation,
                            state,
                            &value,
                        )?;
                    }
                }
            }
            self.authorize(actor, space, &endpoint, "observe")?;
            return Ok(value);
        }
        // Strict values are checked before durable intent and before any provider work.
        let text = if r.tool == "owned.turn" {
            let text = r.args["text"]
                .as_str()
                .filter(|s| !s.trim().is_empty() && s.len() <= 65536)
                .ok_or_else(|| error("Invalid", "text must be 1..65536 bytes"))?;
            Some(text.to_owned())
        } else {
            None
        };
        let turn = if r.tool == "owned.interrupt" {
            Some(id(&r.args, "provider_turn_id")?)
        } else {
            None
        };
        let plan = if launch {
            Some((
                id(&r.args, "plan_id")?,
                positive(&r.args, "expected_plan_revision")?,
            ))
        } else {
            None
        };
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&json!({"tool":r.tool,"args":r.args})).map_err(storage)?
            )
        );
        let mut db = self.db()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let old: Option<(String, String, u64)> = tx
            .query_row(
                "SELECT fingerprint,body,revision FROM requests WHERE space=?1 AND actor=?2 AND request=?3",
                params![space, identity, r.request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(storage)?;
        if let Some((prior, body, revision)) = old {
            if prior != fingerprint {
                return Err(error("RequestConflict", "request_id content differs"));
            }
            drop(tx);
            if let Some((plan, revision)) = plan {
                self.plans
                    .approve(actor, space, &endpoint, &plan, revision)?;
            }
            let mut value: Value = serde_json::from_str(&body).map_err(storage)?;
            if r.tool == "owned.stop" && value["state"] != "unknown" {
                let scope = scope.as_ref().unwrap();
                if let Ok(status) = self.supervisor.stop_as(actor, scope, wait) {
                    value["state"] = json!(if status.worker_exited {
                        "exited"
                    } else {
                        "pending"
                    });
                    value["status"] = json!(status);
                    self.db()?
                        .execute(
                            "UPDATE sessions SET status=?1 WHERE id=?2",
                            params![json!(status).to_string(), scope.session_id],
                        )
                        .map_err(storage)?;
                    value = self.save(
                        value["operation_id"].as_str().unwrap(),
                        revision,
                        ProjectionSource::Observation,
                        value["state"].as_str().unwrap(),
                        &value,
                    )?;
                }
            }
            self.authorize(actor, space, &endpoint, grant)?;
            return Ok(value);
        }
        if r.tool == "owned.turn" {
            let scope = scope.as_ref().unwrap();
            let unknown:u64=tx.query_row("SELECT COUNT(*) FROM requests WHERE session=?1 AND tool='owned.turn' AND state='unknown'",[&scope.session_id],|row|row.get(0)).map_err(storage)?;
            if unknown > 0 {
                return Err(error(
                    "Conflict",
                    "reconcile the unresolved turn before admitting new work",
                ));
            }
        }
        // No provider or Engine calls while the durable intent transaction is held.
        let operation = uuid::Uuid::new_v4().to_string();
        let session = scope
            .as_ref()
            .map(|s| s.session_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let receipt = json!({"state":"unknown","operation_id":operation,"endpoint_id":endpoint,"session_id":session,"binding_generation":scope.as_ref().map(|s|s.binding_generation).unwrap_or(1),"actor_identity":identity});
        tx.execute(
            "INSERT INTO requests(space,actor,request,fingerprint,operation,session,tool,state,body) VALUES(?1,?2,?3,?4,?5,?6,?7,'intent',?8)",
            params![
                space,
                identity,
                r.request_id,
                fingerprint,
                operation,
                session,
                r.tool,
                receipt.to_string()
            ],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        let reservation = if launch || r.tool == "owned.turn" {
            match self.reserve() {
                Ok(r) => Some(r),
                Err(e) => {
                    let value = json!({"state":"rejected","operation_id":operation,"error":e});
                    let retained = self.save(
                        &operation,
                        0,
                        ProjectionSource::Mutation,
                        "rejected",
                        &value,
                    )?;
                    self.authorize(actor, space, &endpoint, grant)?;
                    return if retained == value {
                        Err(e)
                    } else {
                        Ok(retained)
                    };
                }
            }
        } else {
            None
        };
        let result: Result<Value, ApiError> = (|| match r.tool.as_str() {
            "owned.launch" => {
                let (plan, revision) = plan.unwrap();
                let approved = self
                    .plans
                    .approve(actor, space, &endpoint, &plan, revision)?;
                let mut scope = approved.scope().clone();
                scope.session_id = session.clone();
                scope.binding_generation = 1;
                self.db()?
                    .execute(
                        "INSERT INTO sessions(id,scope,profile) VALUES(?1,?2,?3)",
                        params![
                            session,
                            serde_json::to_string(&scope).map_err(storage)?,
                            serde_json::to_string(approved.profile()).map_err(storage)?
                        ],
                    )
                    .map_err(storage)?;
                self.supervisor
                    .start_as(actor, approved, session.clone(), 1, ThreadAdmission::New)
                    .map_err(host_error)?;
                Ok(
                    json!({"state":"accepted","operation_id":operation,"session_id":session,"binding_generation":1,"actor_identity":identity}),
                )
            }
            "owned.turn" => {
                self.supervisor
                    .start_turn_as(
                        actor,
                        scope.as_ref().unwrap(),
                        operation.clone(),
                        text.unwrap(),
                    )
                    .map_err(host_error)?;
                Ok(
                    json!({"state":"accepted","operation_id":operation,"session_id":session,"binding_generation":scope.as_ref().unwrap().binding_generation,"actor_identity":identity}),
                )
            }
            "owned.interrupt" => {
                let observed = self
                    .supervisor
                    .interrupt_turn_with_operation_as(
                        actor,
                        scope.as_ref().unwrap(),
                        operation.clone(),
                        turn.unwrap(),
                        wait,
                    )
                    .map_err(host_error)?;
                Ok(json!({"state":"confirmed","operation_id":operation,"observation":observed}))
            }
            "owned.stop" => {
                let status = self
                    .supervisor
                    .stop_as(actor, scope.as_ref().unwrap(), wait)
                    .map_err(host_error)?;
                self.db()?
                    .execute(
                        "UPDATE sessions SET status=?1 WHERE id=?2",
                        params![json!(status).to_string(), session],
                    )
                    .map_err(storage)?;
                Ok(
                    json!({"state":if status.worker_exited{"exited"}else{"pending"},"operation_id":operation,"status":status}),
                )
            }
            _ => unreachable!(),
        })();
        drop(reservation);
        let delivered = match result {
            Ok(value) => self.save(
                &operation,
                0,
                ProjectionSource::Mutation,
                value["state"].as_str().unwrap_or("unknown"),
                &value,
            ),
            Err(e) => {
                let state = if e.code == "Pending" {
                    "pending"
                } else if matches!(
                    e.code.as_str(),
                    "Denied"
                        | "QueueFull"
                        | "Invalid"
                        | "Conflict"
                        | "Unsupported"
                        | "LaunchApprovalRequired"
                        | "WorkerCredentialRequired"
                ) {
                    "rejected"
                } else {
                    "unknown"
                };
                let value =
                    json!({"state":state,"operation_id":operation,"session_id":session,"error":e});
                let retained =
                    self.save(&operation, 0, ProjectionSource::Mutation, state, &value)?;
                if state == "rejected" && retained == value {
                    Err(e)
                } else {
                    Ok(retained)
                }
            }
        };
        // A concurrent projection may retain a newer result than this command
        // observed. Recheck delivery authority before exposing that body.
        self.authorize(actor, space, &endpoint, grant)?;
        delivered
    }
    fn observation(&self, result: Result<Value, ApiError>) -> Result<Value, ApiError> {
        match result {
            Err(e) if e.code == "Pending" || e.code == "Unknown" => {
                Ok(json!({"state":e.code.to_lowercase()}))
            }
            other => other,
        }
    }
    fn authorized_observation(
        &self,
        actor: &str,
        scope: &HostScope,
        result: Result<Value, ApiError>,
    ) -> Result<Value, ApiError> {
        self.authorize(actor, &scope.space_id, &scope.endpoint_id, "observe")?;
        match result {
            // A server-held durable binding may have no live supervisor after
            // restart. Current actor permission is distinct from live evidence.
            Err(e) if e.code == "Denied" => {
                Ok(json!({"state":"unknown","evidence":"exact live instance unavailable"}))
            }
            other => self.observation(other),
        }
    }
    fn list(&self, actor: &str, space: &str, args: &Value) -> Result<Value, ApiError> {
        // identity.current also authenticates callers when the visible set is empty.
        let auth = self.engine.execute(
            actor,
            &ToolRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                tool: "identity.current".into(),
                space_id: Some(space.into()),
                args: json!({}),
            },
        )?;
        if auth["kind"] != "owner" && auth["space_id"] != space {
            return Err(error("Denied", "space membership required"));
        }
        let identity = auth["id"]
            .as_str()
            .ok_or_else(|| error("Internal", "identity unavailable"))?;
        let limit = match args.get("limit") {
            None => 100,
            Some(v) => v
                .as_u64()
                .filter(|v| *v > 0 && *v <= 100)
                .ok_or_else(|| error("Invalid", "limit must be 1..100"))?,
        } as usize;
        let prefix = format!(
            "{:x}",
            Sha256::digest(format!("{space}:{identity}").as_bytes())
        );
        let after = match args.get("cursor") {
            None | Some(Value::Null) => String::new(),
            Some(v) => {
                let c = v
                    .as_str()
                    .ok_or_else(|| error("Invalid", "cursor required"))?;
                c.strip_prefix(&format!("{prefix}:"))
                    .ok_or_else(|| error("Invalid", "cursor actor/space mismatch"))?
                    .to_owned()
            }
        };
        let db = self.db()?;
        let mut stmt = db
            .prepare("SELECT scope FROM sessions WHERE id>?1 ORDER BY id")
            .map_err(storage)?;
        let rows = stmt
            .query_map([after], |r| r.get::<_, String>(0))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        drop(stmt);
        drop(db);
        let mut items = Vec::new();
        let mut last = String::new();
        let mut more = false;
        for encoded in rows {
            let scope: HostScope = serde_json::from_str(&encoded).map_err(storage)?;
            if scope.space_id != space
                || self
                    .authorize(actor, space, &scope.endpoint_id, "observe")
                    .is_err()
            {
                continue;
            }
            if items.len() == limit {
                more = true;
                break;
            }
            last = scope.session_id.clone();
            items.push(self.status(actor, &scope)?);
        }
        items.retain(|item| {
            item["scope"]["endpoint_id"]
                .as_str()
                .is_some_and(|endpoint| self.authorize(actor, space, endpoint, "observe").is_ok())
        });
        // Reauthenticate even when all rows disappeared during the final delivery check.
        self.engine.execute(
            actor,
            &ToolRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                tool: "identity.current".into(),
                space_id: Some(space.into()),
                args: json!({}),
            },
        )?;
        Ok(json!({"items":items,"next_cursor":if more{Some(format!("{prefix}:{last}"))}else{None}}))
    }
}

#[cfg(test)]
mod durable_projection_tests {
    use super::*;
    use crate::runtime_codex_host::ExecutionProfile;
    use crate::runtime_launch_policy::{TrustedLaunchPlan, WorkerBinding};
    use crate::runtime_supervisor::OperationPhase;
    use std::{sync::Condvar, time::Instant};

    #[test]
    fn same_fact_comparison_requires_exact_terminal_turn_and_operation() {
        let original = json!({"state":"confirmed","operation_id":"op","snapshot":{"phase":"Confirmed","operation_id":"op","provider_turn_id":"turn","provider_outcome":"completed","result":{"private":"retained"}}});
        let observation = json!({"operation_id":"op","thread_id":"trusted-thread","turn_id":"turn","outcome":"completed"});
        assert!(same_turn_fact(&original, &observation, "op"));
        for outcome in ["interrupted", "failed", "in_progress"] {
            let mut other = observation.clone();
            other["outcome"] = json!(outcome);
            assert!(!same_turn_fact(&original, &other, "op"));
        }
        let mut other = observation.clone();
        other["turn_id"] = json!("different-turn");
        assert!(!same_turn_fact(&original, &other, "op"));
        assert!(!same_turn_fact(
            &original,
            &observation,
            "different-operation"
        ));
        let mut insufficient = original.clone();
        insufficient["snapshot"]["provider_outcome"] = Value::Null;
        assert!(!same_turn_fact(&insufficient, &observation, "op"));
        let mut enriched = original.clone();
        enriched["observation"] = observation.clone();
        assert!(same_turn_fact(&enriched, &observation, "op"));
        assert!(!same_turn_fact(&enriched, &other, "op"));
        assert_eq!(
            enriched["snapshot"]["result"],
            original["snapshot"]["result"]
        );
    }

    #[test]
    fn legacy_receipts_migrate_and_recovery_fences_preopen_observations() {
        let f = Fixture::new();
        {
            let db = f.bridge.db().unwrap();
            db.execute_batch("ALTER TABLE requests DROP COLUMN revision;")
                .unwrap();
            for (op, state) in [("settled", "confirmed"), ("unfinished", "accepted")] {
                db.execute(
                    "INSERT INTO requests VALUES('s','a',?1,'fp',?1,'session','owned.turn',?2,?3)",
                    params![
                        op,
                        state,
                        json!({"operation_id":op,"state":state,"result":"retained"}).to_string()
                    ],
                )
                .unwrap();
            }
        }
        let reopened = OwnedControlBridge::open(
            f.dir.path(),
            f.engine.clone(),
            f.supervisor.clone(),
            f.bridge.registry.clone(),
            f.bridge.participants.clone(),
            f.plans.clone(),
        )
        .unwrap();
        let read = |op: &str| -> (Value, u64) {
            let (body, revision): (String, u64) = reopened
                .db()
                .unwrap()
                .query_row(
                    "SELECT body,revision FROM requests WHERE operation=?1",
                    [op],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            (serde_json::from_str(&body).unwrap(), revision)
        };
        assert_eq!(
            read("settled"),
            (
                json!({"operation_id":"settled","state":"confirmed","result":"retained"}),
                0
            )
        );
        let (unknown, revision) = read("unfinished");
        assert_eq!(unknown["state"], "unknown");
        assert_eq!(revision, 1);
        let ack = json!({"operation_id":"unfinished","state":"accepted","snapshot":{"phase":"Accepted","provider_turn_id":"turn1"}});
        // A genuine ACK captured before recovery is still an obsolete projection.
        assert_eq!(
            f.bridge
                .save(
                    "unfinished",
                    0,
                    ProjectionSource::Observation,
                    "accepted",
                    &ack
                )
                .unwrap(),
            unknown
        );
        assert_eq!(
            reopened
                .save(
                    "unfinished",
                    revision,
                    ProjectionSource::Observation,
                    "accepted",
                    &ack
                )
                .unwrap(),
            ack
        );
    }

    #[test]
    fn receipt_owner_fences_stale_writers_and_preserves_operation_facts() {
        let f = Fixture::new();
        let insert = |operation: &str, tool: &str, state: &str| {
            let body = json!({"operation_id":operation,"state":state});
            f.bridge.db().unwrap().execute(
                "INSERT INTO requests(space,actor,request,fingerprint,operation,session,tool,state,body) VALUES('s','a',?1,'fp',?1,'session',?2,?3,?4)",
                params![operation,tool,state,body.to_string()],
            ).unwrap();
            body
        };
        for (op, tool, final_state) in [
            ("launch", "owned.launch", "confirmed"),
            ("stop", "owned.stop", "exited"),
            ("turn", "owned.turn", "confirmed"),
            ("interrupt", "owned.interrupt", "confirmed"),
        ] {
            insert(op, tool, "intent");
            let fact =
                json!({"operation_id":op,"state":final_state,"result":{"outcome":"original"}});
            // Positive exact evidence remains admissible after a weaker writer.
            let pending = json!({"operation_id":op,"state":"pending"});
            f.bridge
                .save(op, 0, ProjectionSource::Mutation, "pending", &pending)
                .unwrap();
            assert_eq!(
                f.bridge
                    .save(op, 0, ProjectionSource::Observation, final_state, &fact)
                    .unwrap(),
                fact
            );
            for state in ["pending", "unknown", "rejected", final_state] {
                let stale =
                    json!({"operation_id":op,"state":state,"result":{"outcome":"contradiction"}});
                assert_eq!(
                    f.bridge
                        .save(op, 2, ProjectionSource::Observation, state, &stale)
                        .unwrap(),
                    fact
                );
            }
        }
        let unknown = insert("uncertain", "owned.turn", "unknown");
        for phase in ["Queued", "Dispatching"] {
            let weaker = json!({"operation_id":"uncertain","state":"accepted","snapshot":{"phase":phase,"provider_turn_id":"turn1"}});
            assert_eq!(
                f.bridge
                    .save(
                        "uncertain",
                        0,
                        ProjectionSource::Observation,
                        "accepted",
                        &weaker
                    )
                    .unwrap(),
                unknown
            );
        }
        let ack = json!({"operation_id":"uncertain","state":"accepted","snapshot":{"phase":"Accepted","provider_turn_id":"turn1"}});
        assert_eq!(
            f.bridge
                .save(
                    "uncertain",
                    0,
                    ProjectionSource::Observation,
                    "accepted",
                    &ack
                )
                .unwrap(),
            ack
        );
        let stale = json!({"operation_id":"uncertain","state":"pending"});
        assert_eq!(
            f.bridge
                .save(
                    "uncertain",
                    0,
                    ProjectionSource::Observation,
                    "pending",
                    &stale
                )
                .unwrap(),
            ack
        );
        let unknown = insert("in-progress", "owned.turn", "unknown");
        assert_eq!(
            f.bridge
                .save(
                    "in-progress",
                    0,
                    ProjectionSource::NoObservation,
                    "unknown",
                    &unknown
                )
                .unwrap(),
            unknown
        );
        let progress = json!({"operation_id":"in-progress","state":"pending","observation":{"outcome":"in_progress"}});
        assert_eq!(
            f.bridge
                .save(
                    "in-progress",
                    0,
                    ProjectionSource::Observation,
                    "pending",
                    &progress
                )
                .unwrap(),
            progress
        );
    }

    fn call(engine: &Engine, actor: &str, space: Option<&str>, tool: &str, args: Value) -> Value {
        engine
            .execute(
                actor,
                &ToolRequest {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    tool: tool.into(),
                    space_id: space.map(Into::into),
                    args,
                },
            )
            .unwrap()
    }
    struct Fixture {
        dir: tempfile::TempDir,
        engine: Engine,
        admin: String,
        observer: String,
        observer_id: String,
        binding: WorkerBinding,
        plans: Arc<LaunchPolicyStore>,
        supervisor: Arc<OwnedSessionSupervisor>,
        bridge: Arc<OwnedControlBridge>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let engine = Engine::open(dir.path()).unwrap();
            let owner =
                std::fs::read_to_string(crate::runtime_core::owner_token_path(dir.path())).unwrap();
            let space = call(
                &engine,
                &owner,
                None,
                "spaces.create",
                json!({"name":"projection"}),
            );
            let sid = space["space"]["id"].as_str().unwrap();
            let admin = space["administrator"]["credential"]
                .as_str()
                .unwrap()
                .to_owned();
            let endpoint = call(
                &engine,
                &admin,
                Some(sid),
                "endpoints.register",
                json!({"name":"owned","kind":"terminal"}),
            );
            let invite = call(
                &engine,
                &admin,
                Some(sid),
                "members.invite",
                json!({"roles":["implementer"]}),
            );
            let worker = call(
                &engine,
                invite["invite_token"].as_str().unwrap(),
                Some(sid),
                "members.register",
                json!({"name":"worker"}),
            );
            let credential = worker["credential"].as_str().unwrap();
            call(
                &engine,
                &admin,
                Some(sid),
                "endpoints.grant",
                json!({"endpoint_id":endpoint["id"],"member_id":worker["id"],"operations":["send","observe","control"]}),
            );
            let invite = call(
                &engine,
                &admin,
                Some(sid),
                "members.invite",
                json!({"roles":["observer"]}),
            );
            let observer = call(
                &engine,
                invite["invite_token"].as_str().unwrap(),
                Some(sid),
                "members.register",
                json!({"name":"G1"}),
            );
            call(
                &engine,
                &admin,
                Some(sid),
                "endpoints.grant",
                json!({"endpoint_id":endpoint["id"],"member_id":observer["id"],"operations":["observe"]}),
            );
            let registry = OwnedCodexRegistry::open(dir.path(), engine.clone()).unwrap();
            let plans = Arc::new(
                LaunchPolicyStore::open(dir.path(), engine.clone(), registry.clone()).unwrap(),
            );
            let participants = Arc::new(
                ParticipantService::open(dir.path(), engine.clone(), Arc::new(registry.clone()))
                    .unwrap(),
            );
            let script=include_str!("../tests/fixtures/owned_codex_server.py").replace("elif method=='turn/interrupt':send({'id':rid,'result':{'interruptedTurn':p['turnId']}})","elif method=='turn/interrupt':\n  import pathlib,time\n  open('interrupt-count','a').write('sent\\n')\n  pathlib.Path('interrupt-effect').touch()\n  while not pathlib.Path('interrupt-release').exists():time.sleep(0.005)\n  send({'id':rid,'result':{'interruptedTurn':p['turnId'],'privateReply':'g2-confirmed-sensitive'}})");
            let path = dir.path().join("provider.py");
            std::fs::write(&path, script).unwrap();
            let python =
                std::process::Command::new(if cfg!(windows) { "python" } else { "python3" })
                    .args(["-c", "import sys;print(sys.executable)"])
                    .output()
                    .unwrap();
            let hb = call(
                &engine,
                credential,
                Some(sid),
                "members.heartbeat",
                json!({}),
            );
            let binding = WorkerBinding {
                space_id: sid.into(),
                endpoint_id: endpoint["id"].as_str().unwrap().into(),
                identity_id: worker["id"].as_str().unwrap().into(),
                attachment_id: hb["attachment_id"].as_str().unwrap().into(),
            };
            plans
                .provision_worker_slot(&admin, "worker", binding.clone(), credential)
                .unwrap();
            plans
                .install(
                    &admin,
                    "plan",
                    None,
                    TrustedLaunchPlan {
                        binding: binding.clone(),
                        worker_slot: "worker".into(),
                        executable: PathBuf::from(String::from_utf8(python.stdout).unwrap().trim()),
                        args: vec![path.to_str().unwrap().into(), "normal".into()],
                        workspace: dir.path().into(),
                        model: "gpt-6-luna".into(),
                        provider_version: "0.163.0".into(),
                        capability_evidence: "controlled protocol provider".into(),
                        timeout: Duration::from_secs(3),
                        profile: ExecutionProfile::ReadOnlyForum,
                    },
                )
                .unwrap();
            let supervisor = Arc::new(
                OwnedSessionSupervisor::new(engine.clone(), registry.clone(), Default::default())
                    .unwrap(),
            );
            let bridge = Arc::new(
                OwnedControlBridge::open(
                    dir.path(),
                    engine.clone(),
                    supervisor.clone(),
                    registry,
                    participants,
                    plans.clone(),
                )
                .unwrap(),
            );
            call(
                &engine,
                credential,
                Some(sid),
                "members.heartbeat",
                json!({"attachment_id":binding.attachment_id}),
            );
            Self {
                dir,
                engine,
                admin,
                observer: observer["credential"].as_str().unwrap().into(),
                observer_id: observer["id"].as_str().unwrap().into(),
                binding,
                plans,
                supervisor,
                bridge,
            }
        }
        fn request(&self, id: &str, tool: &str, args: Value) -> ToolRequest {
            ToolRequest {
                request_id: id.into(),
                tool: tool.into(),
                space_id: Some(self.binding.space_id.clone()),
                args,
            }
        }
        fn target(&self, launch: &Value) -> Value {
            json!({"endpoint_id":self.binding.endpoint_id,"session_id":launch["session_id"],"expected_generation":1})
        }
        fn scope(&self, launch: &Value) -> HostScope {
            HostScope {
                identity_id: self.binding.identity_id.clone(),
                space_id: self.binding.space_id.clone(),
                endpoint_id: self.binding.endpoint_id.clone(),
                attachment_id: self.binding.attachment_id.clone(),
                session_id: launch["session_id"].as_str().unwrap().into(),
                binding_generation: 1,
            }
        }
        fn wait(&self, condition: impl Fn() -> bool) {
            let until = Instant::now() + Duration::from_secs(5);
            while !condition() {
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        fn shutdown(&self) {
            self.supervisor.request_shutdown();
            self.wait(|| self.supervisor.active_count() == 0);
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::write(self.dir.path().join("interrupt-release"), "");
            self.shutdown();
        }
    }

    #[test]
    fn stale_get_cannot_overwrite_delivered_confirmed_receipt_across_reopen() {
        for revoke_g1 in [false, true] {
            let f = Fixture::new();
            let launch=f.bridge.execute(&f.admin,&f.request("launch","owned.launch",json!({"endpoint_id":f.binding.endpoint_id,"plan_id":"plan","expected_plan_revision":1}))).unwrap();
            let scope = f.scope(&launch);
            f.wait(|| {
                f.supervisor.get_as(&f.admin, &scope).unwrap().phase
                    == crate::runtime_supervisor::SessionPhase::Running
            });
            let mut args = f.target(&launch);
            args["text"] = json!("active");
            let turn = f
                .bridge
                .execute(&f.admin, &f.request("turn", "owned.turn", args))
                .unwrap();
            f.wait(|| {
                f.supervisor
                    .operation_as(&f.admin, &scope, turn["operation_id"].as_str().unwrap())
                    .unwrap()
                    .phase
                    == OperationPhase::Accepted
            });
            let mut args = f.target(&launch);
            args["provider_turn_id"] = json!("turn-1");
            let interrupt_request = f.request("interrupt", "owned.interrupt", args);
            let interrupt = f.bridge.execute(&f.admin, &interrupt_request).unwrap();
            let operation = interrupt["operation_id"].as_str().unwrap().to_owned();
            f.wait(|| f.dir.path().join("interrupt-effect").exists());
            assert_eq!(
                f.supervisor
                    .operation_as(&f.admin, &scope, &operation)
                    .unwrap()
                    .phase,
                OperationPhase::Dispatching
            );
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let release = Arc::new((Mutex::new(false), Condvar::new()));
            let control = release.clone();
            let watched = operation.clone();
            *f.bridge.projection_test_hook.lock().unwrap() = Some(Arc::new(move |id, state| {
                if id == watched
                    && state == "accepted"
                    && std::thread::current().name() == Some("stale-get")
                {
                    entered_tx.send(()).unwrap();
                    let (lock, ready) = &*control;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                }
            }));
            let mut args = f.target(&launch);
            args["operation_id"] = json!(operation);
            let request = f.request("get", "owned.operation.get", args);
            let bridge = f.bridge.clone();
            let actor = f.observer.clone();
            let g1request = request.clone();
            let g1 = std::thread::Builder::new()
                .name("stale-get".into())
                .spawn(move || bridge.execute(&actor, &g1request))
                .unwrap();
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            std::fs::write(f.dir.path().join("interrupt-release"), "").unwrap();
            f.wait(|| {
                f.supervisor
                    .operation_as(&f.admin, &scope, &operation)
                    .unwrap()
                    .phase
                    == OperationPhase::Confirmed
            });
            let g2 = f.bridge.execute(&f.admin, &request).unwrap();
            eprintln!("G2 durable/delivered: {g2}");
            assert_eq!(g2["state"], "confirmed");
            assert_eq!(
                g2["snapshot"]["result"]["privateReply"],
                "g2-confirmed-sensitive"
            );
            let db = f.bridge.db().unwrap();
            let saved: String = db
                .query_row(
                    "SELECT body FROM requests WHERE operation=?1",
                    [&operation],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(serde_json::from_str::<Value>(&saved).unwrap(), g2);
            drop(db);
            if revoke_g1 {
                call(
                    &f.engine,
                    &f.admin,
                    Some(&f.binding.space_id),
                    "endpoints.grant",
                    json!({"endpoint_id":f.binding.endpoint_id,"member_id":f.observer_id,"operations":[]}),
                );
            }
            {
                let (lock, ready) = &*release;
                *lock.lock().unwrap() = true;
                ready.notify_all();
            }
            let g1 = g1.join().unwrap();
            eprintln!("G1 after stale save: {g1:?}");
            if revoke_g1 {
                assert_eq!(g1.as_ref().unwrap_err().code, "Denied");
                assert!(!format!("{g1:?}").contains("g2-confirmed-sensitive"));
            }
            let db = f.bridge.db().unwrap();
            let after: String = db
                .query_row(
                    "SELECT body FROM requests WHERE operation=?1",
                    [&operation],
                    |row| row.get(0),
                )
                .unwrap();
            drop(db);
            eprintln!("SQLite after G1: {after}");
            f.shutdown();
            assert!(f.supervisor.get_as(&f.admin, &scope).unwrap().worker_exited);
            let registry = OwnedCodexRegistry::open(f.dir.path(), f.engine.clone()).unwrap();
            let supervisor = Arc::new(
                OwnedSessionSupervisor::new(f.engine.clone(), registry.clone(), Default::default())
                    .unwrap(),
            );
            let participants = Arc::new(
                ParticipantService::open(
                    f.dir.path(),
                    f.engine.clone(),
                    Arc::new(registry.clone()),
                )
                .unwrap(),
            );
            let reopened = OwnedControlBridge::open(
                f.dir.path(),
                f.engine.clone(),
                supervisor.clone(),
                registry,
                participants,
                f.plans.clone(),
            )
            .unwrap();
            let recovered = reopened.execute(&f.admin, &request).unwrap();
            eprintln!("Reopened receipt: {recovered}");
            assert_eq!(supervisor.active_count(), 0);
            assert_eq!(
                serde_json::from_str::<Value>(&after).unwrap(),
                g2,
                "G1 stale projection overwrote G2 durable receipt"
            );
            assert_eq!(recovered, g2);
            if !revoke_g1 {
                assert_eq!(g1.unwrap(), g2);
            }
            assert_eq!(
                std::fs::read_to_string(f.dir.path().join("interrupt-count"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
            assert_eq!(reopened.execute(&f.admin, &interrupt_request).unwrap(), g2);
        }
    }
}
