use linkstart::{
    runtime_core::{owner_token_path, Engine, ToolRequest},
    runtime_operations::OperationService,
    runtime_source_jobs::SourceJobs,
};
use serde_json::{json, Value};
fn request(id: &str, tool: &str, sid: Option<&str>, args: Value) -> ToolRequest {
    ToolRequest {
        request_id: id.into(),
        tool: tool.into(),
        space_id: sid.map(Into::into),
        args,
    }
}
#[test]
fn forum_lookup_is_identity_scoped_durable_and_does_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let space = engine
        .execute(
            &owner,
            &request(
                "space",
                "spaces.create",
                None,
                json!({"name":"receipts", "manager_name":"other identity"}),
            ),
        )
        .unwrap();
    let sid = space["space"]["id"].as_str().unwrap();
    let admin = space["administrator"]["credential"].as_str().unwrap();
    let manager = space["manager"]["credential"].as_str().unwrap();
    let endpoint = engine
        .execute(
            admin,
            &request(
                "endpoint",
                "endpoints.register",
                Some(sid),
                json!({"name":"terminal", "kind":"terminal"}),
            ),
        )
        .unwrap();
    let eid = endpoint["id"].as_str().unwrap();
    let original = engine
        .execute(
            admin,
            &request(
                "create",
                "topics.create",
                Some(sid),
                json!({"title":"receipt", "body":"original", "kind":"task"}),
            ),
        )
        .unwrap();
    let claim = engine.execute(admin, &request("claim", "task.act", Some(sid), json!({"topic_id":original["id"],"action":"claim","comment":"claim work","expected_revision":0}))).unwrap();
    let comment = engine.execute(admin, &request("comment", "task.act", Some(sid), json!({"topic_id":original["id"],"action":"comment","comment":"progress","expected_revision":1}))).unwrap();
    let lookup = request(
        "lookup",
        "operations.get",
        Some(sid),
        json!({"endpoint_id":eid,"source_kind":"forum","operation_request_id":"create"}),
    );
    let jobs = SourceJobs::open(dir.path(), engine.clone()).unwrap();
    let service = OperationService::new(engine.clone(), jobs);
    let conn = rusqlite::Connection::open(dir.path().join("v2.sqlite3")).unwrap();
    let before: String = conn
        .query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let result = service.execute(admin, &lookup).unwrap();
    assert_eq!(result["state"], "confirmed");
    assert_eq!(result["receipt"], original);
    for (id, expected) in [("claim", &claim), ("comment", &comment)] {
        let mut action_lookup = lookup.clone();
        action_lookup.args["operation_request_id"] = json!(id);
        let action = service.execute(admin, &action_lookup).unwrap();
        assert_eq!(action["tool"], "task.act");
        assert_eq!(action["receipt"], *expected);
    }
    assert_eq!(
        service.execute(manager, &lookup).unwrap()["state"],
        "unknown"
    );
    let after: String = conn
        .query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    drop(service);
    let reopened = Engine::open(dir.path()).unwrap();
    let service = OperationService::new(
        reopened.clone(),
        SourceJobs::open(dir.path(), reopened).unwrap(),
    );
    assert_eq!(
        service.execute(admin, &lookup).unwrap()["receipt"],
        original
    );
    let sensitive = request(
        "sensitive",
        "operations.get",
        Some(sid),
        json!({"endpoint_id":eid,"source_kind":"forum","operation_request_id":"space"}),
    );
    assert_eq!(
        service.execute(&owner, &sensitive).unwrap_err().code,
        "Unsupported"
    );
}

