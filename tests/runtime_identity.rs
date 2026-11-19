//! Public CLI identity regression. Locator mutations below are explicit fixture
//! precondition injection, not an observed natural OS PID-reuse experiment.
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_linkstart")
}

fn wait_until(description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out: {description}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn healthy(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(100),
    ) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    if write!(
        stream,
        "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .is_err()
    {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200")
        && response.contains("\"status\":\"ready\"")
}

fn command(state: &Path, action: &str, port: Option<u16>) -> Command {
    let mut command = Command::new(bin());
    command
        .args(["daemon", action, "--json", "--state-dir"])
        .arg(state);
    if let Some(port) = port {
        command.args(["--port", &port.to_string()]);
    }
    command
}

fn run(mut command: Command, input: Option<Vec<u8>>) -> (Output, bool) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(&input).unwrap();
    } else {
        drop(child.stdin.take());
    }
    let deadline = Instant::now() + Duration::from_secs(13);
    let mut timed_out = false;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            // Emergency fixture cleanup addresses only this newly spawned exact
            // process handle. Product stop must never use this strategy.
            timed_out = true;
            child.kill().unwrap();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    (child.wait_with_output().unwrap(), timed_out)
}

struct GenuineRuntime {
    dir: TempDir,
    child: Child,
    port: u16,
    original: Vec<u8>,
}

impl GenuineRuntime {
    fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let port = free_port();
        let child = command(dir.path(), "run", Some(port))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut runtime = Self {
            dir,
            child,
            port,
            original: Vec::new(),
        };
        wait_until("genuine direct Runtime health and locator", || {
            assert!(
                runtime.child.try_wait().unwrap().is_none(),
                "genuine Runtime exited before ready"
            );
            if !healthy(port) {
                return false;
            }
            if let Ok(bytes) = fs::read(runtime.dir.path().join("daemon.json")) {
                runtime.original = bytes;
                return true;
            }
            false
        });
        let record: Value = serde_json::from_slice(&runtime.original).unwrap();
        assert_eq!(record["pid"], runtime.child.id());
        assert_eq!(record["address"], format!("127.0.0.1:{port}"));
        runtime
    }

    fn restore_and_stop(&mut self) {
        // Explicitly undo the injected locator before normal public cleanup.
        fs::write(self.dir.path().join("daemon.json"), &self.original).unwrap();
        let (stopped, timeout) = run(command(self.dir.path(), "stop", None), None);
        assert!(
            !timeout && stopped.status.success(),
            "normal cleanup: {stopped:?}"
        );
        wait_until("normal authenticated stop exits genuine Runtime", || {
            self.child.try_wait().unwrap().is_some()
        });
        assert!(!healthy(self.port));
        assert!(!self.dir.path().join("daemon.json").exists());
    }
}

impl Drop for GenuineRuntime {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            if !self.original.is_empty() {
                let _ = fs::write(self.dir.path().join("daemon.json"), &self.original);
                let _ = run(command(self.dir.path(), "stop", None), None);
            }
            if self.child.try_wait().ok().flatten().is_none() {
                // Abnormal fixture cleanup only; do not label it a passed stop.
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

struct UnrelatedProcess {
    child: Child,
    _dir: TempDir,
}
impl UnrelatedProcess {
    fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "unrelated_live_process",
                "--nocapture",
            ])
            .env("LINKSTART_IDENTITY_TEST_DIRECTORY", dir.path())
            .spawn()
            .unwrap();
        let mut process = Self { child, _dir: dir };
        wait_until("unrelated test-owned process ready", || {
            assert!(process.child.try_wait().unwrap().is_none());
            process._dir.path().join("unrelated-ready").exists()
        });
        process
    }
}
impl Drop for UnrelatedProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "test-owned unrelated process; never runs a Runtime or listener"]
fn unrelated_live_process() {
    let Some(directory) = std::env::var_os("LINKSTART_IDENTITY_TEST_DIRECTORY") else {
        return;
    };
    fs::write(
        PathBuf::from(directory).join("unrelated-ready"),
        b"live process, no Runtime",
    )
    .unwrap();
    loop {
        thread::sleep(Duration::from_millis(100));
    }
}

fn identity_rejected(output: &Output) -> bool {
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    !output.status.success()
        && (text.contains("runtime_identity_unknown") || text.contains("runtime_identity_conflict"))
}

