//! Durable explicitly requested GitHub synchronization jobs, owned by the daemon.
use crate::{
    runtime_core::{ApiError, Engine, ToolRequest},
    runtime_source::{
        normalize_repository, RepositoryPolicy, SourceAdapter, SourceConfig, SourceSnapshot,
        WriteReceipt, WriteState,
    },
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

pub type SourceConfigFactory = dyn Fn(&Path, &Value) -> Result<SourceConfig, String> + Send + Sync;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceJobSnapshot {
    pub job_id: String,
    pub space_id: String,
    pub endpoint_id: String,
    pub status: String,
    pub topic_ids: Vec<String>,
    pub write_receipt: Option<WriteReceipt>,
    pub error: Option<String>,
}
#[derive(Clone)]
pub struct SourceJobs {
    dir: PathBuf,
    engine: Engine,
    factory: Arc<SourceConfigFactory>,
    db: Arc<Mutex<Connection>>,
    cancel: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    gates: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    request: ToolRequest,
    identity_id: String,
    snapshot: SourceJobSnapshot,
    #[serde(default)]
    source_snapshots: Vec<SourceSnapshot>,
    #[serde(default)]
    checkpoint_ready: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Projection {
    creator_identity: String,
    #[serde(default)]
    origin: Option<Value>,
    create_request: ToolRequest,
    create_fingerprint: String,
    topic_id: Option<String>,
    applied_fingerprint: Option<String>,
    #[serde(default)]
    acknowledged: Vec<String>,
    pending: Option<Revision>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Revision {
    #[serde(default)]
    origin: Option<Value>,
    identity_id: String,
    fingerprint: String,
    request: ToolRequest,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl SourceJobs {
    pub fn open(dir: &Path, engine: Engine) -> anyhow::Result<Self> {
        Self::open_with_config_factory(
            dir,
            engine,
            Arc::new(|path, _| Ok(SourceConfig::new(path.into()))),
        )
    }
    pub fn open_with_config_factory(
        dir: &Path,
        engine: Engine,
        factory: Arc<SourceConfigFactory>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir.join("sources"))?;
        let mut db = Connection::open(dir.join("source-jobs.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(2))?;
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;CREATE TABLE IF NOT EXISTS source_jobs(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE IF NOT EXISTS source_topics(id TEXT PRIMARY KEY,record TEXT NOT NULL);")?;
        {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let rows = {
                let mut statement = tx.prepare("SELECT id,record FROM source_jobs")?;
                let values = statement
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                values.collect::<Result<Vec<_>, _>>()?
            };
            for (id, encoded) in rows {
                let mut record: Record = serde_json::from_str(&encoded)?;
                if ["running", "applying"].contains(&record.snapshot.status.as_str()) {
                    record.snapshot.status = "unknown".into();
                    record.snapshot.error=Some("runtime restarted; previous worker outcome is unknown and will not be relaunched".into());
                    tx.execute(
                        "UPDATE source_jobs SET record=?1 WHERE id=?2",
                        params![serde_json::to_string(&record)?, id],
                    )?;
                }
            }
            tx.commit()?;
        }
        Ok(Self {
            dir: dir.into(),
            engine,
            factory,
            db: Arc::new(Mutex::new(db)),
            cancel: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicUsize::new(0)),
            gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }
    fn authorize(
        &self,
        credential: &str,
        space: &str,
        endpoint: &str,
        operation: &str,
    ) -> Result<Value, ApiError> {
        let value = self.engine.execute(
            credential,
            &tool(
                "source:authorize",
                "endpoint.authorize",
                space,
                json!({"endpoint_id":endpoint,"operation":operation}),
            ),
        )?;
        if value["endpoint"]["kind"] != "github" {
            return Err(error("Unsupported", "endpoint is not a GitHub source"));
        }
        Ok(value)
    }
    fn save(&self, record: &Record) -> Result<(), ApiError> {
        self.db
            .lock()
            .map_err(storage)?
            .execute(
                "UPDATE source_jobs SET record=?1 WHERE id=?2",
                params![
                    serde_json::to_string(record).map_err(storage)?,
                    record.snapshot.job_id
                ],
            )
            .map_err(storage)?;
        Ok(())
    }
    fn record(&self, id: &str) -> Result<Record, ApiError> {
        let encoded: String = self
            .db
            .lock()
            .map_err(storage)?
            .query_row("SELECT record FROM source_jobs WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(storage)?
            .ok_or_else(|| error("NotFound", "source job not found"))?;
        serde_json::from_str(&encoded).map_err(storage)
    }
    pub fn start(
        &self,
        credential: &str,
        request: &ToolRequest,
    ) -> Result<SourceJobSnapshot, ApiError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(error(
                "ShuttingDown",
                "source jobs no longer accept requests",
            ));
        }
        let fields: &[&str] = match request.tool.as_str() {
            "sources.sync" => &["endpoint_id"],
            "sources.resume" => &["endpoint_id", "sync_job_id"],
            "sources.writeback" => &["endpoint_id", "number", "body"],
            "sources.reconcile" => &["endpoint_id", "number", "write_request_id"],
            _ => return Err(error("Unsupported", "unknown source operation")),
        };
        if request.request_id.trim().is_empty()
            || request.request_id.len() > 200
            || !request.args.as_object().is_some_and(|a| {
                a.len() == fields.len() && fields.iter().all(|k| a.contains_key(*k))
            })
        {
            return Err(error(
                "Invalid",
                "source operation has invalid fields or request id",
            ));
        }
        let sid = request
            .space_id
            .as_deref()
            .ok_or_else(|| error("Invalid", "taskspace required"))?;
        let eid = request.args["endpoint_id"]
            .as_str()
            .ok_or_else(|| error("Invalid", "endpoint_id required"))?;
        let admission = self.authorize(credential, sid, eid, "manage")?;
        if request.tool == "sources.writeback"
            && admission["endpoint"]["config"]["allow_write_back"].as_bool() != Some(true)
        {
            return Err(error(
                "WriteBackDisabled",
                "repository does not explicitly allow external comments",
            ));
        }
        let identity = admission["identity_id"]
            .as_str()
            .ok_or_else(|| error("Denied", "source caller identity required"))?;
        let source_snapshots = if request.tool == "sources.resume" {
            let original = self.record(
                request.args["sync_job_id"]
                    .as_str()
                    .ok_or_else(|| error("Invalid", "sync_job_id required"))?,
            )?;
            if original.identity_id != identity
                || original.snapshot.space_id != sid
                || original.snapshot.endpoint_id != eid
                || !["sources.sync", "sources.resume"].contains(&original.request.tool.as_str())
            {
                return Err(error("Denied","only this caller's same-endpoint sync checkpoint may resume; external writes cannot resume"));
            }
            if ["running", "applying"].contains(&original.snapshot.status.as_str()) {
                return Err(error(
                    "JobConflict",
                    "original source worker is still active",
                ));
            }
            if !original.checkpoint_ready {
                return Err(error(
                    "NoCheckpoint",
                    "source fetch was not durably completed; explicitly start a new read-only sync",
                ));
            }
            let repository = normalize_repository(
                admission["endpoint"]["config"]["repository"]
                    .as_str()
                    .unwrap_or_default(),
            )
            .map_err(|e| error("SourceConfig", e))?;
            if original
                .source_snapshots
                .iter()
                .any(|s| normalize_repository(&s.repository).as_ref() != Ok(&repository))
            {
                return Err(error(
                    "SourceConfigChanged",
                    "checkpoint belongs to another repository configuration",
                ));
            }
            original.source_snapshots
        } else {
            vec![]
        };
        let record = Record {
            request: request.clone(),
            identity_id: identity.into(),
            snapshot: SourceJobSnapshot {
                job_id: request.request_id.clone(),
                space_id: sid.into(),
                endpoint_id: eid.into(),
                status: "running".into(),
                topic_ids: vec![],
                write_receipt: None,
                error: None,
            },
            source_snapshots,
            checkpoint_ready: request.tool == "sources.resume",
        };
        {
            let mut db = self.db.lock().map_err(storage)?;
            let transaction = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT record FROM source_jobs WHERE id=?1",
                    [&request.request_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            if let Some(previous) = previous {
                let previous: Record = serde_json::from_str(&previous).map_err(storage)?;
                if previous.identity_id != identity {
                    return Err(error("Denied", "source job belongs to another identity"));
                }
                if serde_json::to_value(&previous.request).map_err(storage)?
                    != serde_json::to_value(request).map_err(storage)?
                {
                    return Err(error(
                        "RequestConflict",
                        "source job id already names different inputs",
                    ));
                }
                return Ok(previous.snapshot);
            }
            transaction
                .execute(
                    "INSERT INTO source_jobs(id,record) VALUES(?1,?2)",
                    params![
                        request.request_id,
                        serde_json::to_string(&record).map_err(storage)?
                    ],
                )
                .map_err(storage)?;
            transaction.commit().map_err(storage)?;
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        let guard = Active(self.active.clone());
        let jobs = self.clone();
        let credential = credential.to_string();
        let mut worker = record.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("source-{}", &digest(&request.request_id)[..12]))
            .spawn(move || {
                let _guard = guard;
                let result = jobs.run_operation(&credential, &mut worker);
                let result = if jobs.cancel.load(Ordering::SeqCst) {
                    Err(error(
                        "Cancelled",
                        "source worker stopped; partial effects may exist",
                    ))
                } else {
                    result
                };
                match result {
                    Ok(()) => worker.snapshot.status = "completed".into(),
                    Err(e) => {
                        worker.snapshot.status =
                            if ["Cancelled", "UncertainWrite", "UncertainProjection"]
                                .contains(&e.code.as_str())
                            {
                                "unknown"
                            } else {
                                "failed"
                            }
                            .into();
                        worker.snapshot.error = Some(e.to_string());
                    }
                }
                let _ = jobs.save(&worker);
            });
        if let Err(e) = spawned {
            let mut record = record;
            record.snapshot.status = "failed".into();
            record.snapshot.error = Some("source worker could not start".into());
            self.save(&record)?;
            return Err(error("SourceWorker", e.to_string()));
        }
        Ok(record.snapshot)
    }
    fn checkpoint(&self) -> Result<(), ApiError> {
        if self.cancel.load(Ordering::SeqCst) {
            Err(error(
                "Cancelled",
                "source job cancelled; partial effects may exist",
            ))
        } else {
            Ok(())
        }
    }
    fn adapter(
        &self,
        credential: &str,
        record: &Record,
        endpoint: &Value,
    ) -> Result<(SourceAdapter, String), ApiError> {
        let repository = normalize_repository(
            endpoint["config"]["repository"]
                .as_str()
                .ok_or_else(|| error("Invalid", "GitHub endpoint requires repository"))?,
        )
        .map_err(|e| error("SourceConfig", e))?;
        let eid = endpoint["id"]
            .as_str()
            .ok_or_else(|| error("Invalid", "endpoint identity missing"))?;
        let ledger = self
            .dir
            .join("sources")
            .join(format!("{}.sqlite3", digest(eid)));
        let mut config = (self.factory)(&ledger, endpoint).map_err(|e| error("SourceConfig", e))?;
        config.ledger_path = ledger;
        config.repositories = vec![RepositoryPolicy {
            repository: repository.clone(),
            allow_write_back: endpoint["config"]["allow_write_back"]
                .as_bool()
                .unwrap_or(false),
        }];
        let engine = self.engine.clone();
        let credential = credential.to_string();
        let sid = record.snapshot.space_id.clone();
        let eid = record.snapshot.endpoint_id.clone();
        let expected = endpoint["config"].clone();
        let guard = Arc::new(move || {
            let current = engine
                .execute(
                    &credential,
                    &tool(
                        "source:dispatch-authorize",
                        "endpoint.authorize",
                        &sid,
                        json!({"endpoint_id":eid,"operation":"manage"}),
                    ),
                )
                .map_err(|e| e.to_string())?;
            if current["endpoint"]["kind"] != "github" || current["endpoint"]["config"] != expected
            {
                return Err("SourceConfigChanged: re-open source job against current endpoint configuration".into());
            }
            Ok(())
        });
        Ok((
            SourceAdapter::new(config)
                .map_err(|e| error("SourceConfig", e))?
                .with_authorization_guard(guard)
                .with_cancellation(self.cancel.clone()),
            repository,
        ))
    }
    fn run_operation(&self, credential: &str, record: &mut Record) -> Result<(), ApiError> {
        self.checkpoint()?;
        let gate = self
            .gates
            .lock()
            .map_err(storage)?
            .entry(record.snapshot.endpoint_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = gate.lock().map_err(storage)?;
        match record.request.tool.as_str() {
            "sources.sync" => self.run_sync(credential, record),
            "sources.resume" => self.apply_checkpoint(credential, record),
            "sources.writeback" | "sources.reconcile" => self.run_write(credential, record),
            _ => Err(error("Unsupported", "source operation unavailable")),
        }
    }
    fn run_write(&self, credential: &str, record: &mut Record) -> Result<(), ApiError> {
        self.checkpoint()?;
        let auth = self.authorize(
            credential,
            &record.snapshot.space_id,
            &record.snapshot.endpoint_id,
            "manage",
        )?;
        let (adapter, repository) = self.adapter(credential, record, &auth["endpoint"])?;
        let number = record.request.args["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(|| error("Invalid", "positive GitHub issue/PR number required"))?;
        if record.request.tool == "sources.writeback" {
            let body = record.request.args["body"]
                .as_str()
                .ok_or_else(|| error("Invalid", "comment body required"))?;
            let receipt = adapter
                .post_comment(&repository, number, &record.snapshot.job_id, body)
                .map_err(|e| {
                    error(
                        if e.contains("UncertainWrite") {
                            "UncertainWrite"
                        } else {
                            "SourceWrite"
                        },
                        e,
                    )
                })?;
            let unknown = receipt.state == WriteState::Unknown;
            record.snapshot.write_receipt = Some(receipt);
            self.save(record)?;
            if unknown {
                return Err(error(
                    "UncertainWrite",
                    "external comment effect requires read-only reconciliation",
                ));
            }
        } else {
            let write_id = record.request.args["write_request_id"]
                .as_str()
                .ok_or_else(|| error("Invalid", "original write request id required"))?;
            let mut original = self.record(write_id)?;
            if original.identity_id != record.identity_id
                || original.snapshot.space_id != record.snapshot.space_id
                || original.snapshot.endpoint_id != record.snapshot.endpoint_id
                || original.request.tool != "sources.writeback"
                || original.request.args["number"].as_u64() != Some(number)
            {
                return Err(error(
                    "Denied",
                    "write reconciliation must reference this caller's same endpoint and target",
                ));
            }
            let receipt = adapter
                .reconcile_comment(&repository, number, write_id)
                .map_err(|e| error("SourceReconcile", e))?;
            let unknown = receipt.state == WriteState::Unknown;
            record.snapshot.write_receipt = Some(receipt.clone());
            self.save(record)?;
            if unknown {
                return Err(error(
                    "UncertainWrite",
                    "matching remote evidence not available; no repeat POST allowed",
                ));
            }
            original.snapshot.write_receipt = Some(receipt);
            original.snapshot.status = "completed".into();
            original.snapshot.error = None;
            self.save(&original)?;
        }
        Ok(())
    }
    fn run_sync(&self, credential: &str, record: &mut Record) -> Result<(), ApiError> {
        self.checkpoint()?;
        let auth = self.authorize(
            credential,
            &record.snapshot.space_id,
            &record.snapshot.endpoint_id,
            "manage",
        )?;
        let (adapter, repository) = self.adapter(credential, record, &auth["endpoint"])?;
        let report = adapter
            .ingest_repository(&repository)
            .map_err(|e| error("SourceRead", e))?;
        record.source_snapshots = report.snapshots;
        record.checkpoint_ready = true;
        record.snapshot.status = "applying".into();
        self.save(record)?;
        self.apply_checkpoint(credential, record)
    }
    fn apply_checkpoint(&self, credential: &str, record: &mut Record) -> Result<(), ApiError> {
        self.checkpoint()?;
        self.authorize(
            credential,
            &record.snapshot.space_id,
            &record.snapshot.endpoint_id,
            "manage",
        )?;
        record.snapshot.status = "applying".into();
        self.save(record)?;
        let mut seen = std::collections::HashSet::new();
        for snapshot in record.source_snapshots.clone() {
            if !seen.insert(snapshot.source_id.clone()) {
                continue;
            }
            self.checkpoint()?;
            self.authorize(
                credential,
                &record.snapshot.space_id,
                &record.snapshot.endpoint_id,
                "manage",
            )?;
            let topic = self.project(credential, record, &snapshot)?;
            record.snapshot.topic_ids.push(topic);
            self.save(record)?;
        }
        Ok(())
    }
    fn save_projection(&self, key: &str, projection: &Projection) -> Result<(), ApiError> {
        self.db.lock().map_err(storage)?.execute("INSERT INTO source_topics(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![key,serde_json::to_string(projection).map_err(storage)?]).map_err(storage)?;
        Ok(())
    }
    fn apply(
        &self,
        credential: &str,
        record: &Record,
        request: &ToolRequest,
    ) -> Result<Value, ApiError> {
        self.checkpoint()?;
        self.engine.execute_requiring_endpoint(
            credential,
            request,
            &record.snapshot.endpoint_id,
            "manage",
        )
    }
    fn project(
        &self,
        credential: &str,
        record: &Record,
        snapshot: &SourceSnapshot,
    ) -> Result<String, ApiError> {
        let key = digest(
            &serde_json::to_string(&(
                &record.snapshot.space_id,
                &record.snapshot.endpoint_id,
                &snapshot.source_id,
            ))
            .map_err(storage)?,
        );
        let fingerprint = digest(&serde_json::to_string(snapshot).map_err(storage)?);
        let saved: Option<String> = self
            .db
            .lock()
            .map_err(storage)?
            .query_row(
                "SELECT record FROM source_topics WHERE id=?1",
                [&key],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage)?;
        let mut projection = match saved {
            Some(v) => serde_json::from_str::<Projection>(&v).map_err(storage)?,
            None => {
                let projection = Projection {
                    creator_identity: record.identity_id.clone(),
                    origin: Some(source_origin(&record.snapshot.endpoint_id, snapshot)),
                    create_request: tool(
                        &format!("source:create:{key}"),
                        "topics.create",
                        &record.snapshot.space_id,
                        json!({"title":snapshot.title,"body":source_body(snapshot),"kind":"task"}),
                    ),
                    create_fingerprint: fingerprint.clone(),
                    topic_id: None,
                    applied_fingerprint: None,
                    acknowledged: vec![],
                    pending: None,
                };
                self.save_projection(&key, &projection)?;
                projection
            }
        };
        if projection.topic_id.is_none() {
            if projection.creator_identity != record.identity_id {
                return Err(error(
                    "UncertainProjection",
                    "original source creator must reconcile pending topic creation",
                ));
            }
            self.checkpoint()?;
            let created = match &projection.origin {
                Some(origin) => self.engine.execute_source_projection(credential, &projection.create_request, &record.snapshot.endpoint_id, origin)?,
                None => return Err(error("LegacyProjection", "legacy pending source creation lacks provenance; explicit reimport into an approved endpoint is required")),
            };
            projection.topic_id = Some(
                created["id"]
                    .as_str()
                    .ok_or_else(|| error("Internal", "source topic lacks id"))?
                    .into(),
            );
            projection.applied_fingerprint = Some(projection.create_fingerprint.clone());
            self.save_projection(&key, &projection)?;
        }
        let topic = projection.topic_id.clone().unwrap();
        // Preserve acknowledgement history, not only the latest fingerprint:
        // resuming an older checkpoint must not append an already applied
        // source version after a newer version has been imported.
        for known in [
            Some(projection.create_fingerprint.clone()),
            projection.applied_fingerprint.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if !projection.acknowledged.contains(&known) {
                projection.acknowledged.push(known);
            }
        }
        loop {
            if let Some(mut pending) = projection.pending.clone() {
                if pending.identity_id != record.identity_id {
                    return Err(error(
                        "UncertainProjection",
                        "original source updater must reconcile pending comment",
                    ));
                }
                let mut completed = false;
                for _ in 0..3 {
                    self.checkpoint()?;
                    let applied = match &pending.origin {
                        Some(origin) => self.engine.execute_source_projection(
                            credential,
                            &pending.request,
                            &record.snapshot.endpoint_id,
                            origin,
                        ),
                        None => self.apply(credential, record, &pending.request),
                    };
                    match applied {
                        Ok(_) => {
                            completed = true;
                            break;
                        }
                        Err(e) if e.code == "TopicConflict" => {
                            // Replaying the exact persisted request first repairs a
                            // crash-after-commit. Only a rejected transaction has
                            // no receipt, so only then may expected_revision move.
                            let current = self.apply(
                                credential,
                                record,
                                &tool(
                                    "source:read-revision",
                                    "topics.read",
                                    &record.snapshot.space_id,
                                    json!({"topic_id":topic}),
                                ),
                            )?;
                            pending.request.args["expected_revision"] =
                                current["topic"]["revision"].clone();
                            projection.pending = Some(pending.clone());
                            self.save_projection(&key, &projection)?;
                        }
                        Err(e) => return Err(e),
                    }
                }
                if !completed {
                    return Err(error(
                        "TopicConflict",
                        "source revision conflicted repeatedly; saved intent can be reconciled",
                    ));
                }
                if !projection.acknowledged.contains(&pending.fingerprint) {
                    projection.acknowledged.push(pending.fingerprint.clone());
                }
                projection.applied_fingerprint = Some(pending.fingerprint);
                projection.pending = None;
                self.save_projection(&key, &projection)?;
            }
            if projection.acknowledged.contains(&fingerprint) {
                self.save_projection(&key, &projection)?;
                return Ok(topic);
            }
            let current = self.apply(
                credential,
                record,
                &tool(
                    "source:read-revision",
                    "topics.read",
                    &record.snapshot.space_id,
                    json!({"topic_id":topic}),
                ),
            )?;
            projection.pending = Some(Revision {
                origin: Some(source_origin(&record.snapshot.endpoint_id, snapshot)),
                identity_id: record.identity_id.clone(),
                fingerprint: fingerprint.clone(),
                request: tool(
                    &format!("source:revision:{key}:{fingerprint}"),
                    "task.act",
                    &record.snapshot.space_id,
                    json!({"topic_id":topic,"action":"comment","comment":format!("GitHub source revision\n\n{}",source_body(snapshot)),"expected_revision":current["topic"]["revision"]}),
                ),
            });
            self.save_projection(&key, &projection)?;
        }
    }
    pub fn get(&self, credential: &str, job_id: &str) -> Result<SourceJobSnapshot, ApiError> {
        let record = self.record(job_id)?;
        let auth = self
            .authorize(
                credential,
                &record.snapshot.space_id,
                &record.snapshot.endpoint_id,
                "observe",
            )
            .or_else(|_| {
                self.authorize(
                    credential,
                    &record.snapshot.space_id,
                    &record.snapshot.endpoint_id,
                    "manage",
                )
            })?;
        if auth["identity_id"] != record.identity_id {
            return Err(error("Denied", "source job belongs to another identity"));
        }
        Ok(record.snapshot)
    }
    pub fn cancel_all(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}
fn source_origin(endpoint: &str, snapshot: &SourceSnapshot) -> Value {
    json!({"kind":"github", "endpoint_id":endpoint, "repository":snapshot.repository,
        "source_id":snapshot.source_id, "source_version":snapshot.updated_at,
        "original_author":snapshot.author, "trust":"external"})
}
fn error(code: &str, message: impl Into<String>) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
    }
}
fn storage(_: impl std::fmt::Display) -> ApiError {
    error("Internal", "source job ledger failure")
}
fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn tool(id: &str, name: &str, space: &str, args: Value) -> ToolRequest {
    ToolRequest {
        request_id: id.into(),
        tool: name.into(),
        space_id: Some(space.into()),
        args,
    }
}
fn source_body(snapshot: &SourceSnapshot) -> String {
    format!(
        "GitHub source: {}\nSource ID: {}\nOriginal author: {}\nUpdated: {}\nState: {}\n\n{}",
        snapshot.url,
        snapshot.source_id,
        snapshot
            .author
            .as_ref()
            .map(|a| a.login.as_str())
            .unwrap_or("[deleted]"),
        snapshot.updated_at,
        snapshot.state,
        snapshot.body
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::{runtime_core::owner_token_path, runtime_source::tests::Fixture};
    use serde_json::json;
    use std::time::{Duration, Instant};
    fn request(id: &str, name: &str, space: Option<&str>, args: Value) -> ToolRequest {
        ToolRequest {
            request_id: id.into(),
            tool: name.into(),
            space_id: space.map(String::from),
            args,
        }
    }
    fn setup() -> (tempfile::TempDir, Engine, String, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let space = engine
            .execute(
                &owner,
                &request(
                    "source-space",
                    "spaces.create",
                    None,
                    json!({"name":"source tests"}),
                ),
            )
            .unwrap();
        let sid = space["space"]["id"].as_str().unwrap().to_string();
        let admin = space["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_string();
        let endpoint=engine.execute(&admin,&request("source-endpoint","endpoints.register",Some(&sid),json!({"name":"GitHub","kind":"github","config":{"repository":"acme/project","allow_write_back":false}}))).unwrap();
        (
            dir,
            engine,
            admin,
            sid,
            endpoint["id"].as_str().unwrap().into(),
        )
    }
    fn wait(jobs: &SourceJobs, credential: &str, id: &str) -> SourceJobSnapshot {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = jobs.get(credential, id).unwrap();
            if !["running", "applying"].contains(&snapshot.status.as_str()) {
                return snapshot;
            }
            assert!(
                Instant::now() < until,
                "background source job must finish: {snapshot:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn explicit_sync_returns_promptly_and_imports_authorized_source_through_engine() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let jobs = SourceJobs::open_with_config_factory(
            &dir.path().join("jobs"),
            engine.clone(),
            Arc::new(move |_, _| Ok(config.clone())),
        )
        .unwrap();
        let started = Instant::now();
        let accepted = jobs
            .start(
                &admin,
                &request(
                    "sync-tracer",
                    "sources.sync",
                    Some(&sid),
                    json!({"endpoint_id":eid}),
                ),
            )
            .expect("explicit source sync starts a background job");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "HTTP caller must not wait for gh"
        );
        assert_eq!(accepted.job_id, "sync-tracer");
        let done = wait(&jobs, &admin, "sync-tracer");
        assert_eq!(done.status, "completed", "{done:?}");
        assert_eq!(done.topic_ids.len(), 2);
        let view = engine
            .execute(
                &admin,
                &request(
                    "source-topic-read",
                    "topics.read",
                    Some(&sid),
                    json!({"topic_id":done.topic_ids[0]}),
                ),
            )
            .unwrap();
        let origin = &view["topic"]["provenance"];
        assert_eq!(origin["kind"], "github");
        assert_eq!(origin["trust"], "external");
        assert_eq!(origin["endpoint_id"], eid);
        assert_eq!(origin["repository"], "acme/project");
        assert_eq!(origin["original_author"]["login"], "author");
        assert!(origin["source_id"]
            .as_str()
            .unwrap()
            .starts_with("github.com:"));
        assert!(origin["source_version"].is_string());
        assert!(origin["fetched_at"].is_u64());
        let body = view["topic"]["body"].as_str().unwrap();
        assert!(body.contains("github.com:node:"));
        assert!(body.contains("Original author: author"));
        assert!(body.contains("Original text"));
        // Crash fixture: Engine committed its receipt but SourceJobs had not saved the topic id.
        let db = Connection::open(dir.path().join("jobs/source-jobs.sqlite3")).unwrap();
        let (key, encoded): (String, String) = db
            .query_row(
                "SELECT id,record FROM source_topics WHERE json_extract(record,'$.topic_id')=?1",
                [&done.topic_ids[0]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let mut projection: Projection = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            engine
                .execute(&admin, &projection.create_request)
                .unwrap_err()
                .code,
            "RequestConflict",
            "ordinary replay cannot launder source origin"
        );
        projection.topic_id = None;
        db.execute(
            "UPDATE source_topics SET record=?1 WHERE id=?2",
            params![serde_json::to_string(&projection).unwrap(), key],
        )
        .unwrap();
        drop(db);
        jobs.start(
            &admin,
            &request(
                "receipt-resume",
                "sources.resume",
                Some(&sid),
                json!({"endpoint_id":eid, "sync_job_id":"sync-tracer"}),
            ),
        )
        .unwrap();
        let replay = wait(&jobs, &admin, "receipt-resume");
        assert_eq!(replay.status, "completed");
        assert_eq!(
            replay.topic_ids, done.topic_ids,
            "protected receipt restores the original topic id"
        );
        let restored = engine
            .execute(
                &admin,
                &request(
                    "restored-origin",
                    "topics.read",
                    Some(&sid),
                    json!({"topic_id":done.topic_ids[0]}),
                ),
            )
            .unwrap();
        assert_eq!(
            restored["topic"], view["topic"],
            "receipt replay preserves protected origin without a new topic"
        );
        assert_eq!(jobs.active_count(), 0);
    }
    #[test]
    fn mixed_case_approved_repository_sync_resume_and_write_share_one_identity() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, _) = setup();
        let endpoint = engine.execute(&admin, &request("mixed-endpoint", "endpoints.register", Some(&sid),
            json!({"name":"mixed-case approved source", "kind":"github", "config":{"repository":"Acme/Project", "allow_write_back":true}}))).unwrap();
        let eid = endpoint["id"].as_str().unwrap();
        assert_eq!(
            endpoint["config"]["repository"], "Acme/Project",
            "display config remains as entered"
        );
        let config = fixture.config.clone();
        let factory: Arc<SourceConfigFactory> = Arc::new(move |_, _| Ok(config.clone()));
        let path = dir.path().join("mixed-jobs");
        let jobs =
            SourceJobs::open_with_config_factory(&path, engine.clone(), factory.clone()).unwrap();
        jobs.start(
            &admin,
            &request(
                "mixed-sync",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        let synced = wait(&jobs, &admin, "mixed-sync");
        assert_eq!(
            synced.status, "completed",
            "mixed-case approved source must project successfully: {synced:?}"
        );
        assert_eq!(synced.topic_ids.len(), 2);
        let view = engine
            .execute(
                &admin,
                &request(
                    "mixed-topic",
                    "topics.read",
                    Some(&sid),
                    json!({"topic_id":synced.topic_ids[0]}),
                ),
            )
            .unwrap();
        assert_eq!(view["topic"]["provenance"]["repository"], "acme/project");
        assert_eq!(view["topic"]["provenance"]["trust"], "external");
        drop(jobs);
        let engine = Engine::open(dir.path()).unwrap();
        let jobs = SourceJobs::open_with_config_factory(&path, engine.clone(), factory).unwrap();
        jobs.start(
            &admin,
            &request(
                "mixed-resume",
                "sources.resume",
                Some(&sid),
                json!({"endpoint_id":eid, "sync_job_id":"mixed-sync"}),
            ),
        )
        .unwrap();
        let resumed = wait(&jobs, &admin, "mixed-resume");
        assert_eq!(resumed.status, "completed");
        assert_eq!(resumed.topic_ids, synced.topic_ids);
        jobs.start(
            &admin,
            &request(
                "mixed-write",
                "sources.writeback",
                Some(&sid),
                json!({"endpoint_id":eid, "number":1, "body":"explicitly authorized reply"}),
            ),
        )
        .unwrap();
        let written = wait(&jobs, &admin, "mixed-write");
        assert_eq!(
            written.status, "completed",
            "mixed-case repository write policy must use the same identity: {written:?}"
        );
        assert_eq!(written.write_receipt.unwrap().repository, "acme/project");
        let listed = engine
            .execute(
                &admin,
                &request("mixed-config", "endpoints.list", Some(&sid), json!({})),
            )
            .unwrap();
        assert_eq!(
            listed["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == eid)
                .unwrap()["config"]["repository"],
            "Acme/Project"
        );
    }

    #[test]
    fn repository_identity_gate_rejects_other_repositories_and_invalid_locators() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let jobs = SourceJobs::open_with_config_factory(
            &dir.path().join("jobs"),
            engine.clone(),
            Arc::new(move |_, _| Ok(config.clone())),
        )
        .unwrap();
        jobs.start(
            &admin,
            &request(
                "identity-base",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        let base = wait(&jobs, &admin, "identity-base");
        assert_eq!(base.status, "completed");
        let db = Connection::open(dir.path().join("jobs/source-jobs.sqlite3")).unwrap();
        let (key, encoded): (String, String) = db
            .query_row("SELECT id,record FROM source_topics LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        for (index, repository) in [
            "acme/other",
            "other/project",
            "https://github.com/Acme/Project",
            "Acme/../Project",
            "Acme/Project?redirect=other",
            "Acme/Project\\evil",
        ]
        .iter()
        .enumerate()
        {
            // Durable intent fixture isolates protected dispatch; caller tools cannot inject origin.
            let mut projection: Value = serde_json::from_str(&encoded).unwrap();
            projection["topic_id"] = Value::Null;
            projection["origin"]["repository"] = json!(repository);
            projection["create_request"]["request_id"] = json!(format!("invalid-origin-{index}"));
            db.execute(
                "UPDATE source_topics SET record=?1 WHERE id=?2",
                params![projection.to_string(), key],
            )
            .unwrap();
            let job = format!("identity-replay-{index}");
            jobs.start(
                &admin,
                &request(
                    &job,
                    "sources.resume",
                    Some(&sid),
                    json!({"endpoint_id":eid, "sync_job_id":"identity-base"}),
                ),
            )
            .unwrap();
            let rejected = wait(&jobs, &admin, &job);
            assert_eq!(
                rejected.status, "failed",
                "repository must remain unauthorized: {repository}"
            );
            assert!(rejected
                .error
                .unwrap()
                .contains("Denied: source origin differs from approved endpoint"));
        }
        let topics = engine
            .execute(
                &admin,
                &request("identity-count", "topics.list", Some(&sid), json!({})),
            )
            .unwrap();
        assert_eq!(topics["items"].as_array().unwrap().len(), 2);
        assert!(topics["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["provenance"]["repository"] == "acme/project"));
    }

    #[test]
    fn legacy_pending_create_cannot_gain_forum_trust_on_resume() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let factory: Arc<SourceConfigFactory> = Arc::new(move |_, _| Ok(config.clone()));
        let path = dir.path().join("jobs");
        let jobs =
            SourceJobs::open_with_config_factory(&path, engine.clone(), factory.clone()).unwrap();
        jobs.start(
            &admin,
            &request(
                "first",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        assert_eq!(wait(&jobs, &admin, "first").status, "completed");
        drop(jobs);
        // Historical durable fixture: a saved intent from before provenance existed, never committed.
        let db = Connection::open(path.join("source-jobs.sqlite3")).unwrap();
        let (key, encoded): (String, String) = db
            .query_row("SELECT id,record FROM source_topics LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let mut projection: Value = serde_json::from_str(&encoded).unwrap();
        projection.as_object_mut().unwrap().remove("origin");
        projection["topic_id"] = Value::Null;
        projection["create_request"]["request_id"] = json!("legacy-uncommitted");
        db.execute(
            "UPDATE source_topics SET record=?1 WHERE id=?2",
            params![projection.to_string(), key],
        )
        .unwrap();
        drop(db);
        let jobs = SourceJobs::open_with_config_factory(&path, engine.clone(), factory).unwrap();
        jobs.start(
            &admin,
            &request(
                "resume-legacy",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        let result = wait(&jobs, &admin, "resume-legacy");
        assert_eq!(
            result.status, "failed",
            "legacy pending intent must not be promoted: {result:?}"
        );
        assert!(result.error.unwrap().contains("LegacyProjection"));
        let topics = engine
            .execute(
                &admin,
                &request("count", "topics.list", Some(&sid), json!({})),
            )
            .unwrap();
        assert_eq!(topics["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn source_mapping_survives_reopen_and_revisions_append_without_replacing_human_work() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let factory: Arc<SourceConfigFactory> = Arc::new(move |_, _| Ok(config.clone()));
        let path = dir.path().join("jobs");
        let jobs =
            SourceJobs::open_with_config_factory(&path, engine.clone(), factory.clone()).unwrap();
        jobs.start(
            &admin,
            &request(
                "map-first",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        let first = wait(&jobs, &admin, "map-first");
        assert_eq!(first.status, "completed");
        let topic = first
            .topic_ids
            .iter()
            .find_map(|id| {
                let view = engine
                    .execute(
                        &admin,
                        &request(
                            "find-issue",
                            "topics.read",
                            Some(&sid),
                            json!({"topic_id":id}),
                        ),
                    )
                    .unwrap();
                view["topic"]["body"]
                    .as_str()
                    .unwrap()
                    .contains("I_STABLE")
                    .then(|| id.clone())
            })
            .unwrap();
        let initial = engine
            .execute(
                &admin,
                &request(
                    "initial-origin",
                    "topics.read",
                    Some(&sid),
                    json!({"topic_id":topic}),
                ),
            )
            .unwrap();
        let initial_origin = initial["topic"]["provenance"].clone();
        engine.execute(&admin,&request("human-work","task.act",Some(&sid),json!({"topic_id":topic,"action":"comment","comment":"Human work stays intact","expected_revision":0}))).unwrap();
        drop(jobs);
        let issue_path = fixture.dir.path().join("issues.json");
        let mut issues: Value =
            serde_json::from_slice(&std::fs::read(&issue_path).unwrap()).unwrap();
        issues[0][0]["body"] = json!("Upstream revised text");
        issues[0][0]["updated_at"] = json!("2026-10-21T02:00:00Z");
        std::fs::write(issue_path, serde_json::to_vec(&issues).unwrap()).unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let jobs = SourceJobs::open_with_config_factory(&path, engine.clone(), factory).unwrap();
        jobs.start(
            &admin,
            &request(
                "map-second",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        let second = wait(&jobs, &admin, "map-second");
        assert_eq!(
            second.status, "completed",
            "source revision must map to its existing topic: {second:?}"
        );
        assert_eq!(second.topic_ids, first.topic_ids);
        let view = engine
            .execute(
                &admin,
                &request(
                    "read-revision",
                    "topics.read",
                    Some(&sid),
                    json!({"topic_id":topic}),
                ),
            )
            .unwrap();
        assert!(view["topic"]["body"]
            .as_str()
            .unwrap()
            .contains("Original text"));
        assert_eq!(view["topic"]["provenance"], initial_origin);
        assert_eq!(
            view["comments"][1]["provenance"]["source_version"],
            "2026-10-21T02:00:00Z"
        );
        assert_eq!(view["comments"][1]["provenance"]["trust"], "external");
        assert_eq!(view["comments"].as_array().unwrap().len(), 2);
        assert_eq!(view["comments"][0]["comment"], "Human work stays intact");
        assert!(view["comments"][1]["comment"]
            .as_str()
            .unwrap()
            .contains("Upstream revised text"));
        jobs.start(
            &admin,
            &request(
                "map-third",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        assert_eq!(wait(&jobs, &admin, "map-third").status, "completed");
        let view = engine
            .execute(
                &admin,
                &request(
                    "read-no-duplicate",
                    "topics.read",
                    Some(&sid),
                    json!({"topic_id":topic}),
                ),
            )
            .unwrap();
        assert_eq!(view["comments"].as_array().unwrap().len(), 2);
        jobs.start(
            &admin,
            &request(
                "old-checkpoint",
                "sources.resume",
                Some(&sid),
                json!({"endpoint_id":eid, "sync_job_id":"map-first"}),
            ),
        )
        .unwrap();
        assert_eq!(wait(&jobs, &admin, "old-checkpoint").status, "completed");
        let replayed = engine
            .execute(
                &admin,
                &request(
                    "after-old-checkpoint",
                    "topics.read",
                    Some(&sid),
                    json!({"topic_id":topic}),
                ),
            )
            .unwrap();
        assert_eq!(replayed["topic"]["provenance"], initial_origin);
        assert_eq!(replayed["comments"].as_array().unwrap().len(), 2);
        let topics = engine
            .execute(
                &admin,
                &request("read-topic-count", "topics.list", Some(&sid), json!({})),
            )
            .unwrap();
        assert_eq!(topics["items"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn revoking_endpoint_stops_next_source_process_and_denies_job_status() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let invite = engine
            .execute(
                &admin,
                &request(
                    "source-invite",
                    "members.invite",
                    Some(&sid),
                    json!({"roles":["implementer"]}),
                ),
            )
            .unwrap();
        let member = engine
            .execute(
                invite["invite_token"].as_str().unwrap(),
                &request(
                    "source-member",
                    "members.register",
                    Some(&sid),
                    json!({"name":"source operator"}),
                ),
            )
            .unwrap();
        let credential = member["credential"].as_str().unwrap();
        let mid = member["id"].as_str().unwrap();
        engine
            .execute(
                &admin,
                &request(
                    "source-grant",
                    "endpoints.grant",
                    Some(&sid),
                    json!({"endpoint_id":eid,"member_id":mid,"operations":["manage","observe"]}),
                ),
            )
            .unwrap();
        std::fs::write(fixture.dir.path().join("slow-read"), b"yes").unwrap();
        let config = fixture.config.clone();
        let jobs = SourceJobs::open_with_config_factory(
            &dir.path().join("jobs"),
            engine.clone(),
            Arc::new(move |_, _| Ok(config.clone())),
        )
        .unwrap();
        let req = request(
            "source-revoked",
            "sources.sync",
            Some(&sid),
            json!({"endpoint_id":eid}),
        );
        jobs.start(credential, &req).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while fixture.calls().is_empty() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            jobs.get(&admin, "source-revoked").unwrap_err().code,
            "Denied",
            "status belongs to the initiating identity"
        );
        engine
            .execute(
                &admin,
                &request(
                    "source-revoke",
                    "endpoints.grant",
                    Some(&sid),
                    json!({"endpoint_id":eid,"member_id":mid,"operations":[]}),
                ),
            )
            .unwrap();
        while jobs.active_count() != 0 {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fixture.calls().len(),
            1,
            "revoked source authority must be checked before the next gh command"
        );
        assert_eq!(
            jobs.get(credential, "source-revoked").unwrap_err().code,
            "Denied"
        );
        assert_eq!(jobs.start(credential, &req).unwrap_err().code, "Denied");
        let topics = engine
            .execute(
                &admin,
                &request("revoked-topics", "topics.list", Some(&sid), json!({})),
            )
            .unwrap();
        assert!(topics["items"].as_array().unwrap().is_empty());
    }
    #[test]
    fn background_writeback_keeps_unknown_until_read_only_reconciliation_without_second_post() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let jobs = SourceJobs::open_with_config_factory(
            &dir.path().join("jobs"),
            engine.clone(),
            Arc::new(move |_, _| Ok(config.clone())),
        )
        .unwrap();
        let denied = jobs
            .start(
                &admin,
                &request(
                    "write-disabled",
                    "sources.writeback",
                    Some(&sid),
                    json!({"endpoint_id":eid,"number":1,"body":"External comment"}),
                ),
            )
            .unwrap_err();
        assert_eq!(denied.code, "WriteBackDisabled");
        assert!(fixture.calls().is_empty());
        let writable=engine.execute(&admin,&request("writable-source","endpoints.register",Some(&sid),json!({"name":"Explicit writable source","kind":"github","config":{"repository":"acme/project","allow_write_back":true}}))).unwrap();
        let eid = writable["id"].as_str().unwrap();
        std::fs::write(fixture.dir.path().join("fail-after-write"), b"yes").unwrap();
        let req = request(
            "external-write",
            "sources.writeback",
            Some(&sid),
            json!({"endpoint_id":eid,"number":1,"body":"External comment"}),
        );
        let start = Instant::now();
        jobs.start(&admin, &req).unwrap();
        assert!(start.elapsed() < Duration::from_millis(500));
        let unknown = wait(&jobs, &admin, "external-write");
        assert_eq!(unknown.status, "unknown");
        assert_eq!(
            unknown.write_receipt.unwrap().state,
            crate::runtime_source::WriteState::Unknown
        );
        assert_eq!(jobs.start(&admin, &req).unwrap().status, "unknown");
        assert_eq!(fixture.calls().len(), 1);
        jobs.start(
            &admin,
            &request(
                "external-reconcile",
                "sources.reconcile",
                Some(&sid),
                json!({"endpoint_id":eid,"number":1,"write_request_id":"external-write"}),
            ),
        )
        .unwrap();
        let confirmed = wait(&jobs, &admin, "external-reconcile");
        assert_eq!(confirmed.status, "completed", "{confirmed:?}");
        assert_eq!(
            confirmed.write_receipt.unwrap().state,
            crate::runtime_source::WriteState::Confirmed
        );
        assert_eq!(
            jobs.get(&admin, "external-write").unwrap().status,
            "completed"
        );
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|args| args.iter().any(|a| a == "POST"))
                .count(),
            1
        );
    }
    #[test]
    fn interrupted_job_reopens_unknown_and_cancellation_keeps_shutdown_accounting_truthful() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let factory: Arc<SourceConfigFactory> = Arc::new(move |_, _| Ok(config.clone()));
        let path = dir.path().join("jobs");
        std::fs::write(fixture.dir.path().join("slow-read"), b"yes").unwrap();
        let jobs =
            SourceJobs::open_with_config_factory(&path, engine.clone(), factory.clone()).unwrap();
        let req = request(
            "interrupted-source",
            "sources.sync",
            Some(&sid),
            json!({"endpoint_id":eid}),
        );
        jobs.start(&admin, &req).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while fixture.calls().is_empty() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(jobs.active_count(), 1);
        // A second opener reads the durable boundary a replacement daemon would
        // see. The original worker is explicitly cancelled for test cleanup.
        let reopened = SourceJobs::open_with_config_factory(&path, engine, factory).unwrap();
        let recovered = reopened.get(&admin, "interrupted-source").unwrap();
        jobs.cancel_all();
        assert_eq!(
            jobs.start(
                &admin,
                &request(
                    "after-shutdown",
                    "sources.sync",
                    Some(&sid),
                    json!({"endpoint_id":eid})
                )
            )
            .unwrap_err()
            .code,
            "ShuttingDown"
        );
        while jobs.active_count() != 0 {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            recovered.status, "unknown",
            "replacement runtime must not claim previous process work is live"
        );
        assert_eq!(
            reopened.start(&admin, &req).unwrap().status,
            "unknown",
            "same request cannot blindly relaunch gh"
        );
        assert_eq!(
            fixture.calls().len(),
            1,
            "cancellation must prevent another source command"
        );
        assert_eq!(reopened.active_count(), 0);
    }
    #[test]
    fn explicit_resume_applies_durable_sync_checkpoint_offline_without_duplicate_posts() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let factory: Arc<SourceConfigFactory> = Arc::new(move |_, _| Ok(config.clone()));
        let path = dir.path().join("jobs");
        let jobs =
            SourceJobs::open_with_config_factory(&path, engine.clone(), factory.clone()).unwrap();
        jobs.start(
            &admin,
            &request(
                "checkpoint-sync",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        let initial = wait(&jobs, &admin, "checkpoint-sync");
        assert_eq!(initial.status, "completed");
        assert_eq!(fixture.calls().len(), 2);
        drop(jobs);
        // The adapter executable is now unavailable; recovery must use the
        // persisted source checkpoint and projection receipts, not re-fetch.
        std::fs::remove_file(fixture.dir.path().join("fake-gh.ps1")).unwrap();
        let reopened =
            SourceJobs::open_with_config_factory(&path, engine.clone(), factory).unwrap();
        reopened
            .start(
                &admin,
                &request(
                    "checkpoint-resume",
                    "sources.resume",
                    Some(&sid),
                    json!({"endpoint_id":eid,"sync_job_id":"checkpoint-sync"}),
                ),
            )
            .expect("explicit local apply recovery uses durable source history");
        let resumed = wait(&reopened, &admin, "checkpoint-resume");
        assert_eq!(resumed.status, "completed", "{resumed:?}");
        assert_eq!(resumed.topic_ids, initial.topic_ids);
        assert_eq!(
            fixture.calls().len(),
            2,
            "offline resume must never launch gh"
        );
        let topics = engine
            .execute(
                &admin,
                &request("resume-topic-count", "topics.list", Some(&sid), json!({})),
            )
            .unwrap();
        assert_eq!(topics["items"].as_array().unwrap().len(), 2);
        for topic in resumed.topic_ids {
            let view = engine
                .execute(
                    &admin,
                    &request(
                        "resume-topic-read",
                        "topics.read",
                        Some(&sid),
                        json!({"topic_id":topic}),
                    ),
                )
                .unwrap();
            assert!(
                view["comments"].as_array().unwrap().is_empty(),
                "re-applying an acknowledged snapshot must not append comments"
            );
        }
    }
    #[test]
    fn resuming_an_older_acknowledged_checkpoint_does_not_append_old_source_again() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let jobs = SourceJobs::open_with_config_factory(
            &dir.path().join("jobs"),
            engine.clone(),
            Arc::new(move |_, _| Ok(config.clone())),
        )
        .unwrap();
        jobs.start(
            &admin,
            &request(
                "old-checkpoint",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        let old = wait(&jobs, &admin, "old-checkpoint");
        assert_eq!(old.status, "completed");
        let path = fixture.dir.path().join("issues.json");
        let mut issues: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        issues[0][0]["body"] = json!("New upstream revision");
        issues[0][0]["updated_at"] = json!("2026-10-21T03:00:00Z");
        std::fs::write(path, serde_json::to_vec(&issues).unwrap()).unwrap();
        jobs.start(
            &admin,
            &request(
                "new-checkpoint",
                "sources.sync",
                Some(&sid),
                json!({"endpoint_id":eid}),
            ),
        )
        .unwrap();
        assert_eq!(wait(&jobs, &admin, "new-checkpoint").status, "completed");
        jobs.start(
            &admin,
            &request(
                "old-checkpoint-resume",
                "sources.resume",
                Some(&sid),
                json!({"endpoint_id":eid,"sync_job_id":"old-checkpoint"}),
            ),
        )
        .unwrap();
        assert_eq!(
            wait(&jobs, &admin, "old-checkpoint-resume").status,
            "completed"
        );
        let issue = old
            .topic_ids
            .iter()
            .find_map(|id| {
                let view = engine
                    .execute(
                        &admin,
                        &request(
                            "old-checkpoint-read",
                            "topics.read",
                            Some(&sid),
                            json!({"topic_id":id}),
                        ),
                    )
                    .unwrap();
                view["topic"]["body"]
                    .as_str()
                    .unwrap()
                    .contains("I_STABLE")
                    .then_some(view)
            })
            .unwrap();
        assert_eq!(
            issue["comments"].as_array().unwrap().len(),
            1,
            "a source revision acknowledged before a newer revision must remain acknowledged"
        );
        assert!(issue["comments"][0]["comment"]
            .as_str()
            .unwrap()
            .contains("New upstream revision"));
        assert_eq!(fixture.calls().len(), 4);
    }
}
