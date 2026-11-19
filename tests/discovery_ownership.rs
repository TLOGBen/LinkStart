//! Cross-process discovery mutation tests, plus a genuine CLI lookup witness.
//! The publisher subprocess calls the same owner source used by server::serve;
//! it is not itself a Runtime. The separately started daemon is genuine.
#[path = "../src/discovery.rs"]
mod discovery;

use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn wait_until(description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out: {description}");
        thread::sleep(Duration::from_millis(10));
    }
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Only this test's helper is eligible for emergency fixture cleanup.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct RuntimeCleanup(std::path::PathBuf);
impl Drop for RuntimeCleanup {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_linkstart"))
            .args(["daemon", "stop", "--state-dir"])
            .arg(&self.0)
            .output();
    }
}

#[test]
#[ignore = "subprocess helper; invoked explicitly by discovery race test"]
fn publisher_subprocess() {
    let Some(directory) = std::env::var_os("LINKSTART_DISCOVERY_TEST_DIRECTORY") else {
        return;
    };
    let dir = Path::new(&directory);
    let bytes = fs::read(dir.join("replacement-input.json")).unwrap();
    discovery::publish_with_contention(dir, &bytes, || {
        fs::write(dir.join("contended"), b"publisher waiting for owner").unwrap();
    })
    .unwrap();
    fs::write(dir.join("published"), b"production publish completed").unwrap();
}