fn generic_lookup(state: &Path) -> (Output, bool) {
    let mut command = Command::new(bin());
    command
        .args(["tool", "--state-dir"])
        .arg(state)
        .arg("--credential-file")
        .arg(state.join("v2-owner.token"));
    run(
        command,
        Some(
            serde_json::to_vec(&json!({
                "request_id":"identity-read-only-witness", "tool":"spaces.list", "args":{}
            }))
            .unwrap(),
        ),
    )
}

#[test]
fn stale_locator_with_unrelated_live_pid_never_reports_runtime_reuse() {
    let mut runtime = GenuineRuntime::start();
    let mut unrelated = UnrelatedProcess::start();
    runtime.restore_and_stop();
    let token = fs::read(runtime.dir.path().join("v2-owner.token")).unwrap();
    let mut record: Value = serde_json::from_slice(&runtime.original).unwrap();
    record["pid"] = json!(unrelated.child.id());
    let injected = serde_json::to_vec(&record).unwrap();
    // Controlled stale-PID precondition injection, not natural OS PID reuse.
    fs::write(runtime.dir.path().join("daemon.json"), &injected).unwrap();
    let (start, start_timeout) = run(
        command(runtime.dir.path(), "start", Some(free_port())),
        None,
    );
    let (direct, direct_timeout) = run(command(runtime.dir.path(), "run", Some(free_port())), None);
    let (tool, tool_timeout) = generic_lookup(runtime.dir.path());
    let (stop, stop_timeout) = run(command(runtime.dir.path(), "stop", None), None);
    let unchanged = fs::read(runtime.dir.path().join("daemon.json"))
        .ok()
        .as_deref()
        == Some(injected.as_slice())
        && fs::read(runtime.dir.path().join("v2-owner.token")).unwrap() == token;
    let unrelated_alive = unrelated.child.try_wait().unwrap().is_none();
    eprintln!("injected stale PID={}, no Runtime listener; start={start:?}; direct={direct:?}; tool={tool:?}; stop={stop:?}; data_unchanged={unchanged}; unrelated_alive={unrelated_alive}", unrelated.child.id());
    assert!(!start_timeout && !direct_timeout && !tool_timeout && !stop_timeout);
    assert!(unchanged && unrelated_alive);
    assert!(
        identity_rejected(&start)
            && identity_rejected(&direct)
            && identity_rejected(&tool)
            && identity_rejected(&stop),
        "a live unrelated PID cannot justify Runtime reuse or a trusted consumer"
    );
}

fn assert_mismatch_cannot_route_or_stop(mismatch: &str) {
    let mut runtime = GenuineRuntime::start();
    let mut unrelated = UnrelatedProcess::start();
    let token = fs::read(runtime.dir.path().join("v2-owner.token")).unwrap();
    let mut record: Value = serde_json::from_slice(&runtime.original).unwrap();
    if mismatch == "pid" {
        record["pid"] = json!(unrelated.child.id());
    } else {
        record["instanceId"] = json!(uuid::Uuid::new_v4().to_string());
    }
    let injected = serde_json::to_vec(&record).unwrap();
    // Intentional precondition injection against a genuine live endpoint.
    fs::write(runtime.dir.path().join("daemon.json"), &injected).unwrap();
    let (start, start_timeout) = run(
        command(runtime.dir.path(), "start", Some(free_port())),
        None,
    );
    let (direct, direct_timeout) = run(command(runtime.dir.path(), "run", Some(free_port())), None);
    let (tool, tool_timeout) = generic_lookup(runtime.dir.path());
    let (stop, stop_timeout) = run(command(runtime.dir.path(), "stop", None), None);
    let genuine_alive = runtime.child.try_wait().unwrap().is_none() && healthy(runtime.port);
    let unrelated_alive = unrelated.child.try_wait().unwrap().is_none();
    let unchanged = fs::read(runtime.dir.path().join("daemon.json"))
        .ok()
        .as_deref()
        == Some(injected.as_slice())
        && fs::read(runtime.dir.path().join("v2-owner.token")).unwrap() == token;
    eprintln!("injected {mismatch} mismatch; start={start:?}; direct={direct:?}; tool={tool:?}; stop={stop:?}; genuine_alive={genuine_alive}; unrelated_alive={unrelated_alive}; data_unchanged={unchanged}");
    runtime.restore_and_stop();
    assert!(!start_timeout && !direct_timeout && !tool_timeout && !stop_timeout);
    assert!(
        genuine_alive && unrelated_alive && unchanged,
        "unknown identity must leave genuine Runtime, unrelated process and locator/token intact"
    );
    assert!(
        identity_rejected(&start)
            && identity_rejected(&direct)
            && identity_rejected(&tool)
            && identity_rejected(&stop),
        "every lifecycle consumer must reject mismatched service incarnation"
    );
}

