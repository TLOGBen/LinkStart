//! Runtime-owned workers. Provider I/O never runs under the session map lock.
use crate::runtime_codex_host::{
    HostError, HostScope, OwnedCodexRegistry, OwnedHostConfig, ProviderTurnOutcome,
    ReconciledOperation, RuntimeToolFacade,
};
use crate::runtime_core::{ApiError, Engine};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone)]
pub struct SupervisorOptions {
    pub queue_capacity: usize,
    pub heartbeat_interval: Duration,
    pub pump_interval: Duration,
}
impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            queue_capacity: 16,
            heartbeat_interval: Duration::from_secs(1),
            pump_interval: Duration::from_millis(25),
        }
    }
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum SessionPhase {
    Starting,
    Running,
    Stopping,
    Exited,
    Unknown,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum WorkerStage {
    Spawn,
    Initialize,
    ThreadAdmission,
    Running,
    Cleanup,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum WorkerFailureKind {
    Authorization,
    Unsupported,
    UnknownOutcome,
    Protocol,
    Io,
    Panic,
    Pending,
    Idle,
}
#[derive(Clone, Debug, Serialize)]
pub struct WorkerFailure {
    pub stage: WorkerStage,
    pub kind: WorkerFailureKind,
}
fn failure_kind(error: &HostError) -> WorkerFailureKind {
    match error {
        HostError::Denied(_) => WorkerFailureKind::Authorization,
        HostError::Unsupported(_) => WorkerFailureKind::Unsupported,
        HostError::Unknown { .. } => WorkerFailureKind::UnknownOutcome,
        HostError::Protocol(_) => WorkerFailureKind::Protocol,
        HostError::Io(_) => WorkerFailureKind::Io,
        HostError::Pending { .. } => WorkerFailureKind::Pending,
        HostError::Idle => WorkerFailureKind::Idle,
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct SessionStatus {
    pub scope: HostScope,
    pub phase: SessionPhase,
    pub provider_thread_id: Option<String>,
    pub worker_exited: bool,
    pub heartbeat_last_success_ms: Option<u64>,
    pub heartbeat_observation_pending: bool,
    pub worker_stage: WorkerStage,
    pub worker_failure: Option<WorkerFailure>,
}
impl SessionStatus {
    /// Called only with the session status mutex held.
    fn mark_stopping(&mut self) {
        // The worker can publish a terminal state after stop's JoinHandle check
        // but before stop acquires this mutex. Preserve that complete result.
        if !self.worker_exited {
            self.phase = SessionPhase::Stopping;
        }
    }
}
#[derive(Clone, Debug)]
pub enum ThreadAdmission {
    New,
    ExactResume(String),
}
/// Admission receipt, never a claim that the provider completed the operation.
#[derive(Clone, Debug, Serialize)]
pub struct Accepted {
    pub session_id: String,
    pub binding_generation: u64,
    pub operation_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum OperationPhase {
    Queued,
    Dispatching,
    Accepted,
    Confirmed,
    Rejected,
    Unknown,
}
#[derive(Clone, Debug, Serialize)]
pub struct OperationSnapshot {
    pub operation_id: String,
    pub phase: OperationPhase,
    pub error: Option<String>,
    pub provider_turn_id: Option<String>,
    pub provider_outcome: Option<ProviderTurnOutcome>,
}
struct Operation {
    snapshot: OperationSnapshot,
    fingerprint: String,
}
fn apply_observation(snapshot: &mut OperationSnapshot, outcome: ProviderTurnOutcome) {
    snapshot.phase = if outcome == ProviderTurnOutcome::InProgress {
        OperationPhase::Accepted
    } else {
        OperationPhase::Confirmed
    };
    snapshot.error = match outcome {
        ProviderTurnOutcome::Failed => Some("provider turn failed".into()),
        ProviderTurnOutcome::Interrupted => Some("provider turn interrupted".into()),
        _ => None,
    };
    snapshot.provider_outcome = Some(outcome);
}
#[derive(Debug)]
pub enum SupervisorError {
    Denied,
    QueueFull,
    Pending,
    Closed,
    Invalid,
    Conflict,
    Host(HostError),
}
impl From<HostError> for SupervisorError {
    fn from(e: HostError) -> Self {
        Self::Host(e)
    }
}
impl std::fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SupervisorError {}
enum Command {
    Turn(String, String),
    Read(SyncSender<Result<Value, HostError>>),
    Reconcile(String, SyncSender<Result<ReconciledOperation, HostError>>),
    Interrupt(SyncSender<Result<Value, HostError>>),
}
enum CommandKind {
    Turn,
    Read,
    Reconcile,
    Interrupt,
    Stop,
}
impl CommandKind {
    fn required_grant(&self) -> &'static str {
        match self {
            Self::Turn => "send",
            Self::Read | Self::Reconcile => "observe",
            Self::Interrupt => "control",
            Self::Stop => "manage",
        }
    }
}
impl Command {
    fn required_grant(&self) -> &'static str {
        match self {
            Self::Turn(..) => CommandKind::Turn,
            Self::Read(..) => CommandKind::Read,
            Self::Reconcile(..) => CommandKind::Reconcile,
            Self::Interrupt(..) => CommandKind::Interrupt,
        }
        .required_grant()
    }
    fn deny(self, session: &Session) {
        let denied = || HostError::Denied("current command authority denied".into());
        match self {
            Self::Turn(id, _) => {
                if let Some(op) = session
                    .operations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&id)
                {
                    op.snapshot.phase = OperationPhase::Rejected;
                    op.snapshot.error =
                        Some("current command authority denied before dispatch".into());
                }
            }
            Self::Read(reply) | Self::Interrupt(reply) => {
                let _ = reply.send(Err(denied()));
            }
            Self::Reconcile(_, reply) => {
                let _ = reply.send(Err(denied()));
            }
        }
    }
}
struct Session {
    scope: HostScope,
    tx: SyncSender<Command>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<SessionStatus>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    operations: Mutex<HashMap<String, Operation>>,
}
impl Session {
    fn finished(&self) -> bool {
        match self
            .worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(worker) => worker.is_finished(),
            None => {
                self.status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .worker_exited
            }
        }
    }
    fn snapshot(&self) -> SessionStatus {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        status.worker_exited = self.finished();
        status
    }
    fn join_finished(&self) {
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if worker.as_ref().is_some_and(|w| w.is_finished()) {
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
    }
}
pub struct OwnedSessionSupervisor {
    engine: Engine,
    registry: OwnedCodexRegistry,
    options: SupervisorOptions,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

fn authorize(
    engine: &Engine,
    credential: &str,
    scope: &HostScope,
    operation: &str,
) -> Result<(), SupervisorError> {
    engine
        .with_endpoint_authorization(
            credential,
            &scope.space_id,
            &scope.endpoint_id,
            operation,
            |auth| {
                if auth["identity_id"] != scope.identity_id || scope.identity_id == "owner" {
                    return Err(ApiError {
                        code: "Denied".into(),
                        message: "exact member required".into(),
                    });
                }
                Ok(())
            },
        )
        .map_err(|_| SupervisorError::Denied)
}
impl OwnedSessionSupervisor {
    pub fn new(
        engine: Engine,
        registry: OwnedCodexRegistry,
        options: SupervisorOptions,
    ) -> Result<Self, SupervisorError> {
        if options.queue_capacity == 0
            || options.queue_capacity > 256
            || options.heartbeat_interval.is_zero()
            || options.heartbeat_interval > Duration::from_secs(1)
            || options.pump_interval.is_zero()
            || options.pump_interval > Duration::from_millis(100)
        {
            return Err(SupervisorError::Invalid);
        }
        Ok(Self {
            engine,
            registry,
            options,
            sessions: Mutex::new(HashMap::new()),
        })
    }
    pub fn start(
        &self,
        config: OwnedHostConfig,
        credential: String,
        facade: Arc<dyn RuntimeToolFacade>,
        admission: ThreadAdmission,
    ) -> Result<Accepted, SupervisorError> {
        let scope = config.scope.clone();
        authorize(&self.engine, &credential, &scope, "send")?;
        if matches!(&admission, ThreadAdmission::ExactResume(id) if id.trim().is_empty()) {
            return Err(SupervisorError::Invalid);
        }
        // Reserve before spawn, but release the map before any provider/Engine I/O.
        let (tx, rx) = mpsc::sync_channel(self.options.queue_capacity);
        let status = Arc::new(Mutex::new(SessionStatus {
            scope: scope.clone(),
            phase: SessionPhase::Starting,
            provider_thread_id: None,
            worker_exited: false,
            heartbeat_last_success_ms: None,
            heartbeat_observation_pending: true,
            worker_stage: WorkerStage::Spawn,
            worker_failure: None,
        }));
        let session = Arc::new(Session {
            scope: scope.clone(),
            tx,
            stop: Arc::new(AtomicBool::new(false)),
            status,
            worker: Mutex::new(None),
            operations: Mutex::new(HashMap::new()),
        });
        {
            let mut sessions = self.sessions.lock().map_err(|_| SupervisorError::Closed)?;
            if sessions.contains_key(&scope.session_id) {
                return Err(SupervisorError::Conflict);
            }
            sessions.insert(scope.session_id.clone(), session.clone());
        }
        let registry = self.registry.clone();
        let engine = self.engine.clone();
        let options = self.options.clone();
        let owned = session.clone();
        let worker = thread::Builder::new()
            .name("runtime-owned-session".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_worker(
                        engine, registry, config, credential, facade, admission, rx, &owned,
                        options,
                    )
                }));
                let mut s = owned.status.lock().unwrap_or_else(|e| e.into_inner());
                if s.worker_failure.is_none() {
                    s.worker_failure = match &outcome {
                        Ok(Err(error)) => Some(WorkerFailure {
                            stage: s.worker_stage.clone(),
                            kind: failure_kind(error),
                        }),
                        Err(_) => Some(WorkerFailure {
                            stage: s.worker_stage.clone(),
                            kind: WorkerFailureKind::Panic,
                        }),
                        _ => None,
                    };
                }
                s.phase = if matches!(outcome, Ok(Ok(()))) {
                    SessionPhase::Exited
                } else {
                    SessionPhase::Unknown
                };
                s.worker_exited = true;
                for op in owned
                    .operations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values_mut()
                {
                    if matches!(
                        op.snapshot.phase,
                        OperationPhase::Queued
                            | OperationPhase::Dispatching
                            | OperationPhase::Accepted
                    ) {
                        op.snapshot.phase = OperationPhase::Unknown;
                        op.snapshot.error = Some(
                            "worker exited; reconcile exact operation without resending".into(),
                        );
                    }
                }
            })
            .map_err(|e| {
                self.sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&scope.session_id);
                SupervisorError::Host(HostError::Io(e.to_string()))
            })?;
        *session.worker.lock().map_err(|_| SupervisorError::Closed)? = Some(worker);
        Ok(Accepted {
            session_id: scope.session_id,
            binding_generation: scope.binding_generation,
            operation_id: None,
        })
    }
    fn session(
        &self,
        credential: &str,
        scope: &HostScope,
        operation: &str,
    ) -> Result<Arc<Session>, SupervisorError> {
        authorize(&self.engine, credential, scope, operation)?;
        let session = self
            .sessions
            .lock()
            .map_err(|_| SupervisorError::Closed)?
            .get(&scope.session_id)
            .cloned()
            .ok_or(SupervisorError::Closed)?;
        if session.scope != *scope {
            return Err(SupervisorError::Denied);
        }
        Ok(session)
    }
    pub fn get(
        &self,
        credential: &str,
        scope: &HostScope,
    ) -> Result<SessionStatus, SupervisorError> {
        let s = self.session(credential, scope, "observe")?;
        Ok(s.snapshot())
    }
    pub fn list(&self, credential: &str) -> Vec<SessionStatus> {
        let scopes: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| s.scope.clone())
            .collect();
        scopes
            .iter()
            .filter_map(|scope| self.get(credential, scope).ok())
            .collect()
    }
    fn enqueue(
        &self,
        credential: &str,
        scope: &HostScope,
        command: Command,
    ) -> Result<(), SupervisorError> {
        let session = self.session(credential, scope, command.required_grant())?;
        if session.stop.load(Ordering::Acquire)
            || session
                .status
                .lock()
                .map_err(|_| SupervisorError::Closed)?
                .worker_exited
        {
            return Err(SupervisorError::Closed);
        }
        session.tx.try_send(command).map_err(|e| match e {
            TrySendError::Full(_) => SupervisorError::QueueFull,
            TrySendError::Disconnected(_) => SupervisorError::Closed,
        })
    }
    pub fn start_turn(
        &self,
        credential: &str,
        scope: &HostScope,
        operation_id: String,
        text: String,
    ) -> Result<Accepted, SupervisorError> {
        if operation_id.trim().is_empty() {
            return Err(SupervisorError::Invalid);
        }
        let session = self.session(credential, scope, CommandKind::Turn.required_grant())?;
        use sha2::{Digest, Sha256};
        let fingerprint = format!("{:x}", Sha256::digest(text.as_bytes()));
        let mut operations = session
            .operations
            .lock()
            .map_err(|_| SupervisorError::Closed)?;
        if let Some(op) = operations.get(&operation_id) {
            if op.fingerprint != fingerprint {
                return Err(SupervisorError::Conflict);
            }
        } else {
            if operations.len() >= 1024 {
                return Err(SupervisorError::QueueFull);
            }
            operations.insert(
                operation_id.clone(),
                Operation {
                    snapshot: OperationSnapshot {
                        operation_id: operation_id.clone(),
                        phase: OperationPhase::Queued,
                        error: None,
                        provider_turn_id: None,
                        provider_outcome: None,
                    },
                    fingerprint,
                },
            );
            // Keep operation registration atomic with queue admission, but never hold
            // this lock across Engine/provider work.
            if session.stop.load(Ordering::Acquire) {
                operations.remove(&operation_id);
                return Err(SupervisorError::Closed);
            }
            if let Err(e) = session
                .tx
                .try_send(Command::Turn(operation_id.clone(), text))
            {
                operations.remove(&operation_id);
                return Err(match e {
                    TrySendError::Full(_) => SupervisorError::QueueFull,
                    TrySendError::Disconnected(_) => SupervisorError::Closed,
                });
            }
        }
        Ok(Accepted {
            session_id: scope.session_id.clone(),
            binding_generation: scope.binding_generation,
            operation_id: Some(operation_id),
        })
    }
    pub fn operation(
        &self,
        credential: &str,
        scope: &HostScope,
        operation_id: &str,
    ) -> Result<OperationSnapshot, SupervisorError> {
        let session = self.session(credential, scope, "observe")?;
        let snapshot = session
            .operations
            .lock()
            .map_err(|_| SupervisorError::Closed)?
            .get(operation_id)
            .map(|op| op.snapshot.clone())
            .ok_or(SupervisorError::Closed)?;
        Ok(snapshot)
    }
    pub fn read_exact_thread(
        &self,
        credential: &str,
        scope: &HostScope,
        wait: Duration,
    ) -> Result<Value, SupervisorError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.enqueue(credential, scope, Command::Read(tx))?;
        bounded(rx, wait)
    }
    pub fn reconcile(
        &self,
        credential: &str,
        scope: &HostScope,
        operation_id: String,
        wait: Duration,
    ) -> Result<String, SupervisorError> {
        self.reconcile_observation(credential, scope, operation_id, wait)
            .map(|observed| observed.turn_id)
    }
    pub fn reconcile_observation(
        &self,
        credential: &str,
        scope: &HostScope,
        operation_id: String,
        wait: Duration,
    ) -> Result<ReconciledOperation, SupervisorError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.enqueue(credential, scope, Command::Reconcile(operation_id, tx))?;
        bounded(rx, wait)
    }
    pub fn interrupt(
        &self,
        credential: &str,
        scope: &HostScope,
        wait: Duration,
    ) -> Result<Value, SupervisorError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.enqueue(credential, scope, Command::Interrupt(tx))?;
        bounded(rx, wait)
    }
    pub fn stop(
        &self,
        credential: &str,
        scope: &HostScope,
        wait: Duration,
    ) -> Result<SessionStatus, SupervisorError> {
        let session = self.session(credential, scope, CommandKind::Stop.required_grant())?;
        if session.finished() {
            session.join_finished();
            return Ok(session.snapshot());
        }
        session.stop.store(true, Ordering::Release);
        session
            .status
            .lock()
            .map_err(|_| SupervisorError::Closed)?
            .mark_stopping();
        let fenced = self.registry.fence_binding_timeout(
            &scope.session_id,
            scope.binding_generation,
            wait.min(Duration::from_millis(100)),
        );
        if let Err(e) = fenced {
            return Err(match e {
                HostError::Denied(_) | HostError::Pending { .. } => SupervisorError::Pending,
                other => other.into(),
            });
        }
        let until = std::time::Instant::now() + wait.min(Duration::from_secs(30));
        loop {
            if session.finished() {
                session.join_finished();
                return Ok(session.snapshot());
            }
            if std::time::Instant::now() >= until {
                return Err(SupervisorError::Pending);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    /// Counts the actual worker lifetime, including provider cleanup and heartbeat join.
    pub fn active_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|s| !s.finished())
            .count()
    }
    pub fn request_shutdown(&self) {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for s in sessions {
            s.stop.store(true, Ordering::Release);
            // Immediate local fence; Pending only means an existing callback
            // still needs to drain. Never keep the map lock during that drain.
            let _ = self.registry.fence_binding_timeout(
                &s.scope.session_id,
                s.scope.binding_generation,
                Duration::ZERO,
            );
        }
    }
}
fn bounded<T>(rx: Receiver<Result<T, HostError>>, wait: Duration) -> Result<T, SupervisorError> {
    rx.recv_timeout(wait.min(Duration::from_secs(30)))
        .map_err(|_| SupervisorError::Pending)?
        .map_err(Into::into)
}
impl Drop for OwnedSessionSupervisor {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

fn run_worker(
    engine: Engine,
    registry: OwnedCodexRegistry,
    config: OwnedHostConfig,
    credential: String,
    facade: Arc<dyn RuntimeToolFacade>,
    admission: ThreadAdmission,
    rx: Receiver<Command>,
    session: &Arc<Session>,
    options: SupervisorOptions,
) -> Result<(), HostError> {
    let scope = config.scope.clone();
    let mut host = registry.spawn(config, &credential, facade)?;
    session
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .worker_stage = WorkerStage::Initialize;
    let hb_stop = Arc::new(AtomicBool::new(false));
    let heartbeat_done = hb_stop.clone();
    let stop = session.stop.clone();
    let reg = registry.clone();
    let hb_scope = scope.clone();
    let hb_credential = credential.clone();
    let heartbeat_status = session.status.clone();
    let heartbeat = thread::Builder::new()
        .name("runtime-owned-heartbeat".into())
        .spawn(move || {
            while !heartbeat_done.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                match reg.renew_owned_attachment(&hb_scope, &hb_credential) {
                    Ok(()) => {
                        let mut status = heartbeat_status.lock().unwrap_or_else(|e| e.into_inner());
                        status.heartbeat_last_success_ms = Some(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64,
                        );
                        status.heartbeat_observation_pending = false;
                    }
                    Err(HostError::Idle) => {
                        heartbeat_status
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .heartbeat_observation_pending = true;
                    }
                    Err(_) => {
                        heartbeat_status
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .heartbeat_observation_pending = true;
                        stop.store(true, Ordering::Release);
                        break;
                    }
                }
                let deadline = std::time::Instant::now() + options.heartbeat_interval;
                while std::time::Instant::now() < deadline
                    && !heartbeat_done.load(Ordering::Acquire)
                    && !stop.load(Ordering::Acquire)
                {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        })
        .map_err(|e| HostError::Io(e.to_string()))?;
    // Cleanup is outside the fallible body, including initialize/resume errors.
    let execute = || {
        if session.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        host.initialize()?;
        session
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker_stage = WorkerStage::ThreadAdmission;
        let thread = match admission {
            ThreadAdmission::New => host.start_owned_thread()?,
            ThreadAdmission::ExactResume(id) => host.resume_owned_thread(&id)?,
        };
        {
            let mut status = session.status.lock().unwrap_or_else(|e| e.into_inner());
            status.provider_thread_id = Some(thread);
            status.worker_stage = WorkerStage::Running;
            status.phase = if session.stop.load(Ordering::Acquire) {
                SessionPhase::Stopping
            } else {
                SessionPhase::Running
            };
        }
        while !session.stop.load(Ordering::Acquire) {
            if host.is_exited()? {
                break;
            }
            if let Ok(command) = rx.try_recv() {
                if authorize(&engine, &credential, &scope, command.required_grant()).is_err() {
                    command.deny(session);
                    continue;
                }
                match command {
                    Command::Turn(id, text) => {
                        if let Some(op) = session
                            .operations
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get_mut(&id)
                        {
                            op.snapshot.phase = OperationPhase::Dispatching;
                        }
                        let result = host.start_turn(&id, &text);
                        if let Some(op) = session
                            .operations
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get_mut(&id)
                        {
                            match result {
                                Ok(turn) => {
                                    op.snapshot.phase = OperationPhase::Accepted;
                                    op.snapshot.provider_turn_id = Some(turn);
                                }
                                Err(HostError::Denied(_)) => {
                                    op.snapshot.phase = OperationPhase::Rejected;
                                    op.snapshot.error=Some("host rejected dispatch; inspect exact operation before further work".into());
                                }
                                Err(_) => {
                                    op.snapshot.phase = OperationPhase::Unknown;
                                    op.snapshot.error = Some(
                                        "provider outcome unknown; reconcile without resending"
                                            .into(),
                                    );
                                }
                            }
                        }
                    }
                    Command::Read(reply) => {
                        let _ = reply.send(host.read_exact_thread());
                    }
                    Command::Reconcile(id, reply) => {
                        let observed = host.reconcile_operation_observation(&id);
                        if let Ok(observed) = &observed {
                            if let Some(op) = session
                                .operations
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get_mut(&id)
                            {
                                op.snapshot.provider_turn_id = Some(observed.turn_id.clone());
                                apply_observation(&mut op.snapshot, observed.outcome.clone());
                            }
                        }
                        let _ = reply.send(observed);
                    }
                    Command::Interrupt(reply) => {
                        let _ = reply.send(host.interrupt());
                    }
                }
            }
            match host.pump(options.pump_interval) {
                Ok(v) => {
                    if v["method"] == "turn/completed"
                        && v["params"]["threadId"].as_str() == host.thread_id()
                    {
                        let turn = v["params"]["turn"]["id"].as_str();
                        for op in session
                            .operations
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .values_mut()
                        {
                            if turn.is_some() && op.snapshot.provider_turn_id.as_deref() == turn {
                                if let Some(outcome) = v["params"]["turn"]["status"]
                                    .as_str()
                                    .and_then(ProviderTurnOutcome::from_status)
                                {
                                    apply_observation(&mut op.snapshot, outcome);
                                }
                            }
                        }
                    }
                }
                Err(HostError::Idle) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(execute))
        .unwrap_or_else(|_| Err(HostError::Protocol("owned worker panicked".into())));
    {
        let mut status = session.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = &result {
            status.worker_failure = Some(WorkerFailure {
                stage: status.worker_stage.clone(),
                kind: failure_kind(error),
            });
        }
        status.worker_stage = WorkerStage::Cleanup;
    }
    session.stop.store(true, Ordering::Release);
    session
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mark_stopping();
    hb_stop.store(true, Ordering::Release);
    let _ = heartbeat.join();
    let fence = registry.fence_binding(&scope.session_id, scope.binding_generation);
    let closed = host.close();
    drop(host);
    result.and(fence).and(closed)
}

#[cfg(test)]
mod stopping_transition_tests {
    use super::*;
    fn status(phase: SessionPhase, worker_exited: bool) -> SessionStatus {
        let failure = if phase == SessionPhase::Unknown {
            Some(WorkerFailure {
                stage: WorkerStage::Initialize,
                kind: WorkerFailureKind::UnknownOutcome,
            })
        } else {
            None
        };
        SessionStatus {
            scope: HostScope {
                identity_id: "member".into(),
                space_id: "space".into(),
                endpoint_id: "endpoint".into(),
                attachment_id: "attachment".into(),
                session_id: "owned".into(),
                binding_generation: 1,
            },
            phase,
            provider_thread_id: None,
            worker_exited,
            heartbeat_last_success_ms: None,
            heartbeat_observation_pending: false,
            worker_stage: WorkerStage::Cleanup,
            worker_failure: failure,
        }
    }
    fn assert_terminal_preserved(phase: SessionPhase) {
        // This fixes the latter half of the race deterministically: the worker
        // has published its full terminal state before stop gets this lock.
        // It is a shared transition invariant, not an OS scheduling reproduction.
        let published = Mutex::new(status(phase.clone(), true));
        let before = serde_json::to_value(published.lock().unwrap().clone()).unwrap();
        published.lock().unwrap().mark_stopping();
        let after = serde_json::to_value(published.lock().unwrap().clone()).unwrap();
        assert_eq!(
            after, before,
            "late stop must preserve complete terminal state for {phase:?}"
        );
    }
    #[test]
    fn stopping_transition_preserves_exited() {
        assert_terminal_preserved(SessionPhase::Exited);
    }
    #[test]
    fn stopping_transition_preserves_unknown_with_failure() {
        assert_terminal_preserved(SessionPhase::Unknown);
    }
    #[test]
    fn stopping_transition_admits_starting_and_running() {
        for phase in [SessionPhase::Starting, SessionPhase::Running] {
            let published = Mutex::new(status(phase, false));
            published.lock().unwrap().mark_stopping();
            let after = published.lock().unwrap();
            assert_eq!(after.phase, SessionPhase::Stopping);
            assert!(!after.worker_exited);
            assert!(after.worker_failure.is_none());
        }
    }
}