#[test]
fn current_grant_role_and_credential_fence_existing_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let space = engine
        .execute(
            &owner,
            &request("space", "spaces.create", None, json!({"name":"auth"})),
        )
        .unwrap();
    let sid = space["space"]["id"].as_str().unwrap();
    let admin = space["administrator"]["credential"].as_str().unwrap();
    let invite = engine
        .execute(
            admin,
            &request(
                "invite",
                "members.invite",
                Some(sid),
                json!({"roles":["implementer"]}),
            ),
        )
        .unwrap();
    let worker = engine
        .execute(
            invite["invite_token"].as_str().unwrap(),
            &request(
                "join",
                "members.register",
                Some(sid),
                json!({"name":"worker"}),
            ),
        )
        .unwrap();
    let token = worker["credential"].as_str().unwrap();
    let endpoint = engine
        .execute(
            admin,
            &request(
                "endpoint",
                "endpoints.register",
                Some(sid),
                json!({"name":"local", "kind":"terminal"}),
            ),
        )
        .unwrap();
    let other = engine
        .execute(
            admin,
            &request(
                "other-endpoint",
                "endpoints.register",
                Some(sid),
                json!({"name":"other", "kind":"terminal"}),
            ),
        )
        .unwrap();
    let eid = endpoint["id"].as_str().unwrap();
    let other_id = other["id"].as_str().unwrap();
    for (id, endpoint) in [("grant", eid), ("grant-other", other_id)] {
        engine.execute(admin, &request(id, "endpoints.grant", Some(sid), json!({"endpoint_id":endpoint,"member_id":worker["id"],"operations":["observe"]}))).unwrap();
    }
    let create = request(
        "topic",
        "topics.create",
        Some(sid),
        json!({"title":"auth topic", "body":"body", "kind":"task"}),
    );
    engine
        .execute_requiring_endpoint(token, &create, eid, "observe")
        .unwrap();
    let role_create = request(
        "role-topic",
        "topics.create",
        Some(sid),
        json!({"title":"role topic", "body":"body", "kind":"task"}),
    );
    engine
        .execute_requiring_role(token, &role_create, "implementer")
        .unwrap();
    let service = OperationService::new(
        engine.clone(),
        SourceJobs::open(dir.path(), engine.clone()).unwrap(),
    );
    let lookup = request(
        "read",
        "operations.get",
        Some(sid),
        json!({"endpoint_id":other_id,"source_kind":"forum","operation_request_id":"topic"}),
    );
    assert_eq!(
        service.execute(token, &lookup).unwrap()["state"],
        "confirmed"
    );
    engine
        .execute(
            admin,
            &request(
                "revoke-grant",
                "endpoints.grant",
                Some(sid),
                json!({"endpoint_id":eid,"member_id":worker["id"],"operations":[]}),
            ),
        )
        .unwrap();
    // The query endpoint is still authorized, but the original endpoint is revoked.
    assert_eq!(service.execute(token, &lookup).unwrap_err().code, "Denied");
    engine
        .execute(
            admin,
            &request(
                "lower-role",
                "members.set_roles",
                Some(sid),
                json!({"member_id":worker["id"],"roles":["observer"]}),
            ),
        )
        .unwrap();
    let role_lookup = request(
        "read-role",
        "operations.get",
        Some(sid),
        json!({"endpoint_id":other_id,"source_kind":"forum","operation_request_id":"role-topic"}),
    );
    assert_eq!(
        service.execute(token, &role_lookup).unwrap_err().code,
        "Denied"
    );
    engine
        .execute(
            admin,
            &request(
                "revoke-token",
                "members.revoke_credential",
                Some(sid),
                json!({"member_id":worker["id"],"expected_epoch":0}),
            ),
        )
        .unwrap();
    assert_eq!(
        service.execute(token, &role_lookup).unwrap_err().code,
        "Unauthenticated"
    );
    assert_eq!(
        service
            .execute(invite["invite_token"].as_str().unwrap(), &lookup)
            .unwrap_err()
            .code,
        "Unauthenticated"
    );
}

