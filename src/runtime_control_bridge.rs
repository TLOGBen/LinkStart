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
pub struct OwnedControlBridge {
    path: PathBuf,
    engine: Engine,
    supervisor: Arc<OwnedSessionSupervisor>,
    registry: OwnedCodexRegistry,
    participants: Arc<ParticipantService>,
    plans: Arc<LaunchPolicyStore>,
    admission: Mutex<Admission>,
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
        let db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS requests(space TEXT NOT NULL,actor TEXT NOT NULL,request TEXT NOT NULL,fingerprint TEXT NOT NULL,operation TEXT UNIQUE NOT NULL,session TEXT NOT NULL,tool TEXT NOT NULL,state TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(space,actor,request));
          CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,scope TEXT NOT NULL,profile TEXT NOT NULL,status TEXT);
          UPDATE requests SET state='unknown',body=json_set(body,'$.state','unknown','$.evidence','runtime reopened; original side effect is not retried') WHERE state IN ('intent','accepted','pending');")?;
        crate::runtime_core::protect(&path)?;
        Ok(Self {
            path,
            engine,
            supervisor,
            registry,
            participants,
            plans,
            admission: Mutex::new(Admission::default()),
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
    fn save(&self, operation: &str, state: &str, body: &Value) -> Result<(), ApiError> {
        self.db()?
            .execute(
                "UPDATE requests SET state=?1,body=?2 WHERE operation=?3",
                params![state, body.to_string(), operation],
            )
            .map_err(storage)?;
        Ok(())
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
            let row:Option<(String,String,String)>=self.db()?.query_row("SELECT state,body,tool FROM requests WHERE operation=?1 AND space=?2 AND session=?3",params![operation,space,scope.session_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(storage)?;
            let (state, body, tool) =
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
                if value["state"] != "pending" {
                    self.save(
                        &operation,
                        value["state"].as_str().unwrap_or("unknown"),
                        &value,
                    )?;
                }
            } else {
                if let Ok(snapshot) = self.supervisor.operation_as(actor, scope, &operation) {
                    let state = match snapshot.phase {
                        OperationPhase::Confirmed => "confirmed",
                        OperationPhase::Rejected => "rejected",
                        OperationPhase::Unknown => "unknown",
                        _ => "accepted",
                    };
                    value = json!({"state":state,"operation_id":operation,"snapshot":snapshot});
                    self.save(&operation, state, &value)?;
                } else if (tool == "owned.launch" || tool == "owned.stop") && state != "unknown" {
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
                        self.save(&operation, state, &value)?;
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
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT fingerprint,body FROM requests WHERE space=?1 AND actor=?2 AND request=?3",
                params![space, identity, r.request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(storage)?;
        if let Some((prior, body)) = old {
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
                    self.save(
                        value["operation_id"].as_str().unwrap(),
                        value["state"].as_str().unwrap(),
                        &value,
                    )?;
                }
            }
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
            "INSERT INTO requests VALUES(?1,?2,?3,?4,?5,?6,?7,'intent',?8)",
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
                    self.save(&operation, "rejected", &value)?;
                    return Err(e);
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
                    .interrupt_turn_as(actor, scope.as_ref().unwrap(), turn.unwrap(), wait)
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
        match result {
            Ok(value) => {
                self.save(
                    &operation,
                    value["state"].as_str().unwrap_or("unknown"),
                    &value,
                )?;
                Ok(value)
            }
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
                self.save(&operation, state, &value)?;
                if matches!(state, "pending" | "unknown") {
                    Ok(value)
                } else {
                    Err(e)
                }
            }
        }
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