#[test]
fn mismatched_pid_cannot_route_tools_or_stop_genuine_runtime() {
    assert_mismatch_cannot_route_or_stop("pid");
}

#[test]
fn mismatched_incarnation_cannot_route_tools_or_stop_genuine_runtime() {
    assert_mismatch_cannot_route_or_stop("instanceId");
}

#[test]
fn healthy_incarnation_reuses_routes_and_stops_normally() {
    let mut runtime = GenuineRuntime::start();
    let record: Value = serde_json::from_slice(&runtime.original).unwrap();
    assert!(!record["instanceId"].as_str().unwrap().is_empty());
    for action in ["start", "run"] {
        let (result, timeout) = run(command(runtime.dir.path(), action, Some(free_port())), None);
        assert!(!timeout && result.status.success(), "{result:?}");
    }
    let (result, timeout) = generic_lookup(runtime.dir.path());
    assert!(!timeout && result.status.success(), "{result:?}");
    assert_eq!(
        fs::read(runtime.dir.path().join("daemon.json")).unwrap(),
        runtime.original
    );
    runtime.restore_and_stop();
}

#[test]
fn legacy_live_and_dead_pid_open_listener_fail_closed_for_all_consumers() {
    for mode in ["legacy", "dead-pid"] {
        let mut runtime = GenuineRuntime::start();
        let mut record: Value = serde_json::from_slice(&runtime.original).unwrap();
        if mode == "legacy" {
            record.as_object_mut().unwrap().remove("instanceId");
        } else {
            record["pid"] = json!(4294967294_u32);
        }
        let injected = serde_json::to_vec(&record).unwrap();
        fs::write(runtime.dir.path().join("daemon.json"), &injected).unwrap();
        for action in ["start", "run", "stop"] {
            let (result, timeout) = run(command(runtime.dir.path(), action, None), None);
            assert!(!timeout && identity_rejected(&result), "{mode}: {result:?}");
        }
        let (result, timeout) = generic_lookup(runtime.dir.path());
        assert!(!timeout && identity_rejected(&result), "{mode}: {result:?}");
        assert!(runtime.child.try_wait().unwrap().is_none() && healthy(runtime.port));
        assert_eq!(
            fs::read(runtime.dir.path().join("daemon.json")).unwrap(),
            injected
        );
        runtime.restore_and_stop();
    }
}

#[test]
fn malformed_slow_oversized_status_and_nonloopback_never_reach_credentials() {
    for mode in [
        "malformed",
        "slow",
        "oversized",
        "status",
        "nonloopback",
        "hostname",
    ] {
        let dir = TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut address = listener.local_addr().unwrap().to_string();
        let server = if mode == "nonloopback" || mode == "hostname" {
            address = if mode == "hostname" {
                "localhost:1"
            } else {
                "192.0.2.1:1"
            }
            .into();
            None
        } else {
            Some(thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    bytes.push(byte[0]);
                }
                let request = String::from_utf8(bytes).unwrap();
                assert!(request.starts_with("GET /v1/health HTTP/1.1"));
                assert!(!request.contains("Authorization"));
                if mode == "slow" {
                    // Drip data keeps per-read deadlines alive; total budget must still win.
                    for _ in 0..35 {
                        if stream.write_all(b"x").is_err() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                } else {
                    let response = match mode {
                        "malformed" => "HTTP/1.1 200 OK\r\n\r\nnot-json".into(),
                        "status" => "HTTP/1.1 503 Unavailable\r\n\r\n{}".into(),
                        _ => format!("HTTP/1.1 200 OK\r\n\r\n{}", "x".repeat(17000)),
                    };
                    let _ = stream.write_all(response.as_bytes());
                }
            }))
        };
        let record = json!({"pid":std::process::id(),"address":address,"version":env!("CARGO_PKG_VERSION"),"protocolMajor":"v1","instanceId":"bounded-fixture"});
        fs::write(
            dir.path().join("daemon.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        // Credential intentionally absent: failure must occur before opening it.
        let began = Instant::now();
        let (result, timeout) = generic_lookup(dir.path());
        assert!(!timeout && identity_rejected(&result), "{mode}: {result:?}");
        assert!(
            began.elapsed() < Duration::from_secs(4),
            "{mode}: elapsed {:?}",
            began.elapsed()
        );
        assert!(!dir.path().join("v2-owner.token").exists());
        if let Some(server) = server {
            server.join().unwrap();
        }
    }
}

