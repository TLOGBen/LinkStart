use serde_json::Value;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    thread,
    time::Duration,
};
use tempfile::TempDir;

fn bin() -> String {
    env!("CARGO_BIN_EXE_linkstart").to_string()
}
fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn control(dir: &TempDir, port: u16, args: &[&str]) -> Value {
    eprintln!("control start: {args:?}");
    let mut command = Command::new(bin());
    command.args(args).args([
        "--json",
        "--state-dir",
        dir.path().to_str().unwrap(),
        "--port",
        &port.to_string(),
    ]);
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(command.output());
    });
    let output = receive
        .recv_timeout(Duration::from_secs(8))
        .expect("control output must reach EOF while the detached daemon stays alive")
        .unwrap();
    eprintln!("control returned: {args:?}");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("exactly one JSON stdout value")
}
fn alive(pid: u64) -> bool {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn GetExitCodeProcess(handle: isize, code: *mut u32) -> i32;
            fn CloseHandle(handle: isize) -> i32;
        }
        unsafe {
            let h = OpenProcess(0x1000, 0, pid as u32);
            if h == 0 {
                return false;
            }
            let mut code = 0;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            return ok && code == 259;
        }
    }
    #[cfg(not(windows))]
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}
struct DaemonCleanup(std::path::PathBuf);
impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let _ = Command::new(bin())
            .args(["daemon", "stop", "--state-dir"])
            .arg(&self.0)
            .output();
    }
}
fn health(port: u16) -> bool {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(
        format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
    .unwrap();
    let mut response = String::new();
    s.read_to_string(&mut response).unwrap();
    response.starts_with("HTTP/1.1 200") && response.contains("\"status\":\"ready\"")
}

#[test]
fn lifecycle_json_is_one_truthful_control_result() {
    let dir = TempDir::new().unwrap();
    let _cleanup = DaemonCleanup(dir.path().to_path_buf());
    let p = port();
    let started = control(&dir, p, &["daemon", "start"]);
    assert_eq!(started["state"], "started");
    let old_pid = started["pid"].as_u64().unwrap();
    assert!(alive(old_pid));
    assert!(health(p));
    let status = Command::new(bin())
        .args([
            "status",
            "--json",
            "--state-dir",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(status.status.success());
    let _: Value = serde_json::from_slice(&status.stdout).unwrap();
    let restarted = control(&dir, p, &["daemon", "restart"]);
    assert_eq!(restarted["state"], "started");
    assert_eq!(restarted["stoppedBeforeRestart"], true);
    let new_pid = restarted["pid"].as_u64().unwrap();
    assert_ne!(old_pid, new_pid);
    assert!(!alive(old_pid));
    assert!(alive(new_pid));
    assert!(health(p));
    thread::sleep(Duration::from_millis(100));
    assert!(alive(new_pid));
    assert!(health(p));
    let discovery: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("daemon.json")).unwrap()).unwrap();
    assert_eq!(discovery["pid"], new_pid);
    let stopped = Command::new(bin())
        .args([
            "daemon",
            "stop",
            "--json",
            "--state-dir",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(stopped.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&stopped.stdout).unwrap()["stopped"],
        true
    );
    thread::sleep(Duration::from_millis(100));
    assert!(!alive(new_pid));
    assert!(!dir.path().join("daemon.json").exists());
}

#[test]
fn start_reports_json_failure_when_port_is_occupied() {
    let dir = TempDir::new().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = occupied.local_addr().unwrap().port();
    let output = Command::new(bin())
        .args([
            "daemon",
            "start",
            "--json",
            "--state-dir",
            dir.path().to_str().unwrap(),
            "--port",
            &p.to_string(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result: Value =
        serde_json::from_slice(&output.stdout).expect("failure stdout is one JSON value");
    assert_eq!(result["state"], "failed");
}

struct FixtureChild(std::process::Child);
impl Drop for FixtureChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// This child belongs only to this test. A normal CLI stop must never kill it
// when the runtime says cleanup is pending or the transport fails.
fn fixture_child() -> FixtureChild {
    #[cfg(windows)]
    let child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 60",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    #[cfg(not(windows))]
    let child = Command::new("sleep").arg("60").spawn().unwrap();
    FixtureChild(child)
}

fn scripted_shutdown(
    reply: &str,
    pid: u32,
) -> (TcpListener, std::sync::mpsc::Receiver<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let serving = listener.try_clone().unwrap();
    serving.set_nonblocking(true).unwrap();
    let reply = reply.to_string();
    let address = listener.local_addr().unwrap().to_string();
    let (send, receive) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut requests = Vec::new();
        while requests.len() < 2 && std::time::Instant::now() < deadline {
            let Ok((mut stream, _)) = serving.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut byte).is_err() {
                    break;
                }
                bytes.push(byte[0]);
            }
            let headers = String::from_utf8(bytes).unwrap();
            if headers.is_empty() {
                continue;
            } // listener-exit probes carry no request
            if headers.starts_with("GET /v1/health HTTP/1.1\r\n") {
                assert!(!headers.contains("Authorization:"));
                let body = serde_json::json!({"status":"ready","pid":pid,"address":address,"instanceId":"fixture-instance","version":env!("CARGO_PKG_VERSION"),"protocolMajor":"v1"}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                continue;
            }
            assert!(headers.starts_with("POST /v2/tools HTTP/1.1\r\n"));
            assert!(headers.contains("Authorization: Bearer fixture-owner\r\n"));
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            requests.push(serde_json::from_slice(&body).unwrap());
            let body = if requests.len() == 1 {
                r#"{"status":"prepared","ticket":"fixture-ticket"}"#
            } else {
                &reply
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
        let _ = send.send(requests);
    });
    (listener, receive)
}

#[test]
fn pending_or_uncertain_shutdown_never_kills_recorded_process_or_restarts() {
    for response in [
        r#"{"status":"pending","history_error":"fixture barrier blocked"}"#,
        r#"{"status":"stopping","history":"pending"}"#,
        "not-json",
    ] {
        for action in ["stop", "restart"] {
            let dir = TempDir::new().unwrap();
            let child = fixture_child();
            let (listener, requests) = scripted_shutdown(response, child.0.id());
            let address = listener.local_addr().unwrap();
            let record = serde_json::json!({"pid":child.0.id(),"address":address.to_string(),"version":env!("CARGO_PKG_VERSION"),"protocolMajor":"v1","instanceId":"fixture-instance"});
            let bytes = serde_json::to_vec(&record).unwrap();
            std::fs::write(dir.path().join("daemon.json"), &bytes).unwrap();
            std::fs::write(dir.path().join("v2-owner.token"), "fixture-owner\n").unwrap();
            let output = Command::new(bin())
                .args(["daemon", action, "--json", "--state-dir"])
                .arg(dir.path())
                .output()
                .unwrap();
            assert!(!output.status.success(), "must reject {action}: {response}");
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["state"], "failed");
            assert!(
                alive(child.0.id().into()),
                "{action} killed the recorded process after {response}"
            );
            assert_eq!(
                std::fs::read(dir.path().join("daemon.json")).unwrap(),
                bytes
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("v2-owner.token")).unwrap(),
                "fixture-owner\n"
            );
            let requests = requests.recv_timeout(Duration::from_secs(6)).unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0]["tool"], "runtime.prepare_shutdown");
            assert_eq!(requests[1]["tool"], "runtime.confirm_shutdown");
            assert_eq!(requests[1]["args"]["ticket"], "fixture-ticket");
            assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-owner"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-owner"));
        }
    }
}

