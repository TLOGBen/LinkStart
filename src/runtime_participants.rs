//! Durable participant bookkeeping. No provider dispatch or agent policy runs here.
use crate::runtime_core::{ApiError, Engine, ToolRequest};
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

fn error(code: &str, message: impl Into<String>) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
    }
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    error("Internal", e.to_string())
}
fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str, ApiError> {
    v[k].as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error("Invalid", format!("{k} must be a nonempty string")))
}
fn number(v: &Value, k: &str) -> Result<u64, ApiError> {
    v[k].as_u64()
        .ok_or_else(|| error("Invalid", format!("{k} must be an unsigned integer")))
}

/// Constructed by the service, never deserialized from an untrusted caller.
#[derive(Clone, Debug)]
pub struct BindingContext {
    pub identity_id: String,
    pub space_id: String,
    pub endpoint_id: String,
    pub attachment_id: String,
    pub session_id: String,
    pub adapter_kind: String,
    pub provider_thread_id: Option<String>,
    pub pending_intents: Vec<Value>,
    pub binding_generation: u64,
    pub previous_binding: Option<Value>,
}
/// Only the trusted host may attest to these facts. Capabilities are informational,
/// and never confer Engine permissions or prove workspace isolation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnedAdmission {
    pub session_id: String,
    pub instance_id: String,
    pub host_generation: u64,
    pub adapter_kind: String,
    pub provider_thread_id: Option<String>,
    pub capabilities: Vec<String>,
    pub reconciled: bool,
}
/// Trusted in-process dependency: check an owned Session registry and exact provider
/// thread. Must be bounded and must not call Engine, provider I/O, or this service.
/// Do not expose a constructor that obtains this implementation from HTTP input.
pub trait TrustedHost: Send + Sync {
    fn admit(&self, context: &BindingContext) -> Result<OwnedAdmission, ApiError>;
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Ledger {
    bindings: Vec<Value>,
    intents: Vec<Value>,
    receipts: Vec<Value>,
    checkpoints: Vec<Value>,
    requests: Vec<Value>,
}
#[derive(Clone)]
pub struct ParticipantService {
    path: PathBuf,
    engine: Engine,
    host: Arc<dyn TrustedHost>,
}
impl ParticipantService {
    pub fn open(dir: &Path, engine: Engine, host: Arc<dyn TrustedHost>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            std::fs::canonicalize(dir)? == engine.private_storage_directory()?,
            "participant ledger directory must match Engine private storage"
        );
        crate::runtime_core::protect(dir)?;
        let path = dir.join("v2-participants.sqlite");
        for suffix in ["", "-wal", "-shm"] {
            let file = dir.join(format!("v2-participants.sqlite{suffix}"));
            if file.exists() {
                crate::runtime_core::protect(&file)?;
            }
        }
        let db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS participant_ledger(id INTEGER PRIMARY KEY CHECK(id=1),json TEXT NOT NULL);")?;
        let empty = serde_json::to_string(&Ledger::default())?;
        db.execute(
            "INSERT OR IGNORE INTO participant_ledger(id,json) VALUES(1,?1)",
            [empty],
        )?;
        for suffix in ["", "-wal", "-shm"] {
            let file = dir.join(format!("v2-participants.sqlite{suffix}"));
            if file.exists() {
                crate::runtime_core::protect(&file)?;
            }
        }
        Ok(Self { path, engine, host })
    }
    fn connect(&self) -> Result<Connection, ApiError> {
        let db = Connection::open(&self.path).map_err(internal)?;
        db.busy_timeout(Duration::from_secs(5)).map_err(internal)?;
        Ok(db)
    }
    fn load(db: &Connection) -> Result<Ledger, ApiError> {
        let raw: String = db
            .query_row("SELECT json FROM participant_ledger WHERE id=1", [], |r| {
                r.get(0)
            })
            .map_err(internal)?;
        serde_json::from_str(&raw).map_err(internal)
    }
    /// All operations, including replay, pass current Engine authorization before
    /// entering the participant transaction. Lock order is core -> participant DB.
    pub fn execute(&self, credential: &str, r: &ToolRequest) -> Result<Value, ApiError> {
        if r.request_id.trim().is_empty() {
            return Err(error("Invalid", "request_id required"));
        }
        let sid = r
            .space_id
            .as_deref()
            .ok_or_else(|| error("Invalid", "space_id required"))?;
        let allowed: &[&str] = match r.tool.as_str() {
            "participants.bind" => &[
                "participant_id",
                "endpoint_id",
                "session_id",
                "attachment_id",
                "adapter_kind",
                "provider_thread_id",
                "initial_cursor",
                "expected_generation",
            ],
            "participants.read" => &["participant_id", "cursor", "limit"],
            "participants.receipt" => &[
                "participant_id",
                "event_id",
                "binding_generation",
                "state",
                "provider_turn_id",
                "evidence_ref",
            ],
            "participants.pause" | "participants.resume" => {
                &["participant_id", "expected_generation"]
            }
            "work.checkpoint.get" => &["participant_id", "work_id"],
            "work.checkpoint.set" => &[
                "participant_id",
                "work_id",
                "binding_generation",
                "expected_revision",
                "topic_id",
                "assignment_revision",
                "submission_id",
                "completed",
                "pending",
                "unknown",
                "artifacts",
                "provider_resume_ref",
            ],
            "operations.get" => &["participant_id", "operation_request_id"],
            _ => return Err(error("Unsupported", "unknown participant tool")),
        };
        let args = r
            .args
            .as_object()
            .ok_or_else(|| error("Invalid", "args must be object"))?;
        if args.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err(error("Invalid", "unknown argument"));
        }
        for key in [
            "provider_turn_id",
            "evidence_ref",
            "provider_resume_ref",
            "submission_id",
        ] {
            if let Some(value) = args.get(key) {
                if !value.is_null() && !value.as_str().is_some_and(|s| !s.trim().is_empty()) {
                    return Err(error(
                        "Invalid",
                        format!("{key} must be a nonempty string or null"),
                    ));
                }
            }
        }
        // Never persist the bearer passed to this call, even if repeated as evidence.
        if credential.is_empty()
            || serde_json::to_string(r)
                .map_err(internal)?
                .contains(credential)
        {
            return Err(error(
                "Invalid",
                "credential must not appear in participant data",
            ));
        }
        let eid = if r.tool == "participants.bind" {
            text(&r.args, "endpoint_id")?.to_owned()
        } else {
            let ledger = Self::load(&self.connect()?)?;
            let pid = text(&r.args, "participant_id")?;
            let binding = ledger
                .bindings
                .iter()
                .find(|b| b["participant_id"] == pid && b["space_id"] == sid)
                .ok_or_else(|| error("NotFound", "participant not found"))?;
            text(binding, "endpoint_id")?.to_owned()
        };
        let operation = if matches!(
            r.tool.as_str(),
            "participants.bind"
                | "participants.resume"
                | "participants.receipt"
                | "work.checkpoint.set"
        ) {
            "send"
        } else {
            "observe"
        };
        self.engine
            .with_endpoint_authorization(credential, sid, &eid, operation, |auth| {
                let mut db = self.connect()?;
                let tx = db
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(internal)?;
                let mut ledger = Self::load(&tx)?;
                let result = self.apply(&mut ledger, r, auth)?;
                if r.tool != "operations.get" {
                    tx.execute(
                        "UPDATE participant_ledger SET json=?1 WHERE id=1",
                        [serde_json::to_string(&ledger).map_err(internal)?],
                    )
                    .map_err(internal)?;
                }
                tx.commit().map_err(internal)?;
                Ok(result)
            })
    }
    fn admit(
        &self,
        l: &Ledger,
        b: &Value,
        auth: &Value,
        reconcile: bool,
    ) -> Result<OwnedAdmission, ApiError> {
        let aid = text(b, "attachment_id")?;
        if !auth["identity"]["attachments"].as_array().is_some_and(|a| {
            a.iter()
                .any(|x| x["attachment_id"] == aid && x["state"] == "alive")
        }) {
            return Err(error("Denied", "current self attachment required"));
        }
        let context = BindingContext {
            identity_id: text(b, "identity_id")?.into(),
            space_id: text(b, "space_id")?.into(),
            endpoint_id: text(b, "endpoint_id")?.into(),
            attachment_id: aid.into(),
            session_id: text(b, "session_id")?.into(),
            adapter_kind: text(b, "adapter_kind")?.into(),
            provider_thread_id: b["provider_thread_id"].as_str().map(str::to_owned),
            pending_intents: l
                .intents
                .iter()
                .filter(|i| i["participant_id"] == b["participant_id"] && i["state"] == "pending")
                .cloned()
                .map(|intent| {
                    intent_with_event_origin(
                        intent,
                        auth["activity"]
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or(&[]),
                    )
                })
                .collect(),
            binding_generation: number(b, "binding_generation")?,
            previous_binding: l
                .bindings
                .iter()
                .find(|old| old["participant_id"] == b["participant_id"])
                .cloned(),
        };
        let admission = self.host.admit(&context)?;
        if admission.session_id != context.session_id
            || admission.adapter_kind != context.adapter_kind
            || admission.provider_thread_id != context.provider_thread_id
            || admission.instance_id.is_empty()
            || admission.host_generation == 0
        {
            return Err(error(
                "Denied",
                "host admission does not match exact owned session",
            ));
        }
        if reconcile && !admission.reconciled {
            return Err(error(
                "Conflict",
                "host reconciliation incomplete; remain paused",
            ));
        }
        Ok(admission)
    }
    fn apply(&self, l: &mut Ledger, r: &ToolRequest, auth: &Value) -> Result<Value, ApiError> {
        let identity = text(auth, "identity_id")?;
        let sid = r.space_id.as_deref().unwrap();
        if identity == "owner" {
            return Err(error(
                "Denied",
                "participant requires persistent member identity",
            ));
        }
        let events = auth["activity"]
            .as_array()
            .ok_or_else(|| error("Internal", "Engine activity snapshot missing"))?;
        let topics = auth["topics"]
            .as_array()
            .ok_or_else(|| error("Internal", "Engine topics snapshot missing"))?;
        let bind = r.tool == "participants.bind";
        let index = if bind && r.args.get("participant_id").is_none() {
            None
        } else {
            Some(
                l.bindings
                    .iter()
                    .position(|b| {
                        b["participant_id"] == r.args["participant_id"]
                            && b["space_id"] == sid
                            && b["identity_id"] == identity
                    })
                    .ok_or_else(|| error("Denied", "self participant required"))?,
            )
        };
        let mut b = index.map(|i| l.bindings[i].clone()).unwrap_or(Value::Null);
        let prior = l
            .requests
            .iter()
            .find(|x| x["identity_id"] == identity && x["request_id"] == r.request_id)
            .cloned();
        let control_replay = prior.as_ref().is_some_and(|old| {
            let generation = if r.tool == "participants.pause" {
                &old["result"]["binding"]["binding_generation"]
            } else {
                &old["result"]["binding_generation"]
            };
            generation == &b["binding_generation"]
                && old["request"] == serde_json::to_value(r).unwrap_or(Value::Null)
        });
        // Generation is checked before replay so an old delivery can never become current.
        if matches!(
            r.tool.as_str(),
            "participants.receipt" | "work.checkpoint.set"
        ) {
            if number(&r.args, "binding_generation")? != number(&b, "binding_generation")? {
                return Err(error("Conflict", "stale binding generation"));
            }
            if b["paused"] == true {
                return Err(error(
                    "Conflict",
                    "participant paused; pause does not cancel a turn",
                ));
            }
            let live = serde_json::to_value(self.admit(l, &b, auth, false)?).map_err(internal)?;
            if live["instance_id"] != b["admission"]["instance_id"]
                || live["host_generation"] != b["admission"]["host_generation"]
            {
                return Err(error(
                    "Conflict",
                    "owned host changed; explicit resume required",
                ));
            }
        }
        if matches!(
            r.tool.as_str(),
            "participants.pause" | "participants.resume"
        ) && !control_replay
            && number(&r.args, "expected_generation")? != number(&b, "binding_generation")?
        {
            return Err(error("Conflict", "stale binding generation"));
        }
        if let Some(old) = prior.as_ref() {
            if old["request"] != serde_json::to_value(r).map_err(internal)? {
                return Err(error("Conflict", "request_id reused with different intent"));
            }
            if bind {
                let current = l
                    .bindings
                    .iter()
                    .find(|b| b["participant_id"] == old["result"]["participant_id"])
                    .ok_or_else(|| error("Conflict", "binding no longer exists"))?;
                let live =
                    serde_json::to_value(self.admit(l, current, auth, true)?).map_err(internal)?;
                if current["binding_generation"] != old["result"]["binding_generation"]
                    || live["instance_id"] != current["admission"]["instance_id"]
                    || live["host_generation"] != current["admission"]["host_generation"]
                {
                    return Err(error(
                        "Conflict",
                        "binding changed; explicit resume required",
                    ));
                }
            }
            if r.tool == "participants.resume" {
                let live =
                    serde_json::to_value(self.admit(l, &b, auth, true)?).map_err(internal)?;
                if live["instance_id"] != b["admission"]["instance_id"]
                    || live["host_generation"] != b["admission"]["host_generation"]
                {
                    return Err(error("Conflict", "host changed after resume"));
                }
            }
            return Ok(old["result"].clone());
        }
        let result = match r.tool.as_str() {
            "participants.bind" => {
                if index.is_some() {
                    if b["endpoint_id"] != r.args["endpoint_id"] {
                        return Err(error("Denied", "rebind cannot change endpoint"));
                    }
                    if number(&r.args, "expected_generation")? != number(&b, "binding_generation")?
                    {
                        return Err(error("Conflict", "stale binding generation"));
                    }
                    if r.args.get("initial_cursor").is_some() {
                        return Err(error("Invalid", "rebind preserves the initial boundary"));
                    }
                    b["binding_generation"] = json!(next_binding_generation(&b)?);
                } else {
                    if number(&r.args, "expected_generation")? != 0 {
                        return Err(error("Conflict", "new binding generation must be zero"));
                    }
                    let start = match text(&r.args, "initial_cursor")? {
                        "beginning" => 0,
                        "snapshot" => events.len(),
                        _ => {
                            return Err(error(
                                "Invalid",
                                "initial_cursor must be beginning or snapshot",
                            ))
                        }
                    };
                    b = json!({"participant_id":uuid::Uuid::new_v4().to_string(),"identity_id":identity,"space_id":sid,"endpoint_id":r.args["endpoint_id"],"binding_generation":1,"cursor":start,"initial_cursor":start});
                }
                for key in ["session_id", "attachment_id", "adapter_kind"] {
                    b[key] = json!(text(&r.args, key)?);
                }
                b["provider_thread_id"] = r
                    .args
                    .get("provider_thread_id")
                    .cloned()
                    .unwrap_or(Value::Null);
                b["paused"] = json!(false);
                if !b["provider_thread_id"].is_null()
                    && !b["provider_thread_id"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty())
                {
                    return Err(error(
                        "Invalid",
                        "provider_thread_id must be a nonempty string or null",
                    ));
                }
                if l.bindings.iter().any(|x| {
                    x["participant_id"] != b["participant_id"]
                        && x["identity_id"] == identity
                        && x["space_id"] == sid
                        && x["endpoint_id"] == b["endpoint_id"]
                        && x["session_id"] == b["session_id"]
                        && x["attachment_id"] == b["attachment_id"]
                }) {
                    return Err(error(
                        "Conflict",
                        "session attachment already bound; use resume",
                    ));
                }
                b["admission"] =
                    serde_json::to_value(self.admit(l, &b, auth, true)?).map_err(internal)?;
                if let Some(i) = index {
                    l.bindings[i] = b.clone()
                } else {
                    l.bindings.push(b.clone())
                };
                b
            }
            "participants.read" => {
                let start = match r.args.get("cursor") {
                    None => number(&b, "cursor")? as usize,
                    Some(Value::String(c)) => parse_cursor(c, sid)?,
                    _ => return Err(error("Invalid", "cursor must be explicit string")),
                };
                let limit = r
                    .args
                    .get("limit")
                    .map(|v| {
                        v.as_u64()
                            .filter(|n| (1..=100).contains(n))
                            .ok_or_else(|| error("Invalid", "limit must be 1..100"))
                    })
                    .transpose()?
                    .unwrap_or(50) as usize;
                if start > events.len() {
                    return Ok(
                        json!({"events":[],"next_cursor":cursor(sid,events.len()),"retained_from":cursor(sid,0),"gap":true,"paused":b["paused"],"binding_generation":b["binding_generation"],"durable_cursor":cursor(sid,number(&b,"cursor")? as usize)}),
                    );
                }
                let end = (start + limit).min(events.len());
                let mut output = Vec::new();
                for (offset, event) in events.iter().enumerate().take(end).skip(start) {
                    let eid = text(event, "id")?;
                    let operation_id = format!("inbox:{eid}");
                    let envelope = json!({"event_id":eid,"space_id":sid,"topic_id":event["topic_id"],"topic_revision":event["revision_after"],"submission_id":event["submission"]["id"],"provenance":event_provenance(event),"causation_id":event["request_id"],"correlation_id":event["topic_id"],"created_at":event["created_at"],"activity":event,"operation_request_id":operation_id});
                    if let Some(intent) = l.intents.iter_mut().find(|i| {
                        i["participant_id"] == b["participant_id"]
                            && i["operation_request_id"] == operation_id
                    }) {
                        // Correct legacy cached metadata without changing delivery state or identity.
                        intent["envelope"] = envelope.clone();
                    } else {
                        l.intents.push(json!({"participant_id":b["participant_id"],"operation_request_id":operation_id,"event_id":eid,"position":offset+1,"state":"pending","kind":"inbox","envelope":envelope}));
                    }
                    output.push(envelope);
                }
                json!({"events":output,"next_cursor":cursor(sid,end),"retained_from":cursor(sid,0),"gap":false,"paused":b["paused"],"binding_generation":b["binding_generation"],"durable_cursor":cursor(sid,number(&b,"cursor")? as usize)})
            }
            "participants.receipt" => {
                let state = text(&r.args, "state")?;
                if !["delivered", "handled", "blocked"].contains(&state) {
                    return Err(error("Invalid", "unknown receipt state"));
                }
                let intent = l
                    .intents
                    .iter_mut()
                    .find(|i| {
                        i["participant_id"] == b["participant_id"]
                            && i["event_id"] == r.args["event_id"]
                    })
                    .ok_or_else(|| error("NotFound", "event has no durable inbox intent"))?;
                let result = json!({"participant_id":b["participant_id"],"event_id":intent["event_id"],"binding_generation":b["binding_generation"],"state":state,"provider_turn_id":r.args["provider_turn_id"],"evidence_ref":r.args["evidence_ref"],"request_id":r.request_id,"task_state_changed":false});
                intent["state"] = json!("confirmed");
                intent["receipt"] = result.clone();
                l.receipts.push(result.clone());
                let mut current = number(&b, "cursor")?;
                while l.intents.iter().any(|i| {
                    i["participant_id"] == b["participant_id"]
                        && i["position"] == current + 1
                        && i["state"] == "confirmed"
                }) {
                    current += 1;
                }
                b["cursor"] = json!(current);
                l.bindings[index.unwrap()] = b;
                result
            }
            "participants.pause" => {
                b["paused"] = json!(true);
                b["binding_generation"] = json!(next_binding_generation(&b)?);
                l.bindings[index.unwrap()] = b.clone();
                json!({"binding":b,"provider_turn_cancelled":false})
            }
            "participants.resume" => {
                b["binding_generation"] = json!(next_binding_generation(&b)?);
                b["admission"] =
                    serde_json::to_value(self.admit(l, &b, auth, true)?).map_err(internal)?;
                b["paused"] = json!(false);
                l.bindings[index.unwrap()] = b.clone();
                b
            }
            "operations.get" => {
                let rid = text(&r.args, "operation_request_id")?;
                let existing = l
                    .intents
                    .iter()
                    .find(|i| {
                        i["participant_id"] == b["participant_id"]
                            && i["operation_request_id"] == rid
                    })
                    .cloned();
                if let Some(intent) = existing {
                    intent_with_event_origin(intent, events)
                } else {
                    l.requests.iter().find(|x| {
                    x["identity_id"]==identity && x["request_id"]==rid &&
                    (x["result"]["participant_id"]==b["participant_id"] || x["result"]["binding"]["participant_id"]==b["participant_id"])
                }).map(|x|json!({"operation_request_id":rid,"state":"confirmed","receipt":x["result"]}))
                    .ok_or_else(||error("Unsupported","foreign operation lookup needs Engine/SourceJobs read-only receipt API"))?
                }
            }
            "work.checkpoint.get" => {
                let work = text(&r.args, "work_id")?;
                l.checkpoints
                    .iter()
                    .find(|c| c["participant_id"] == b["participant_id"] && c["work_id"] == work)
                    .cloned()
                    .unwrap_or(
                        json!({"participant_id":b["participant_id"],"work_id":work,"revision":0}),
                    )
            }
            "work.checkpoint.set" => {
                let work = text(&r.args, "work_id")?;
                let topic = text(&r.args, "topic_id")?;
                let assignment = number(&r.args, "assignment_revision")?;
                let t = topics
                    .iter()
                    .find(|t| t["id"] == topic)
                    .ok_or_else(|| error("NotFound", "checkpoint topic not found in space"))?;
                if t["revision"] != assignment {
                    return Err(error("Conflict", "stale topic revision"));
                }
                if !r.args["submission_id"].is_null()
                    && t["current_submission"]["id"] != r.args["submission_id"]
                {
                    return Err(error("Conflict", "stale submission"));
                }
                for key in ["completed", "pending", "unknown", "artifacts"] {
                    if !r.args[key].is_array() {
                        return Err(error("Invalid", format!("{key} must be array")));
                    }
                }
                for key in ["pending", "unknown"] {
                    for reference in r.args[key].as_array().unwrap() {
                        let rid = reference.as_str().ok_or_else(|| {
                            error("Invalid", "operation reference must be string")
                        })?;
                        if !l.intents.iter().any(|i| {
                            i["participant_id"] == b["participant_id"]
                                && i["operation_request_id"] == rid
                        }) {
                            return Err(error(
                                "Unsupported",
                                "operation reference has no durable participant intent",
                            ));
                        }
                    }
                }
                let existing = l.checkpoints.iter().position(|c| {
                    c["participant_id"] == b["participant_id"] && c["work_id"] == work
                });
                let revision = existing
                    .map(|i| l.checkpoints[i]["revision"].as_u64().unwrap())
                    .unwrap_or(0);
                if number(&r.args, "expected_revision")? != revision {
                    return Err(error("Conflict", "checkpoint revision changed"));
                }
                let mut cp = r.args.clone();
                cp["revision"] = json!(revision + 1);
                cp["task_state_changed"] = json!(false);
                cp.as_object_mut().unwrap().remove("expected_revision");
                if let Some(i) = existing {
                    l.checkpoints[i] = cp.clone()
                } else {
                    l.checkpoints.push(cp.clone())
                };
                cp
            }
            _ => unreachable!(),
        };
        if !matches!(
            r.tool.as_str(),
            "participants.read" | "work.checkpoint.get" | "operations.get"
        ) {
            l.requests.push(json!({"identity_id":identity,"request_id":r.request_id,"request":r,"result":result}));
        }
        Ok(result)
    }
}
fn next_binding_generation(binding: &Value) -> Result<u64, ApiError> {
    number(binding, "binding_generation")?
        .checked_add(1)
        .ok_or_else(|| error("Conflict", "binding generation exhausted"))
}
fn event_provenance(event: &Value) -> Value {
    event
        .get("provenance")
        .filter(|origin| origin.is_object())
        .cloned()
        .unwrap_or_else(|| json!({"kind":"unknown"}))
}
fn intent_with_event_origin(mut intent: Value, events: &[Value]) -> Value {
    let origin = events
        .iter()
        .find(|event| event["id"] == intent["event_id"])
        .map(event_provenance)
        .unwrap_or_else(|| json!({"kind":"unknown"}));
    if let Some(envelope) = intent["envelope"].as_object_mut() {
        envelope.insert("provenance".into(), origin);
    }
    intent
}
fn cursor(sid: &str, offset: usize) -> String {
    format!("{sid}:{offset}")
}
fn parse_cursor(c: &str, sid: &str) -> Result<usize, ApiError> {
    c.strip_prefix(&format!("{sid}:"))
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| error("Invalid", "cursor belongs to another space or is malformed"))
}
