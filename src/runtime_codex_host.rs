//! Owned Codex 0.163 stdio transport. No agent workflow or automatic retries.
use crate::runtime_core::{ApiError, Engine};
use crate::runtime_participants::{BindingContext, OwnedAdmission, TrustedHost};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum HostError {
    Idle,
    Unsupported(String),
    Unknown { operation_id: String },
    Protocol(String),
    Io(String),
    Denied(String),
    Pending { active_callbacks: usize },
}
impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for HostError {}
fn io(e: impl std::fmt::Display) -> HostError {
    HostError::Io(e.to_string())
}
fn denied(s: &str) -> HostError {
    HostError::Denied(s.into())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HostScope {
    pub identity_id: String,
    pub space_id: String,
    pub endpoint_id: String,
    pub attachment_id: String,
    pub session_id: String,
    pub binding_generation: u64,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTurnOutcome {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}
impl ProviderTurnOutcome {
    pub fn from_status(status: &str) -> Option<Self> {
        match status {
            "inProgress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "interrupted" => Some(Self::Interrupted),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct ReconciledOperation {
    pub operation_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub outcome: ProviderTurnOutcome,
}
#[derive(Clone, Debug, Serialize)]
pub struct ApprovedWorkspace {
    root: PathBuf,
    space_id: String,
    endpoint_id: String,
    approved_by: String,
    codex_version: String,
    capability_evidence: String,
}
#[derive(Clone, Debug, Serialize)]
pub enum ExecutionProfile {
    ReadOnlyForum,
    WorkspaceWrite(ApprovedWorkspace),
}
pub struct OwnedHostConfig {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub workspace: PathBuf,
    pub model: String,
    pub scope: HostScope,
    pub timeout: Duration,
    pub profile: ExecutionProfile,
}
/// Installed only by the Runtime. Implementations must retain credentials privately
/// and invoke the actual authorized facade on every call, including replay.
pub trait RuntimeToolFacade: Send + Sync {
    fn call(&self, scope: &HostScope, request: &Value) -> Result<Value, String>;
}
/// Real authorized forum facade. Filesystem profile does not confer forum roles.
/// The bearer stays exclusively in this object.
pub struct EngineForumFacade {
    engine: Engine,
    credential: String,
}
impl EngineForumFacade {
    pub fn new(engine: Engine, credential: String) -> Self {
        Self { engine, credential }
    }
}
impl RuntimeToolFacade for EngineForumFacade {
    fn call(&self, scope: &HostScope, request: &Value) -> Result<Value, String> {
        if self.credential.is_empty()
            || serde_json::to_string(request)
                .map_err(|_| "invalid request")?
                .contains(&self.credential)
        {
            return Err("credential in request rejected".into());
        }
        let r: crate::runtime_core::ToolRequest =
            serde_json::from_value(request.clone()).map_err(|_| "invalid tool envelope")?;
        if r.space_id.as_deref() != Some(&scope.space_id)
            || ![
                "identity.current",
                "topics.read",
                "topics.list",
                "activity.read",
                "members.list",
                "endpoints.list",
                "topics.create",
                "task.act",
            ]
            .contains(&r.tool.as_str())
        {
            return Err("tool outside supported forum scope".into());
        }
        let operation = if ["topics.create", "task.act"].contains(&r.tool.as_str()) {
            "send"
        } else {
            "observe"
        };
        self.engine
            .with_endpoint_authorization(
                &self.credential,
                &scope.space_id,
                &scope.endpoint_id,
                operation,
                |auth| {
                    if auth["identity_id"] != scope.identity_id
                        || !auth["identity"]["attachments"]
                            .as_array()
                            .is_some_and(|aa| {
                                aa.iter().any(|a| {
                                    a["attachment_id"] == scope.attachment_id
                                        && a["state"] == "alive"
                                })
                            })
                    {
                        return Err(ApiError {
                            code: "Denied".into(),
                            message: "current exact attachment required".into(),
                        });
                    }
                    Ok(())
                },
            )
            .map_err(|_| "current authorization denied")?;
        self.engine
            .execute_requiring_endpoint(&self.credential, &r, &scope.endpoint_id, operation)
            .map_err(|_| "Runtime facade denied".into())
    }
}
struct Fact {
    active_callbacks: usize,
    fenced_from: Option<u64>,
    workspace_write: bool,
    scope: HostScope,
    instance: String,
    generation: u64,
    thread: Option<String>,
    alive: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
    reconciled: bool,
}
#[derive(Clone)]
pub struct OwnedCodexRegistry {
    dir: PathBuf,
    engine: Engine,
    facts: Arc<Mutex<HashMap<String, Fact>>>,
    drained: Arc<Condvar>,
}
/// Does not retain a facts lock while the facade acquires Engine locks. Drop also
/// releases admission during panic unwinding, so a fence can finish draining.
struct CallbackLease {
    facts: Arc<Mutex<HashMap<String, Fact>>>,
    drained: Arc<Condvar>,
    session: String,
    instance: String,
}
impl Drop for CallbackLease {
    fn drop(&mut self) {
        let mut facts = self.facts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = facts
            .get_mut(&self.session)
            .filter(|f| f.instance == self.instance)
        {
            f.active_callbacks = f.active_callbacks.saturating_sub(1);
        }
        self.drained.notify_all();
    }
}
impl OwnedCodexRegistry {
    /// Refresh only an actual held, unfenced host's existing attachment. The
    /// liveness lease participates in lifecycle drain but never holds facts
    /// across Engine I/O. Initialization need not have created a thread yet.
    pub fn renew_owned_attachment(
        &self,
        scope: &HostScope,
        credential: &str,
    ) -> Result<(), HostError> {
        let _lease = {
            let mut facts = self.facts.lock().map_err(io)?;
            let f = facts
                .get_mut(&scope.session_id)
                .ok_or_else(|| denied("unknown owned session"))?;
            if f.scope != *scope
                || f.fenced_from.is_some()
                || !f.alive.load(Ordering::Acquire)
                || f.child
                    .try_lock()
                    .map_err(|_| HostError::Idle)?
                    .try_wait()
                    .map_err(io)?
                    .is_some()
            {
                return Err(denied("current exact live owned host required"));
            }
            f.active_callbacks = f
                .active_callbacks
                .checked_add(1)
                .ok_or_else(|| denied("lease limit"))?;
            CallbackLease {
                facts: self.facts.clone(),
                drained: self.drained.clone(),
                session: scope.session_id.clone(),
                instance: f.instance.clone(),
            }
        };
        self.engine
            .with_endpoint_authorization(
                credential,
                &scope.space_id,
                &scope.endpoint_id,
                "send",
                |auth| {
                    if scope.identity_id == "owner"
                        || auth["identity_id"] != scope.identity_id
                        || !auth["identity"]["attachments"]
                            .as_array()
                            .is_some_and(|aa| {
                                aa.iter().any(|a| a["attachment_id"] == scope.attachment_id)
                            })
                    {
                        return Err(ApiError {
                            code: "Denied".into(),
                            message: "exact member attachment required".into(),
                        });
                    }
                    Ok(())
                },
            )
            .map_err(|_| denied("current renewal authorization denied"))?;
        self.engine
            .execute_requiring_endpoint(
                credential,
                &crate::runtime_core::ToolRequest {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    tool: "members.heartbeat".into(),
                    space_id: Some(scope.space_id.clone()),
                    args: json!({"attachment_id":scope.attachment_id}),
                },
                &scope.endpoint_id,
                "send",
            )
            .map_err(|_| denied("attachment renewal denied"))?;
        Ok(())
    }
    /// Trusted management entry point; never deserialize an approval from HTTP.
    pub fn approve_workspace(
        &self,
        credential: &str,
        space_id: &str,
        endpoint_id: &str,
        workspace: &Path,
        codex_version: &str,
        capability_evidence: &str,
    ) -> Result<ApprovedWorkspace, HostError> {
        #[cfg(not(windows))]
        {
            let _ = (
                credential,
                space_id,
                endpoint_id,
                workspace,
                codex_version,
                capability_evidence,
            );
            return Err(HostError::Unsupported(
                "workspace-write capability verified only on Windows Codex 0.163.0".into(),
            ));
        }
        #[cfg(windows)]
        {
            if codex_version != "0.163.0" || capability_evidence.trim().is_empty() {
                return Err(HostError::Unsupported(
                    "exact verified provider version and capability evidence required".into(),
                ));
            }
            let root = workspace.canonicalize().map_err(io)?;
            if !root.is_dir() {
                return Err(denied("canonical workspace directory required"));
            }
            let approved_by = self
                .engine
                .with_endpoint_authorization(credential, space_id, endpoint_id, "manage", |auth| {
                    if auth["identity_id"] != "owner"
                        && !auth["identity"]["roles"].as_array().is_some_and(|roles| {
                            roles.iter().any(|r| r == "human_admin" || r == "manager")
                        })
                    {
                        return Err(ApiError {
                            code: "Denied".into(),
                            message: "workspace approval requires administrator".into(),
                        });
                    }
                    Ok(auth["identity_id"].as_str().unwrap_or("").to_string())
                })
                .map_err(|e| HostError::Denied(e.to_string()))?;
            Ok(ApprovedWorkspace {
                root,
                space_id: space_id.into(),
                endpoint_id: endpoint_id.into(),
                approved_by,
                codex_version: codex_version.into(),
                capability_evidence: capability_evidence.into(),
            })
        }
    }
    /// Runtime pause/rebind/shutdown hook. Fence before publishing the new binding.
    /// Old callbacks fail immediately; provider interruption/cleanup is separate.
    pub fn fence_binding(
        &self,
        session_id: &str,
        expected_generation: u64,
    ) -> Result<(), HostError> {
        self.fence_binding_timeout(session_id, expected_generation, Duration::from_secs(5))
    }
    /// Must be called outside Engine/ParticipantService locks. Stops admission
    /// first, then releases facts while waiting. Pending never means paused.
    /// Retry Pending with the SAME original expected generation.
    pub fn fence_binding_timeout(
        &self,
        session_id: &str,
        expected_generation: u64,
        timeout: Duration,
    ) -> Result<(), HostError> {
        let deadline = Instant::now() + timeout.min(Duration::from_secs(30));
        let mut facts = self.facts.lock().map_err(io)?;
        let f = facts
            .get_mut(session_id)
            .ok_or_else(|| denied("unknown owned session"))?;
        if match f.fenced_from {
            Some(from) => from != expected_generation,
            None => f.scope.binding_generation != expected_generation,
        } {
            return Err(denied("stale binding fence"));
        }
        if f.fenced_from.is_none() {
            f.scope.binding_generation = expected_generation
                .checked_add(1)
                .ok_or_else(|| denied("generation exhausted"))?;
            f.fenced_from = Some(expected_generation);
        }
        f.reconciled = false;
        let instance = f.instance.clone();
        loop {
            let f = facts
                .get(session_id)
                .filter(|f| f.instance == instance)
                .ok_or_else(|| denied("host replaced during drain"))?;
            if f.active_callbacks == 0 {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HostError::Pending {
                    active_callbacks: f.active_callbacks,
                });
            }
            facts = self.drained.wait_timeout(facts, remaining).map_err(io)?.0;
        }
    }
    pub fn open(dir: &Path, engine: Engine) -> anyhow::Result<Self> {
        anyhow::ensure!(
            dir.canonicalize()? == engine.private_storage_directory()?,
            "host storage must match Engine"
        );
        crate::runtime_core::protect(dir)?;
        let me = Self {
            dir: dir.canonicalize()?,
            engine,
            facts: Arc::new(Mutex::new(HashMap::new())),
            drained: Arc::new(Condvar::new()),
        };
        for suffix in ["", "-wal", "-shm"] {
            let p = me.dir.join(format!("v2-codex-host.sqlite{suffix}"));
            if p.exists() {
                crate::runtime_core::protect(&p)?;
            }
        }
        let db = me.db()?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS owned_threads(thread TEXT PRIMARY KEY,scope TEXT NOT NULL,model TEXT NOT NULL,workspace TEXT NOT NULL,profile TEXT NOT NULL); CREATE TABLE IF NOT EXISTS host_generations(session TEXT PRIMARY KEY,generation INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS host_operations(id TEXT PRIMARY KEY,thread TEXT NOT NULL,state TEXT NOT NULL,turn TEXT);")?;
        for suffix in ["", "-wal", "-shm"] {
            let p = me.dir.join(format!("v2-codex-host.sqlite{suffix}"));
            if p.exists() {
                crate::runtime_core::protect(&p)?;
            }
        }
        Ok(me)
    }
    fn db(&self) -> Result<Connection, HostError> {
        let db = Connection::open(self.dir.join("v2-codex-host.sqlite")).map_err(io)?;
        db.busy_timeout(Duration::from_secs(5)).map_err(io)?;
        Ok(db)
    }
    /// Config is trusted Runtime configuration. Scope is independently authenticated;
    /// arbitrary HTTP config, argv, executable and workspace must never reach here.
    pub fn spawn(
        &self,
        mut config: OwnedHostConfig,
        credential: &str,
        facade: Arc<dyn RuntimeToolFacade>,
    ) -> Result<OwnedCodexHost, HostError> {
        if config.model.trim().is_empty() || config.scope.binding_generation == 0 {
            return Err(denied("explicit model and generation required"));
        }
        if credential.is_empty()
            || config.args.iter().any(|a| a.contains(credential))
            || config.scope.session_id.contains(credential)
        {
            return Err(denied("credential must not enter process configuration"));
        }
        config.workspace = config.workspace.canonicalize().map_err(io)?;
        #[cfg(not(windows))]
        if matches!(config.profile, ExecutionProfile::WorkspaceWrite(_)) {
            return Err(HostError::Unsupported(
                "workspace-write requires verified Windows containment".into(),
            ));
        }
        #[cfg(windows)]
        if let ExecutionProfile::WorkspaceWrite(approval) = &config.profile {
            if approval.root != config.workspace
                || approval.space_id != config.scope.space_id
                || approval.endpoint_id != config.scope.endpoint_id
            {
                return Err(denied(
                    "workspace profile differs from approved scope/canonical root",
                ));
            }
        }
        let scope = &config.scope;
        self.engine
            .with_endpoint_authorization(
                credential,
                &scope.space_id,
                &scope.endpoint_id,
                "send",
                |auth| {
                    let valid = auth["identity_id"] == scope.identity_id
                        && scope.identity_id != "owner"
                        && auth["identity"]["attachments"]
                            .as_array()
                            .is_some_and(|aa| {
                                aa.iter().any(|a| {
                                    a["attachment_id"] == scope.attachment_id
                                        && a["state"] == "alive"
                                })
                            });
                    if valid {
                        Ok(())
                    } else {
                        Err(ApiError {
                            code: "Denied".into(),
                            message: "exact live member attachment required".into(),
                        })
                    }
                },
            )
            .map_err(|e| HostError::Denied(e.to_string()))?;
        let session_hash = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(scope.session_id.as_bytes()))
        };
        let lock_path = self.dir.join(format!("codex-{session_hash}.lock"));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(io)?;
        lock.try_lock()
            .map_err(|_| denied("owned session already has a live host"))?;
        crate::runtime_core::protect(&lock_path).map_err(io)?;
        let db = self.db()?;
        let generation:u64=db.query_row("INSERT INTO host_generations(session,generation) VALUES(?1,1) ON CONFLICT(session) DO UPDATE SET generation=generation+1 RETURNING generation",[&scope.session_id],|r|r.get(0)).map_err(io)?;
        let mut command = Command::new(&config.executable);
        command
            .args(&config.args)
            .current_dir(&config.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // The initial user thread must not run (and spawn descendants) before
            // assignment to the held no-breakaway KILL_ON_JOB_CLOSE Job.
            command.creation_flags(0x00000004); // CREATE_SUSPENDED
        }
        // No credential is supplied to environment, command line, provider history or ledger.
        let mut child = command.spawn().map_err(io)?;
        #[cfg(windows)]
        let job = match crate::runtime_terminal::owned_job::Job::assign(child.id()) {
            Ok(j) => j,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HostError::Unsupported(e));
            }
        };
        #[cfg(windows)]
        if let Err(e) = windows_start::resume_initial_thread(child.id()) {
            let _ = job.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(HostError::Unsupported(e));
        }
        let stdin = child.stdin.take().ok_or_else(|| io("missing stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| io("missing stdout"))?;
        let (tx, rx) = mpsc::sync_channel(256);
        let alive = Arc::new(AtomicBool::new(true));
        let reader_alive = alive.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let v = line
                    .map_err(|e| e.to_string())
                    .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()));
                if tx.send(v).is_err() {
                    break;
                }
            }
            reader_alive.store(false, Ordering::Release);
        });
        let child = Arc::new(Mutex::new(child));
        let instance = uuid::Uuid::new_v4().to_string();
        self.facts.lock().map_err(io)?.insert(
            scope.session_id.clone(),
            Fact {
                active_callbacks: 0,
                fenced_from: None,
                workspace_write: matches!(config.profile, ExecutionProfile::WorkspaceWrite(_)),
                scope: scope.clone(),
                instance: instance.clone(),
                generation,
                thread: None,
                alive: alive.clone(),
                child: child.clone(),
                reconciled: false,
            },
        );
        Ok(OwnedCodexHost {
            registry: self.clone(),
            config,
            facade,
            child,
            stdin: Some(stdin),
            rx,
            alive,
            instance,
            generation,
            _lock: lock,
            thread_lock: None,
            next_id: 0,
            thread: None,
            turn: None,
            inflight: None,
            initialized: false,
            notifications: Vec::new(),
            #[cfg(windows)]
            job,
        })
    }
}
impl TrustedHost for OwnedCodexRegistry {
    fn admit(&self, c: &BindingContext) -> Result<OwnedAdmission, ApiError> {
        let reject = || ApiError {
            code: "Denied".into(),
            message: "current exact owned host required".into(),
        };
        let facts = self.facts.try_lock().map_err(|_| reject())?;
        let f = facts.get(&c.session_id).ok_or_else(reject)?;
        if f.child
            .try_lock()
            .map_err(|_| reject())?
            .try_wait()
            .map_err(|_| reject())?
            .is_some()
            || !f.alive.load(Ordering::Acquire)
            || !f.reconciled
            || f.scope.identity_id != c.identity_id
            || f.scope.space_id != c.space_id
            || f.scope.endpoint_id != c.endpoint_id
            || f.scope.attachment_id != c.attachment_id
            || f.scope.binding_generation != c.binding_generation
            || c.adapter_kind != "codex-app-server-0.163"
            || f.thread != c.provider_thread_id
            || f.thread.is_none()
        {
            return Err(reject());
        }
        Ok(OwnedAdmission {
            session_id: c.session_id.clone(),
            instance_id: f.instance.clone(),
            host_generation: f.generation,
            adapter_kind: c.adapter_kind.clone(),
            provider_thread_id: f.thread.clone(),
            capabilities: vec![
                "structured_tools".into(),
                "exact_owned_thread_resume".into(),
                if f.workspace_write {
                    "approved_workspace_write".into()
                } else {
                    "read_only_forum".into()
                },
            ],
            reconciled: true,
        })
    }
}
pub struct OwnedCodexHost {
    registry: OwnedCodexRegistry,
    config: OwnedHostConfig,
    facade: Arc<dyn RuntimeToolFacade>,
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    rx: Receiver<Result<Value, String>>,
    alive: Arc<AtomicBool>,
    instance: String,
    generation: u64,
    _lock: File,
    thread_lock: Option<File>,
    next_id: u64,
    thread: Option<String>,
    turn: Option<String>,
    inflight: Option<String>,
    initialized: bool,
    notifications: Vec<Value>,
    #[cfg(windows)]
    job: crate::runtime_terminal::owned_job::Job,
}
impl OwnedCodexHost {
    fn callback_lease(&self) -> Result<CallbackLease, HostError> {
        let mut facts = self.registry.facts.lock().map_err(io)?;
        let f = facts
            .get_mut(&self.config.scope.session_id)
            .ok_or_else(|| denied("host removed"))?;
        if f.instance != self.instance
            || f.generation != self.generation
            || f.scope != self.config.scope
            || !f.reconciled
            || f.fenced_from.is_some()
            || !f.alive.load(Ordering::Acquire)
        {
            return Err(denied("callback generation is fenced"));
        }
        f.active_callbacks = f
            .active_callbacks
            .checked_add(1)
            .ok_or_else(|| denied("callback lease limit"))?;
        Ok(CallbackLease {
            facts: self.registry.facts.clone(),
            drained: self.registry.drained.clone(),
            session: self.config.scope.session_id.clone(),
            instance: self.instance.clone(),
        })
    }
    fn acquire_thread_lock(&mut self, thread: &str) -> Result<(), HostError> {
        use sha2::{Digest, Sha256};
        let name = format!("codex-thread-{:x}.lock", Sha256::digest(thread.as_bytes()));
        let path = self.registry.dir.join(name);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(io)?;
        lock.try_lock()
            .map_err(|_| denied("exact owned thread already has a live host"))?;
        crate::runtime_core::protect(&path).map_err(io)?;
        self.thread_lock = Some(lock);
        Ok(())
    }
    /// Checks the held child handle and, on Windows, the held Job object.
    pub fn is_exited(&mut self) -> Result<bool, HostError> {
        let exited = self
            .child
            .lock()
            .map_err(io)?
            .try_wait()
            .map_err(io)?
            .is_some();
        #[cfg(windows)]
        return Ok(exited && !self.job.active().map_err(io)?);
        #[cfg(not(windows))]
        Ok(exited)
    }
    pub fn pid(&self) -> u32 {
        self.child.lock().expect("owned child lock").id()
    }
    pub fn thread_id(&self) -> Option<&str> {
        self.thread.as_deref()
    }
    fn fence(&self) -> Result<(), HostError> {
        let f = self.registry.facts.lock().map_err(io)?;
        if f.get(&self.config.scope.session_id).is_some_and(|f| {
            f.instance == self.instance
                && f.generation == self.generation
                && f.scope == self.config.scope
                && f.alive.load(Ordering::Acquire)
        }) {
            Ok(())
        } else {
            Err(denied("stale or exited owned host"))
        }
    }
    fn publish(&self, reconciled: bool) -> Result<(), HostError> {
        let mut facts = self.registry.facts.lock().map_err(io)?;
        let f = facts
            .get_mut(&self.config.scope.session_id)
            .ok_or_else(|| denied("host removed"))?;
        if f.instance != self.instance
            || f.generation != self.generation
            || f.scope != self.config.scope
        {
            return Err(denied("stale host publication"));
        }
        f.thread = self.thread.clone();
        f.reconciled = reconciled;
        Ok(())
    }
    fn send(&mut self, v: Value) -> Result<(), HostError> {
        self.fence()?;
        let s = self.stdin.as_mut().ok_or_else(|| io("closed stdin"))?;
        serde_json::to_writer(&mut *s, &v).map_err(io)?;
        s.write_all(b"\n").map_err(io)?;
        s.flush().map_err(io)
    }
    fn dispatch(&mut self, v: &Value) -> Result<(), HostError> {
        if v.get("method").is_some() && v.get("id").is_some() {
            if v["method"] != "item/tool/call" {
                return self.send(json!({"id":v["id"],"error":{"code":-32601,"message":"server method is not admitted"}}));
            }
            let p = &v["params"];
            let valid = v["method"] == "item/tool/call"
                && p["tool"] == "linkstart_call"
                && (p.get("namespace").is_none() || p["namespace"].is_null())
                && p["threadId"].as_str() == self.thread.as_deref()
                && self.turn.as_deref().is_some_and(|t| p["turnId"] == t)
                && p["callId"].is_string()
                && self.inflight.is_some()
                && self.fence().is_ok();
            let _lease = if valid {
                self.callback_lease().ok()
            } else {
                None
            };
            let answer = if _lease.is_some() {
                self.facade.call(&self.config.scope, &p["arguments"])
            } else {
                Err("tool scope/thread/turn is not currently admitted".into())
            };
            let result = match answer {
                Ok(value) => {
                    json!({"success":true,"contentItems":[{"type":"inputText","text":serde_json::to_string(&value).map_err(io)?}]})
                }
                Err(_) => {
                    json!({"success":false,"contentItems":[{"type":"inputText","text":"Runtime tool denied"}]})
                }
            };
            self.send(json!({"id":v["id"],"result":result}))?;
        } else if v.get("method").is_some() {
            let p = &v["params"];
            if ["turn/started", "turn/completed"]
                .iter()
                .any(|method| v["method"] == *method)
                && self.inflight.is_some()
            {
                let observed = p["turn"]["id"].as_str();
                if p["threadId"].as_str() != self.thread.as_deref()
                    || observed.is_none()
                    || self.turn.as_deref().is_some_and(|t| Some(t) != observed)
                {
                    self.publish(false)?;
                    return Err(HostError::Unknown {
                        operation_id: self.inflight.clone().unwrap(),
                    });
                }
            }
            if v["method"] == "turn/started"
                && p["threadId"].as_str() == self.thread.as_deref()
                && self.inflight.is_some()
            {
                if let Some(t) = p["turn"]["id"].as_str() {
                    if self.turn.as_deref().is_none_or(|old| old == t) {
                        self.turn = Some(t.into());
                    }
                }
            }
            if v["method"] == "turn/completed"
                && p["threadId"].as_str() == self.thread.as_deref()
                && p["turn"]["id"].as_str() == self.turn.as_deref()
            {
                if let Some(operation) = &self.inflight {
                    self.registry
                        .db()?
                        .execute(
                            "UPDATE host_operations SET state='completed',turn=?2 WHERE id=?1",
                            params![operation, p["turn"]["id"].as_str()],
                        )
                        .map_err(io)?;
                }
                self.inflight = None;
                self.turn = None;
            }
            if self.notifications.len() >= 256 {
                return Err(HostError::Protocol("notification backlog exceeded".into()));
            }
            self.notifications.push(v.clone());
        }
        Ok(())
    }
    fn request(
        &mut self,
        method: &str,
        params: Value,
        operation: &str,
    ) -> Result<Value, HostError> {
        self.next_id += 1;
        let id = self.next_id;
        let outcome = (|| {
            self.send(json!({"id":id,"method":method,"params":params}))?;
            let until = Instant::now() + self.config.timeout;
            loop {
                let v = self
                    .rx
                    .recv_timeout(until.saturating_duration_since(Instant::now()))
                    .map_err(io)?
                    .map_err(HostError::Protocol)?;
                if v.get("method").is_none() && v["id"] == id {
                    if v.get("error").is_some() {
                        return Err(HostError::Protocol("provider rejected request".into()));
                    }
                    return v
                        .get("result")
                        .cloned()
                        .ok_or_else(|| HostError::Protocol("response missing result".into()));
                }
                self.dispatch(&v)?;
            }
        })();
        if outcome.is_err() {
            let _ = self.publish(false);
            return Err(HostError::Unknown {
                operation_id: operation.into(),
            });
        }
        outcome
    }
    pub fn initialize(&mut self) -> Result<Value, HostError> {
        if self.initialized {
            return Err(denied("already initialized"));
        }
        let r=self.request("initialize",json!({"clientInfo":{"name":"linkstart-owned","version":"0.1.6"},"capabilities":{"experimentalApi":true}}),"initialize")?;
        let version_ok = r["userAgent"]
            .as_str()
            .is_some_and(|s| s.split_whitespace().any(|part| part.ends_with("/0.163.0")));
        let write = matches!(self.config.profile, ExecutionProfile::WorkspaceWrite(_));
        if !version_ok
            || (write && (r["platformFamily"] != "windows" || r["platformOs"] != "windows"))
        {
            return Err(HostError::Unsupported(
                "observed app-server version/platform has no verified capability".into(),
            ));
        }
        self.send(json!({"method":"initialized","params":{}}))?;
        self.initialized = true;
        Ok(r)
    }
    fn thread_params(&self) -> Value {
        let write = matches!(self.config.profile, ExecutionProfile::WorkspaceWrite(_));
        json!({"model":self.config.model,"cwd":self.config.workspace,"sandbox":if write{"workspace-write"}else{"read-only"},"approvalPolicy":"never","config":{"model_reasoning_effort":"low","tools.web_search":false,"tools.view_image":false,"features.shell_tool":write,"sandbox_workspace_write.network_access":false}})
    }
    pub fn start_owned_thread(&mut self) -> Result<String, HostError> {
        if !self.initialized || self.thread.is_some() {
            return Err(denied("initialize once before creating one owned thread"));
        }
        let mut p = self.thread_params();
        p["allowProviderModelFallback"] = json!(false);
        p["ephemeral"] = json!(false);
        p["dynamicTools"] = json!([{"type":"function","name":"linkstart_call","description":"Call the authorized LinkStart Runtime facade.","inputSchema":{"type":"object","properties":{"request_id":{"type":"string"},"tool":{"type":"string"},"space_id":{"type":"string"},"args":{"type":"object"}},"required":["request_id","tool","space_id","args"],"additionalProperties":false}}]);
        let r = self.request("thread/start", p, "thread/start")?;
        self.accept_thread(&r, None)?;
        let thread = self.thread.clone().unwrap();
        self.acquire_thread_lock(&thread)?;
        self.registry
            .db()?
            .execute(
                "INSERT INTO owned_threads(thread,scope,model,workspace,profile) VALUES(?1,?2,?3,?4,?5)",
                params![
                    thread,
                    serde_json::to_string(&self.config.scope).map_err(io)?,
                    self.config.model,
                    self.config.workspace.to_string_lossy(),
                    serde_json::to_string(&self.config.profile).map_err(io)?
                ],
            )
            .map_err(io)?;
        self.publish(true)?;
        Ok(thread)
    }
    fn accept_thread(&mut self, r: &Value, expected: Option<&str>) -> Result<(), HostError> {
        let t = r["thread"]["id"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| HostError::Protocol("missing exact thread".into()))?;
        if expected.is_some_and(|e| e != t) || r["model"] != self.config.model {
            return Err(HostError::Unsupported("exact thread/model changed".into()));
        }
        let cwd = r["cwd"]
            .as_str()
            .and_then(|p| Path::new(p).canonicalize().ok());
        let write = matches!(self.config.profile, ExecutionProfile::WorkspaceWrite(_));
        let sandbox = &r["sandbox"];
        let roots_ok = sandbox.get("writableRoots").is_none_or(|roots| {
            roots.as_array().is_some_and(|roots| {
                roots.iter().all(|p| {
                    p.as_str()
                        .and_then(|p| Path::new(p).canonicalize().ok())
                        .is_some_and(|p| p == self.config.workspace)
                })
            })
        });
        if cwd.as_deref() != Some(self.config.workspace.as_path())
            || r["approvalPolicy"] != "never"
            || sandbox["type"] != if write { "workspaceWrite" } else { "readOnly" }
            || sandbox.get("networkAccess").is_some_and(|v| v != false)
            || !roots_ok
        {
            return Err(HostError::Unsupported(
                "effective cwd/approval/sandbox differs from approved profile".into(),
            ));
        }
        self.thread = Some(t.into());
        Ok(())
    }
    pub fn resume_owned_thread(&mut self, thread: &str) -> Result<String, HostError> {
        if !self.initialized || self.thread.is_some() {
            return Err(denied("fresh initialized owned host required"));
        }
        let record: Result<(String, String, String, String), _> = self.registry.db()?.query_row(
            "SELECT scope,model,workspace,profile FROM owned_threads WHERE thread=?1",
            [thread],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        );
        let (scope, model, workspace, profile) = record.map_err(|_| {
            HostError::Unsupported("external thread has no private ownership record".into())
        })?;
        let old: HostScope = serde_json::from_str(&scope).map_err(io)?;
        let mut current = self.config.scope.clone();
        current.binding_generation = old.binding_generation;
        current.session_id = old.session_id.clone();
        if profile != serde_json::to_string(&self.config.profile).map_err(io)?
            || current != old
            || model != self.config.model
            || workspace != self.config.workspace.to_string_lossy()
        {
            return Err(denied("owned thread scope/profile mismatch"));
        }
        self.acquire_thread_lock(thread)?;
        let mut p = self.thread_params();
        p["threadId"] = json!(thread);
        let r = self.request("thread/resume", p, "thread/resume")?;
        self.accept_thread(&r, Some(thread))?;
        // Unknown turn effects remain unknown even after a successful exact resume.
        let unresolved: u64 = self
            .registry
            .db()?
            .query_row(
                "SELECT count(*) FROM host_operations WHERE thread=?1 AND state!='completed'",
                [thread],
                |r| r.get(0),
            )
            .map_err(io)?;
        self.publish(unresolved == 0)?;
        Ok(thread.into())
    }
    pub fn start_turn(&mut self, operation_id: &str, text: &str) -> Result<String, HostError> {
        if self.inflight.is_some() || operation_id.is_empty() {
            return Err(denied("idle host and durable operation ID required"));
        }
        let thread = self
            .thread
            .clone()
            .ok_or_else(|| denied("owned thread required"))?;
        let db = self.registry.db()?;
        let unresolved: Option<String> = db
            .query_row(
                "SELECT id FROM host_operations WHERE thread=?1 AND state!='completed' LIMIT 1",
                [&thread],
                |r| r.get(0),
            )
            .optional()
            .map_err(io)?;
        if let Some(original_operation) = unresolved {
            return Err(HostError::Unknown {
                operation_id: original_operation,
            });
        }
        db.execute(
            "INSERT INTO host_operations(id,thread,state) VALUES(?1,?2,'unknown')",
            params![operation_id, thread],
        )
        .map_err(|_| denied("operation already exists; reconcile without resending"))?;
        self.inflight = Some(operation_id.into());
        let marker = format!("[linkstart-operation:{operation_id}]\n{text}");
        let r=self.request("turn/start",json!({"threadId":thread,"model":self.config.model,"effort":"low","input":[{"type":"text","text":marker}]}),operation_id)?;
        let turn = r["turn"]["id"]
            .as_str()
            .ok_or_else(|| HostError::Unknown {
                operation_id: operation_id.into(),
            })?
            .to_owned();
        if self.turn.as_deref().is_some_and(|t| t != turn) {
            self.publish(false)?;
            return Err(HostError::Unknown {
                operation_id: operation_id.into(),
            });
        }
        let early_completed = self.notifications.iter().any(|v| {
            v["method"] == "turn/completed"
                && v["params"]["threadId"] == thread
                && v["params"]["turn"]["id"] == turn
        });
        if early_completed {
            self.turn = None;
            self.inflight = None;
        } else {
            self.turn = Some(turn.clone());
        }
        db.execute(
            "UPDATE host_operations SET state=?3,turn=?2 WHERE id=?1",
            params![
                operation_id,
                turn,
                if early_completed {
                    "completed"
                } else {
                    "accepted"
                }
            ],
        )
        .map_err(io)?;
        self.publish(true)?;
        Ok(turn)
    }
    pub fn pump(&mut self, timeout: Duration) -> Result<Value, HostError> {
        if !self.notifications.is_empty() {
            return Ok(self.notifications.remove(0));
        }
        let received = match self.rx.recv_timeout(timeout) {
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(HostError::Idle),
            Err(e) => Err(io(e)),
            Ok(v) => v.map_err(HostError::Protocol),
        };
        let v = match received {
            Ok(v) => v,
            Err(e) => {
                if let Some(operation) = self.inflight.clone() {
                    self.registry
                        .db()?
                        .execute(
                            "UPDATE host_operations SET state='unknown' WHERE id=?1",
                            [&operation],
                        )
                        .map_err(io)?;
                    self.publish(false)?;
                    return Err(HostError::Unknown {
                        operation_id: operation,
                    });
                }
                return Err(e);
            }
        };
        if v.get("method").is_none() {
            self.publish(false)?;
            return Err(self
                .inflight
                .as_ref()
                .map(|operation| HostError::Unknown {
                    operation_id: operation.clone(),
                })
                .unwrap_or_else(|| {
                    HostError::Protocol("unexpected response outside request".into())
                }));
        }
        self.dispatch(&v)?;
        if !self.notifications.is_empty() {
            self.notifications.remove(0);
        }
        Ok(v)
    }
    /// Read-only reconciliation evidence. No absence inference or automatic retry.
    pub fn read_exact_thread(&mut self) -> Result<Value, HostError> {
        let t = self
            .thread
            .clone()
            .ok_or_else(|| denied("owned thread required"))?;
        let r = self.request(
            "thread/read",
            json!({"threadId":t,"includeTurns":true}),
            "thread/read",
        )?;
        if r["thread"]["id"] != t {
            return Err(denied("thread read changed identity"));
        }
        Ok(r)
    }
    /// Positive exact user-message evidence only. Missing/truncated history stays
    /// unknown. This method never starts a replacement turn or a new thread.
    pub fn reconcile_operation(&mut self, operation_id: &str) -> Result<String, HostError> {
        self.reconcile_operation_observation(operation_id)
            .map(|observed| observed.turn_id)
    }
    /// Exact provider evidence; terminal outcome is distinct from forum/task success.
    pub fn reconcile_operation_observation(
        &mut self,
        operation_id: &str,
    ) -> Result<ReconciledOperation, HostError> {
        let thread = self
            .thread
            .clone()
            .ok_or_else(|| denied("owned thread required"))?;
        let db = self.registry.db()?;
        let owner: String = db
            .query_row(
                "SELECT thread FROM host_operations WHERE id=?1",
                [operation_id],
                |r| r.get(0),
            )
            .map_err(|_| denied("operation not owned"))?;
        if owner != thread {
            return Err(denied("operation belongs to another thread"));
        }
        let read = self.read_exact_thread()?;
        let marker = format!("[linkstart-operation:{operation_id}]\n");
        let turns = read["thread"]["turns"]
            .as_array()
            .ok_or_else(|| HostError::Unknown {
                operation_id: operation_id.into(),
            })?;
        let matched: Vec<_> = turns
            .iter()
            .filter(|t| {
                t["items"].as_array().is_some_and(|items| {
                    items.iter().any(|i| {
                        i["type"] == "userMessage"
                            && i["content"].as_array().is_some_and(|cc| {
                                cc.iter().any(|c| {
                                    c["type"] == "text"
                                        && c["text"]
                                            .as_str()
                                            .is_some_and(|s| s.starts_with(&marker))
                                })
                            })
                    })
                })
            })
            .collect();
        if matched.len() != 1 {
            return Err(HostError::Unknown {
                operation_id: operation_id.into(),
            });
        }
        let turn = matched[0]["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HostError::Unknown {
                operation_id: operation_id.into(),
            })?
            .to_string();
        let outcome = matched[0]["status"]
            .as_str()
            .and_then(ProviderTurnOutcome::from_status)
            .ok_or_else(|| HostError::Unknown {
                operation_id: operation_id.into(),
            })?;
        let state = if outcome == ProviderTurnOutcome::InProgress {
            "accepted"
        } else {
            "completed"
        };
        if state == "accepted" {
            if self
                .inflight
                .as_deref()
                .is_some_and(|current| current != operation_id)
            {
                return Err(HostError::Unknown {
                    operation_id: operation_id.into(),
                });
            }
            self.turn = Some(turn.clone());
            self.inflight = Some(operation_id.into());
        } else if self.inflight.as_deref() == Some(operation_id) {
            self.turn = None;
            self.inflight = None;
        }
        db.execute(
            "UPDATE host_operations SET state=?4,turn=?2 WHERE id=?1 AND thread=?3",
            params![operation_id, turn, thread, state],
        )
        .map_err(io)?;
        let unknown: u64 = db
            .query_row(
                "SELECT count(*) FROM host_operations WHERE thread=?1 AND state='unknown'",
                [&thread],
                |r| r.get(0),
            )
            .map_err(io)?;
        self.publish(unknown == 0)?;
        Ok(ReconciledOperation {
            operation_id: operation_id.into(),
            thread_id: thread,
            turn_id: turn,
            outcome,
        })
    }
    pub fn interrupt(&mut self) -> Result<Value, HostError> {
        let t = self
            .thread
            .clone()
            .ok_or_else(|| denied("owned thread required"))?;
        let turn = self
            .turn
            .clone()
            .ok_or_else(|| denied("known exact active turn required"))?;
        self.request(
            "turn/interrupt",
            json!({"threadId":t,"turnId":turn}),
            "turn/interrupt",
        )
    }
    pub fn close(&mut self) -> Result<(), HostError> {
        self.alive.store(false, Ordering::Release);
        let _ = self.publish(false);
        self.stdin.take();
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if self
                .child
                .lock()
                .map_err(io)?
                .try_wait()
                .map_err(io)?
                .is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        #[cfg(windows)]
        self.job.terminate().map_err(io)?;
        if self
            .child
            .lock()
            .map_err(io)?
            .try_wait()
            .map_err(io)?
            .is_none()
        {
            self.child.lock().map_err(io)?.kill().map_err(io)?;
        }
        self.child.lock().map_err(io)?.wait().map_err(io)?;
        #[cfg(windows)]
        if self.job.active().map_err(io)? {
            return Err(io("owned descendants still live"));
        }
        Ok(())
    }
}
impl Drop for OwnedCodexHost {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(windows)]
mod windows_start {
    use std::{ffi::c_void, io, mem};
    type Handle = *mut c_void;
    #[repr(C)]
    struct ThreadEntry {
        size: u32,
        usage: u32,
        id: u32,
        owner: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
        fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
        fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
        fn OpenThread(access: u32, inherit: i32, id: u32) -> Handle;
        fn ResumeThread(thread: Handle) -> u32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    struct OwnedHandle(Handle);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    /// Only for a just-created CREATE_SUSPENDED child held by std::process::Child.
    /// No thread names, PID adoption, native undocumented APIs, or leaked handles.
    pub(super) fn resume_initial_thread(pid: u32) -> Result<(), String> {
        unsafe {
            let raw = CreateToolhelp32Snapshot(0x00000004, 0); // TH32CS_SNAPTHREAD
            if raw as isize == -1 {
                return Err(io::Error::last_os_error().to_string());
            }
            let snapshot = OwnedHandle(raw);
            let mut entry: ThreadEntry = mem::zeroed();
            entry.size = mem::size_of::<ThreadEntry>() as u32;
            if Thread32First(snapshot.0, &mut entry) == 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            let mut ids = Vec::new();
            loop {
                if entry.owner == pid {
                    ids.push(entry.id);
                }
                entry.size = mem::size_of::<ThreadEntry>() as u32;
                if Thread32Next(snapshot.0, &mut entry) == 0 {
                    if io::Error::last_os_error().raw_os_error() != Some(18) {
                        return Err(io::Error::last_os_error().to_string());
                    } // ERROR_NO_MORE_FILES
                    break;
                }
            }
            if ids.len() != 1 {
                return Err("suspended owned child must have one initial thread".into());
            }
            let raw = OpenThread(0x0002, 0, ids[0]); // THREAD_SUSPEND_RESUME
            if raw.is_null() {
                return Err(io::Error::last_os_error().to_string());
            }
            let thread = OwnedHandle(raw);
            match ResumeThread(thread.0) {
                1 => Ok(()),
                u32::MAX => Err(io::Error::last_os_error().to_string()),
                _ => Err("unexpected initial thread suspend count".into()),
            }
        }
    }
}
