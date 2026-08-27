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
    let output = Command::new(bin())
        .args(args)
        .args([
            "--json",
            "--state-dir",
            dir.path().to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("exactly one JSON stdout value")
}
fn alive(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
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
