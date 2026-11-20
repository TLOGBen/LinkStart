//! Runtime-owned native terminals. Raw PTY bytes never claim structured provider semantics.
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionView {
    pub id: String,
    pub generation: u64,
    pub status: String,
    pub process_id: Option<u32>,
    pub instance_id: String,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct ControlLease {
    pub lease: String,
    pub generation: u64,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct OutputView {
    pub session_id: String,
    pub next_cursor: u64,
    pub bytes: Vec<u8>,
    pub text: String,
    pub gap: bool,
    pub status: String,
}

struct Session {
    view: SessionView,
    child: Box<dyn Child + Send + Sync>,
    master: Option<Box<dyn MasterPty + Send>>,
    input: Option<mpsc::SyncSender<InputWrite>>,
    control: Option<(String, String, Instant)>,
    output: Arc<Mutex<OutputBuffer>>,
    history: Option<crate::runtime_history::HistorySink>,
    #[cfg(windows)]
    job: owned_job::Job,
    // False once the owned process group was seen empty. Its id may then be
    // reused by an unrelated group, so it is never signalled again.
    #[cfg(not(windows))]
    group_open: bool,
}
struct OutputBuffer {
    bytes: VecDeque<u8>,
    end: u64,
}
struct InputWrite {
    bytes: Vec<u8>,
    reply: Option<mpsc::SyncSender<Result<(), String>>>,
}
struct Inner {
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
    instance: String,
    capacity: usize,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for session in self
            .sessions
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            let mut s = session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            #[cfg(windows)]
            let _ = s.job.terminate();
            #[cfg(not(windows))]
            s.kill_group_if_open();
            let _ = s.child.kill();
        }
    }
}
#[derive(Clone)]
pub struct TerminalManager {
    inner: Arc<Inner>,
}
impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}
impl TerminalManager {
    pub fn renew(
        &self,
        id: &str,
        identity: &str,
        lease: &str,
        generation: u64,
    ) -> Result<ControlLease, String> {
        let session = self.session(id)?;
        let mut s = session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.refresh();
        s.check(identity, lease, generation)?;
        s.control = Some((
            identity.into(),
            lease.into(),
            Instant::now() + Duration::from_secs(60),
        ));
        Ok(ControlLease {
            lease: lease.into(),
            generation,
        })
    }
    pub fn stop_owned(&self, id: &str, deadline: Duration) -> Result<SessionView, String> {
        self.stop(id, deadline, false)
    }
    pub fn force_owned(&self, id: &str, deadline: Duration) -> Result<SessionView, String> {
        self.stop(id, deadline, true)
    }
    fn stop(&self, id: &str, deadline: Duration, force: bool) -> Result<SessionView, String> {
        let session = self.session(id)?;
        {
            let mut s = session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.refresh();
            s.control = None;
            s.view.generation += 1;
            if let Some(history) = &s.history {
                history.status(&s.view);
            }
            if s.view.status == "exited" {
                return Ok(s.view.clone());
            }
            if force {
                #[cfg(windows)]
                s.job.terminate()?;
                #[cfg(not(windows))]
                s.kill_group_if_open();
            } else {
                let _ = s
                    .input
                    .as_ref()
                    .ok_or("terminal input closed")?
                    .try_send(InputWrite {
                        bytes: vec![3],
                        reply: None,
                    });
            }
        }
        let until = Instant::now() + deadline;
        loop {
            let mut s = session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.refresh();
            if s.view.status == "exited" {
                return Ok(s.view.clone());
            }
            if Instant::now() >= until {
                let mut view = s.view.clone();
                view.status = "stop_pending".into();
                return Ok(view);
            }
            drop(s);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn shutdown(&self, deadline: Duration) -> Vec<SessionView> {
        let until = Instant::now() + deadline;
        self.list()
            .into_iter()
            .filter_map(|s| {
                match self.stop_owned(&s.id, until.saturating_duration_since(Instant::now())) {
                    Ok(v) if v.status == "exited" => None,
                    Ok(v) => Some(v),
                    Err(_) => {
                        let mut v = s;
                        v.status = "stop_failed".into();
                        Some(v)
                    }
                }
            })
            .collect()
    }
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                instance: Uuid::new_v4().to_string(),
                capacity: capacity.max(1),
            }),
        }
    }
    fn session(&self, id: &str) -> Result<Arc<Mutex<Session>>, String> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| "terminal not found".into())
    }
    pub fn list(&self) -> Vec<SessionView> {
        let sessions: Vec<_> = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        sessions
            .iter()
            .map(|s| {
                let mut s = s.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                s.refresh();
                s.view.clone()
            })
            .collect()
    }
    pub fn acquire(&self, id: &str, identity: &str) -> Result<ControlLease, String> {
        let session = self.session(id)?;
        let mut s = session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.refresh();
        if s.view.status != "live" {
            return Err("terminal exited".into());
        }
        if s.control
            .as_ref()
            .is_some_and(|(_, _, expiry)| *expiry > Instant::now())
        {
            return Err("control lease already held".into());
        }
        s.view.generation += 1;
        if let Some(history) = &s.history {
            history.status(&s.view);
        }
        let lease = Uuid::new_v4().to_string();
        s.control = Some((
            identity.into(),
            lease.clone(),
            Instant::now() + Duration::from_secs(60),
        ));
        Ok(ControlLease {
            lease,
            generation: s.view.generation,
        })
    }
    /// Identity currently holding an unexpired control lease, if any.
    pub fn holder(&self, id: &str) -> Option<String> {
        let session = self.session(id).ok()?;
        let s = session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.control
            .as_ref()
            .filter(|(_, _, expiry)| *expiry > Instant::now())
            .map(|(identity, _, _)| identity.clone())
    }
    /// Ends `identity`'s lease after its authority was withdrawn; bumping the
    /// generation fences any write already carrying the old lease.
    pub fn revoke_control(&self, id: &str, identity: &str) {
        let Ok(session) = self.session(id) else {
            return;
        };
        let mut s = session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.control.as_ref().is_some_and(|(i, _, _)| i == identity) {
            s.control = None;
            s.view.generation += 1;
            if let Some(history) = &s.history {
                history.status(&s.view);
            }
        }
    }
    pub fn release(&self, id: &str, identity: &str, lease: &str) -> Result<(), String> {
        let session = self.session(id)?;
        let mut s = session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.check(identity, lease, s.view.generation)?;
        s.control = None;
        s.view.generation += 1;
        if let Some(history) = &s.history {
            history.status(&s.view);
        }
        Ok(())
    }
    pub fn write(
        &self,
        id: &str,
        identity: &str,
        lease: &str,
        generation: u64,
        input: &[u8],
    ) -> Result<(), String> {
        if input.len() > 16384 {
            return Err("raw input exceeds 16384 byte limit; not delivered".into());
        }
        let session = self.session(id)?;
        let mut s = session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.refresh();
        s.check(identity, lease, generation)?;
        // A partial write is Unknown to caller; never retry it here.
        let (reply, completion) = mpsc::sync_channel(1);
        s.input
            .as_ref()
            .ok_or("terminal input closed")?
            .try_send(InputWrite {
                bytes: input.to_vec(),
                reply: Some(reply),
            })
            .map_err(|_| "terminal input backpressure; not delivered".to_string())?;
        completion.recv_timeout(Duration::from_millis(250)).map_err(|_|"input delivery unknown: native writer did not acknowledge within 250ms; do not retry".to_string())?
    }
    pub fn resize(
        &self,
        id: &str,
        identity: &str,
        lease: &str,
        generation: u64,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        if cols == 0 || rows == 0 {
            return Err("terminal size must be positive".into());
        }
        let session = self.session(id)?;
        let mut s = session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.refresh();
        s.check(identity, lease, generation)?;
        s.master
            .as_ref()
            .ok_or("terminal PTY closed")?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())
    }
    pub fn observe(&self, id: &str, cursor: u64) -> Result<OutputView, String> {
        let session = self.session(id)?;
        let (output, status) = {
            let mut s = session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.refresh();
            (s.output.clone(), s.view.status.clone())
        };
        let b = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = b.end - b.bytes.len() as u64;
        if cursor > b.end {
            return Err("output cursor is ahead of session".into());
        }
        let bytes: Vec<u8> = b
            .bytes
            .iter()
            .skip(cursor.saturating_sub(start) as usize)
            .copied()
            .collect();
        Ok(OutputView {
            session_id: id.into(),
            next_cursor: b.end,
            text: String::from_utf8_lossy(&bytes).into_owned(),
            bytes,
            gap: cursor < start,
            status,
        })
    }
    pub fn new() -> Self {
        Self::with_capacity(65536)
    }
    pub fn create_recorded(
        &self,
        endpoint_id: &str,
        history: &crate::runtime_history::HistoryStore,
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    ) -> Result<SessionView, String> {
        self.create_inner(command, args, cwd, Some((endpoint_id, history)))
    }
    pub fn create(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    ) -> Result<SessionView, String> {
        self.create_inner(command, args, cwd, None)
    }
    fn create_inner(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        recording: Option<(&str, &crate::runtime_history::HistoryStore)>,
    ) -> Result<SessionView, String> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        #[cfg(windows)]
        let gate = Uuid::new_v4().to_string();
        #[cfg(windows)]
        let mut cmd = {
            fn quote(s: &str) -> String {
                format!("'{}'", s.replace('\'', "''"))
            }
            let script=format!("$g=[Console]::ReadLine(); if($g -cne '{}'){{exit 125}}; & {} {}; exit $LASTEXITCODE",gate,quote(&command),args.iter().map(|a|quote(a)).collect::<Vec<_>>().join(" "));
            let mut c = CommandBuilder::new("powershell.exe");
            c.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = CommandBuilder::new(command);
            c.args(args);
            c
        };
        for key in inherited_host_agent_markers() {
            cmd.env_remove(key);
        }
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        // Children inherit "ignore Ctrl-C" from whoever launched the daemon (IDEs,
        // agents, services). Restore normal processing so owned terminals can be
        // interrupted, including the graceful stop's Ctrl-C.
        #[cfg(windows)]
        owned_job::restore_ctrl_c_inheritance();
        let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        // No caller-specified executable runs until its trusted launcher belongs
        // to the job; every later descendant inherits that job atomically.
        #[cfg(windows)]
        let job = match owned_job::Job::assign(
            child.process_id().ok_or("native child lacks process id")?,
        ) {
            Ok(job) => job,
            Err(e) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = Arc::new(Mutex::new(
            pair.master.take_writer().map_err(|e| e.to_string())?,
        ));
        let (input, input_rx) = mpsc::sync_channel::<InputWrite>(8);
        let view = SessionView {
            id: Uuid::new_v4().to_string(),
            generation: 1,
            status: "live".into(),
            process_id: child.process_id(),
            instance_id: self.inner.instance.clone(),
        };
        let history = if let Some((eid, store)) = recording {
            match store.record_session(eid, &view) {
                Ok(sink) => Some(sink),
                Err(e) => {
                    let _ = child.kill();
                    return Err(e);
                }
            }
        } else {
            None
        };
        let output = Arc::new(Mutex::new(OutputBuffer {
            bytes: VecDeque::new(),
            end: 0,
        }));
        let session = Arc::new(Mutex::new(Session {
            view: view.clone(),
            child,
            master: Some(pair.master),
            input: Some(input),
            control: None,
            output: output.clone(),
            history: history.clone(),
            #[cfg(windows)]
            job,
            // portable-pty makes each Unix child a setsid session and group leader,
            // so the group id equals the child pid.
            #[cfg(not(windows))]
            group_open: true,
        }));
        // Continuously drain the native pipe: clients never own the reader.
        let capacity = self.inner.capacity;
        let reader_writer = writer.clone();
        #[cfg(windows)]
        let (startup_ready, startup_wait) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            let mut startup_query = VecDeque::new();
            let mut cursor_answered = !cfg!(windows);
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                // ConPTY's inherited-cursor startup handshake is independent of a browser.
                // The newly allocated PTY starts at row 1, column 1; later application
                // queries remain raw output for the attached terminal emulator.
                if !cursor_answered {
                    for byte in &buf[..n] {
                        startup_query.push_back(*byte);
                        if startup_query.len() > 4 {
                            startup_query.pop_front();
                        }
                        if startup_query.iter().copied().eq(b"\x1b[6n".iter().copied()) {
                            let _ = reader_writer
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .write_all(b"\x1b[1;1R");
                            cursor_answered = true;
                            #[cfg(windows)]
                            let _ = startup_ready.send(());
                            break;
                        }
                    }
                }
                if let Some(sink) = &history {
                    sink.output(&buf[..n]);
                }
                let mut b = output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                b.end += n as u64;
                b.bytes.extend(&buf[..n]);
                let excess = b.bytes.len().saturating_sub(capacity);
                b.bytes.drain(..excess);
            }
            if let Some(sink) = &history {
                sink.close_output();
            }
        });
        #[cfg(windows)]
        startup_wait
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "ConPTY startup cursor handshake timed out".to_string())?;
        #[cfg(windows)]
        writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_all(format!("{gate}\r\n").as_bytes())
            .map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            while let Ok(request) = input_rx.recv() {
                let mut writer = writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let result = writer
                    .write_all(&request.bytes)
                    .and_then(|_| writer.flush())
                    .map_err(|e| format!("input delivery unknown: {e}"));
                if let Some(reply) = request.reply {
                    let _ = reply.send(result);
                }
            }
        });
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(view.id.clone(), session);
        Ok(view)
    }
}