// A credential-file pipe is reached only after genuine health inspection. Its
// open handshake witnesses preflight without sleeps, hooks or fake health.
struct CredentialPipe {
    path: PathBuf,
    #[cfg(windows)]
    writer: fs::File,
}
impl CredentialPipe {
    fn new(directory: &Path) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
            unsafe extern "system" {
                fn CreateNamedPipeW(
                    name: *const u16,
                    access: u32,
                    mode: u32,
                    instances: u32,
                    out_size: u32,
                    in_size: u32,
                    timeout: u32,
                    security: *mut std::ffi::c_void,
                ) -> *mut std::ffi::c_void;
            }
            let _ = directory;
            let path = PathBuf::from(format!(
                r"\\.\pipe\linkstart-identity-{}",
                uuid::Uuid::new_v4()
            ));
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let handle = unsafe {
                CreateNamedPipeW(wide.as_ptr(), 2, 1, 1, 4096, 4096, 0, std::ptr::null_mut())
            };
            assert_ne!(handle as isize, -1, "create credential pipe");
            Self {
                path,
                writer: unsafe { fs::File::from_raw_handle(handle) },
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            unsafe extern "C" {
                fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
            }
            let path = directory.join(format!("credential-{}", uuid::Uuid::new_v4()));
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { mkfifo(name.as_ptr(), 0o600) }, 0);
            Self { path }
        }
    }
    fn witness(self) -> fs::File {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            unsafe extern "system" {
                fn ConnectNamedPipe(
                    handle: *mut std::ffi::c_void,
                    overlapped: *mut std::ffi::c_void,
                ) -> i32;
            }
            wait_until("credential pipe reader after health", || {
                let ok =
                    unsafe { ConnectNamedPipe(self.writer.as_raw_handle(), std::ptr::null_mut()) };
                let code = std::io::Error::last_os_error().raw_os_error();
                ok != 0 || code == Some(535)
            });
            self.writer
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut writer = None;
            wait_until("credential FIFO reader after health", || {
                writer = fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(0x800)
                    .open(&self.path)
                    .ok();
                writer.is_some()
            });
            writer.unwrap()
        }
    }
}

fn finish_child(mut child: Child) -> Output {
    let mut timeout = false;
    let deadline = Instant::now() + Duration::from_secs(13);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            timeout = true;
            child.kill().unwrap();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!timeout, "public CLI timed out: {output:?}");
    output
}

fn paused_tool(runtime: &GenuineRuntime, name: &str) -> Child {
    let pipe = CredentialPipe::new(runtime.dir.path());
    let mut child = Command::new(bin())
        .args(["tool", "--state-dir"])
        .arg(runtime.dir.path())
        .arg("--credential-file")
        .arg(&pipe.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            &serde_json::to_vec(&json!({
                "request_id":name,"tool":"spaces.create","args":{"name":name}
            }))
            .unwrap(),
        )
        .unwrap();
    let mut writer = pipe.witness();
    writer
        .write_all(&fs::read(runtime.dir.path().join("v2-owner.token")).unwrap())
        .unwrap();
    drop(writer);
    child
}

