use linkstart::runtime_core::{owner_token_path, Engine, ToolRequest};
use serde_json::{json, Value};
use std::path::PathBuf;

fn request(id: &str, tool: &str, space_id: Option<&str>, args: Value) -> ToolRequest {
    ToolRequest {
        request_id: id.into(),
        tool: tool.into(),
        space_id: space_id.map(str::to_owned),
        args,
    }
}

// Run against a dedicated local directory. Credentials stay in this host process.
fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!("usage: cargo run --example runtime_core -- <dedicated-state-dir>")
    })?);
    let engine = Engine::open(&dir)?;
    let owner = std::fs::read_to_string(owner_token_path(&dir))?;
    let space_request = request(
        "example-space-v1",
        "spaces.create",
        None,
        json!({"name":"Library example", "administrator_name":"Local human"}),
    );
    let created = engine.execute(&owner, &space_request)?;
    let sid = created["space"]["id"].as_str().unwrap();
    let administrator = created["administrator"]["credential"].as_str().unwrap();
    let task_request = request(
        "example-task-v1",
        "topics.create",
        Some(sid),
        json!({"title":"Verify durable core", "body":"Acceptance: read after reopen", "kind":"task"}),
    );
    let task = engine.execute(administrator, &task_request)?;
    drop(engine);
    let reopened = Engine::open(&dir)?;
    anyhow::ensure!(
        reopened.execute(&owner, &space_request)? == created,
        "bootstrap receipt changed"
    );
    anyhow::ensure!(
        reopened.execute(administrator, &task_request)? == task,
        "task receipt changed"
    );
    let readback = reopened.execute(
        administrator,
        &request(
            "example-read",
            "topics.read",
            Some(sid),
            json!({"topic_id":task["id"]}),
        ),
    )?;
    anyhow::ensure!(readback["topic"] == task, "persisted topic changed");
    println!(
        "{}",
        json!({"space_id":sid,"topic":readback["topic"],"reopened":true,"deduplicated":true})
    );
    Ok(())
}
