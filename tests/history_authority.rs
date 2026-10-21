use linkstart::{
    runtime_history::{HistoryStore, Retention},
    runtime_terminal::SessionView,
};
use std::time::Duration;

#[test]
#[ignore = "subprocess helper invoked only by cross-process authority test"]
fn process_new_writer() {
    let dir = std::path::PathBuf::from(std::env::var_os("HISTORY_AUTHORITY_STATE").unwrap());
    std::fs::write(dir.join("child-started"), b"started").unwrap();
    let history = HistoryStore::open(&dir, retention()).unwrap();
    let mut session = view("process-session");
    let sink = history.record_session("endpoint", &session).unwrap();
    assert!(sink.output(b"cross-process durable history"));
    session.status = "exited".into();
    sink.status(&session);
    sink.close_output();
    history.shutdown_flush(Duration::from_secs(3)).unwrap();
    std::fs::write(dir.join("child-completed"), b"completed").unwrap();
}

#[test]
fn root_os_lock_serializes_cross_process_open_and_fences_old_registration() {
    let dir = tempfile::tempdir().unwrap();
    let old = HistoryStore::open(dir.path(), retention()).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("connections/.writer-lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "process_new_writer", "--nocapture"])
        .env("HISTORY_AUTHORITY_STATE", dir.path())
        .spawn()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while !dir.path().join("child-started").exists() {
            assert!(std::time::Instant::now() < until, "child startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(child.try_wait().unwrap().is_none());
        assert!(
            !dir.path().join("child-completed").exists(),
            "open must not bypass root OS lock"
        );
        drop(lock);
        let until = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                std::time::Instant::now() < until,
                "child authority operation deadlocked"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let journal = dir
            .path()
            .join("connections/endpoint/sessions/process-session/events.jsonl");
        let before = std::fs::read(&journal).unwrap();
        assert!(old
            .record_session("endpoint", &view("old-process-session"))
            .is_err());
        assert_eq!(std::fs::read(journal).unwrap(), before);
    }));
    if result.is_err() {
        let _ = child.kill();
    }
    child.wait().unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[cfg(windows)]
#[test]
fn native_large_final_output_reaches_eof_and_is_durable() {
    use linkstart::runtime_terminal::TerminalManager;
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryStore::open(
        dir.path(),
        Retention {
            queue_capacity: 4096,
            ..retention()
        },
    )
    .unwrap();
    let manager = TerminalManager::new();
    let native = manager.create_recorded("endpoint", &history, "cmd.exe".into(), vec![
        "/D".into(), "/Q".into(), "/C".into(),
        "for /L %i in (1,1,4096) do @echo NATIVE_TAIL_%i_abcdefghijklmnopqrstuvwxyz0123456789".into(),
    ], None).unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(20);
    while manager
        .list()
        .iter()
        .any(|s| s.id == native.id && s.status != "exited")
    {
        assert!(
            std::time::Instant::now() < until,
            "native process exit timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(manager.shutdown(Duration::from_secs(1)).is_empty());
    history.shutdown_flush(Duration::from_secs(5)).unwrap();
    // Native exit, reader EOF and a durable barrier are complete before reading.
    {
        let mut cursor = 0;
        let mut bytes = Vec::new();
        let mut gap = false;
        let checkpoint = loop {
            let page = history.read("endpoint", &native.id, cursor, 1000).unwrap();
            bytes.extend(page.events.iter().flat_map(|e| e.bytes.clone()));
            gap |= page.gap;
            cursor = page.next_cursor;
            if cursor >= page.checkpoint.durable_cursor {
                break page.checkpoint;
            }
        };
        assert!(!checkpoint.unknown_tail);
        assert!(String::from_utf8_lossy(&bytes).contains("NATIVE_TAIL_4096_"));
        assert!(
            !gap,
            "large-tail run must retain actual bytes without a Gap"
        );
        assert!(bytes.len() > 128 * 1024);
        assert_eq!(bytes.len() as u64, checkpoint.raw_cursor);
        let text = String::from_utf8_lossy(&bytes);
        for index in 1..=4096 {
            assert!(
                text.contains(&format!("NATIVE_TAIL_{index}_")),
                "missing native line {index}"
            );
        }
        println!(
            "native tail readback: {} durable raw bytes, process exited",
            bytes.len()
        );
    }
}

fn view(id: &str) -> SessionView {
    SessionView {
        id: id.into(),
        generation: 1,
        status: "live".into(),
        process_id: None,
        instance_id: "public-host".into(),
    }
}
fn retention() -> Retention {
    Retention {
        max_sessions_per_endpoint: 1,
        ..Retention::default()
    }
}

#[test]
fn fenced_store_cannot_register_or_retire_newer_durable_history() {
    let dir = tempfile::tempdir().unwrap();
    let old = HistoryStore::open(dir.path(), retention()).unwrap();
    let new = HistoryStore::open(dir.path(), retention()).unwrap();
    let mut session = view("new-session");
    let sink = new.record_session("endpoint", &session).unwrap();
    assert!(sink.output(b"new owner durable evidence"));
    session.status = "exited".into();
    sink.status(&session);
    sink.close_output();
    new.flush(Duration::from_secs(3)).unwrap();
    let path = dir.path().join("connections/endpoint");
    let manifest = std::fs::read(path.join("manifest.json")).unwrap();
    let journal = std::fs::read(path.join("sessions/new-session/events.jsonl")).unwrap();
    let result = old.record_session("endpoint", &view("old-session"));
    assert!(
        result.is_err(),
        "fenced store must reject registration before any retirement mutation"
    );
    assert_eq!(std::fs::read(path.join("manifest.json")).unwrap(), manifest);
    assert_eq!(
        std::fs::read(path.join("sessions/new-session/events.jsonl")).unwrap(),
        journal
    );
    assert!(!path.join("sessions/old-session").exists());
    assert_eq!(new.list("endpoint").unwrap().len(), 1);
}

#[test]
fn exited_status_cannot_retire_a_reader_before_its_tail_and_eof() {
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryStore::open(dir.path(), retention()).unwrap();
    let mut session = view("tail-session");
    let sink = history.record_session("endpoint", &session).unwrap();
    session.status = "exited".into();
    sink.status(&session);
    history.flush(Duration::from_secs(3)).unwrap();
    assert!(
        history
            .record_session("endpoint", &view("next-session"))
            .is_err(),
        "process exit must not retire an output reader before EOF"
    );
    for _ in 0..16 {
        assert!(sink.output(&vec![b'x'; 8192]));
        history.flush(Duration::from_secs(3)).unwrap();
    }
    sink.close_output();
    history.flush(Duration::from_secs(3)).unwrap();
    let page = history.read("endpoint", &session.id, 0, 1000).unwrap();
    assert_eq!(
        page.events.iter().map(|e| e.bytes.len()).sum::<usize>(),
        16 * 8192
    );
    history
        .record_session("endpoint", &view("next-session"))
        .unwrap();
}

#[test]
fn admission_queued_before_eof_cannot_overtake_later_tail_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryStore::open(dir.path(), retention()).unwrap();
    let mut session = view("queued-tail");
    let sink = history.record_session("endpoint", &session).unwrap();
    session.status = "exited".into();
    sink.status(&session);
    history.flush(Duration::from_secs(3)).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("connections/.writer-lock"))
        .unwrap();
    lock.lock().unwrap();
    let contender = history.clone();
    let (started, ready) = std::sync::mpsc::channel();
    let admission = std::thread::spawn(move || {
        started.send(()).unwrap();
        contender.record_session("endpoint", &view("next-queued-session"))
    });
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    // Hold the OS mutation lock while admission queues, then finish the reader
    // with output behind that admission. This samples this scheduling order;
    // it does not claim all possible thread schedules.
    std::thread::sleep(Duration::from_millis(150));
    assert!(sink.output(&vec![b'q'; 16 * 8192]));
    sink.close_output();
    drop(lock);
    assert!(
        admission.join().unwrap().is_err(),
        "queued tail must prevent retirement even after EOF"
    );
    history.flush(Duration::from_secs(3)).unwrap();
    let page = history.read("endpoint", &session.id, 0, 1000).unwrap();
    assert_eq!(
        page.events.iter().map(|e| e.bytes.len()).sum::<usize>(),
        16 * 8192
    );
    let next = history
        .record_session("endpoint", &view("next-queued-session"))
        .unwrap();
    next.close_output();
    history.shutdown_flush(Duration::from_secs(3)).unwrap();
}

#[test]
fn unrelated_root_lock_wait_does_not_block_healthy_history() {
    let dir = tempfile::tempdir().unwrap();
    let a = HistoryStore::open(&dir.path().join("a"), retention()).unwrap();
    let a_sink = a.record_session("endpoint", &view("healthy-a")).unwrap();
    assert!(a_sink.output(b"healthy root A baseline"));
    a.flush(Duration::from_secs(1)).unwrap();
    let b_path = dir.path().join("b");
    let b = HistoryStore::open(&b_path, retention()).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(b_path.join("connections/.writer-lock"))
        .unwrap();
    lock.lock().unwrap();
    let (started, ready) = std::sync::mpsc::channel();
    let blocked = std::thread::spawn(move || {
        started.send(()).unwrap();
        let sink = b.record_session("endpoint", &view("blocked-b"))?;
        sink.close_output();
        b.shutdown_flush(Duration::from_secs(3))
    });
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert!(a_sink.output(b"healthy root A while B waits"));
    let start = std::time::Instant::now();
    let flushed = a.flush(Duration::from_secs(1));
    println!(
        "unrelated-root A flush elapsed={:?}, result={flushed:?}",
        start.elapsed()
    );
    let reader = a.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let reading = std::thread::spawn(move || {
        let _ = tx.send(reader.read("endpoint", "healthy-a", 0, 1000));
    });
    let independent_read = rx.recv_timeout(Duration::from_secs(1));
    // Always release this run's lock/join its workers before asserting, including RED.
    drop(lock);
    blocked.join().unwrap().unwrap();
    reading.join().unwrap();
    assert!(
        flushed.is_ok(),
        "unrelated root B must not consume A's flush budget: {flushed:?}"
    );
    let page = independent_read
        .expect("unrelated root B must not block A's read")
        .unwrap();
    let bytes: Vec<_> = page.events.iter().flat_map(|e| e.bytes.clone()).collect();
    assert_eq!(
        bytes,
        b"healthy root A baselinehealthy root A while B waits"
    );
    a_sink.close_output();
    a.shutdown_flush(Duration::from_secs(3)).unwrap();
}

#[test]
fn canonical_root_alias_still_waits_for_same_lock_and_fences_old_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let old = HistoryStore::open(&root, retention()).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("connections/.writer-lock"))
        .unwrap();
    lock.lock().unwrap();
    let alias = root.join(".");
    assert_eq!(
        std::fs::canonicalize(&root).unwrap(),
        std::fs::canonicalize(&alias).unwrap()
    );
    let (ready, started) = std::sync::mpsc::channel();
    let (tx, rx) = std::sync::mpsc::channel();
    let opener = std::thread::spawn(move || {
        ready.send(()).unwrap();
        let _ = tx.send(HistoryStore::open(&alias, retention()));
    });
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    let early = rx.recv_timeout(Duration::from_millis(150));
    drop(lock);
    let bypassed = early.is_ok();
    let new = match early {
        Ok(result) => result,
        Err(_) => rx.recv_timeout(Duration::from_secs(3)).unwrap(),
    }
    .unwrap();
    opener.join().unwrap();
    assert!(
        !bypassed,
        "canonical alias must not bypass same-root OS authority"
    );
    let sink = new.record_session("endpoint", &view("alias-new")).unwrap();
    sink.close_output();
    new.shutdown_flush(Duration::from_secs(3)).unwrap();
    assert!(old.record_session("endpoint", &view("alias-old")).is_err());
}
