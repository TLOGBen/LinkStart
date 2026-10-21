//! Durable native terminal history. It is recovery evidence, not provider conversation resume.
use crate::runtime_terminal::SessionView;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Debug)]
pub struct Retention {
    pub segment_bytes: usize,
    pub segments_per_session: usize,
    pub max_sessions_per_endpoint: usize,
    pub queue_capacity: usize,
}
impl Default for Retention {
    fn default() -> Self {
        Self {
            segment_bytes: 256 * 1024,
            segments_per_session: 16,
            max_sessions_per_endpoint: 20,
            queue_capacity: 128,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub endpoint_id: String,
    pub session_id: String,
    pub instance_id: String,
    pub generation: u64,
    pub status: String,
    pub durable_cursor: u64,
    pub retained_from: u64,
    pub raw_cursor: u64,
    pub unknown_tail: bool,
    pub process_resume: String,
    pub updated_at: u64,
    pub writer_instance: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub cursor: u64,
    pub at: u64,
    pub kind: String,
    pub raw_start: u64,
    pub raw_end: u64,
    pub bytes: Vec<u8>,
    pub detail: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistorySession {
    pub endpoint_id: String,
    pub session: SessionView,
    pub checkpoint: Checkpoint,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryPage {
    pub events: Vec<HistoryEvent>,
    pub next_cursor: u64,
    pub retained_from: u64,
    pub gap: bool,
    pub checkpoint: Checkpoint,
}
#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    endpoint_id: String,
    sessions: Vec<SessionView>,
}
struct Metrics {
    produced: AtomicU64,
    pending_outputs: AtomicU64,
    lost_from: AtomicU64,
    lost_to: AtomicU64,
    closed: AtomicBool,
    status: Mutex<Option<SessionView>>,
}
#[derive(Clone)]
struct Target {
    path: PathBuf,
    metrics: Arc<Metrics>,
    writer_instance: String,
    retention: Retention,
    safe_root: PathBuf,
}
#[derive(Clone)]
pub struct HistorySink {
    target: Target,
    sender: mpsc::SyncSender<Command>,
    error: Arc<Mutex<Option<String>>>,
}
struct Inner {
    root: PathBuf,
    retention: Retention,
    instance: String,
    shutting_down: AtomicBool,
    sender: mpsc::SyncSender<Command>,
    sinks: Mutex<HashMap<String, HistorySink>>,
    error: Arc<Mutex<Option<String>>>,
}
#[derive(Clone)]
pub struct HistoryStore {
    inner: Arc<Inner>,
}
enum Command {
    Register(Registration, mpsc::SyncSender<Result<Target, String>>),
    Output(Target, u64, Vec<u8>),
    Barrier(mpsc::SyncSender<Result<(), String>>),
}
struct Registration {
    endpoint_id: String,
    view: SessionView,
    root: PathBuf,
    instance: String,
    retention: Retention,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn valid_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Err(
            "history identifier must be bounded ASCII letters, digits, hyphens or underscores"
                .into(),
        )
    } else {
        Ok(())
    }
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(&std::fs::read(path).map_err(error)?).map_err(error)
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let temporary = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    let mut file = File::create(&temporary).map_err(error)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(error)?)
        .map_err(error)?;
    file.sync_all().map_err(error)?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(error)
}
fn protect(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(error)?;
    }
    #[cfg(windows)]
    {
        let who = std::process::Command::new("whoami")
            .output()
            .map_err(error)?;
        if !who.status.success() {
            return Err("cannot identify history owner".into());
        }
        let user = String::from_utf8(who.stdout).map_err(error)?;
        let grant = format!("{}:(OI)(CI)F", user.trim());
        let result = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &grant])
            .output()
            .map_err(error)?;
        if !result.status.success() {
            return Err("cannot protect connection history ACL".into());
        }
    }
    Ok(())
}
// Independently opened handles lock this canonical root's persistent lock file
// in both the current process and other processes. No process-global gate or
// per-root registry couples unrelated roots or retains abandoned root entries.
// No queue reply is awaited while locked. The OS lock covers authority checks
// AND mutations: handoff/recovery, registration, retirement, journal/checkpoints.
struct RootMutation {
    _file: File,
}
impl RootMutation {
    fn lock(root: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(".writer-lock"))
            .map_err(error)?;
        file.lock().map_err(error)?;
        Ok(Self { _file: file })
    }
}
fn ensure_root_current(root: &Path, writer: &str) -> Result<(), String> {
    let instance: String = read_json(&root.join(".writer-instance.json"))?;
    if instance != writer {
        return Err("history writer fenced by a newer runtime".into());
    }
    Ok(())
}
fn recover_tail(session: &Path) -> Result<bool, String> {
    let path = session.join("events.jsonl");
    let size = std::fs::metadata(&path).map_err(error)?.len();
    if size > 16 * 1024 * 1024 {
        return Err("history segment exceeds safe recovery byte budget".into());
    }
    let bytes = std::fs::read(&path).map_err(error)?;
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        return Ok(false);
    }
    let complete = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut quarantine = File::create(session.join("recovered-tail.bin")).map_err(error)?;
    quarantine.write_all(&bytes[complete..]).map_err(error)?;
    quarantine.sync_all().map_err(error)?;
    let journal = OpenOptions::new().write(true).open(path).map_err(error)?;
    journal.set_len(complete as u64).map_err(error)?;
    journal.sync_all().map_err(error)?;
    Ok(true)
}
fn recover(root: &Path, instance: &str) -> Result<(), String> {
    for entry in std::fs::read_dir(root).map_err(error)? {
        let entry = entry.map_err(error)?;
        if !entry.file_type().map_err(error)?.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        let manifest: Manifest = read_json(&manifest_path)?;
        valid_id(&manifest.endpoint_id)?;
        if entry.file_name().to_string_lossy() != manifest.endpoint_id {
            return Err("history manifest endpoint identity mismatch".into());
        }
        for session in manifest.sessions {
            valid_id(&session.id)?;
            let endpoint = std::fs::canonicalize(entry.path()).map_err(error)?;
            let sessions = std::fs::canonicalize(entry.path().join("sessions")).map_err(error)?;
            let session_dir =
                std::fs::canonicalize(entry.path().join("sessions").join(&session.id))
                    .map_err(error)?;
            if endpoint.parent() != Some(root)
                || sessions.parent() != Some(endpoint.as_path())
                || session_dir.parent() != Some(sessions.as_path())
            {
                return Err("recovery history resolves outside known session directory".into());
            }
            let path = entry
                .path()
                .join("sessions")
                .join(&session.id)
                .join("checkpoint.json");
            let incomplete = recover_tail(path.parent().ok_or("invalid recovery session path")?)?;
            let mut cp: Checkpoint = read_json(&path)?;
            if incomplete {
                cp.unknown_tail = true;
            }
            if cp.session_id != session.id || cp.endpoint_id != manifest.endpoint_id {
                return Err("history checkpoint binding mismatch".into());
            }
            if cp.status != "exited" {
                cp.status = "interrupted".into();
                cp.unknown_tail = true;
            }
            cp.writer_instance = instance.into();
            cp.updated_at = now();
            atomic_json(&path, &cp)?;
        }
    }
    Ok(())
}
fn barrier(sender: &mpsc::SyncSender<Command>, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::sync_channel(1);
    let mut command = Command::Barrier(tx);
    loop {
        match sender.try_send(command) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(c)) => {
                if Instant::now() >= deadline {
                    return Err("history flush pending: bounded writer queue is full".into());
                }
                command = c;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err("history writer unavailable".into())
            }
        }
    }
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "history flush pending: durable writer deadline exceeded".to_string())?
}
fn log_paths(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut logs = Vec::new();
    for entry in std::fs::read_dir(path).map_err(error)? {
        let entry = entry.map_err(error)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rotated = name
            .strip_prefix("events-")
            .and_then(|s| s.strip_suffix(".jsonl"))
            .is_some_and(|s| s.len() == 12 && s.bytes().all(|b| b.is_ascii_digit()));
        if (name == "events.jsonl" || rotated) && entry.file_type().map_err(error)?.is_file() {
            logs.push(entry.path());
        }
    }
    logs.sort_by_key(|p| {
        let name = p.file_name().unwrap().to_string_lossy();
        if name == "events.jsonl" {
            "zzzz".into()
        } else {
            name.into_owned()
        }
    });
    Ok(logs)
}
fn remove_segment(path: &Path, session: &Path, safe_root: &Path) -> Result<(), String> {
    let session = std::fs::canonicalize(session).map_err(error)?;
    let resolved = std::fs::canonicalize(path).map_err(error)?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid history segment name")?;
    if !session.starts_with(safe_root)
        || !resolved.starts_with(safe_root)
        || resolved.parent() != Some(session.as_path())
        || !name.starts_with("events-")
        || !name.ends_with(".jsonl")
    {
        return Err("refusing retention delete outside known session segments".into());
    }
    std::fs::remove_file(resolved).map_err(error)
}
fn remove_session(
    sessions: &Path,
    id: &str,
    safe_root: &Path,
    expected_sessions: &Path,
) -> Result<(), String> {
    valid_id(id)?;
    let root = std::fs::canonicalize(sessions).map_err(error)?;
    let target = std::fs::canonicalize(sessions.join(id)).map_err(error)?;
    if root != expected_sessions
        || !root.starts_with(safe_root)
        || !target.starts_with(safe_root)
        || target.parent() != Some(root.as_path())
        || target.file_name().and_then(|s| s.to_str()) != Some(id)
    {
        return Err("refusing retention removal outside the known sessions directory".into());
    }
    std::fs::remove_dir_all(target).map_err(error)
}
fn earliest_cursor(path: &Path, fallback: u64) -> Result<u64, String> {
    use std::io::BufRead;
    for log in log_paths(path)? {
        if let Some(line) = std::io::BufReader::new(File::open(log).map_err(error)?)
            .lines()
            .next()
        {
            let event: HistoryEvent = serde_json::from_str(&line.map_err(error)?).map_err(error)?;
            return Ok(event.cursor);
        }
    }
    Ok(fallback)
}
impl HistoryStore {
    pub fn open(root: &Path, retention: Retention) -> Result<Self, String> {
        if retention.queue_capacity == 0
            || retention.queue_capacity > 4096
            || retention.segment_bytes < 1024
            || retention.segment_bytes > 16 * 1024 * 1024
            || retention.segments_per_session == 0
            || retention.max_sessions_per_endpoint == 0
        {
            return Err("invalid bounded history retention".into());
        }
        std::fs::create_dir_all(root).map_err(error)?;
        let state_root = std::fs::canonicalize(root).map_err(error)?;
        let root = root.join("connections");
        std::fs::create_dir_all(&root).map_err(error)?;
        let root = std::fs::canonicalize(root).map_err(error)?;
        if root.parent() != Some(state_root.as_path()) {
            return Err("connection history directory resolves outside state root".into());
        }
        let instance = uuid::Uuid::new_v4().to_string();
        {
            let _guard = RootMutation::lock(&root)?;
            protect(&root)?;
            // The writer fence must survive a missing/broken session checkpoint.
            // Reopening a runtime fences older writers before recovery touches data.
            atomic_json(&root.join(".writer-instance.json"), &instance)?;
            recover(&root, &instance)?;
        }
        let (sender, rx) = mpsc::sync_channel(retention.queue_capacity);
        let shared_error = Arc::new(Mutex::new(None));
        let writer_error = shared_error.clone();
        std::thread::Builder::new()
            .name("linkstart-history".into())
            .spawn(move || writer(rx, writer_error))
            .map_err(error)?;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                retention,
                instance,
                shutting_down: AtomicBool::new(false),
                sender,
                sinks: Mutex::new(HashMap::new()),
                error: shared_error,
            }),
        })
    }
    pub fn record_session(
        &self,
        endpoint_id: &str,
        view: &SessionView,
    ) -> Result<HistorySink, String> {
        valid_id(endpoint_id)?;
        valid_id(&view.id)?;
        valid_id(&view.instance_id)?;
        if view.status.len() > 64 {
            return Err("bounded native session status required".into());
        }
        let mut sinks = self.inner.sinks.lock().map_err(error)?;
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err("history is shutting down; new recording rejected".into());
        }
        let key = format!("{endpoint_id}/{}", view.id);
        if sinks.contains_key(&key) {
            return Err("session history is already registered".into());
        }
        let (reply, result) = mpsc::sync_channel(1);
        self.inner
            .sender
            .try_send(Command::Register(
                Registration {
                    endpoint_id: endpoint_id.into(),
                    view: view.clone(),
                    root: self.inner.root.clone(),
                    instance: self.inner.instance.clone(),
                    retention: self.inner.retention.clone(),
                },
                reply,
            ))
            .map_err(|_| "history registration backpressure".to_string())?;
        // Filesystem registration was synchronous before this command boundary.
        // Wait for completion without holding root/process mutation authority.
        let target = result
            .recv()
            .map_err(|_| "history writer stopped during registration".to_string())??;
        sinks.retain(|_, sink| sink.target.path.exists());
        let sink = HistorySink {
            target,
            sender: self.inner.sender.clone(),
            error: self.inner.error.clone(),
        };
        sinks.insert(key, sink.clone());
        Ok(sink)
    }
    fn session_path(&self, eid: &str, sid: &str) -> Result<PathBuf, String> {
        valid_id(eid)?;
        valid_id(sid)?;
        Ok(self.inner.root.join(eid).join("sessions").join(sid))
    }
    pub fn checkpoint(&self, eid: &str, sid: &str) -> Result<Checkpoint, String> {
        let mut checkpoint: Checkpoint =
            read_json(&self.session_path(eid, sid)?.join("checkpoint.json"))?;
        if let Some(sink) = self
            .inner
            .sinks
            .lock()
            .map_err(error)?
            .get(&format!("{eid}/{sid}"))
        {
            checkpoint.unknown_tail |=
                sink.target.metrics.produced.load(Ordering::Acquire) > checkpoint.raw_cursor;
        }
        Ok(checkpoint)
    }
    pub fn list(&self, eid: &str) -> Result<Vec<HistorySession>, String> {
        valid_id(eid)?;
        let manifest: Manifest = read_json(&self.inner.root.join(eid).join("manifest.json"))?;
        manifest
            .sessions
            .into_iter()
            .map(|mut session| {
                let checkpoint = self.checkpoint(eid, &session.id)?;
                session.status = checkpoint.status.clone();
                Ok(HistorySession {
                    endpoint_id: eid.into(),
                    session,
                    checkpoint,
                })
            })
            .collect()
    }
    pub fn read(
        &self,
        eid: &str,
        sid: &str,
        cursor: u64,
        limit: usize,
    ) -> Result<HistoryPage, String> {
        if limit == 0 || limit > 1000 {
            return Err("history page limit must be 1..1000".into());
        }
        let mut checkpoint = self.checkpoint(eid, sid)?;
        let _guard = RootMutation::lock(&self.inner.root)?;
        if cursor > checkpoint.durable_cursor {
            return Err("history cursor is ahead of durable checkpoint".into());
        }
        let path = self.session_path(eid, sid)?;
        checkpoint.retained_from = checkpoint
            .retained_from
            .max(earliest_cursor(&path, checkpoint.retained_from)?);
        let mut events = Vec::new();
        let mut corrupt = false;
        use std::io::BufRead;
        'logs: for log in log_paths(&path)? {
            for line in std::io::BufReader::new(File::open(log).map_err(error)?).lines() {
                let line = line.map_err(error)?;
                let Ok(event) = serde_json::from_str::<HistoryEvent>(&line) else {
                    corrupt = true;
                    continue;
                };
                if event.cursor >= cursor && event.cursor < checkpoint.durable_cursor {
                    events.push(event);
                    if events.len() == limit {
                        break 'logs;
                    }
                }
            }
        }
        let has_gap = events.iter().any(|event| event.kind == "gap");
        let next_cursor = events.last().map(|event| event.cursor + 1).unwrap_or(
            cursor
                .max(checkpoint.retained_from)
                .min(checkpoint.durable_cursor),
        );
        Ok(HistoryPage {
            events,
            next_cursor,
            retained_from: checkpoint.retained_from,
            gap: cursor < checkpoint.retained_from || has_gap || corrupt || checkpoint.unknown_tail,
            checkpoint,
        })
    }
    pub fn shutdown_flush(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let pending = {
                let sinks = self.inner.sinks.lock().map_err(error)?;
                self.inner.shutting_down.store(true, Ordering::Release);
                sinks
                    .values()
                    .filter(|sink| !sink.target.metrics.closed.load(Ordering::Acquire))
                    .count()
            };
            if pending == 0 {
                return self.flush(deadline.saturating_duration_since(Instant::now()));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "history shutdown pending: {pending} native readers have not reached EOF"
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    pub fn flush(&self, timeout: Duration) -> Result<(), String> {
        barrier(&self.inner.sender, timeout)?;
        if let Some(e) = self.inner.error.lock().map_err(error)?.clone() {
            Err(e)
        } else {
            Ok(())
        }
    }
}
impl HistorySink {
    pub fn output(&self, bytes: &[u8]) -> bool {
        let mut accepted = true;
        for chunk in bytes.chunks(8192) {
            self.target
                .metrics
                .pending_outputs
                .fetch_add(1, Ordering::AcqRel);
            let start = self
                .target
                .metrics
                .produced
                .fetch_add(chunk.len() as u64, Ordering::AcqRel);
            if self
                .sender
                .try_send(Command::Output(self.target.clone(), start, chunk.to_vec()))
                .is_err()
            {
                self.target
                    .metrics
                    .pending_outputs
                    .fetch_sub(1, Ordering::AcqRel);
                self.target
                    .metrics
                    .lost_from
                    .fetch_min(start, Ordering::AcqRel);
                self.target
                    .metrics
                    .lost_to
                    .fetch_max(start + chunk.len() as u64, Ordering::AcqRel);
                accepted = false;
            }
        }
        accepted
    }
    pub fn status(&self, view: &SessionView) {
        if let Ok(mut status) = self.target.metrics.status.lock() {
            *status = Some(view.clone());
        }
    }
    pub fn close_output(&self) {
        self.target.metrics.closed.store(true, Ordering::Release);
    }
    pub fn flush(&self, timeout: Duration) -> Result<Checkpoint, String> {
        barrier(&self.sender, timeout)?;
        if let Some(e) = self.error.lock().map_err(error)?.clone() {
            return Err(e);
        }
        read_json(&self.target.path.join("checkpoint.json"))
    }
}
struct Disk {
    target: Target,
    checkpoint: Checkpoint,
    next: u64,
    file: File,
    segment_number: u64,
    dirty: bool,
    append_error: Option<String>,
}
impl Disk {
    fn open(target: Target) -> Result<Self, String> {
        let checkpoint: Checkpoint = read_json(&target.path.join("checkpoint.json"))?;
        let file = OpenOptions::new()
            .append(true)
            .open(target.path.join("events.jsonl"))
            .map_err(error)?;
        let next = checkpoint.durable_cursor;
        Ok(Self {
            target,
            checkpoint,
            next,
            file,
            segment_number: 0,
            dirty: false,
            append_error: None,
        })
    }
    fn ensure_current(&self) -> Result<bool, String> {
        ensure_root_current(&self.target.safe_root, &self.target.writer_instance)?;
        let bytes = match std::fs::read(self.target.path.join("checkpoint.json")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(error(e)),
        };
        let cp: Checkpoint = serde_json::from_slice(&bytes).map_err(error)?;
        if cp.writer_instance != self.target.writer_instance
            || cp.session_id != self.checkpoint.session_id
            || cp.endpoint_id != self.checkpoint.endpoint_id
        {
            return Err("history checkpoint writer or session binding mismatch".into());
        }
        Ok(true)
    }
    fn append(&mut self, event: HistoryEvent) -> Result<(), String> {
        if let Some(e) = &self.append_error {
            return Err(format!("history append failed; repair storage and restart runtime for journal recovery: {e}"));
        }
        let result = self.append_inner(event);
        if let Err(e) = &result {
            self.append_error = Some(e.clone());
        }
        result
    }
    fn append_inner(&mut self, event: HistoryEvent) -> Result<(), String> {
        let mut line = serde_json::to_vec(&event).map_err(error)?;
        line.push(b'\n');
        if line.len() > self.target.retention.segment_bytes {
            return Err("history event exceeds segment byte budget".into());
        }
        let current = self.file.metadata().map_err(error)?.len() as usize;
        if current > 0 && current.saturating_add(line.len()) > self.target.retention.segment_bytes {
            self.file.sync_all().map_err(error)?;
            self.segment_number += 1;
            std::fs::rename(
                self.target.path.join("events.jsonl"),
                self.target
                    .path
                    .join(format!("events-{:012}.jsonl", self.segment_number)),
            )
            .map_err(error)?;
            self.file = OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(self.target.path.join("events.jsonl"))
                .map_err(error)?;
            let mut logs = log_paths(&self.target.path)?;
            while logs.len() > self.target.retention.segments_per_session {
                remove_segment(&logs.remove(0), &self.target.path, &self.target.safe_root)?;
            }
            self.checkpoint.retained_from = earliest_cursor(&self.target.path, self.next)?;
        }
        self.file.write_all(&line).map_err(error)?;
        self.next += 1;
        self.dirty = true;
        Ok(())
    }
    fn gap_to(&mut self, end: u64) -> Result<(), String> {
        if end > self.checkpoint.raw_cursor {
            let event = HistoryEvent {
                cursor: self.next,
                at: now(),
                kind: "gap".into(),
                raw_start: self.checkpoint.raw_cursor,
                raw_end: end,
                bytes: vec![],
                detail: json!({"reason":"writer_backpressure","source":"nonblocking_native_reader"}),
            };
            self.append(event)?;
            self.checkpoint.raw_cursor = end;
        }
        Ok(())
    }
    fn output(&mut self, start: u64, bytes: Vec<u8>) -> Result<(), String> {
        let _guard = RootMutation::lock(&self.target.safe_root)?;
        self.ensure_current()?;
        self.gap_to(start)?;
        let chunk_size = ((self.target.retention.segment_bytes - 512) / 4).clamp(1, 8192);
        let mut raw = start;
        for chunk in bytes.chunks(chunk_size) {
            let event = HistoryEvent {
                cursor: self.next,
                at: now(),
                kind: "output".into(),
                raw_start: raw,
                raw_end: raw + chunk.len() as u64,
                bytes: chunk.to_vec(),
                detail: json!({}),
            };
            self.append(event)?;
            raw += chunk.len() as u64;
            self.checkpoint.raw_cursor = self.checkpoint.raw_cursor.max(raw);
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), String> {
        let _guard = RootMutation::lock(&self.target.safe_root)?;
        self.flush_locked()
    }
    fn flush_locked(&mut self) -> Result<(), String> {
        if let Some(e) = &self.append_error {
            return Err(format!("history append failed; repair storage and restart runtime for journal recovery: {e}"));
        }
        if !self.ensure_current()? {
            self.dirty = true;
        }
        self.gap_to(self.target.metrics.lost_to.load(Ordering::Acquire))?;
        let closed = self.target.metrics.closed.load(Ordering::Acquire);
        let view = self.target.metrics.status.lock().map_err(error)?.clone();
        let desired = if let Some(view) = &view {
            if closed && view.status == "live" {
                "output_closed".to_string()
            } else {
                view.status.clone()
            }
        } else if closed {
            "output_closed".into()
        } else {
            self.checkpoint.status.clone()
        };
        let generation = view
            .as_ref()
            .map(|v| v.generation)
            .unwrap_or(self.checkpoint.generation);
        let unknown =
            self.target.metrics.produced.load(Ordering::Acquire) > self.checkpoint.raw_cursor;
        if desired != self.checkpoint.status || generation != self.checkpoint.generation {
            let event = HistoryEvent {
                cursor: self.next,
                at: now(),
                kind: "session".into(),
                raw_start: self.checkpoint.raw_cursor,
                raw_end: self.checkpoint.raw_cursor,
                bytes: vec![],
                detail: json!({"status":desired,"generation":generation}),
            };
            self.append(event)?;
            self.checkpoint.status = desired;
            self.checkpoint.generation = generation;
        }
        if unknown != self.checkpoint.unknown_tail {
            self.checkpoint.unknown_tail = unknown;
            self.dirty = true;
        }
        if !self.dirty {
            return Ok(());
        }
        self.file.sync_all().map_err(error)?;
        self.checkpoint.durable_cursor = self.next;
        self.checkpoint.updated_at = now();
        atomic_json(&self.target.path.join("checkpoint.json"), &self.checkpoint)?;
        self.dirty = false;
        Ok(())
    }
}
fn register_session(
    registration: Registration,
    disks: &mut HashMap<PathBuf, Disk>,
) -> Result<Target, String> {
    let _guard = RootMutation::lock(&registration.root)?;
    ensure_root_current(&registration.root, &registration.instance)?;
    let endpoint_id = registration.endpoint_id.as_str();
    let view = &registration.view;
    let endpoint = registration.root.join(endpoint_id);
    let sessions_dir = endpoint.join("sessions");
    std::fs::create_dir_all(&endpoint).map_err(error)?;
    let resolved_endpoint = std::fs::canonicalize(&endpoint).map_err(error)?;
    if resolved_endpoint.parent() != Some(registration.root.as_path()) {
        return Err("endpoint history resolves outside state directory".into());
    }
    std::fs::create_dir_all(&sessions_dir).map_err(error)?;
    let resolved_sessions = std::fs::canonicalize(&sessions_dir).map_err(error)?;
    if resolved_endpoint.parent() != Some(registration.root.as_path())
        || resolved_sessions.parent() != Some(resolved_endpoint.as_path())
    {
        return Err("session directory resolves outside connection history".into());
    }
    std::fs::create_dir_all(endpoint.join("artifacts")).map_err(error)?;
    let manifest_path = endpoint.join("manifest.json");
    let mut manifest = if manifest_path.exists() {
        read_json::<Manifest>(&manifest_path)?
    } else {
        Manifest {
            version: 1,
            endpoint_id: endpoint_id.into(),
            sessions: vec![],
        }
    };
    if manifest.endpoint_id != endpoint_id {
        return Err("history manifest endpoint mismatch".into());
    }
    if manifest.sessions.iter().any(|s| s.id == view.id) {
        return Err("recorded session cannot be silently reused".into());
    }
    while manifest.sessions.len() >= registration.retention.max_sessions_per_endpoint {
        let candidate = manifest
            .sessions
            .iter()
            .position(|session| {
                let path = sessions_dir.join(&session.id);
                if disks.get(&path).is_some_and(|disk| {
                    !disk.target.metrics.closed.load(Ordering::Acquire)
                        || disk.target.metrics.pending_outputs.load(Ordering::Acquire) != 0
                }) {
                    return false;
                }
                read_json::<Checkpoint>(&sessions_dir.join(&session.id).join("checkpoint.json"))
                    .is_ok_and(|cp| {
                        ![
                            "recording",
                            "live",
                            "root_exited_children_live",
                            "stop_pending",
                        ]
                        .contains(&cp.status.as_str())
                    })
            })
            .ok_or(
                "history capacity is occupied by active sessions; stop and flush before admission",
            )?;
        let old_id = manifest.sessions[candidate].id.clone();
        valid_id(&old_id)?;
        let old_path = sessions_dir.join(&old_id);
        if let Some(disk) = disks.get_mut(&old_path) {
            if !disk.target.metrics.closed.load(Ordering::Acquire)
                || disk.target.metrics.pending_outputs.load(Ordering::Acquire) != 0
            {
                return Err("history capacity is occupied by a reader before EOF".into());
            }
            disk.flush_locked()?;
        }
        // EOF and zero pending output prove that even chunks enqueued AFTER
        // this registration command have drained before retirement. Drop the
        // file handle before Windows directory removal.
        disks.remove(&old_path);
        manifest.sessions.remove(candidate);
        atomic_json(&manifest_path, &manifest)?;
        remove_session(
            &sessions_dir,
            &old_id,
            &registration.root,
            &resolved_sessions,
        )?;
    }
    let path = sessions_dir.join(&view.id);
    std::fs::create_dir_all(&path).map_err(error)?;
    if std::fs::canonicalize(&path).map_err(error)?.parent() != Some(resolved_sessions.as_path()) {
        return Err("session history resolves outside known session directory".into());
    }
    let checkpoint = Checkpoint {
        endpoint_id: endpoint_id.into(),
        session_id: view.id.clone(),
        instance_id: view.instance_id.clone(),
        generation: view.generation,
        status: "recording".into(),
        durable_cursor: 0,
        retained_from: 0,
        raw_cursor: 0,
        unknown_tail: true,
        process_resume: "unsupported".into(),
        updated_at: now(),
        writer_instance: registration.instance.clone(),
    };
    atomic_json(&path.join("checkpoint.json"), &checkpoint)?;
    File::create(path.join("events.jsonl"))
        .map_err(error)?
        .sync_all()
        .map_err(error)?;
    manifest.sessions.push(view.clone());
    atomic_json(&manifest_path, &manifest)?;
    let target = Target {
        path,
        metrics: Arc::new(Metrics {
            produced: AtomicU64::new(0),
            pending_outputs: AtomicU64::new(0),
            lost_from: AtomicU64::new(u64::MAX),
            lost_to: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            status: Mutex::new(None),
        }),
        writer_instance: registration.instance.clone(),
        retention: registration.retention.clone(),
        safe_root: registration.root.clone(),
    };
    let disk = Disk::open(target.clone()).map_err(|e| {
        format!("history registration failed; repair storage and restart runtime for recovery: {e}")
    })?;
    disks.insert(target.path.clone(), disk);
    Ok(target)
}
fn writer(rx: mpsc::Receiver<Command>, shared_error: Arc<Mutex<Option<String>>>) {
    let mut sessions = HashMap::<PathBuf, Disk>::new();
    let mut registration_error = None;
    loop {
        let command = rx.recv_timeout(Duration::from_millis(100));
        let result = match command {
            Ok(Command::Register(registration, reply)) => {
                let result = register_session(registration, &mut sessions);
                if let Err(e) = &result {
                    if e.starts_with("history registration failed;") {
                        registration_error = Some(e.clone());
                    }
                }
                let _ = reply.send(result.clone());
                // Admission rejection is returned to its caller; it is not a
                // persistent error of previously registered journals.
                Ok(())
            }
            Ok(Command::Output(target, start, bytes)) => {
                let end = start + bytes.len() as u64;
                let result = sessions
                    .get_mut(&target.path)
                    .ok_or_else(|| "history session registration missing".to_string())
                    .and_then(|disk| disk.output(start, bytes));
                target
                    .metrics
                    .pending_outputs
                    .fetch_sub(1, Ordering::AcqRel);
                if result.is_err() {
                    // A checkpoint access fault may reject a complete queued chunk.
                    // Recovery must disclose those missing raw bytes, never certify them.
                    target.metrics.lost_from.fetch_min(start, Ordering::AcqRel);
                    target.metrics.lost_to.fetch_max(end, Ordering::AcqRel);
                }
                result
            }
            Ok(Command::Barrier(reply)) => {
                let result = flush_disks(&mut sessions)
                    .and_then(|_| registration_error.clone().map_or(Ok(()), Err));
                // Only a successful barrier over every registered journal may clear
                // transient checkpoint/fsync errors. Ordinary output success cannot.
                if result.is_ok() {
                    if let Ok(mut saved) = shared_error.lock() {
                        *saved = None;
                    }
                }
                let _ = reply.send(result.clone());
                result
            }
            Err(mpsc::RecvTimeoutError::Timeout) => flush_disks(&mut sessions)
                .and_then(|_| registration_error.clone().map_or(Ok(()), Err)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = flush_disks(&mut sessions);
                return;
            }
        };
        if let Err(e) = result {
            if let Ok(mut saved) = shared_error.lock() {
                *saved = Some(e);
            }
        }
    }
}
fn flush_disks(sessions: &mut HashMap<PathBuf, Disk>) -> Result<(), String> {
    for disk in sessions.values_mut() {
        disk.flush()?;
    }
    Ok(())
}
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::runtime_terminal::TerminalManager;
    use std::time::Instant;
    #[test]
    fn native_output_is_recorded_without_any_client_observe() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let terminal = TerminalManager::new();
        let session = terminal
            .create_recorded(
                "endpoint-one",
                &history,
                "cmd.exe".into(),
                vec!["/D".into(), "/C".into(), "set /a 7*9".into()],
                None,
            )
            .unwrap();
        let until = Instant::now() + Duration::from_secs(8);
        let page = loop {
            history.flush(Duration::from_secs(1)).unwrap();
            let page = history.read("endpoint-one", &session.id, 0, 100).unwrap();
            let bytes: Vec<u8> = page.events.iter().flat_map(|e| e.bytes.clone()).collect();
            if String::from_utf8_lossy(&bytes).contains("63") {
                break page;
            }
            assert!(
                Instant::now() < until,
                "native output must be recorded while clients are detached"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(page.checkpoint.durable_cursor > 0);
        assert_eq!(page.checkpoint.process_resume, "unsupported");
        assert!(dir
            .path()
            .join("connections/endpoint-one/manifest.json")
            .is_file());
        assert!(dir
            .path()
            .join(format!(
                "connections/endpoint-one/sessions/{}/events.jsonl",
                session.id
            ))
            .is_file());
    }

    #[test]
    fn reopen_preserves_raw_output_and_marks_old_runtime_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let terminal = TerminalManager::new();
        let view = terminal
            .create_recorded("recover", &history, "cmd.exe".into(), vec![], None)
            .unwrap();
        let lease = terminal.acquire(&view.id, "human").unwrap();
        terminal
            .write(
                &view.id,
                "human",
                &lease.lease,
                lease.generation,
                b"echo RECOVERY_MARKER_392\r\n",
            )
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            history.flush(Duration::from_secs(1)).unwrap();
            let page = history.read("recover", &view.id, 0, 100).unwrap();
            let bytes: Vec<_> = page
                .events
                .iter()
                .flat_map(|event| event.bytes.clone())
                .collect();
            if String::from_utf8_lossy(&bytes).contains("RECOVERY_MARKER_392") {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(terminal);
        history.flush(Duration::from_secs(1)).unwrap();
        drop(history);
        let reopened = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let listed = reopened.list("recover").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session.id, view.id);
        assert_eq!(listed[0].session.status, "interrupted");
        assert!(listed[0].checkpoint.unknown_tail);
        assert_eq!(listed[0].checkpoint.process_resume, "unsupported");
        let page = reopened.read("recover", &view.id, 0, 100).unwrap();
        assert!(page
            .events
            .iter()
            .any(|event| String::from_utf8_lossy(&event.bytes).contains("RECOVERY_MARKER_392")));
    }

    #[test]
    fn retention_rotates_bounded_readable_segments_and_reports_removed_history_gap() {
        let dir = tempfile::tempdir().unwrap();
        let retention = Retention {
            segment_bytes: 1024,
            segments_per_session: 2,
            max_sessions_per_endpoint: 2,
            queue_capacity: 128,
        };
        let history = HistoryStore::open(dir.path(), retention).unwrap();
        let view = SessionView {
            id: "bounded-session".into(),
            generation: 1,
            status: "live".into(),
            process_id: None,
            instance_id: "runtime-one".into(),
        };
        let sink = history.record_session("bounded-endpoint", &view).unwrap();
        for n in 0..20 {
            assert!(sink.output(format!("event-{n}:{}", "x".repeat(120)).as_bytes()));
            sink.flush(Duration::from_secs(1)).unwrap();
        }
        let page = history.read("bounded-endpoint", &view.id, 0, 1000).unwrap();
        assert!(
            page.gap,
            "retention must explicitly identify removed earlier events"
        );
        assert!(page.retained_from > 0);
        let bytes: Vec<_> = page
            .events
            .iter()
            .flat_map(|event| event.bytes.clone())
            .collect();
        assert!(String::from_utf8_lossy(&bytes).contains("event-19:"));
        let path = dir
            .path()
            .join("connections/bounded-endpoint/sessions/bounded-session");
        let logs = std::fs::read_dir(&path)
            .unwrap()
            .map(Result::unwrap)
            .filter(|e| e.file_name().to_string_lossy().starts_with("events"))
            .collect::<Vec<_>>();
        assert_eq!(logs.len(), 2);
        assert!(logs
            .iter()
            .all(|entry| entry.metadata().unwrap().len() <= 1024));
        assert!(path.join("events.jsonl").is_file());
    }

    #[test]
    fn full_async_writer_queue_reports_gap_without_blocking_native_output() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(
            dir.path(),
            Retention {
                queue_capacity: 1,
                ..Retention::default()
            },
        )
        .unwrap();
        let view = SessionView {
            id: "pressure-session".into(),
            generation: 1,
            status: "live".into(),
            process_id: None,
            instance_id: "runtime-pressure".into(),
        };
        let sink = history.record_session("pressure", &view).unwrap();
        let bytes = vec![b'x'; 4 * 1024 * 1024];
        let before = Instant::now();
        let all_accepted = sink.output(&bytes);
        let elapsed = before.elapsed();
        history.flush(Duration::from_secs(3)).unwrap();
        let page = history.read("pressure", &view.id, 0, 1000).unwrap();
        assert!(
            !all_accepted,
            "bounded single-slot queue must expose the overflow"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "output producer must never wait for disk writer"
        );
        assert!(page.events.iter().any(|event| event.kind == "gap"
            && event.raw_end > event.raw_start
            && event.detail["reason"] == "writer_backpressure"));
        assert!(page.gap);
        assert_eq!(page.checkpoint.raw_cursor, bytes.len() as u64);
        assert!(
            !page.checkpoint.unknown_tail,
            "known missing bytes must be represented by a durable Gap event"
        );
    }

    #[test]
    fn one_endpoint_keeps_separate_native_sessions_and_evicts_only_closed_known_history() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(
            dir.path(),
            Retention {
                max_sessions_per_endpoint: 2,
                ..Retention::default()
            },
        )
        .unwrap();
        let terminal = TerminalManager::new();
        let create = |marker: &str| {
            let view = terminal
                .create_recorded(
                    "shared-endpoint",
                    &history,
                    "cmd.exe".into(),
                    vec!["/D".into(), "/C".into(), format!("echo {marker}")],
                    None,
                )
                .unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                let exited = terminal
                    .list()
                    .iter()
                    .find(|s| s.id == view.id)
                    .unwrap()
                    .status
                    == "exited";
                history.flush(Duration::from_secs(1)).unwrap();
                let page = history.read("shared-endpoint", &view.id, 0, 100).unwrap();
                let bytes: Vec<_> = page
                    .events
                    .iter()
                    .flat_map(|event| event.bytes.clone())
                    .collect();
                if exited && String::from_utf8_lossy(&bytes).contains(marker) {
                    break;
                }
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(10));
            }
            view
        };
        let one = create("HISTORY_SESSION_ONE");
        let two = create("HISTORY_SESSION_TWO");
        assert_ne!(one.id, two.id);
        let second = history.read("shared-endpoint", &two.id, 0, 100).unwrap();
        let second_bytes: Vec<_> = second
            .events
            .iter()
            .flat_map(|event| event.bytes.clone())
            .collect();
        assert!(!String::from_utf8_lossy(&second_bytes).contains("HISTORY_SESSION_ONE"));
        let sentinel = dir.path().join("preserve-outside-sessions.txt");
        std::fs::write(&sentinel, b"keep").unwrap();
        let three = create("HISTORY_SESSION_THREE");
        let listed = history.list("shared-endpoint").unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|s| s.session.id == two.id));
        assert!(listed.iter().any(|s| s.session.id == three.id));
        assert!(history.read("shared-endpoint", &one.id, 0, 100).is_err());
        assert!(!dir
            .path()
            .join(format!("connections/shared-endpoint/sessions/{}", one.id))
            .exists());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"keep");
    }

    #[test]
    fn crash_tail_recovery_keeps_complete_records_and_marks_unavailable_tail() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let view = SessionView {
            id: "tail-session".into(),
            generation: 1,
            status: "live".into(),
            process_id: None,
            instance_id: "old-runtime".into(),
        };
        let sink = history.record_session("tail-endpoint", &view).unwrap();
        assert!(sink.output(b"complete native output"));
        sink.flush(Duration::from_secs(1)).unwrap();
        drop(sink);
        drop(history);
        let session_dir = dir
            .path()
            .join("connections/tail-endpoint/sessions/tail-session");
        let mut file = OpenOptions::new()
            .append(true)
            .open(session_dir.join("events.jsonl"))
            .unwrap();
        file.write_all(b"{\"cursor\":999").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let recovered = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let page = recovered.read("tail-endpoint", &view.id, 0, 100).unwrap();
        assert!(page.checkpoint.unknown_tail);
        assert!(page
            .events
            .iter()
            .any(|event| event.bytes == b"complete native output"));
        let bytes = std::fs::read(session_dir.join("events.jsonl")).unwrap();
        assert!(
            bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .all(|line| serde_json::from_slice::<Value>(line).is_ok()),
            "recovered JSONL must contain only complete readable records"
        );
        assert!(session_dir.join("recovered-tail.bin").is_file());
        assert!(page.gap);
    }

    #[test]
    fn shutdown_flush_stays_pending_until_reader_eof_and_reports_durable_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let view = SessionView {
            id: "flush-session".into(),
            generation: 1,
            status: "live".into(),
            process_id: None,
            instance_id: "flush-runtime".into(),
        };
        let sink = history.record_session("flush-endpoint", &view).unwrap();
        sink.output(b"observed bytes");
        history.flush(Duration::from_secs(1)).unwrap();
        let before = Instant::now();
        assert!(
            history.shutdown_flush(Duration::from_millis(30)).is_err(),
            "shutdown cannot certify an output producer that has not reached EOF"
        );
        assert!(before.elapsed() < Duration::from_secs(1));
        sink.close_output();
        history.shutdown_flush(Duration::from_secs(1)).unwrap();
        let checkpoint = dir
            .path()
            .join("connections/flush-endpoint/sessions/flush-session/checkpoint.json");
        std::fs::remove_file(&checkpoint).unwrap();
        std::fs::create_dir(&checkpoint).unwrap();
        assert!(
            history.shutdown_flush(Duration::from_millis(100)).is_err(),
            "disk checkpoint failure must remain Pending/Failed to shutdown caller"
        );
    }

    #[test]
    fn checkpoint_target_repair_retries_barrier_and_retains_real_durable_bytes() {
        for restore_saved_checkpoint in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let history = HistoryStore::open(dir.path(), Retention::default()).unwrap();
            let view = SessionView {
                id: "repair-session".into(),
                generation: 1,
                status: "live".into(),
                process_id: None,
                instance_id: "repair-runtime".into(),
            };
            let sink = history.record_session("repair-endpoint", &view).unwrap();
            sink.output(b"durable-before-fault");
            sink.flush(Duration::from_secs(1)).unwrap();
            let checkpoint = dir
                .path()
                .join("connections/repair-endpoint/sessions/repair-session/checkpoint.json");
            let saved = std::fs::read(&checkpoint).unwrap();
            std::fs::remove_file(&checkpoint).unwrap();
            std::fs::create_dir(&checkpoint).unwrap();
            sink.output(b"during-fault");
            assert!(sink.flush(Duration::from_secs(1)).is_err());
            sink.close_output();
            assert!(history.shutdown_flush(Duration::from_secs(1)).is_err());
            std::fs::remove_dir(&checkpoint).unwrap();
            if restore_saved_checkpoint {
                std::fs::write(&checkpoint, &saved).unwrap();
            }
            history
                .shutdown_flush(Duration::from_secs(1))
                .expect("repaired checkpoint target must pass a new durable barrier");
            let page = history
                .read("repair-endpoint", "repair-session", 0, 100)
                .unwrap();
            let bytes: Vec<_> = page.events.iter().flat_map(|e| e.bytes.clone()).collect();
            assert_eq!(bytes, b"durable-before-fault");
            assert!(page.events.iter().any(|event| event.kind == "gap"
                && event.raw_end - event.raw_start == b"during-fault".len() as u64));
            assert_eq!(page.checkpoint.status, "output_closed");
            assert!(!page.checkpoint.unknown_tail);
        }
    }

    #[test]
    fn atomic_checkpoint_failure_retries_fsync_and_clears_only_after_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let view = SessionView {
            id: "atomic-session".into(),
            generation: 1,
            status: "live".into(),
            process_id: None,
            instance_id: "atomic-runtime".into(),
        };
        let sink = history.record_session("atomic-endpoint", &view).unwrap();
        sink.output(b"before-atomic-fault");
        sink.flush(Duration::from_secs(1)).unwrap();
        let cp = dir
            .path()
            .join("connections/atomic-endpoint/sessions/atomic-session/checkpoint.json");
        let before: Checkpoint = read_json(&cp).unwrap();
        let tmp = cp.with_file_name("checkpoint.json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        sink.close_output();
        assert!(history.shutdown_flush(Duration::from_secs(1)).is_err());
        let failed: Checkpoint = read_json(&cp).unwrap();
        assert_eq!(
            failed.durable_cursor, before.durable_cursor,
            "failed atomic replace cannot advance persisted checkpoint"
        );
        std::fs::remove_dir(&tmp).unwrap();
        history.shutdown_flush(Duration::from_secs(1)).unwrap();
        let after: Checkpoint = read_json(&cp).unwrap();
        assert!(after.durable_cursor > before.durable_cursor);
        assert_eq!(after.status, "output_closed");
    }

    #[test]
    fn journal_append_failure_stays_explicit_until_restart_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(
            dir.path(),
            Retention {
                segment_bytes: 1024,
                ..Retention::default()
            },
        )
        .unwrap();
        let view = SessionView {
            id: "append-session".into(),
            generation: 1,
            status: "live".into(),
            process_id: None,
            instance_id: "append-runtime".into(),
        };
        let sink = history.record_session("append-endpoint", &view).unwrap();
        sink.output(b"first");
        sink.flush(Duration::from_secs(1)).unwrap();
        let collision = dir
            .path()
            .join("connections/append-endpoint/sessions/append-session/events-000000000001.jsonl");
        std::fs::create_dir(&collision).unwrap();
        sink.output(&vec![b'x'; 2000]);
        sink.close_output();
        assert!(history.shutdown_flush(Duration::from_secs(1)).is_err());
        std::fs::remove_dir(&collision).unwrap();
        let error = history.shutdown_flush(Duration::from_secs(1)).unwrap_err();
        assert!(
            error.contains("restart runtime for journal recovery"),
            "{error}"
        );
    }

    #[test]
    fn missing_checkpoint_repair_cannot_revive_a_fenced_older_writer() {
        let dir = tempfile::tempdir().unwrap();
        let old = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let view = SessionView {
            id: "fenced-session".into(),
            generation: 1,
            status: "live".into(),
            process_id: None,
            instance_id: "older-runtime".into(),
        };
        let sink = old.record_session("fenced-endpoint", &view).unwrap();
        sink.output(b"original");
        sink.flush(Duration::from_secs(1)).unwrap();
        let _new = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let cp = dir
            .path()
            .join("connections/fenced-endpoint/sessions/fenced-session/checkpoint.json");
        std::fs::remove_file(&cp).unwrap();
        sink.close_output();
        let error = old.shutdown_flush(Duration::from_secs(1)).unwrap_err();
        assert!(error.contains("fenced"), "{error}");
        assert!(
            !cp.exists(),
            "older writer must never rebuild checkpoint after new runtime opened"
        );
    }

    #[test]
    fn checkpoint_tracks_native_control_generation_without_persisting_control_secret() {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(dir.path(), Retention::default()).unwrap();
        let terminal = TerminalManager::new();
        let view = terminal
            .create_recorded("generation", &history, "cmd.exe".into(), vec![], None)
            .unwrap();
        let lease = terminal.acquire(&view.id, "controller").unwrap();
        history.flush(Duration::from_secs(1)).unwrap();
        assert_eq!(
            history
                .checkpoint("generation", &view.id)
                .unwrap()
                .generation,
            lease.generation
        );
        terminal
            .release(&view.id, "controller", &lease.lease)
            .unwrap();
        history.flush(Duration::from_secs(1)).unwrap();
        assert!(
            history
                .checkpoint("generation", &view.id)
                .unwrap()
                .generation
                > lease.generation
        );
        let manifest =
            std::fs::read_to_string(dir.path().join("connections/generation/manifest.json"))
                .unwrap();
        assert!(
            !manifest.contains(&lease.lease),
            "history manifest cannot persist capability credentials"
        );
    }
}
