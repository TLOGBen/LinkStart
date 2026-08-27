use serde_json::{json, Value};
use std::{
    net::TcpListener,
    process::{Child, Command},
    thread,
    time::Duration,
};
use tempfile::TempDir;

fn bin() -> String {
    env!("CARGO_BIN_EXE_linkstart").to_string()
}
fn post(url: &str, token: Option<&str>, origin: Option<&str>, body: Value) -> (u16, Value) {
    let output_file = tempfile::NamedTempFile::new().unwrap();
    let output_path = output_file.path().to_str().unwrap().to_owned();
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "-o",
        &output_path,
        "-w",
        "%{http_code}",
        "-X",
        "POST",
        url,
        "-H",
        "Content-Type: application/json",
    ]);
    if let Some(t) = token {
        cmd.args(["-H", &format!("Authorization: Bearer {t}")]);
    }
    if let Some(o) = origin {
        cmd.args(["-H", &format!("Origin: {o}")]);
    }
    let code = String::from_utf8(cmd.arg("-d").arg(body.to_string()).output().unwrap().stdout)
        .unwrap()
        .parse()
        .unwrap();
    let data: Value = serde_json::from_slice(&std::fs::read(output_file.path()).unwrap()).unwrap();
    (code, data)
}
fn preflight(url: &str, origin: &str, host: Option<&str>) -> (u16, String) {
    let headers = tempfile::NamedTempFile::new().unwrap();
    let mut command = Command::new("curl");
    command.args([
        "-sS",
        "-D",
        headers.path().to_str().unwrap(),
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-X",
        "OPTIONS",
        url,
        "-H",
        &format!("Origin: {origin}"),
        "-H",
        "Access-Control-Request-Method: POST",
        "-H",
        "Access-Control-Request-Headers: authorization, content-type",
        "-H",
        "Access-Control-Request-Private-Network: true",
    ]);
    if let Some(host) = host {
        command.args(["-H", &format!("Host: {host}")]);
    }
    let status = String::from_utf8(command.output().unwrap().stdout)
        .unwrap()
        .parse()
        .unwrap();
    (
        status,
        String::from_utf8(std::fs::read(headers.path()).unwrap())
            .unwrap()
            .to_ascii_lowercase(),
    )
}
fn start(dir: &TempDir, port: u16) -> Child {
    let c = Command::new(bin())
        .args([
            "daemon",
            "run",
            "--state-dir",
            dir.path().to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .spawn()
        .unwrap();
    for _ in 0..30 {
        if Command::new("curl")
            .args(["-fsS", &format!("http://127.0.0.1:{port}/v1/health")])
            .output()
            .unwrap()
            .status
            .success()
        {
            return c;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon did not start")
}
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
#[test]
fn durable_http_round_trip_and_restart() {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let mut child = start(&dir, port);
    let base = format!("http://127.0.0.1:{port}");
    let (_, con) = post(
        &(base.clone() + "/v1/connections"),
        None,
        None,
        json!({"protocolMajor":"v1","callsign":"測試"}),
    );
    let cid = con["connectionId"].as_str().unwrap();
    let cc = con["capability"].as_str().unwrap();
    let origin = "http://127.0.0.1:4173";
    let (_, app) = post(
        &(base.clone() + "/v1/apps"),
        Some(cc),
        None,
        json!({"protocolMajor":"v1","connectionId":cid,"manifest":{"appId":"demo","displayName":"Demo","originPolicy":{"exactOrigin":origin}}}),
    );
    let iid = app["instanceId"].as_str().unwrap();
    let ac = app["capability"].as_str().unwrap();
    let event_url = format!("{base}/v1/apps/{iid}/events");
    let (good, good_headers) = preflight(&event_url, origin, None);
    assert_eq!(good, 204);
    assert!(good_headers.contains("access-control-allow-origin: http://127.0.0.1:4173"));
    assert!(good_headers.contains("access-control-allow-private-network: true"));
    let (bad_origin, bad_origin_headers) = preflight(&event_url, "https://evil.example", None);
    assert_eq!(bad_origin, 403);
    assert!(!bad_origin_headers.contains("access-control-allow-origin"));
    assert!(!bad_origin_headers.contains("access-control-allow-private-network"));
    let (bad_host, bad_host_headers) = preflight(&event_url, origin, Some("evil.example"));
    assert_eq!(bad_host, 403);
    assert!(!bad_host_headers.contains("access-control-allow-origin"));
    assert!(!bad_host_headers.contains("access-control-allow-private-network"));
    let (_, receipt) = post(
        &event_url,
        Some(ac),
        Some(origin),
        json!({"eventId":"e-1","payload":{"answer":"同意","n":1}}),
    );
    assert_eq!(receipt["status"], "received");
    let (_, dupe) = post(
        &event_url,
        Some(ac),
        Some(origin),
        json!({"eventId":"e-1","payload":{"n":1,"answer":"同意"}}),
    );
    assert_eq!(dupe["duplicate"], true);
    let (c, conflict) = post(
        &event_url,
        Some(ac),
        Some(origin),
        json!({"eventId":"e-1","payload":{"n":2}}),
    );
    assert_eq!(c, 200);
    assert_eq!(conflict["error"], "event_id_conflict");
    let ack_url = format!("{base}/v1/connections/{cid}/ack");
    let (_, ack) = post(&ack_url, Some(cc), None, json!({"eventId":"e-1"}));
    assert_eq!(ack["status"], "delivered");
    let stream_url = format!("{base}/v1/apps/{iid}/stream");
    let sse = Command::new("curl")
        .args([
            "-sN",
            "--max-time",
            "2",
            &stream_url,
            "-H",
            &format!("Authorization: Bearer {ac}"),
            "-H",
            &format!("Origin: {origin}"),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    let feedback_url = format!("{base}/v1/connections/{cid}/feedback/{iid}");
    let (_, feedback) = post(
        &feedback_url,
        Some(cc),
        None,
        json!({"feedbackId":"f-1","inReplyToEventId":"e-1","payload":{"message":"已收到"}}),
    );
    assert_eq!(feedback["feedbackId"], "f-1");
    let sse_output = sse.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&sse_output.stdout).contains("event: feedback"));
    child.kill().unwrap();
    child.wait().unwrap();
    let mut restarted = start(&dir, port);
    let output = Command::new(bin())
        .args([
            "status",
            "--json",
            "--state-dir",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["onlineConnections"], 1);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(ac));
    restarted.kill().unwrap();
}