#[cfg(windows)]
pub(crate) mod owned_job {
    use std::{ffi::c_void, io, mem, ptr};
    type Handle = *mut c_void;
    #[repr(C)]
    struct BasicLimits {
        process_time: i64,
        job_time: i64,
        flags: u32,
        min_working: usize,
        max_working: usize,
        active: u32,
        affinity: usize,
        priority: u32,
        scheduling: u32,
    }
    #[repr(C)]
    struct Limits {
        basic: BasicLimits,
        io: [u64; 6],
        process_memory: usize,
        job_memory: usize,
        peak_process: usize,
        peak_job: usize,
    }
    #[repr(C)]
    struct Accounting {
        times: [i64; 4],
        faults: u32,
        total: u32,
        active: u32,
        terminated: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: u32, data: *const c_void, len: u32) -> i32;
        fn QueryInformationJobObject(
            job: Handle,
            class: u32,
            data: *mut c_void,
            len: u32,
            result: *mut u32,
        ) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, code: u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn SetConsoleCtrlHandler(handler: *const c_void, add: i32) -> i32;
    }
    // The daemon never stops on a console Ctrl-C/Break (a child sharing its hidden
    // console can raise one); it stops through its own shutdown. Handlers are not
    // inherited, so children keep default processing.
    unsafe extern "system" fn swallow_ctrl_c(event: u32) -> i32 {
        (event <= 1) as i32
    }
    pub fn restore_ctrl_c_inheritance() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            SetConsoleCtrlHandler(ptr::null(), 0);
            SetConsoleCtrlHandler(swallow_ctrl_c as *const c_void, 1);
        });
    }
    pub struct Job(usize);
    impl Job {
        pub fn assign(pid: u32) -> Result<Self, String> {
            unsafe {
                let handle = CreateJobObjectW(ptr::null_mut(), ptr::null());
                if handle.is_null() {
                    return Err(io::Error::last_os_error().to_string());
                }
                let job = Self(handle as usize);
                let mut limits: Limits = mem::zeroed();
                limits.basic.flags = 0x2000; // KILL_ON_JOB_CLOSE; no breakaway.
                if SetInformationJobObject(
                    handle,
                    9,
                    &limits as *const _ as _,
                    mem::size_of::<Limits>() as u32,
                ) == 0
                {
                    return Err(io::Error::last_os_error().to_string());
                }
                let process = OpenProcess(0x0100 | 0x0001, 0, pid); // SET_QUOTA | TERMINATE, only just-created owned root.
                if process.is_null() {
                    return Err(io::Error::last_os_error().to_string());
                }
                let result = AssignProcessToJobObject(handle, process);
                let error = io::Error::last_os_error();
                CloseHandle(process);
                if result == 0 {
                    return Err(format!(
                        "cannot contain owned terminal process tree: {error}"
                    ));
                }
                Ok(job)
            }
        }
        pub fn terminate(&self) -> Result<(), String> {
            unsafe {
                if TerminateJobObject(self.0 as Handle, 1) == 0 {
                    Err(io::Error::last_os_error().to_string())
                } else {
                    Ok(())
                }
            }
        }
        pub fn active(&self) -> Result<bool, String> {
            unsafe {
                let mut a: Accounting = mem::zeroed();
                if QueryInformationJobObject(
                    self.0 as Handle,
                    1,
                    &mut a as *mut _ as _,
                    mem::size_of::<Accounting>() as u32,
                    ptr::null_mut(),
                ) == 0
                {
                    Err(io::Error::last_os_error().to_string())
                } else {
                    Ok(a.active != 0)
                }
            }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0 as Handle);
            }
        }
    }
}
impl Session {
    // While any member remains, the kernel keeps the group id reserved, so a
    // signal to it can only reach owned processes.
    #[cfg(not(windows))]
    fn group_live(&mut self) -> bool {
        if self.group_open {
            let pid = self.view.process_id.unwrap_or(0) as libc::pid_t;
            self.group_open = pid > 0 && unsafe { libc::killpg(pid, 0) } == 0;
        }
        self.group_open
    }
    #[cfg(not(windows))]
    fn kill_group_if_open(&mut self) {
        if self.group_live() {
            let pid = self.view.process_id.unwrap_or(0) as libc::pid_t;
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }
    fn refresh(&mut self) {
        if let Ok(Some(_)) = self.child.try_wait() {
            #[cfg(windows)]
            let children_live = self.job.active().unwrap_or(true);
            #[cfg(not(windows))]
            let children_live = self.group_live();
            if children_live {
                self.view.status = "root_exited_children_live".into();
                self.control = None;
                if let Some(history) = &self.history {
                    history.status(&self.view);
                }
                return;
            }
            self.view.status = "exited".into();
            self.control = None;
            // Only actual owned process-tree exit permits closing ConPTY.
            // Retaining the master after exit keeps the independent reader from EOF,
            // preventing the durable shutdown barrier from ever completing.
            self.input.take();
            self.master.take();
            if let Some(history) = &self.history {
                history.status(&self.view);
            }
        }
    }
    fn check(&self, identity: &str, lease: &str, generation: u64) -> Result<(), String> {
        if self.view.status != "live" {
            return Err("terminal exited".into());
        }
        if self.view.generation != generation
            || !self
                .control
                .as_ref()
                .is_some_and(|(i, l, e)| i == identity && l == lease && *e > Instant::now())
        {
            return Err("stale or invalid terminal control lease".into());
        }
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        // Stop owned children before master drop closes ConPTY (which can wait
        // for those children); this also covers failed creation after spawn.
        #[cfg(windows)]
        let _ = self.job.terminate();
        let _ = self.child.kill();
    }
}

/// The daemon's own variables that every child it spawns (owned terminals, the
/// owned Codex host, `gh`) must have removed; see `host_agent_marker`.
pub(crate) fn inherited_host_agent_markers() -> Vec<std::ffi::OsString> {
    let hosted = std::env::var_os("CLAUDECODE").is_some()
        || std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_some();
    std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_str().is_some_and(|k| host_agent_marker(k, hosted)))
        .collect()
}