fn restart_same_address(runtime: &mut GenuineRuntime) {
    let previous: Value = serde_json::from_slice(&runtime.original).unwrap();
    runtime.child = command(runtime.dir.path(), "run", Some(runtime.port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("replacement genuine Runtime ready", || {
        assert!(runtime.child.try_wait().unwrap().is_none());
        if !healthy(runtime.port) {
            return false;
        }
        if let Ok(bytes) = fs::read(runtime.dir.path().join("daemon.json")) {
            runtime.original = bytes;
            return true;
        }
        false
    });
    let next: Value = serde_json::from_slice(&runtime.original).unwrap();
    assert_ne!(previous["pid"], next["pid"]);
    assert_ne!(previous["instanceId"], next["instanceId"]);
    assert_eq!(previous["address"], next["address"]);
}

#[test]
fn transport_race_tool_cannot_mutate_replacement_runtime() {
    let mut runtime = GenuineRuntime::start();
    let token = fs::read(runtime.dir.path().join("v2-owner.token")).unwrap();
    let name = format!("stale-target-{}", uuid::Uuid::new_v4());
    let mut tool = paused_tool(&runtime, &name);
    runtime.restore_and_stop();
    restart_same_address(&mut runtime);
    assert_eq!(
        fs::read(runtime.dir.path().join("v2-owner.token")).unwrap(),
        token
    );
    drop(tool.stdin.take());
    let result = finish_child(tool);
    let (lookup, timeout) = generic_lookup(runtime.dir.path());
    assert!(!timeout && lookup.status.success(), "{lookup:?}");
    let mutation = String::from_utf8_lossy(&lookup.stdout).contains(&name);
    let alive = runtime.child.try_wait().unwrap().is_none();
    eprintln!("ordinary replacement: original_success={} replacement_mutation={mutation} replacement_alive={alive}", result.status.success());
    runtime.restore_and_stop();
    assert!(
        !result.status.success() && !mutation && alive,
        "stale A-targeted tool reached B"
    );
}

#[test]
fn transport_race_tool_never_sends_credential_to_replacement_listener() {
    let mut runtime = GenuineRuntime::start();
    let mut tool = paused_tool(&runtime, "credential-routing-witness");
    runtime.restore_and_stop();
    let listener = TcpListener::bind(("127.0.0.1", runtime.port)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let collector = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                // Private transient request bytes are never returned or logged.
                let mut bytes = Vec::new();
                while bytes.len() < 8192 && !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    bytes.push(byte[0]);
                }
                let authorization_seen = bytes
                    .windows(14)
                    .any(|w| w.eq_ignore_ascii_case(b"authorization:"));
                let _ = stream.write_all(
                    b"HTTP/1.1 503 Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
                return authorization_seen;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    });
    drop(tool.stdin.take());
    let result = finish_child(tool);
    let authorization_seen = collector.join().unwrap();
    eprintln!(
        "replacement collector: authorization_seen={authorization_seen} original_success={}",
        result.status.success()
    );
    assert!(
        !authorization_seen && !result.status.success(),
        "bearer reached a replacement listener"
    );
}

#[cfg(unix)]
#[test]
fn transport_race_stop_cannot_shutdown_replacement_runtime() {
    let mut runtime = GenuineRuntime::start();
    let token_path = runtime.dir.path().join("v2-owner.token");
    let backup = runtime.dir.path().join("owner-backup");
    fs::rename(&token_path, &backup).unwrap();
    let mut pipe = CredentialPipe::new(runtime.dir.path());
    fs::rename(&pipe.path, &token_path).unwrap();
    pipe.path = token_path.clone();
    let stale_stop = command(runtime.dir.path(), "stop", None)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut writer = pipe.witness();
    let call = |tool: &str, args: Value| {
        let mut cmd = Command::new(bin());
        cmd.args(["tool", "--state-dir"])
            .arg(runtime.dir.path())
            .arg("--credential-file")
            .arg(&backup);
        let (result, timeout) = run(
            cmd,
            Some(
                serde_json::to_vec(&json!({
                    "request_id":uuid::Uuid::new_v4().to_string(),"tool":tool,"args":args
                }))
                .unwrap(),
            ),
        );
        assert!(!timeout && result.status.success(), "{result:?}");
        serde_json::from_slice::<Value>(&result.stdout).unwrap()
    };
    let prepared = call("runtime.prepare_shutdown", json!({}));
    let confirmed = call(
        "runtime.confirm_shutdown",
        json!({"ticket":prepared["ticket"]}),
    );
    assert_eq!(confirmed["status"], "stopping");
    assert_eq!(confirmed["history"], "durable");
    wait_until("A exits following public prepare/confirm", || {
        runtime.child.try_wait().unwrap().is_some()
    });
    assert!(!healthy(runtime.port));
    fs::rename(&token_path, runtime.dir.path().join("blocked-fifo")).unwrap();
    fs::rename(&backup, &token_path).unwrap();
    let token = fs::read(&token_path).unwrap();
    restart_same_address(&mut runtime);
    let locator = runtime.original.clone();
    writer.write_all(&token).unwrap();
    drop(writer);
    let result = finish_child(stale_stop);
    let alive = runtime.child.try_wait().unwrap().is_none() && healthy(runtime.port);
    let unchanged = fs::read(runtime.dir.path().join("daemon.json"))
        .ok()
        .as_deref()
        == Some(locator.as_slice())
        && fs::read(&token_path).unwrap() == token;
    eprintln!("ordinary replacement stop: original_success={} replacement_alive={alive} locator_token_unchanged={unchanged}", result.status.success());
    if alive {
        runtime.restore_and_stop();
    }
    assert!(
        !result.status.success() && alive && unchanged,
        "stale A-targeted stop reached B"
    );
}