#[test]
#[ignore = "subprocess helper; its owner deliberately dies without Drop"]
fn lock_holder_subprocess() {
    let Some(directory) = std::env::var_os("LINKSTART_DISCOVERY_TEST_DIRECTORY") else {
        return;
    };
    let dir = Path::new(&directory);
    let _owner = discovery::Owner::acquire(dir).unwrap();
    fs::write(dir.join("held"), b"independently opened OS owner acquired").unwrap();
    loop {
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "subprocess helper; controls the existing production initialization lock"]
fn initialization_holder_subprocess() {
    let Some(directory) = std::env::var_os("LINKSTART_DISCOVERY_TEST_DIRECTORY") else {
        return;
    };
    let dir = Path::new(&directory);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("v2-initialize.lock"))
        .unwrap();
    lock.lock().unwrap();
    fs::write(
        dir.join("initialization-held"),
        b"existing initialization OS lock held",
    )
    .unwrap();
    loop {
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn slow_genuine_start_reports_unknown_identity_then_recovers_and_stops_normally() {
    let dir = TempDir::new().unwrap();
    let _cleanup = RuntimeCleanup(dir.path().to_owned());
    let mut blocker = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "initialization_holder_subprocess",
                "--nocapture",
            ])
            .env("LINKSTART_DISCOVERY_TEST_DIRECTORY", dir.path())
            .spawn()
            .unwrap(),
    );
    wait_until("existing initialization lock held", || {
        dir.path().join("initialization-held").exists()
    });
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let began = Instant::now();
    let start = Command::new(env!("CARGO_BIN_EXE_linkstart"))
        .args(["daemon", "start", "--json", "--state-dir"])
        .arg(dir.path())
        .args(["--port", &port.to_string()])
        .output()
        .unwrap();
    assert!(!start.status.success(), "{start:?}");
    let elapsed = began.elapsed();
    assert!(
        elapsed >= Duration::from_secs(10) && elapsed < Duration::from_secs(13),
        "{elapsed:?}"
    );
    let result: Value = serde_json::from_slice(&start.stdout).unwrap();
    assert_eq!(result["state"], "failed");
    let error = result["error"].as_str().unwrap();
    assert!(
        error.contains("startup_outcome_unknown") && error.contains("daemon_start_timeout"),
        "{error}"
    );
    assert!(error.contains(&format!("address=127.0.0.1:{port}")));
    assert!(error.contains(&format!("state_dir={}", dir.path().display())));
    let pid: u32 = error
        .split("pid=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(discovery::read(dir.path()).unwrap().is_none());
    blocker.0.kill().unwrap();
    blocker.0.wait().unwrap();
    let mut record = None;
    wait_until(
        "same genuine child publishes after stalled initialization recovers",
        || {
            record = discovery::read(dir.path())
                .unwrap()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            record.is_some()
        },
    );
    assert_eq!(record.unwrap()["pid"], pid);
    let reused = Command::new(env!("CARGO_BIN_EXE_linkstart"))
        .args(["daemon", "start", "--json", "--state-dir"])
        .arg(dir.path())
        .args(["--port", &port.to_string()])
        .output()
        .unwrap();
    assert!(reused.status.success(), "{reused:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&reused.stdout).unwrap()["pid"],
        pid
    );
    let stop = Command::new(env!("CARGO_BIN_EXE_linkstart"))
        .args(["daemon", "stop", "--json", "--state-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(stop.status.success(), "{stop:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&stop.stdout).unwrap()["stopped"],
        true
    );
    assert!(discovery::read(dir.path()).unwrap().is_none());
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    eprintln!("genuine slow initialization: bounded Unknown after {elapsed:?}, same pid={pid} published on release, normal authenticated barrier stop succeeded");
}

#[test]
fn owner_timeout_is_explicit_and_crash_releases_persistent_lock() {
    let dir = TempDir::new().unwrap();
    let old = serde_json::to_vec(&json!({
        "pid":4294967294_u32, "address":"127.0.0.1:1",
        "version":"0.1.6", "protocolMajor":"v1"
    }))
    .unwrap();
    discovery::publish(dir.path(), &old).unwrap();
    fs::write(
        dir.path().join("v2-owner.token"),
        b"existing-credential-must-not-change",
    )
    .unwrap();
    let mut holder = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "lock_holder_subprocess",
                "--nocapture",
            ])
            .env("LINKSTART_DISCOVERY_TEST_DIRECTORY", dir.path())
            .spawn()
            .unwrap(),
    );
    wait_until("child holds independently opened OS lock", || {
        dir.path().join("held").exists()
    });
    for action in ["stop", "run"] {
        let began = Instant::now();
        let result = Command::new(env!("CARGO_BIN_EXE_linkstart"))
            .args(["daemon", action, "--json", "--state-dir"])
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(!result.status.success(), "{action}: {result:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("daemon_discovery_lock_timeout"),
            "{result:?}"
        );
        assert!(began.elapsed() < Duration::from_secs(5));
        assert_eq!(fs::read(dir.path().join("daemon.json")).unwrap(), old);
        assert_eq!(
            fs::read(dir.path().join("v2-owner.token")).unwrap(),
            b"existing-credential-must-not-change"
        );
        eprintln!("{action} explicit lock timeout after {:?}", began.elapsed());
    }
    holder.0.kill().unwrap();
    let exit = holder.0.wait().unwrap();
    assert!(!exit.success());
    let replacement = br#"{"replacement":"after-crash"}"#;
    discovery::publish(dir.path(), replacement).unwrap();
    assert_eq!(discovery::read(dir.path()).unwrap().unwrap(), replacement);
    assert!(dir.path().join(".daemon-discovery.lock").is_file());
    assert!(!discovery::remove_if_matches(dir.path(), &old).unwrap());
    assert!(discovery::remove_if_matches(dir.path(), replacement).unwrap());
    assert!(discovery::read(dir.path()).unwrap().is_none());
    eprintln!("forced helper exit released lock; same persistent sidecar reused");
}

