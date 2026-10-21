//! Explicit durable scheduling of read-only source jobs. Credentials stay in worker memory.
use crate::{
    runtime_core::{ApiError, Engine, ToolRequest},
    runtime_source_jobs::SourceJobs,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchSnapshot {
    pub watch_id: String,
    pub space_id: String,
    pub endpoint_id: String,
    pub status: String,
    pub interval_seconds: u64,
    pub round: u64,
    pub last_job_id: Option<String>,
    pub last_job_status: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    identity_id: String,
    snapshot: WatchSnapshot,
}
type Signal = Arc<(Mutex<bool>, Condvar)>;
#[derive(Clone)]
pub struct WatchManager {
    engine: Engine,
    jobs: SourceJobs,
    db: Arc<Mutex<Connection>>,
    workers: Arc<Mutex<HashMap<String, Signal>>>,
    cancelled: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    interval_floor: u64,
    unpublished: Arc<Mutex<HashMap<String, Record>>>,
}
impl WatchManager {
    /// Open once at daemon startup. Interrupted watches need explicit reauthentication/rearm.
    pub fn open(dir: &Path, engine: Engine, jobs: SourceJobs) -> anyhow::Result<Self> {
        Self::open_with_floor(dir, engine, jobs, 30)
    }
    fn open_with_floor(
        dir: &Path,
        engine: Engine,
        jobs: SourceJobs,
        interval_floor: u64,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let db = Connection::open(dir.join("source-watches.sqlite3"))?;
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;CREATE TABLE IF NOT EXISTS source_watches(id TEXT PRIMARY KEY,record TEXT NOT NULL);")?;
        let rows = {
            let mut q = db.prepare("SELECT record FROM source_watches")?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for row in rows {
            let mut r: Record = serde_json::from_str(&row)?;
            if ["active", "stopping"].contains(&r.snapshot.status.as_str()) {
                r.snapshot.status = "unknown".into();
                r.snapshot.error = Some("Runtime restarted; reauthenticate and explicitly start the same watch to authorize a new read-only round; inspect last_job_id for the previous outcome".into());
                db.execute(
                    "UPDATE source_watches SET record=?1 WHERE id=?2",
                    params![serde_json::to_string(&r)?, r.snapshot.watch_id],
                )?;
            }
        }
        Ok(Self {
            engine,
            jobs,
            db: Arc::new(Mutex::new(db)),
            workers: Arc::new(Mutex::new(HashMap::new())),
            cancelled: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicUsize::new(0)),
            interval_floor,
            unpublished: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    fn record(&self, id: &str) -> Result<Record, ApiError> {
        if let Some(record) = self
            .unpublished
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
        {
            return Ok(record);
        }
        let raw = self
            .db
            .lock()
            .map_err(storage)?
            .query_row("SELECT record FROM source_watches WHERE id=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(storage)?
            .ok_or_else(|| error("NotFound", "source watch not found"))?;
        serde_json::from_str(&raw).map_err(storage)
    }
    fn save(&self, r: &Record) -> Result<(), ApiError> {
        let mut unpublished = self.unpublished.lock().unwrap_or_else(|e| e.into_inner());
        self.save_durable(r)?;
        unpublished.remove(&r.snapshot.watch_id);
        Ok(())
    }
    fn save_durable(&self, r: &Record) -> Result<(), ApiError> {
        let db = self.db.lock().map_err(storage)?;
        let mut updated = r.clone();
        if updated.snapshot.status == "active" {
            let previous: Option<String> = db
                .query_row(
                    "SELECT record FROM source_watches WHERE id=?1",
                    [&r.snapshot.watch_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            if let Some(previous) = previous {
                let previous: Record = serde_json::from_str(&previous).map_err(storage)?;
                if previous.snapshot.status == "stopping" {
                    updated.snapshot.status = "stopping".into();
                }
            }
        }
        db.execute("INSERT INTO source_watches(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![updated.snapshot.watch_id,serde_json::to_string(&updated).map_err(storage)?]).map_err(storage)?;
        Ok(())
    }
    fn publish_terminal(&self, mut record: Record) {
        let mut unpublished = self.unpublished.lock().unwrap_or_else(|e| e.into_inner());
        if self.save_durable(&record).is_err() {
            let outcome = format!(
                "{}: {}",
                record.snapshot.status,
                record.snapshot.error.as_deref().unwrap_or("watch stopped")
            );
            record.snapshot.status = "unknown".into();
            record.snapshot.error = Some(format!("PublicationPending: watch worker exited but terminal status could not be persisted ({outcome}); stop after storage recovery or reopen and explicitly reauthenticate to recover; status/replay never rearms it."));
            unpublished.insert(record.snapshot.watch_id.clone(), record);
        } else {
            unpublished.remove(&record.snapshot.watch_id);
        }
    }
    fn authorize(
        &self,
        credential: &str,
        space: &str,
        endpoint: &str,
        operation: &str,
    ) -> Result<String, ApiError> {
        let auth = self.engine.execute(
            credential,
            &tool(
                "watch:authorize",
                "endpoint.authorize",
                space,
                json!({"endpoint_id":endpoint,"operation":operation}),
            ),
        )?;
        if auth["endpoint"]["kind"] != "github" {
            return Err(error("Unsupported", "watch requires a GitHub endpoint"));
        }
        auth["identity_id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| error("Denied", "caller identity required"))
    }
    fn check(&self, credential: &str, r: &Record, operation: &str) -> Result<(), ApiError> {
        let identity = self.authorize(
            credential,
            &r.snapshot.space_id,
            &r.snapshot.endpoint_id,
            operation,
        )?;
        if identity != r.identity_id {
            return Err(error("Denied", "watch belongs to another identity"));
        }
        Ok(())
    }
    pub fn start(
        &self,
        credential: &str,
        request: &ToolRequest,
    ) -> Result<WatchSnapshot, ApiError> {
        let mut workers = self.workers.lock().map_err(storage)?;
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(error("ShuttingDown", "watches no longer accept requests"));
        }
        if request.tool != "sources.watch.start"
            || request.request_id.trim().is_empty()
            || request.request_id.len() > 200
            || !request.args.as_object().is_some_and(|a| {
                a.len() == 2 && a.contains_key("endpoint_id") && a.contains_key("interval_seconds")
            })
        {
            return Err(error(
                "Invalid",
                "watch requires exact endpoint_id and interval_seconds fields",
            ));
        }
        let sid = request
            .space_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| error("Invalid", "taskspace required"))?;
        let eid = request.args["endpoint_id"]
            .as_str()
            .ok_or_else(|| error("Invalid", "endpoint_id required"))?;
        let interval = request.args["interval_seconds"]
            .as_u64()
            .filter(|v| *v >= self.interval_floor && *v <= 86400)
            .ok_or_else(|| {
                error(
                    "Invalid",
                    format!("interval_seconds must be {}..86400", self.interval_floor),
                )
            })?;
        let identity = self.authorize(credential, sid, eid, "manage")?;
        let previous = match self.record(&request.request_id) {
            Ok(r) => Some(r),
            Err(e) if e.code == "NotFound" => None,
            Err(e) => return Err(e),
        };
        let mut record = if let Some(r) = previous {
            if r.identity_id != identity {
                return Err(error("Denied", "watch belongs to another identity"));
            }
            if r.snapshot.space_id != sid
                || r.snapshot.endpoint_id != eid
                || r.snapshot.interval_seconds != interval
            {
                return Err(error(
                    "RequestConflict",
                    "watch id already names different configuration",
                ));
            }
            if workers.contains_key(&request.request_id)
                || r.snapshot.status != "unknown"
                || self
                    .unpublished
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&request.request_id)
            {
                return Ok(r.snapshot);
            }
            if let Some(id) = &r.snapshot.last_job_id {
                match self.jobs.get(credential, id) {
                    Ok(job) if ["running", "applying"].contains(&job.status.as_str()) => {
                        return Err(error(
                            "JobConflict",
                            "previous source worker is still active",
                        ))
                    }
                    Ok(_) => {}
                    Err(e) if e.code == "NotFound" => {}
                    Err(e) => return Err(e),
                }
            }
            r
        } else {
            Record {
                identity_id: identity,
                snapshot: WatchSnapshot {
                    watch_id: request.request_id.clone(),
                    space_id: sid.into(),
                    endpoint_id: eid.into(),
                    status: "active".into(),
                    interval_seconds: interval,
                    round: 0,
                    last_job_id: None,
                    last_job_status: None,
                    error: None,
                },
            }
        };
        // One controller per caller/endpoint/space. The source service also serializes endpoint work.
        for id in workers.keys() {
            let other = self.record(id)?;
            if other.identity_id == record.identity_id
                && other.snapshot.space_id == sid
                && other.snapshot.endpoint_id == eid
            {
                return Err(error(
                    "WatchConflict",
                    "this caller already watches the endpoint in this space",
                ));
            }
        }
        if workers.len() >= 64 {
            return Err(error("WatchLimit", "active watch limit reached"));
        }
        record.snapshot.status = "active".into();
        record.snapshot.error = None;
        self.save(&record)?;
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        workers.insert(request.request_id.clone(), signal.clone());
        self.active.fetch_add(1, Ordering::SeqCst);
        let manager = self.clone();
        let credential = credential.to_string();
        let worker_record = record.clone();
        let launched = std::thread::Builder::new()
            .name("source-watch".into())
            .spawn(move || {
                let mut r = worker_record;
                let outcome = manager.run(&credential, &mut r, &signal);
                match outcome {
                    Ok(()) => r.snapshot.status = "stopped".into(),
                    Err(e) => {
                        r.snapshot.status =
                            if ["UncertainWatch", "Internal"].contains(&e.code.as_str()) {
                                "unknown"
                            } else {
                                "failed"
                            }
                            .into();
                        r.snapshot.error = Some(e.to_string());
                    }
                }
                {
                    let mut workers = manager.workers.lock().unwrap_or_else(|e| e.into_inner());
                    manager.publish_terminal(r.clone());
                    workers.remove(&r.snapshot.watch_id);
                }
                manager.active.fetch_sub(1, Ordering::SeqCst);
            });
        if launched.is_err() {
            workers.remove(&request.request_id);
            self.active.fetch_sub(1, Ordering::SeqCst);
            record.snapshot.status = "failed".into();
            record.snapshot.error = Some("watch worker could not start".into());
            self.publish_terminal(record);
            return Err(error("WatchWorker", "watch worker could not start"));
        }
        Ok(record.snapshot)
    }
    fn run(&self, credential: &str, r: &mut Record, signal: &Signal) -> Result<(), ApiError> {
        loop {
            let (job_id, mut job) = {
                // Serialize admission with stop/shutdown; after they return no new sync starts.
                let _admission = self.workers.lock().map_err(storage)?;
                if self.stopped(signal)? {
                    return Ok(());
                }
                self.check(credential, r, "manage")?;
                r.snapshot.round = r
                    .snapshot
                    .round
                    .checked_add(1)
                    .ok_or_else(|| error("Invalid", "watch round exhausted"))?;
                let job_id = format!(
                    "watch:{:x}:{}",
                    Sha256::digest(r.snapshot.watch_id.as_bytes()),
                    r.snapshot.round
                );
                r.snapshot.last_job_id = Some(job_id.clone());
                r.snapshot.last_job_status = Some("admitting".into());
                self.save(r)?;
                let request = tool(
                    &job_id,
                    "sources.sync",
                    &r.snapshot.space_id,
                    json!({"endpoint_id":r.snapshot.endpoint_id}),
                );
                let job = self.jobs.start(credential, &request)?;
                (job_id, job)
            };
            loop {
                r.snapshot.last_job_status = Some(job.status.clone());
                self.save(r)?;
                if !["running", "applying"].contains(&job.status.as_str()) {
                    break;
                }
                // Stop prevents new dispatch but current bounded job remains visible until terminal.
                std::thread::sleep(Duration::from_millis(25));
                job = self.jobs.get(credential, &job_id)?;
            }
            if self.stopped(signal)? {
                return Ok(());
            }
            if job.status != "completed" {
                return Err(error(
                    if job.status == "unknown" {
                        "UncertainWatch"
                    } else {
                        "SourceJobFailed"
                    },
                    job.error
                        .unwrap_or_else(|| "source sync did not complete".into()),
                ));
            }
            let guard = signal.0.lock().map_err(storage)?;
            let _ = signal
                .1
                .wait_timeout_while(
                    guard,
                    Duration::from_secs(r.snapshot.interval_seconds),
                    |stop| !*stop && !self.cancelled.load(Ordering::SeqCst),
                )
                .map_err(storage)?;
        }
    }
    fn stopped(&self, signal: &Signal) -> Result<bool, ApiError> {
        Ok(self.cancelled.load(Ordering::SeqCst) || *signal.0.lock().map_err(storage)?)
    }
    /// Read-only status lookup; never polls, dispatches or rearms a job.
    pub fn get(&self, credential: &str, id: &str) -> Result<WatchSnapshot, ApiError> {
        let r = self.record(id)?;
        self.check(credential, &r, "observe")
            .or_else(|_| self.check(credential, &r, "manage"))?;
        Ok(r.snapshot)
    }
    pub fn list(&self, credential: &str, space_id: &str) -> Result<Vec<WatchSnapshot>, ApiError> {
        self.engine.execute(
            credential,
            &tool("watch:list", "identity.current", space_id, json!({})),
        )?;
        let ids = {
            let db = self.db.lock().map_err(storage)?;
            let mut q = db
                .prepare("SELECT id FROM source_watches ORDER BY id")
                .map_err(storage)?;
            let rows = q
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(storage)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage)?
        };
        let mut snapshots = Vec::new();
        for id in ids {
            match self.get(credential, &id) {
                Ok(s) if s.space_id == space_id => snapshots.push(s),
                Ok(_) => {}
                Err(e) if ["Denied", "Unauthenticated", "NotFound"].contains(&e.code.as_str()) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(snapshots)
    }
    pub fn stop(&self, credential: &str, id: &str) -> Result<WatchSnapshot, ApiError> {
        let workers = self.workers.lock().map_err(storage)?;
        let mut r = self.record(id)?;
        self.check(credential, &r, "manage")?;
        if let Some(signal) = workers.get(id) {
            *signal.0.lock().map_err(storage)? = true;
            signal.1.notify_all();
            r.snapshot.status = "stopping".into();
        } else {
            r.snapshot.status = "stopped".into();
        }
        self.save(&r)?;
        Ok(r.snapshot)
    }
    pub fn cancel_all(&self) {
        if let Ok(workers) = self.workers.lock() {
            self.cancelled.store(true, Ordering::SeqCst);
            for signal in workers.values() {
                if let Ok(mut stopped) = signal.0.lock() {
                    *stopped = true;
                    signal.1.notify_all();
                }
            }
        } else {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }
    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}
fn error(code: &str, message: impl Into<String>) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
    }
}
fn storage(_: impl std::fmt::Display) -> ApiError {
    error("Internal", "source watch ledger failure")
}
fn tool(id: &str, name: &str, space: &str, args: Value) -> ToolRequest {
    ToolRequest {
        request_id: id.into(),
        tool: name.into(),
        space_id: if space.is_empty() {
            None
        } else {
            Some(space.into())
        },
        args,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::{runtime_core::owner_token_path, runtime_source::tests::Fixture};
    use std::time::{Duration, Instant};
    #[test]
    fn terminal_watch_publication_failure_is_visible_and_explicit_stop_recovers() {
        use std::sync::mpsc;
        let (dir, engine, admin, sid, eid) = setup();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let jobs = SourceJobs::open_with_config_factory(
            dir.path(),
            engine.clone(),
            Arc::new(move |_, _| {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                Err("controlled source configuration failure".into())
            }),
        )
        .unwrap();
        let manager = WatchManager::open(dir.path(), engine.clone(), jobs.clone()).unwrap();
        let req = tool(
            "watch-publication-fault",
            "sources.watch.start",
            &sid,
            json!({"endpoint_id":eid,"interval_seconds":30}),
        );
        manager.start(&admin, &req).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let blocker = Connection::open(dir.path().join("source-watches.sqlite3")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        release_tx.send(()).unwrap();
        until(|| manager.active_count() == 0 && jobs.active_count() == 0);
        let locked = manager.get(&admin, &req.request_id).unwrap();
        let replay_locked = manager.start(&admin, &req);
        blocker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            locked.status, "unknown",
            "absent watch worker must not remain active/stopping"
        );
        assert!(locked
            .error
            .as_deref()
            .unwrap()
            .contains("PublicationPending"));
        assert_eq!(
            replay_locked.unwrap().status,
            "unknown",
            "pending watch replay must not rearm work"
        );
        assert_eq!(manager.list(&admin, &sid).unwrap()[0].status, "unknown");
        assert!(manager.get("invalid", &req.request_id).is_err());
        assert_eq!(
            manager.stop(&admin, &req.request_id).unwrap().status,
            "stopped"
        );
        assert_eq!(
            manager.get(&admin, &req.request_id).unwrap().status,
            "stopped"
        );
        manager.cancel_all();
        jobs.cancel_all();
        drop(manager);
        let reopened = WatchManager::open(dir.path(), engine, jobs).unwrap();
        assert_eq!(
            reopened.get(&admin, &req.request_id).unwrap().status,
            "stopped"
        );
        assert!(
            entered_rx.try_recv().is_err(),
            "status/stop/reopen never dispatch another source operation"
        );
    }

    fn setup() -> (tempfile::TempDir, Engine, String, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let s = engine
            .execute(
                &owner,
                &tool("space", "spaces.create", "", json!({"name":"watch"})),
            )
            .unwrap();
        let sid = s["space"]["id"].as_str().unwrap().to_string();
        let admin = s["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_string();
        let endpoint = engine
            .execute(
                &owner,
                &tool(
                    "endpoint",
                    "endpoints.register",
                    &sid,
                    json!({"name":"GitHub","kind":"github","config":{"repository":"acme/project"}}),
                ),
            )
            .unwrap();
        (
            dir,
            engine,
            admin,
            sid,
            endpoint["id"].as_str().unwrap().into(),
        )
    }
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            assert!(Instant::now() < deadline, "watch condition timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn timer_imports_new_items_and_stop_prevents_further_dispatch() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let jobs = SourceJobs::open_with_config_factory(
            dir.path(),
            engine.clone(),
            Arc::new(move |_, _| Ok(config.clone())),
        )
        .unwrap();
        let watches =
            WatchManager::open_with_floor(dir.path(), engine.clone(), jobs.clone(), 1).unwrap();
        let request = tool(
            "watch-one",
            "sources.watch.start",
            &sid,
            json!({"endpoint_id":eid,"interval_seconds":1}),
        );
        watches.start(&admin, &request).unwrap();
        until(|| {
            watches
                .get(&admin, "watch-one")
                .unwrap()
                .last_job_status
                .as_deref()
                == Some("completed")
        });
        let issue_path = fixture.dir.path().join("issues.json");
        let mut issues: Value =
            serde_json::from_slice(&std::fs::read(&issue_path).unwrap()).unwrap();
        let mut item = issues[0][0].clone();
        item["node_id"] = json!("I_NEW");
        item["number"] = json!(3);
        item["html_url"] = json!("https://github.com/acme/project/issues/3");
        issues[0].as_array_mut().unwrap().push(item);
        std::fs::write(issue_path, serde_json::to_vec(&issues).unwrap()).unwrap();
        until(|| {
            engine
                .execute(&admin, &tool("list", "topics.list", &sid, json!({})))
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .len()
                == 3
        });
        watches.stop(&admin, "watch-one").unwrap();
        until(|| watches.active_count() == 0);
        let calls = fixture.calls().len();
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(fixture.calls().len(), calls);
        assert_eq!(watches.get(&admin, "watch-one").unwrap().status, "stopped");
        assert_eq!(jobs.active_count(), 0);
    }
    #[test]
    fn restart_is_read_only_until_explicit_rearm_and_never_replays_previous_round() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let factory: Arc<crate::runtime_source_jobs::SourceConfigFactory> =
            Arc::new(move |_, _| Ok(config.clone()));
        let jobs =
            SourceJobs::open_with_config_factory(dir.path(), engine.clone(), factory.clone())
                .unwrap();
        let manager =
            WatchManager::open_with_floor(dir.path(), engine.clone(), jobs.clone(), 1).unwrap();
        let request = tool(
            "restart-watch",
            "sources.watch.start",
            &sid,
            json!({"endpoint_id":eid,"interval_seconds":30}),
        );
        manager.start(&admin, &request).unwrap();
        until(|| {
            manager
                .get(&admin, "restart-watch")
                .unwrap()
                .last_job_status
                .as_deref()
                == Some("completed")
        });
        let previous = manager.get(&admin, "restart-watch").unwrap();
        manager.cancel_all();
        until(|| manager.active_count() == 0);
        // Reconstruct the durable checkpoint left by process death before watch finalization.
        let mut interrupted = manager.record("restart-watch").unwrap();
        interrupted.snapshot.status = "active".into();
        manager.save(&interrupted).unwrap();
        let job_id = previous.last_job_id.clone().unwrap();
        let db = Connection::open(dir.path().join("source-jobs.sqlite3")).unwrap();
        let raw: String = db
            .query_row(
                "SELECT record FROM source_jobs WHERE id=?1",
                [&job_id],
                |r| r.get(0),
            )
            .unwrap();
        let mut raw: Value = serde_json::from_str(&raw).unwrap();
        raw["snapshot"]["status"] = json!("running");
        db.execute(
            "UPDATE source_jobs SET record=?1 WHERE id=?2",
            params![raw.to_string(), job_id],
        )
        .unwrap();
        drop(manager);
        drop(jobs);
        let calls = fixture.calls().len();
        let jobs =
            SourceJobs::open_with_config_factory(dir.path(), engine.clone(), factory).unwrap();
        let manager = WatchManager::open(dir.path(), engine, jobs.clone()).unwrap();
        assert_eq!(
            manager.get(&admin, "restart-watch").unwrap().status,
            "unknown"
        );
        assert_eq!(manager.active_count(), 0);
        assert_eq!(manager.list(&admin, &sid).unwrap().len(), 1);
        assert_eq!(fixture.calls().len(), calls);
        assert_eq!(jobs.get(&admin, &job_id).unwrap().status, "unknown");
        manager.start(&admin, &request).unwrap();
        manager.start(&admin, &request).unwrap();
        until(|| {
            let s = manager.get(&admin, "restart-watch").unwrap();
            s.round > previous.round && s.last_job_status.as_deref() == Some("completed")
        });
        let current = manager.get(&admin, "restart-watch").unwrap();
        assert_eq!(current.round, previous.round + 1);
        assert_ne!(current.last_job_id, previous.last_job_id);
        assert_eq!(jobs.get(&admin, &job_id).unwrap().status, "unknown");
        assert_eq!(
            fixture.calls().len(),
            calls + 2,
            "one new read-only round, never relaunch old job"
        );
        manager.cancel_all();
        until(|| manager.active_count() == 0);
        let encoded = std::fs::read(dir.path().join("source-watches.sqlite3")).unwrap();
        assert!(
            !encoded.windows(admin.len()).any(|s| s == admin.as_bytes()),
            "watch ledger must not persist credential"
        );
        assert_eq!(
            manager.start(&admin, &request).unwrap_err().code,
            "ShuttingDown"
        );
    }
    #[test]
    fn current_grant_revocation_stops_next_round_and_hides_status_from_other_callers() {
        let fixture = Fixture::new();
        let (dir, engine, admin, sid, eid) = setup();
        let invitation = engine
            .execute(
                &admin,
                &tool(
                    "invite",
                    "members.invite",
                    &sid,
                    json!({"roles":["implementer"]}),
                ),
            )
            .unwrap();
        let member = engine
            .execute(
                invitation["invite_token"].as_str().unwrap(),
                &tool(
                    "join",
                    "members.register",
                    &sid,
                    json!({"name":"source worker"}),
                ),
            )
            .unwrap();
        let token = member["credential"].as_str().unwrap();
        engine.execute(&admin,&tool("grant","endpoints.grant",&sid,json!({"endpoint_id":eid,"member_id":member["id"],"operations":["manage","observe"]}))).unwrap();
        let config = fixture.config.clone();
        let jobs = SourceJobs::open_with_config_factory(
            dir.path(),
            engine.clone(),
            Arc::new(move |_, _| Ok(config.clone())),
        )
        .unwrap();
        let manager = WatchManager::open_with_floor(dir.path(), engine.clone(), jobs, 1).unwrap();
        let request = tool(
            "grant-watch",
            "sources.watch.start",
            &sid,
            json!({"endpoint_id":eid,"interval_seconds":1}),
        );
        manager.start(token, &request).unwrap();
        until(|| {
            manager
                .get(token, "grant-watch")
                .unwrap()
                .last_job_status
                .as_deref()
                == Some("completed")
        });
        assert_eq!(
            manager.get(&admin, "grant-watch").unwrap_err().code,
            "Denied"
        );
        assert_eq!(manager.list(&admin, &sid).unwrap().len(), 0);
        let conflict = tool(
            "duplicate-watch",
            "sources.watch.start",
            &sid,
            request.args.clone(),
        );
        assert_eq!(
            manager.start(token, &conflict).unwrap_err().code,
            "WatchConflict"
        );
        let calls = fixture.calls().len();
        engine
            .execute(
                &admin,
                &tool(
                    "revoke",
                    "endpoints.grant",
                    &sid,
                    json!({"endpoint_id":eid,"member_id":member["id"],"operations":[]}),
                ),
            )
            .unwrap();
        until(|| manager.active_count() == 0);
        assert_eq!(fixture.calls().len(), calls);
        assert_eq!(
            manager.get(token, "grant-watch").unwrap_err().code,
            "Denied"
        );
        assert_eq!(
            manager.record("grant-watch").unwrap().snapshot.status,
            "failed"
        );
    }
    #[test]
    fn stop_preserves_inflight_accounting_until_bounded_source_job_finishes() {
        let mut fixture = Fixture::new();
        fixture.config.timeout = Duration::from_millis(2500);
        std::fs::write(fixture.dir.path().join("slow-read"), b"yes").unwrap();
        let (dir, engine, admin, sid, eid) = setup();
        let config = fixture.config.clone();
        let jobs = SourceJobs::open_with_config_factory(
            dir.path(),
            engine.clone(),
            Arc::new(move |_, _| Ok(config.clone())),
        )
        .unwrap();
        let manager = WatchManager::open_with_floor(dir.path(), engine, jobs.clone(), 1).unwrap();
        manager
            .start(
                &admin,
                &tool(
                    "inflight",
                    "sources.watch.start",
                    &sid,
                    json!({"endpoint_id":eid,"interval_seconds":1}),
                ),
            )
            .unwrap();
        until(|| fixture.calls().len() == 1);
        assert_eq!(manager.stop(&admin, "inflight").unwrap().status, "stopping");
        assert_eq!(manager.active_count(), 1);
        until(|| manager.active_count() == 0);
        assert_eq!(jobs.active_count(), 0);
        assert_eq!(manager.get(&admin, "inflight").unwrap().status, "stopped");
        assert_eq!(manager.get(&admin, "inflight").unwrap().round, 1);
        assert!(
            fixture.calls().len() <= 2,
            "only the accepted Issue/PR read round may drain"
        );
    }
}
