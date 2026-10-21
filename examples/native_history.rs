//! Run on Windows: cargo run --example native_history --locked -- <new-state-directory>
//! Calls the public library as a trusted in-process host; it does not authenticate users.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use linkstart::{
        runtime_history::{HistoryStore, Retention},
        runtime_terminal::TerminalManager,
    };
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };
    let state = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("provide a new state directory")?,
    );
    std::fs::create_dir(&state)?;
    let history = HistoryStore::open(&state, Retention::default())?;
    let manager = TerminalManager::new();
    let first = manager.create_recorded(
        "example",
        &history,
        "cmd.exe".into(),
        vec!["/Q".into()],
        None,
    )?;
    let second = manager.create_recorded(
        "example",
        &history,
        "cmd.exe".into(),
        vec!["/Q".into()],
        None,
    )?;
    assert_ne!(first.id, second.id);
    assert_ne!(first.process_id, second.process_id);
    assert_eq!(manager.list().len(), 2);
    println!(
        "doctor: two distinct owned native sessions: {} {}",
        first.id, second.id
    );
    let old = manager.acquire(&first.id, "trusted-host-a")?;
    assert!(manager.acquire(&first.id, "trusted-host-b").is_err());
    manager.release(&first.id, "trusted-host-a", &old.lease)?;
    let control = manager.acquire(&first.id, "trusted-host-b")?;
    assert!(control.generation > old.generation);
    assert!(manager
        .write(
            &first.id,
            "trusted-host-a",
            &old.lease,
            old.generation,
            b"echo STALE\r\n"
        )
        .is_err());
    manager.renew(
        &first.id,
        "trusted-host-b",
        &control.lease,
        control.generation,
    )?;
    manager.resize(
        &first.id,
        "trusted-host-b",
        &control.lease,
        control.generation,
        100,
        30,
    )?;
    let other = manager.acquire(&second.id, "trusted-host-c")?;
    manager.write(
        &first.id,
        "trusted-host-b",
        &control.lease,
        control.generation,
        b"set /a 314159*2\r\n",
    )?;
    manager.write(
        &second.id,
        "trusted-host-c",
        &other.lease,
        other.generation,
        b"set /a 271828*3\r\n",
    )?;
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let a = manager.observe(&first.id, 0)?;
        let b = manager.observe(&second.id, 0)?;
        if a.text.contains("628318") && b.text.contains("815484") {
            assert!(!a.text.contains("815484") && !b.text.contains("628318"));
            assert!(!a.text.contains("STALE"));
            println!(
                "output: distinct computed answers 628318 and 815484; released writer rejected"
            );
            break;
        }
        if Instant::now() >= until {
            return Err("native output timed out".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    manager.write(
        &first.id,
        "trusted-host-b",
        &control.lease,
        control.generation,
        b"exit\r\n",
    )?;
    manager.write(
        &second.id,
        "trusted-host-c",
        &other.lease,
        other.generation,
        b"exit\r\n",
    )?;
    let until = Instant::now() + Duration::from_secs(15);
    while manager.list().iter().any(|s| s.status != "exited") {
        if Instant::now() >= until {
            return Err("native owned tree exit timed out".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(manager.shutdown(Duration::from_secs(2)).is_empty());
    history.shutdown_flush(Duration::from_secs(5))?;
    let mut expected = Vec::new();
    for (session, answer) in [(&first, "628318"), (&second, "815484")] {
        let page = history.read("example", &session.id, 0, 1000)?;
        let bytes: Vec<u8> = page.events.iter().flat_map(|e| e.bytes.clone()).collect();
        assert!(String::from_utf8_lossy(&bytes).contains(answer));
        assert_eq!(page.checkpoint.status, "exited");
        assert_eq!(page.checkpoint.process_resume, "unsupported");
        let path = state.join("connections/example/sessions").join(&session.id);
        let checkpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("checkpoint.json"))?)?;
        assert_eq!(checkpoint["status"], "exited");
        // Independent filesystem readback, rather than trusting only the library response.
        let mut disk_bytes = Vec::new();
        for entry in std::fs::read_dir(&path)? {
            let file = entry?.path();
            if file.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                for line in std::fs::read_to_string(file)?.lines() {
                    let event: linkstart::runtime_history::HistoryEvent =
                        serde_json::from_str(line)?;
                    disk_bytes.extend(event.bytes);
                }
            }
        }
        assert_eq!(disk_bytes, bytes);
        println!(
            "persisted: {} bytes={} checkpoint={}",
            session.id,
            bytes.len(),
            checkpoint
        );
        expected.push((session.id.clone(), bytes));
    }
    drop(manager);
    drop(history);
    let reopened = HistoryStore::open(&state, Retention::default())?;
    assert_eq!(reopened.list("example")?.len(), 2);
    for (id, bytes) in expected {
        let page = reopened.read("example", &id, 0, 1000)?;
        let recovered: Vec<u8> = page.events.iter().flat_map(|e| e.bytes.clone()).collect();
        assert_eq!(recovered, bytes);
        assert_eq!(page.checkpoint.process_resume, "unsupported");
    }
    reopened.shutdown_flush(Duration::from_secs(5))?;
    println!("PASS: public library control/output, two native exits, durable disk readback and reopen; state={}", state.display());
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This example requires Windows cmd.exe and ConPTY; Unix owned-tree shutdown is unsupported.");
    std::process::exit(2);
}