/// Variables an agent host injects to mark (and authenticate) its own session.
/// When the daemon is started from inside such a session, an owned CLI must not
/// inherit them: it would believe it is that session's child, and host tokens
/// would reach every program a member launches. User configuration (for example
/// API endpoints, provider selection, client certificates, `SKIP_*_AUTH`) is
/// left alone, so this is an explicit list, not a prefix rule.
///
/// `CLAUDE_CODE_OAUTH_TOKEN` is ambiguous: a host injects it, but it is also how
/// a user authenticates a headless Claude Code. It is removed only when `hosted`
/// (the daemon itself runs inside an agent host session).
fn host_agent_marker(key: &str, hosted: bool) -> bool {
    const EXACT: &[&str] = &[
        "CLAUDECODE",
        "CLAUDE_PID",
        "CLAUDE_EFFORT",
        "AI_AGENT",
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_SSE_PORT",
        "CLAUDE_CODE_ACCOUNT_UUID",
        "CLAUDE_CODE_ORGANIZATION_UUID",
        "CLAUDE_CODE_USER_EMAIL",
        "CLAUDE_CODE_DESKTOP_APP_VERSION",
        "CLAUDE_CODE_REPORT_FINDINGS",
        "CLAUDE_CODE_TERMINAL_MCP_TOOLS",
        "CLAUDE_CODE_STARTUP_FAILURE_RESULTS",
    ];
    // Session identity, host IPC and host-held auth: never meaningful to a new CLI.
    const PREFIX: &[&str] = &[
        "CLAUDE_AGENT_SDK_",
        "CLAUDE_PREVIEW_",
        "CODEX_SANDBOX",
        "CLAUDE_CODE_SESSION_",
        "CLAUDE_CODE_HOST_",
        "CLAUDE_CODE_MESSAGING_",
        "CLAUDE_CODE_SDK_",
        "CLAUDE_CODE_EMIT_",
    ];
    let key = key.to_ascii_uppercase();
    EXACT.contains(&key.as_str())
        || PREFIX.iter().any(|p| key.starts_with(p))
        || (key == "CLAUDE_CODE_OAUTH_SCOPES")
        || (hosted && key == "CLAUDE_CODE_OAUTH_TOKEN")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_poisoned_session_lock_does_not_cascade() {
        // K16: revocation fences and shutdown walk every session; one session
        // whose lock a panic poisoned must not take them down with it.
        let manager = TerminalManager::new();
        let (command, args) = if cfg!(windows) {
            ("cmd.exe", vec!["/Q".into()])
        } else {
            ("/bin/sh", vec!["-c".into(), "sleep 30".into()])
        };
        let poisoned = manager.create(command.into(), args.clone(), None).unwrap();
        let healthy = manager.create(command.into(), args, None).unwrap();
        let session = manager.session(&poisoned.id).unwrap();
        let _ = std::thread::spawn(move || {
            let _guard = session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            panic!("panic while holding a session lock");
        })
        .join();
        assert!(manager.session(&poisoned.id).unwrap().is_poisoned());
        // What a revocation fence does for every session, the poisoned one first.
        for id in [&poisoned.id, &healthy.id] {
            assert!(manager.holder(id).is_none());
            manager.revoke_control(id, "someone");
            manager.acquire(id, "agent").unwrap();
            assert_eq!(manager.holder(id).as_deref(), Some("agent"));
            manager.revoke_control(id, "agent");
            assert!(manager.holder(id).is_none());
        }
        assert_eq!(
            manager
                .force_owned(&poisoned.id, Duration::from_secs(5))
                .unwrap()
                .status,
            "exited"
        );
        // Shutdown walks every session, the poisoned one included.
        assert!(manager.shutdown(Duration::from_secs(5)).is_empty());
        assert!(manager.list().iter().all(|view| view.status == "exited"));
    }
    #[test]
    fn headless_user_oauth_token_survives_unless_daemon_is_hosted() {
        // A user's own headless login must reach the CLI they launch.
        assert!(!host_agent_marker("CLAUDE_CODE_OAUTH_TOKEN", false));
        assert!(!host_agent_marker("claude_code_oauth_token", false));
        // Inside an agent host the same name carries the host's session token.
        assert!(host_agent_marker("CLAUDE_CODE_OAUTH_TOKEN", true));
        assert!(host_agent_marker("CLAUDE_CODE_OAUTH_SCOPES", false));
        assert!(!host_agent_marker("CLAUDE_CODE_SKIP_BEDROCK_AUTH", true));
        assert!(!host_agent_marker("CLAUDE_CODE_CLIENT_CERT", true));
        // What gh and Codex need from the daemon's environment stays (K13).
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_CONFIG_DIR",
            "HOME",
            "USERPROFILE",
            "PATH",
            "CODEX_HOME",
            "OPENAI_API_KEY",
        ] {
            assert!(!host_agent_marker(key, true), "{key}");
        }
    }
    #[test]
    fn child_does_not_inherit_host_agent_session_markers() {
        // Only names no other test reads; user configuration must survive.
        std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "parent-marker");
        std::env::set_var("CLAUDE_CODE_MESSAGING_TOKEN", "host-secret");
        std::env::set_var("CLAUDE_CODE_SKIP_BEDROCK_AUTH", "1");
        std::env::set_var("LINKSTART_ENV_KEEP_PROBE", "kept");
        let manager = TerminalManager::new();
        let (command, args) = if cfg!(windows) {
            (
                "cmd.exe",
                vec!["/Q".into(), "/C".into(), "set & echo ENV_DONE".into()],
            )
        } else {
            ("/bin/sh", vec!["-c".into(), "env; echo ENV_DONE".into()])
        };
        let s = manager.create(command.into(), args, None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let text = loop {
            let text = manager.observe(&s.id, 0).unwrap().text;
            if text.contains("ENV_DONE") {
                break text;
            }
            assert!(Instant::now() < deadline, "env never printed: {text}");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(text.contains("LINKSTART_ENV_KEEP_PROBE=kept"), "{text}");
        assert!(text.contains("CLAUDE_CODE_SKIP_BEDROCK_AUTH=1"), "{text}");
        assert!(!text.contains("CLAUDE_CODE_CHILD_SESSION"), "{text}");
        assert!(!text.contains("host-secret"), "{text}");
    }
    #[test]
    fn owned_terminal_starts_a_real_child() {
        let manager = TerminalManager::new();
        let session = manager.create(
            if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.into(),
            vec![],
            None,
        );
        assert!(
            session.is_ok(),
            "Runtime must own a native PTY child: {session:?}"
        );
    }
    #[test]
    fn release_rejoin_keeps_child_and_fences_old_writer() {
        let manager = TerminalManager::new();
        let session = manager
            .create(
                if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.into(),
                vec![],
                None,
            )
            .unwrap();
        let first = manager
            .acquire(&session.id, "alice")
            .expect("controller can acquire");
        assert!(manager.acquire(&session.id, "bob").is_err());
        manager.release(&session.id, "alice", &first.lease).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let second = manager.acquire(&session.id, "bob").unwrap();
        assert!(second.generation > first.generation);
        assert_eq!(manager.list()[0].process_id, session.process_id);
        assert!(manager
            .write(
                &session.id,
                "alice",
                &first.lease,
                first.generation,
                b"echo BAD\r\n"
            )
            .is_err());
        assert!(manager
            .resize(&session.id, "alice", &first.lease, first.generation, 80, 30)
            .is_err());
        manager
            .resize(&session.id, "bob", &second.lease, second.generation, 80, 30)
            .unwrap();
        let arithmetic = if cfg!(windows) {
            b"set /a 314159*2\r\n".as_slice()
        } else {
            b"expr 314159 \\* 2\n".as_slice()
        };
        manager
            .write(
                &session.id,
                "bob",
                &second.lease,
                second.generation,
                arithmetic,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let output = manager.observe(&session.id, 0).unwrap();
            if output.text.contains("628318") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "observer must see child's output: {}",
                output.text
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn slow_observer_gets_gap_while_other_terminal_stays_responsive() {
        let manager = TerminalManager::with_capacity(128);
        let s = manager
            .create(
                if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.into(),
                vec![],
                None,
            )
            .unwrap();
        let lease = manager.acquire(&s.id, "writer").unwrap();
        let input = format!("echo {}\r\n", "x".repeat(2000));
        manager
            .write(
                &s.id,
                "writer",
                &lease.lease,
                lease.generation,
                input.as_bytes(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let output = manager.observe(&s.id, 0).unwrap();
            if output.next_cursor > 128 {
                assert!(output.gap, "lost history must be explicit");
                assert!(output.bytes.len() <= 128, "history must be bounded");
                // Resuming from next_cursor returns exactly what was produced
                // since, never a repeat; the program may still be writing
                // (hypothesis load-deadlines, P4), so the read need not be empty.
                let next = manager.observe(&s.id, output.next_cursor).unwrap();
                let produced = next.next_cursor - output.next_cursor;
                assert_eq!(next.gap, produced > 128, "gap only when history was lost");
                assert_eq!(
                    next.bytes.len() as u64,
                    produced.min(128),
                    "no repeated or missing bytes"
                );
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        let started = Instant::now();
        let other = manager
            .create(
                if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.into(),
                vec![],
                None,
            )
            .unwrap();
        assert_ne!(s.id, other.id);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn owned_tree_stop_reports_truth_and_force_cleans_descendant() {
        let manager = TerminalManager::new();
        // A child with an actual descendant that ignores interactive Ctrl-C.
        let command = if cfg!(windows) {
            "powershell.exe"
        } else {
            "/bin/sh"
        };
        let args = if cfg!(windows) {
            vec!["-NoProfile","-Command","$p=Start-Process -WindowStyle Hidden -PassThru powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep 120'; Write-Output ('DESCENDANT='+$p.Id); Start-Sleep 120"]
        } else {
            vec![
                "-c",
                "trap '' INT HUP; sleep 120 & echo DESCENDANT=$!; wait",
            ]
        };
        let s = manager
            .create(
                command.into(),
                args.into_iter().map(String::from).collect(),
                None,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let descendant;
        loop {
            let output = manager.observe(&s.id, 0).unwrap();
            if let Some(tail) = output.text.split("DESCENDANT=").nth(1) {
                if let Ok(pid) = tail
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u32>()
                {
                    descendant = pid;
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "must observe real descendant: {}",
                output.text
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let result = manager.stop_owned(&s.id, Duration::from_millis(100));
        assert!(result.is_ok(), "owned stop must exist: {result:?}");
        let stopped = manager.force_owned(&s.id, Duration::from_secs(3)).unwrap();
        assert_eq!(stopped.status, "exited");
        #[cfg(windows)]
        assert!(
            std::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "if(Get-Process -Id {descendant} -ErrorAction SilentlyContinue){{exit 1}}"
                    )
                ])
                .status()
                .unwrap()
                .success(),
            "owned descendant must exit"
        );
        #[cfg(not(windows))]
        {
            let until = Instant::now() + Duration::from_secs(3);
            while std::path::Path::new(&format!("/proc/{descendant}")).exists() {
                assert!(Instant::now() < until, "owned descendant must exit");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(manager.shutdown(Duration::from_millis(100)).is_empty());
    }
    #[test]
    #[cfg(not(windows))]
    fn unix_graceful_stop_never_reports_exit_while_descendants_live() {
        let manager = TerminalManager::new();
        // The root exits on Ctrl-C; its child ignores INT/HUP and stays in the group.
        let s = manager
            .create(
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "(trap '' INT HUP; exec sleep 120) & echo DESCENDANT=$!; wait".into(),
                ],
                None,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let descendant = loop {
            let text = manager.observe(&s.id, 0).unwrap().text;
            if let Some(pid) = text.split("DESCENDANT=").nth(1).and_then(|t| {
                t.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<u32>()
                    .ok()
            }) {
                break pid;
            }
            assert!(Instant::now() < deadline, "must observe descendant: {text}");
            std::thread::sleep(Duration::from_millis(20));
        };
        let stopped = manager
            .stop_owned(&s.id, Duration::from_millis(500))
            .unwrap();
        assert_ne!(
            stopped.status, "exited",
            "descendant {descendant} is still alive"
        );
        assert!(std::path::Path::new(&format!("/proc/{descendant}")).exists());
        let forced = manager.force_owned(&s.id, Duration::from_secs(3)).unwrap();
        assert_eq!(forced.status, "exited");
        let until = Instant::now() + Duration::from_secs(3);
        while std::path::Path::new(&format!("/proc/{descendant}")).exists() {
            assert!(Instant::now() < until, "owned descendant must exit");
            std::thread::sleep(Duration::from_millis(20));
        }
        // A closed group is never signalled again, even on repeated force.
        assert_eq!(
            manager
                .force_owned(&s.id, Duration::from_secs(1))
                .unwrap()
                .status,
            "exited"
        );
    }
    #[test]
    fn heartbeat_renews_only_current_terminal_controller() {
        let manager = TerminalManager::new();
        let s = manager
            .create(
                if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.into(),
                vec![],
                None,
            )
            .unwrap();
        let lease = manager.acquire(&s.id, "alice").unwrap();
        assert!(manager
            .renew(&s.id, "bob", &lease.lease, lease.generation)
            .is_err());
        let renewed = manager
            .renew(&s.id, "alice", &lease.lease, lease.generation)
            .expect("valid controller heartbeat renews lease");
        assert_eq!(renewed.lease, lease.lease);
        assert_eq!(renewed.generation, lease.generation);
        manager.release(&s.id, "alice", &lease.lease).unwrap();
        assert!(manager
            .renew(&s.id, "alice", &lease.lease, lease.generation)
            .is_err());
    }
    #[test]
    fn input_overload_is_rejected_without_preventing_bounded_cleanup() {
        let manager = TerminalManager::new();
        let s = manager
            .create(
                if cfg!(windows) {
                    "powershell.exe"
                } else {
                    "/bin/sh"
                }
                .into(),
                if cfg!(windows) {
                    vec![
                        "-NoProfile".into(),
                        "-Command".into(),
                        "Start-Sleep 3".into(),
                    ]
                } else {
                    vec!["-c".into(), "exec sleep 30".into()]
                },
                None,
            )
            .unwrap();
        let lease = manager.acquire(&s.id, "alice").unwrap();
        let started = Instant::now();
        let input = vec![b'x'; 8 * 1024 * 1024];
        let result = manager.write(&s.id, "alice", &lease.lease, lease.generation, &input);
        assert!(
            result.is_err(),
            "oversized raw input must be rejected before delivery"
        );
        assert!(result.unwrap_err().contains("not delivered"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            manager
                .force_owned(&s.id, Duration::from_secs(3))
                .unwrap()
                .status,
            "exited"
        );
        assert_eq!(manager.observe(&s.id, 0).unwrap().status, "exited");
    }
}