#[test]
fn atomic_replacement_readers_observe_only_complete_publications() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    };
    let dir = TempDir::new().unwrap();
    let first = serde_json::to_vec(&json!({"value":"a".repeat(16384)})).unwrap();
    let second = serde_json::to_vec(&json!({"value":"b".repeat(32768)})).unwrap();
    discovery::publish(dir.path(), &first).unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let reader = {
        let dir = dir.path().to_owned();
        let first = first.clone();
        let second = second.clone();
        let finished = finished.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut observations = 0;
            while !finished.load(Ordering::Acquire) {
                let bytes = discovery::read(&dir)
                    .unwrap()
                    .expect("replace never unlinks locator");
                assert!(bytes == first || bytes == second, "incomplete publication");
                observations += 1;
            }
            observations
        })
    };
    barrier.wait();
    for index in 0..80 {
        discovery::publish(dir.path(), if index % 2 == 0 { &second } else { &first }).unwrap();
    }
    finished.store(true, Ordering::Release);
    let observations = reader.join().unwrap();
    assert!(observations > 0);
    eprintln!(
        "80 atomic replacements over existing locator, {observations} complete reader snapshots"
    );
}

#[test]
fn genuine_direct_run_different_ports_have_one_discovery_owner() {
    let dir = TempDir::new().unwrap();
    let _cleanup = RuntimeCleanup(dir.path().to_owned());
    let ports: Vec<u16> = (0..2)
        .map(|_| {
            TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        })
        .collect();
    let mut children: Vec<OwnedChild> = ports
        .iter()
        .map(|port| {
            OwnedChild(
                Command::new(env!("CARGO_BIN_EXE_linkstart"))
                    .args(["daemon", "run", "--json", "--state-dir"])
                    .arg(dir.path())
                    .args(["--port", &port.to_string()])
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        })
        .collect();
    let mut record = None;
    wait_until("one direct run publishes and the other exits", || {
        record = discovery::read(dir.path())
            .unwrap()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        record.is_some()
            && children
                .iter_mut()
                .any(|child| child.0.try_wait().unwrap().is_some())
    });
    let record = record.unwrap();
    let pid = record["pid"].as_u64().unwrap() as u32;
    let owner_index = children
        .iter()
        .position(|child| child.0.id() == pid)
        .unwrap();
    assert!(children[owner_index].0.try_wait().unwrap().is_none());
    assert!(children[1 - owner_index].0.try_wait().unwrap().is_some());
    assert!(std::net::TcpStream::connect(("127.0.0.1", ports[1 - owner_index])).is_err());
    let reused = Command::new(env!("CARGO_BIN_EXE_linkstart"))
        .args(["daemon", "start", "--json", "--state-dir"])
        .arg(dir.path())
        .args(["--port", &ports[1 - owner_index].to_string()])
        .output()
        .unwrap();
    assert!(reused.status.success(), "{reused:?}");
    let reused: Value = serde_json::from_slice(&reused.stdout).unwrap();
    assert_eq!(reused["state"], "reused");
    assert_eq!(reused["pid"], record["pid"]);
    assert_eq!(reused["address"], record["address"]);
    let stopped = Command::new(env!("CARGO_BIN_EXE_linkstart"))
        .args(["daemon", "stop", "--json", "--state-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&stopped.stdout).unwrap()["stopped"],
        true
    );
    assert!(discovery::read(dir.path()).unwrap().is_none());
    wait_until("winning direct run exits after authenticated stop", || {
        children[owner_index].0.try_wait().unwrap().is_some()
    });
    eprintln!("same-state different ports: exactly one genuine Runtime, truthful reuse, authenticated stop and locator cleanup");
}

#[test]
fn failed_direct_run_bind_preserves_existing_locator_and_credential() {
    let dir = TempDir::new().unwrap();
    let old = serde_json::to_vec(&json!({
        "pid":4294967294_u32, "address":"127.0.0.1:1",
        "version":"0.1.6", "protocolMajor":"v1"
    }))
    .unwrap();
    discovery::publish(dir.path(), &old).unwrap();
    let credential = b"existing-owner-credential-must-not-rotate";
    fs::write(dir.path().join("v2-owner.token"), credential).unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_linkstart"))
        .args(["daemon", "run", "--state-dir"])
        .arg(dir.path())
        .args(["--port", &occupied.local_addr().unwrap().port().to_string()])
        .output()
        .unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert_eq!(discovery::read(dir.path()).unwrap().unwrap(), old);
    assert_eq!(
        fs::read(dir.path().join("v2-owner.token")).unwrap(),
        credential
    );
    assert!(!dir.path().join("v2.sqlite3").exists());
    // Reacquiring after the failed publisher also proves its guard was dropped.
    discovery::publish(dir.path(), b"replacement-after-failed-bind").unwrap();
    eprintln!("failed direct-run bind retained original locator/token and released owner");
}

#[test]
fn stale_cleanup_cannot_erase_cross_process_publication_or_cli_locator() {
    let live = TempDir::new().unwrap();
    let _runtime_cleanup = RuntimeCleanup(live.path().to_owned());
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let start = Command::new(env!("CARGO_BIN_EXE_linkstart"))
        .args(["daemon", "start", "--json", "--state-dir"])
        .arg(live.path())
        .args(["--port", &port.to_string()])
        .output()
        .unwrap();
    assert!(start.status.success(), "{:?}", start);
    let started: Value = serde_json::from_slice(&start.stdout).unwrap();
    assert_eq!(started["state"], "started");
    let replacement = fs::read(live.path().join("daemon.json")).unwrap();
    let record: Value = serde_json::from_slice(&replacement).unwrap();
    assert_eq!(record["pid"], started["pid"]);
    assert_eq!(record["address"], started["address"]);

    let race = TempDir::new().unwrap();
    // An old stopped owner's snapshot is a fixture. All concurrent mutations
    // below use the production cleanup/publication functions, never direct write.
    let old = serde_json::to_vec(&json!({
        "pid": 4294967294_u32,
        "address": "127.0.0.1:1",
        "version": "old-snapshot",
        "protocolMajor": "1"
    }))
    .unwrap();
    discovery::publish(race.path(), &old).unwrap();
    fs::write(race.path().join("replacement-input.json"), &replacement).unwrap();
    eprintln!("before locator: {}", String::from_utf8_lossy(&old));
    let mut publisher = None;
    discovery::remove_with_checkpoint(race.path(), &old, || {
        publisher = Some(OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "publisher_subprocess",
                    "--nocapture",
                ])
                .env("LINKSTART_DISCOVERY_TEST_DIRECTORY", race.path())
                .spawn()
                .unwrap(),
        ));
        wait_until(
            "publisher completed or observed real owner contention",
            || race.path().join("published").exists() || race.path().join("contended").exists(),
        );
        eprintln!(
            "cleanup checkpoint: published={}, contended={}",
            race.path().join("published").exists(),
            race.path().join("contended").exists()
        );
    })
    .unwrap();
    let publisher = publisher.as_mut().unwrap();
    let mut exit = None;
    wait_until("publisher subprocess exit", || {
        exit = publisher.0.try_wait().unwrap();
        exit.is_some()
    });
    assert!(exit.unwrap().success());
    let after = fs::read(race.path().join("daemon.json"));
    eprintln!("after locator: {after:?}");

    let mut lookup = Command::new(env!("CARGO_BIN_EXE_linkstart"))
        .args(["tool", "--state-dir"])
        .arg(race.path())
        .arg("--credential-file")
        .arg(live.path().join("v2-owner.token"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    lookup
        .stdin
        .take()
        .unwrap()
        .write_all(
            &serde_json::to_vec(&json!({
                "request_id": "discovery-owner-witness",
                "tool": "spaces.list",
                "args": {}
            }))
            .unwrap(),
        )
        .unwrap();
    let lookup = lookup.wait_with_output().unwrap();
    eprintln!("generic CLI exit: {}", lookup.status);
    assert!(
        after.as_ref().is_ok_and(|bytes| bytes == &replacement) && lookup.status.success(),
        "replacement must remain complete and discoverable; after={after:?}; CLI={lookup:?}"
    );
}