#[test]
fn legacy_and_sensitive_receipts_do_not_hydrate_and_unknown_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let space = engine
        .execute(
            &owner,
            &request("space", "spaces.create", None, json!({"name":"legacy"})),
        )
        .unwrap();
    let sid = space["space"]["id"].as_str().unwrap();
    let admin = space["administrator"]["credential"].as_str().unwrap();
    let endpoint = engine
        .execute(
            admin,
            &request(
                "endpoint",
                "endpoints.register",
                Some(sid),
                json!({"name":"local", "kind":"terminal"}),
            ),
        )
        .unwrap();
    let eid = endpoint["id"].as_str().unwrap();
    engine
        .execute(
            admin,
            &request(
                "invite",
                "members.invite",
                Some(sid),
                json!({"roles":["observer"]}),
            ),
        )
        .unwrap();
    engine
        .execute(
            admin,
            &request(
                "topic",
                "topics.create",
                Some(sid),
                json!({"title":"legacy", "body":"body", "kind":"task"}),
            ),
        )
        .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("v2.sqlite3")).unwrap();
    let raw: String = db
        .query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let mut state: Value = serde_json::from_str(&raw).unwrap();
    for receipt in state["receipts"].as_array_mut().unwrap() {
        if receipt["request_id"] == "topic" {
            receipt.as_object_mut().unwrap().remove("operation_scope");
        }
    }
    let before = serde_json::to_string(&state).unwrap();
    db.execute("UPDATE core_state SET json=?1 WHERE id=1", [&before])
        .unwrap();
    let service = OperationService::new(
        engine.clone(),
        SourceJobs::open(dir.path(), engine).unwrap(),
    );
    for id in ["invite", "topic"] {
        let lookup = request(
            "read",
            "operations.get",
            Some(sid),
            json!({"endpoint_id":eid,"source_kind":"forum","operation_request_id":id}),
        );
        assert_eq!(
            service.execute(admin, &lookup).unwrap_err().code,
            "Unsupported"
        );
    }
    let mut lookup = request(
        "read",
        "operations.get",
        Some(sid),
        json!({"endpoint_id":eid,"source_kind":"forum","operation_request_id":"unrecorded"}),
    );
    let missing = service.execute(admin, &lookup).unwrap();
    assert_eq!(missing["state"], "unknown");
    assert_eq!(missing["lookup"], "not_found");
    assert!(missing["evidence"]
        .as_str()
        .unwrap()
        .contains("external effect is unknown"));
    lookup.args.as_object_mut().unwrap().remove("source_kind");
    assert_eq!(
        service.execute(admin, &lookup).unwrap_err().code,
        "Unsupported"
    );
    let after: String = db
        .query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn real_source_job_unknown_survives_reopen_without_remote_reads_or_resend() {
    use linkstart::runtime_source::{RepositoryPolicy, SourceConfig};
    use std::{
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicU64::new(100_000));
    let read_clock = clock.clone();
    let engine = Engine::open_with_clock(
        dir.path(),
        Arc::new(move || read_clock.load(Ordering::SeqCst)),
    )
    .unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let space = engine
        .execute(
            &owner,
            &request(
                "space",
                "spaces.create",
                None,
                json!({"name":"source", "manager_name":"other"}),
            ),
        )
        .unwrap();
    let sid = space["space"]["id"].as_str().unwrap();
    let admin = space["administrator"]["credential"].as_str().unwrap();
    let other = space["manager"]["credential"].as_str().unwrap();
    let endpoint = engine.execute(admin, &request("endpoint", "endpoints.register", Some(sid), json!({"name":"source", "kind":"github", "config":{"repository":"example/project", "allow_write_back":true}}))).unwrap();
    let eid = endpoint["id"].as_str().unwrap();
    let second_endpoint = engine.execute(admin, &request("endpoint-2", "endpoints.register", Some(sid), json!({"name":"other source scope", "kind":"github", "config":{"repository":"example/project"}}))).unwrap();
    let foreign = engine
        .execute(
            &owner,
            &request(
                "foreign-space",
                "spaces.create",
                None,
                json!({"name":"other space"}),
            ),
        )
        .unwrap();
    let foreign_sid = foreign["space"]["id"].as_str().unwrap();
    let foreign_endpoint = engine.execute(&owner, &request("foreign-endpoint", "endpoints.register", Some(foreign_sid), json!({"name":"foreign", "kind":"github", "config":{"repository":"example/project"}}))).unwrap();
    let script = dir.path().join("gh.py");
    let calls = dir.path().join("calls.txt");
    std::fs::write(&script, r#"import json,pathlib,sys,time
p=pathlib.Path(sys.argv[1])
with p.open('a') as f:f.write(' '.join(sys.argv[2:])+'\n')
if 'POST' in sys.argv:
    body=json.load(sys.stdin)['body']
    if not body.startswith('confirmed'):sys.exit(1)
    p.with_suffix('.entered').write_text('accepted input')
    deadline=time.monotonic()+15
    while not p.with_suffix('.release').exists():
        if time.monotonic()>deadline:sys.exit(2)
        time.sleep(.01)
    print(json.dumps({'id':42,'body':body,'html_url':'https://github.com/example/project/issues/1#issuecomment-42','user':{'id':7,'login':'fake-gh','node_id':'fake-user'}}))
else:print('[[]]')
"#).unwrap();
    let ledger = dir.path().join("source.sqlite3");
    let mut config = SourceConfig::new(ledger.clone());
    config.repositories = vec![RepositoryPolicy {
        repository: "example/project".into(),
        allow_write_back: true,
    }];
    config.gh_program = "python".into();
    config.gh_prefix_args = vec![
        script.to_str().unwrap().into(),
        calls.to_str().unwrap().into(),
    ];
    let factory = Arc::new(move |_: &std::path::Path, _: &Value| Ok(config.clone()));
    let jobs_path = dir.path().join("jobs");
    let jobs =
        SourceJobs::open_with_config_factory(&jobs_path, engine.clone(), factory.clone()).unwrap();
    jobs.start(
        admin,
        &request(
            "write",
            "sources.writeback",
            Some(sid),
            json!({"endpoint_id":eid,"number":1,"body":"reply"}),
        ),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while jobs.active_count() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(jobs.get(admin, "write").unwrap().status, "unknown");
    // The same request ID legally names distinct records in the two stores.
    let forum = engine
        .execute(
            admin,
            &request(
                "write",
                "topics.create",
                Some(sid),
                json!({"title":"forum with source ID", "body":"body", "kind":"task"}),
            ),
        )
        .unwrap();
    jobs.start(
        admin,
        &request(
            "confirmed-write",
            "sources.writeback",
            Some(sid),
            json!({"endpoint_id":eid,"number":1,"body":"confirmed reply"}),
        ),
    )
    .unwrap();
    while !calls.with_extension("entered").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let pending_service = OperationService::new(engine.clone(), jobs.clone());
    let pending_lookup = request(
        "pending-lookup",
        "operations.get",
        Some(sid),
        json!({"endpoint_id":eid,"source_kind":"source_job","operation_request_id":"confirmed-write"}),
    );
    let pending = pending_service.execute(admin, &pending_lookup);
    // Always release the real process before asserting the query result.
    std::fs::write(calls.with_extension("release"), "continue").unwrap();
    while jobs.active_count() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pending.unwrap()["state"], "pending");
    assert_eq!(
        pending_service.execute(admin, &pending_lookup).unwrap()["state"],
        "confirmed"
    );
    assert_eq!(
        jobs.get(admin, "confirmed-write")
            .unwrap()
            .write_receipt
            .unwrap()
            .comment_id,
        Some(42)
    );
    drop(pending_service);
    // Also complete a real read-only sync through the same process boundary.
    jobs.start(
        admin,
        &request(
            "sync",
            "sources.sync",
            Some(sid),
            json!({"endpoint_id":eid}),
        ),
    )
    .unwrap();
    while jobs.active_count() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let original_calls = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(
        original_calls
            .lines()
            .filter(|l| l.contains("POST"))
            .count(),
        2
    );
    drop(jobs);
    let read_clock = clock.clone();
    let reopened = Engine::open_with_clock(
        dir.path(),
        Arc::new(move || read_clock.load(Ordering::SeqCst)),
    )
    .unwrap();
    let auth_engine = reopened.clone();
    let jobs = SourceJobs::open_with_config_factory(&jobs_path, reopened.clone(), factory).unwrap();
    let service = OperationService::new(reopened, jobs);
    let core_db = rusqlite::Connection::open(dir.path().join("v2.sqlite3")).unwrap();
    let jobs_db = rusqlite::Connection::open(jobs_path.join("source-jobs.sqlite3")).unwrap();
    use sha2::{Digest, Sha256};
    let source_db = rusqlite::Connection::open(
        jobs_path
            .join("sources")
            .join(format!("{:x}.sqlite3", Sha256::digest(eid.as_bytes()))),
    )
    .unwrap();
    let read = || -> (String, Vec<String>, Vec<String>) {
        let core = core_db
            .query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0))
            .unwrap();
        let records = jobs_db
            .prepare("SELECT record FROM source_jobs ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap();
        let writes = source_db
            .prepare("SELECT receipt FROM writes ORDER BY request_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap();
        (core, records, writes)
    };
    let before = read();
    let lookup = request(
        "lookup",
        "operations.get",
        Some(sid),
        json!({"endpoint_id":eid,"source_kind":"source_job","operation_request_id":"write"}),
    );
    for _ in 0..3 {
        assert_eq!(service.execute(admin, &lookup).unwrap()["state"], "unknown");
    }
    let mut forum_lookup = lookup.clone();
    forum_lookup.args["source_kind"] = json!("forum");
    assert_eq!(
        service.execute(admin, &forum_lookup).unwrap()["receipt"],
        forum
    );
    let mut wrong_endpoint = lookup.clone();
    wrong_endpoint.args["endpoint_id"] = second_endpoint["id"].clone();
    assert_eq!(
        service.execute(admin, &wrong_endpoint).unwrap_err().code,
        "Denied"
    );
    let mut cross_space = lookup.clone();
    cross_space.space_id = Some(foreign_sid.into());
    cross_space.args["endpoint_id"] = foreign_endpoint["id"].clone();
    for kind in ["forum", "source_job"] {
        cross_space.args["source_kind"] = json!(kind);
        assert_eq!(
            service.execute(admin, &cross_space).unwrap_err().code,
            "Denied"
        );
    }
    // Owner is not a cross-identity receipt recovery mechanism either.
    assert_eq!(service.execute(&owner, &lookup).unwrap_err().code, "Denied");
    assert_eq!(service.execute(other, &lookup).unwrap_err().code, "Denied");
    let mut completed = lookup.clone();
    completed.args["operation_request_id"] = json!("sync");
    assert_eq!(
        service.execute(admin, &completed).unwrap()["state"],
        "confirmed"
    );
    completed.args["operation_request_id"] = json!("confirmed-write");
    let confirmed = service.execute(admin, &completed).unwrap();
    assert_eq!(confirmed["state"], "confirmed");
    assert_eq!(confirmed["receipt"]["write_receipt"]["comment_id"], 42);
    completed.args["operation_request_id"] = json!("absent");
    assert_eq!(
        service.execute(admin, &completed).unwrap()["lookup"],
        "not_found"
    );
    assert_eq!(before, read());
    assert_eq!(original_calls, std::fs::read_to_string(&calls).unwrap());
    let rotated = auth_engine.execute(&owner, &request("rotate-admin", "members.rotate_credential", Some(sid), json!({"member_id":space["administrator"]["id"],"expected_epoch":0,"ttl_seconds":1}))).unwrap();
    let current = rotated["credential"].as_str().unwrap();
    for query in [&lookup, &forum_lookup] {
        assert_eq!(
            service.execute(admin, query).unwrap_err().code,
            "Unauthenticated"
        );
        assert!(service.execute(current, query).is_ok());
    }
    clock.store(101_000, Ordering::SeqCst);
    for query in [&lookup, &forum_lookup] {
        assert_eq!(
            service.execute(current, query).unwrap_err().code,
            "Unauthenticated"
        );
    }
    assert_eq!(original_calls, std::fs::read_to_string(calls).unwrap());
}
