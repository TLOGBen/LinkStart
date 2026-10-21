use linkstart::{
    runtime_core::{owner_token_path, Engine, ToolRequest},
    runtime_source_jobs::{SourceJobSnapshot, SourceJobs},
    runtime_watch::WatchManager,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn request(id: &str, tool: &str, sid: Option<&str>, args: Value) -> ToolRequest {
    ToolRequest {
        request_id: id.into(),
        tool: tool.into(),
        space_id: sid.map(str::to_owned),
        args,
    }
}

fn wait(jobs: &SourceJobs, credential: &str, id: &str) -> anyhow::Result<SourceJobSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let snapshot = jobs.get(credential, id)?;
        if !["running", "applying"].contains(&snapshot.status.as_str()) {
            anyhow::ensure!(
                snapshot.status == "completed",
                "source job ended: {snapshot:?}"
            );
            return Ok(snapshot);
        }
        anyhow::ensure!(Instant::now() < deadline, "source job deadline exceeded");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// Reads the explicitly selected repository using the host's existing gh login.
// Bootstrap credentials stay in this process and are never printed.
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    anyhow::ensure!(
        args.len() == 2,
        "usage: cargo run --example runtime_sources -- <dedicated-state-dir> <owner/name>"
    );
    let dir = PathBuf::from(&args[0]);
    let repository = args[1]
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("repository must be UTF-8"))?;
    let engine = Engine::open(&dir)?;
    let owner = std::fs::read_to_string(owner_token_path(&dir))?;
    let created = engine.execute(
        &owner,
        &request(
            "source-example-space-v1",
            "spaces.create",
            None,
            json!({"name":"Source library example", "administrator_name":"Local human"}),
        ),
    )?;
    let sid = created["space"]["id"].as_str().unwrap();
    let credential = created["administrator"]["credential"].as_str().unwrap();
    let endpoint = engine.execute(credential, &request("source-example-endpoint-v1", "endpoints.register", Some(sid),
        json!({"name":"Explicit GitHub source", "kind":"github", "config":{"repository":repository, "allow_write_back":false}})))?;
    let eid = endpoint["id"].as_str().unwrap();
    let jobs = SourceJobs::open(&dir, engine.clone())?;
    let watches = WatchManager::open(&dir, engine.clone(), jobs.clone())?;
    let result = (|| -> anyhow::Result<Value> {
        let sync_request = request(
            "source-example-sync-v1",
            "sources.sync",
            Some(sid),
            json!({"endpoint_id":eid}),
        );
        jobs.start(credential, &sync_request)?;
        let synced = wait(&jobs, credential, &sync_request.request_id)?;
        anyhow::ensure!(
            jobs.start(credential, &sync_request)?.topic_ids == synced.topic_ids,
            "stable sync replay changed topics"
        );
        let resume = request(
            "source-example-resume-v1",
            "sources.resume",
            Some(sid),
            json!({"endpoint_id":eid, "sync_job_id":sync_request.request_id}),
        );
        jobs.start(credential, &resume)?;
        let resumed = wait(&jobs, credential, &resume.request_id)?;
        anyhow::ensure!(
            resumed.topic_ids == synced.topic_ids,
            "checkpoint resume changed topics"
        );
        let mut external_topics = 0;
        for topic_id in &synced.topic_ids {
            let read = engine.execute(
                credential,
                &request(
                    "source-example-read",
                    "topics.read",
                    Some(sid),
                    json!({"topic_id":topic_id}),
                ),
            )?;
            anyhow::ensure!(
                read["topic"]["provenance"]["kind"] == "github"
                    && read["topic"]["provenance"]["trust"] == "external",
                "source provenance missing"
            );
            external_topics += 1;
        }
        let watch = watches.start(
            credential,
            &request(
                "source-example-watch-v1",
                "sources.watch.start",
                Some(sid),
                json!({"endpoint_id":eid, "interval_seconds":30}),
            ),
        )?;
        let current = watches.get(credential, &watch.watch_id)?;
        anyhow::ensure!(
            watches
                .list(credential, sid)?
                .iter()
                .any(|w| w.watch_id == current.watch_id),
            "watch missing from list"
        );
        watches.stop(credential, &watch.watch_id)?;
        Ok(
            json!({"repository":repository, "sync_status":synced.status, "topic_count":synced.topic_ids.len(),
            "external_provenance_count":external_topics, "stable_replay":true, "checkpoint_resume":true, "watch_id":watch.watch_id}),
        )
    })();
    // Stop admission before cancelling bounded source work, even on an error.
    watches.cancel_all();
    jobs.cancel_all();
    let deadline = Instant::now() + Duration::from_secs(40);
    while watches.active_count() != 0 || jobs.active_count() != 0 {
        anyhow::ensure!(Instant::now() < deadline, "shutdown still pending");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut report = result?;
    report["watch_status"] = json!(watches.get(credential, "source-example-watch-v1")?.status);
    report["active_watches"] = json!(watches.active_count());
    report["active_source_jobs"] = json!(jobs.active_count());
    println!("{}", report);
    Ok(())
}