#[test]
fn invalid_discovery_or_unavailable_transport_never_kills_or_initializes_owner() {
    for (address, credential) in [
        ("192.0.2.1:45831".to_string(), Some("fixture-owner")),
        (format!("127.0.0.1:{}", port()), Some("fixture-owner")),
        (format!("127.0.0.1:{}", port()), None),
    ] {
        let dir = TempDir::new().unwrap();
        let child = fixture_child();
        let bytes = serde_json::to_vec(&serde_json::json!({"pid":child.0.id(),"address":address,"version":env!("CARGO_PKG_VERSION"),"protocolMajor":"v1"})).unwrap();
        std::fs::write(dir.path().join("daemon.json"), &bytes).unwrap();
        let owner = dir.path().join("v2-owner.token");
        if let Some(credential) = credential {
            std::fs::write(&owner, credential).unwrap();
        }
        let output = Command::new(bin())
            .args(["daemon", "restart", "--json", "--state-dir"])
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["state"],
            "failed"
        );
        assert!(
            alive(child.0.id().into()),
            "discovery/transport failure killed the recorded process"
        );
        assert_eq!(
            std::fs::read(dir.path().join("daemon.json")).unwrap(),
            bytes
        );
        assert_eq!(std::fs::read_to_string(owner).ok().as_deref(), credential);
    }
}
