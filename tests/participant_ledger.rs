use linkstart::{
    runtime_core::{owner_token_path, ApiError, Engine, ToolRequest},
    runtime_participants::{BindingContext, OwnedAdmission, ParticipantService, TrustedHost},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

struct Host;
impl TrustedHost for Host {
    fn admit(&self, context: &BindingContext) -> Result<OwnedAdmission, ApiError> {
        Ok(OwnedAdmission {
            session_id: context.session_id.clone(),
            instance_id: "owned-host".into(),
            host_generation: 1,
            adapter_kind: "test-owned".into(),
            provider_thread_id: None,
            capabilities: vec!["structured_tools".into()],
            reconciled: true,
        })
    }
}
fn req(id: &str, tool: &str, sid: &str, args: Value) -> ToolRequest {
    ToolRequest {
        request_id: id.into(),
        tool: tool.into(),
        space_id: Some(sid.into()),
        args,
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    engine: Engine,
    token: String,
    sid: String,
    eid: String,
    attachment: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let space = engine
            .execute(
                owner.trim(),
                &ToolRequest {
                    request_id: "space".into(),
                    tool: "spaces.create".into(),
                    space_id: None,
                    args: json!({"name":"participants"}),
                },
            )
            .unwrap();
        let sid = space["space"]["id"].as_str().unwrap().to_owned();
        let token = space["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_owned();
        let ep = engine
            .execute(
                &token,
                &req(
                    "endpoint",
                    "endpoints.register",
                    &sid,
                    json!({"name":"host","kind":"terminal"}),
                ),
            )
            .unwrap();
        let eid = ep["id"].as_str().unwrap().to_owned();
        let heartbeat = engine
            .execute(&token, &req("hb", "members.heartbeat", &sid, json!({})))
            .unwrap();
        let attachment = heartbeat["attachment_id"].as_str().unwrap().to_owned();
        Self {
            dir,
            engine,
            token,
            sid,
            eid,
            attachment,
        }
    }
    fn service(&self) -> ParticipantService {
        ParticipantService::open(
            self.dir.path(),
            Engine::open(self.dir.path()).unwrap(),
            Arc::new(Host),
        )
        .unwrap()
    }
    fn bind(&self, s: &ParticipantService) -> Value {
        s.execute(&self.token,&req("bind","participants.bind",&self.sid,json!({"endpoint_id":self.eid,"session_id":"owned-session","attachment_id":self.attachment,"adapter_kind":"test-owned","initial_cursor":"beginning","expected_generation":0}))).unwrap()
    }
    fn call(
        &self,
        s: &ParticipantService,
        id: &str,
        tool: &str,
        args: Value,
    ) -> Result<Value, ApiError> {
        s.execute(&self.token, &req(id, tool, &self.sid, args))
    }
}
#[test]
fn offline_activity_survives_reopen_and_checkpoint_cas_has_one_winner() {
    let f = Fixture::new();
    let service = f.service();
    let binding = f.bind(&service);
    let pid = binding["participant_id"].as_str().unwrap();
    let topic = f
        .engine
        .execute(
            &f.token,
            &req(
                "topic",
                "topics.create",
                &f.sid,
                json!({"title":"offline","body":"work","kind":"task"}),
            ),
        )
        .unwrap();
    drop(service);
    let a = f.service();
    let b = f.service();
    let page = f
        .call(
            &a,
            "read",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert!(page["next_cursor"].is_string());
    assert_eq!(page["gap"], false);
    let again = f
        .call(
            &b,
            "read2",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    assert_eq!(
        again["events"][0]["event_id"],
        page["events"][0]["event_id"]
    );
    let args = json!({"participant_id":pid,"binding_generation":1,"work_id":"work","expected_revision":0,"topic_id":topic["id"],"assignment_revision":0,"completed":[],"pending":[],"unknown":[],"artifacts":[]});
    let gate = Arc::new(std::sync::Barrier::new(2));
    let launch = |service: ParticipantService, id: &str| {
        let gate = gate.clone();
        let token = f.token.clone();
        let request = req(id, "work.checkpoint.set", &f.sid, args.clone());
        std::thread::spawn(move || {
            gate.wait();
            service.execute(&token, &request)
        })
    };
    let first = launch(a, "cp-a");
    let second = launch(b, "cp-b");
    let results = [first.join().unwrap(), second.join().unwrap()];
    assert_eq!(results.iter().filter(|x| x.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|x| x.as_ref().err())
            .next()
            .unwrap()
            .code,
        "Conflict"
    );
}

struct ChangingHost(AtomicU64);
impl TrustedHost for ChangingHost {
    fn admit(&self, c: &BindingContext) -> Result<OwnedAdmission, ApiError> {
        let mut a = Host.admit(c)?;
        a.host_generation = self.0.load(Ordering::SeqCst);
        Ok(a)
    }
}
#[test]
fn checkpoint_requires_live_owned_host_and_pause_fences_old_generation() {
    let f = Fixture::new();
    let host = Arc::new(ChangingHost(AtomicU64::new(1)));
    let service = ParticipantService::open(f.dir.path(), f.engine.clone(), host.clone()).unwrap();
    let b = f.bind(&service);
    let pid = b["participant_id"].as_str().unwrap();
    let topic = f
        .engine
        .execute(
            &f.token,
            &req(
                "t",
                "topics.create",
                &f.sid,
                json!({"title":"task","body":"work","kind":"task"}),
            ),
        )
        .unwrap();
    let cp = json!({"participant_id":pid,"binding_generation":1,"work_id":"work","expected_revision":0,"topic_id":topic["id"],"assignment_revision":0,"completed":[],"pending":[],"unknown":[],"artifacts":[]});
    host.0.store(2, Ordering::SeqCst);
    assert_eq!(
        f.call(&service, "changed", "work.checkpoint.set", cp.clone())
            .unwrap_err()
            .code,
        "Conflict"
    );
    host.0.store(1, Ordering::SeqCst);
    let pause = f
        .call(
            &service,
            "pause",
            "participants.pause",
            json!({"participant_id":pid,"expected_generation":1}),
        )
        .unwrap();
    assert_eq!(pause["provider_turn_cancelled"], false);
    assert_eq!(
        f.call(&service, "paused", "work.checkpoint.set", cp)
            .unwrap_err()
            .code,
        "Conflict"
    );
}

#[test]
fn receipt_is_durable_contiguous_and_never_completes_forum_work() {
    let f = Fixture::new();
    let a = f.service();
    let binding = f.bind(&a);
    let pid = binding["participant_id"].as_str().unwrap();
    for n in 0..2 {
        f.engine
            .execute(
                &f.token,
                &req(
                    &format!("t{n}"),
                    "topics.create",
                    &f.sid,
                    json!({"title":"task","body":"work","kind":"task"}),
                ),
            )
            .unwrap();
    }
    let page = f
        .call(
            &a,
            "read",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    let events = page["events"].as_array().unwrap();
    let receipt = |event: &Value| json!({"participant_id":pid,"binding_generation":1,"event_id":event["event_id"],"state":"handled","evidence_ref":"history:turn"});
    let second = f
        .call(
            &a,
            "ack-second",
            "participants.receipt",
            receipt(&events[1]),
        )
        .unwrap();
    assert_eq!(second["task_state_changed"], false);
    let reread = f
        .call(
            &a,
            "reread",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    assert_eq!(reread["events"].as_array().unwrap().len(), 2);
    let first = f
        .call(&a, "ack-first", "participants.receipt", receipt(&events[0]))
        .unwrap();
    drop(a);
    let b = f.service();
    assert_eq!(
        first,
        f.call(&b, "ack-first", "participants.receipt", receipt(&events[0]))
            .unwrap()
    );
    let empty = f
        .call(
            &b,
            "empty",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    assert!(empty["events"].as_array().unwrap().is_empty());
    assert!(empty["next_cursor"].is_string());
    let operation = f
        .call(
            &b,
            "op",
            "operations.get",
            json!({"participant_id":pid,"operation_request_id":events[0]["operation_request_id"]}),
        )
        .unwrap();
    assert_eq!(operation["state"], "confirmed");
    let topic = f
        .engine
        .execute(
            &f.token,
            &req(
                "topic-get",
                "topics.read",
                &f.sid,
                json!({"topic_id":events[0]["topic_id"]}),
            ),
        )
        .unwrap();
    assert_eq!(topic["topic"]["state"], "unclaimed");
}

#[test]
fn pause_and_resume_replay_are_idempotent_but_old_receipt_is_fenced() {
    let f = Fixture::new();
    let a = f.service();
    let binding = f.bind(&a);
    let pid = binding["participant_id"].as_str().unwrap();
    let args = json!({"participant_id":pid,"expected_generation":1});
    let paused = f
        .call(&a, "pause", "participants.pause", args.clone())
        .unwrap();
    assert_eq!(
        paused,
        f.call(&a, "pause", "participants.pause", args).unwrap()
    );
    let args = json!({"participant_id":pid,"expected_generation":2});
    let resumed = f
        .call(&a, "resume", "participants.resume", args.clone())
        .unwrap();
    assert_eq!(resumed["binding_generation"], 3);
    assert_eq!(
        resumed,
        f.call(&a, "resume", "participants.resume", args).unwrap()
    );
    assert_eq!(
        f.call(
            &a,
            "late",
            "participants.receipt",
            json!({"participant_id":pid,"binding_generation":1,"event_id":"late","state":"handled"})
        )
        .unwrap_err()
        .code,
        "Conflict"
    );
}

#[test]
fn checkpoint_references_only_known_durable_intents_and_lookup_is_read_only() {
    let f = Fixture::new();
    let s = f.service();
    let b = f.bind(&s);
    let pid = b["participant_id"].as_str().unwrap();
    let topic = f
        .engine
        .execute(
            &f.token,
            &req(
                "t",
                "topics.create",
                &f.sid,
                json!({"title":"task","body":"work","kind":"task"}),
            ),
        )
        .unwrap();
    let mut cp = json!({"participant_id":pid,"binding_generation":1,"work_id":"work","expected_revision":0,"topic_id":topic["id"],"assignment_revision":0,"completed":[],"pending":["invented"],"unknown":[],"artifacts":[]});
    assert_eq!(
        f.call(&s, "bad", "work.checkpoint.set", cp.clone())
            .unwrap_err()
            .code,
        "Unsupported"
    );
    assert_eq!(
        f.call(
            &s,
            "get-unknown",
            "operations.get",
            json!({"participant_id":pid,"operation_request_id":"invented"})
        )
        .unwrap_err()
        .code,
        "Unsupported"
    );
    let page = f
        .call(
            &s,
            "read",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    let rid = &page["events"][0]["operation_request_id"];
    cp["pending"] = json!([rid]);
    cp["unknown"] = json!([rid]);
    let saved = f.call(&s, "save", "work.checkpoint.set", cp).unwrap();
    assert_eq!(saved["revision"], 1);
    let operation = f
        .call(
            &s,
            "get",
            "operations.get",
            json!({"participant_id":pid,"operation_request_id":rid}),
        )
        .unwrap();
    assert_eq!(operation["state"], "pending");
    assert_eq!(
        operation,
        f.call(
            &s,
            "get-again",
            "operations.get",
            json!({"participant_id":pid,"operation_request_id":rid})
        )
        .unwrap()
    );
}

#[test]
fn explicit_initial_boundary_cursor_scope_and_gap_are_preserved() {
    let f = Fixture::new();
    let s = f.service();
    f.engine
        .execute(
            &f.token,
            &req(
                "t",
                "topics.create",
                &f.sid,
                json!({"title":"existing","body":"work","kind":"task"}),
            ),
        )
        .unwrap();
    let mut args = json!({"endpoint_id":f.eid,"session_id":"owned-session","attachment_id":f.attachment,"adapter_kind":"test-owned","expected_generation":0});
    assert_eq!(
        f.call(&s, "missing-start", "participants.bind", args.clone())
            .unwrap_err()
            .code,
        "Invalid"
    );
    args["initial_cursor"] = json!("snapshot");
    let b = f.call(&s, "snapshot", "participants.bind", args).unwrap();
    let pid = b["participant_id"].as_str().unwrap();
    let page = f
        .call(&s, "r", "participants.read", json!({"participant_id":pid}))
        .unwrap();
    assert!(page["events"].as_array().unwrap().is_empty());
    assert_eq!(page["durable_cursor"], format!("{}:1", f.sid));
    assert_eq!(
        f.call(
            &s,
            "foreign",
            "participants.read",
            json!({"participant_id":pid,"cursor":"other-space:0"})
        )
        .unwrap_err()
        .code,
        "Invalid"
    );
    let gap = f
        .call(
            &s,
            "gap",
            "participants.read",
            json!({"participant_id":pid,"cursor":format!("{}:999",f.sid)}),
        )
        .unwrap();
    assert_eq!(gap["gap"], true);
    assert!(gap["next_cursor"].is_string());
    assert_eq!(
        f.call(
            &s,
            "null",
            "participants.read",
            json!({"participant_id":pid,"cursor":null})
        )
        .unwrap_err()
        .code,
        "Invalid"
    );
}

#[test]
fn endpoint_and_credential_revocation_reject_replay_and_self_binding_is_enforced() {
    let f = Fixture::new();
    let s = f.service();
    let invite = f
        .engine
        .execute(
            &f.token,
            &req(
                "i",
                "members.invite",
                &f.sid,
                json!({"roles":["implementer"]}),
            ),
        )
        .unwrap();
    let member = f
        .engine
        .execute(
            invite["invite_token"].as_str().unwrap(),
            &req("reg", "members.register", &f.sid, json!({"name":"worker"})),
        )
        .unwrap();
    let token = member["credential"].as_str().unwrap();
    f.engine.execute(&f.token,&req("grant","endpoints.grant",&f.sid,json!({"endpoint_id":f.eid,"member_id":member["id"],"operations":["observe","send"]}))).unwrap();
    let heartbeat = f
        .engine
        .execute(
            token,
            &req("worker-hb", "members.heartbeat", &f.sid, json!({})),
        )
        .unwrap();
    let mut bind = req(
        "worker-bind",
        "participants.bind",
        &f.sid,
        json!({"endpoint_id":f.eid,"session_id":"worker-session","attachment_id":f.attachment,"adapter_kind":"test-owned","initial_cursor":"beginning","expected_generation":0}),
    );
    assert_eq!(s.execute(token, &bind).unwrap_err().code, "Denied");
    bind.args["attachment_id"] = heartbeat["attachment_id"].clone();
    let bound = s.execute(token, &bind).unwrap();
    let foreign = req(
        "foreign-pid",
        "participants.read",
        &f.sid,
        json!({"participant_id":bound["participant_id"]}),
    );
    assert_eq!(s.execute(&f.token, &foreign).unwrap_err().code, "Denied");
    f.engine
        .execute(
            &f.token,
            &req(
                "ungrant",
                "endpoints.grant",
                &f.sid,
                json!({"endpoint_id":f.eid,"member_id":member["id"],"operations":[]}),
            ),
        )
        .unwrap();
    assert_eq!(s.execute(token, &bind).unwrap_err().code, "Denied");
    f.engine.execute(&f.token,&req("regrant","endpoints.grant",&f.sid,json!({"endpoint_id":f.eid,"member_id":member["id"],"operations":["observe","send"]}))).unwrap();
    assert_eq!(s.execute(token, &bind).unwrap(), bound);
    f.engine
        .execute(
            &f.token,
            &req(
                "revoke",
                "members.revoke_credential",
                &f.sid,
                json!({"member_id":member["id"],"expected_epoch":0}),
            ),
        )
        .unwrap();
    assert_eq!(s.execute(token, &bind).unwrap_err().code, "Unauthenticated");
}

#[test]
fn explicit_rebind_preserves_participant_history_and_fences_the_previous_session() {
    let f = Fixture::new();
    let s = f.service();
    let b = f.bind(&s);
    let pid = b["participant_id"].as_str().unwrap();
    f.engine
        .execute(
            &f.token,
            &req(
                "t",
                "topics.create",
                &f.sid,
                json!({"title":"existing","body":"work","kind":"task"}),
            ),
        )
        .unwrap();
    let before = f
        .call(
            &s,
            "read",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    let rebind = json!({"participant_id":pid,"endpoint_id":f.eid,"session_id":"replacement-owned-session","attachment_id":f.attachment,"adapter_kind":"test-owned","expected_generation":1});
    let rebound = f.call(&s, "rebind", "participants.bind", rebind).unwrap();
    assert_eq!(rebound["participant_id"], pid);
    assert_eq!(rebound["binding_generation"], 2);
    let after = f
        .call(
            &s,
            "read-after",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    assert_eq!(before["events"], after["events"]);
    assert_eq!(f.call(&s,"late","participants.receipt",json!({"participant_id":pid,"binding_generation":1,"event_id":before["events"][0]["event_id"],"state":"delivered"})).unwrap_err().code,"Conflict");
}

struct UnavailableHost;
impl TrustedHost for UnavailableHost {
    fn admit(&self, _: &BindingContext) -> Result<OwnedAdmission, ApiError> {
        Err(ApiError {
            code: "Unavailable".into(),
            message: "no owned session registry entry".into(),
        })
    }
}
struct UnreconciledHost;
impl TrustedHost for UnreconciledHost {
    fn admit(&self, c: &BindingContext) -> Result<OwnedAdmission, ApiError> {
        let mut admission = Host.admit(c)?;
        admission.reconciled = false;
        Ok(admission)
    }
}
#[test]
fn restart_without_owned_host_remains_readable_but_cannot_resume_or_mutate() {
    let f = Fixture::new();
    let live = f.service();
    let b = f.bind(&live);
    let pid = b["participant_id"].as_str().unwrap();
    f.engine
        .execute(
            &f.token,
            &req(
                "t",
                "topics.create",
                &f.sid,
                json!({"title":"offline","body":"work","kind":"task"}),
            ),
        )
        .unwrap();
    drop(live);
    let unavailable = ParticipantService::open(
        f.dir.path(),
        Engine::open(f.dir.path()).unwrap(),
        Arc::new(UnavailableHost),
    )
    .unwrap();
    let page = f
        .call(
            &unavailable,
            "r",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        f.call(
            &unavailable,
            "resume",
            "participants.resume",
            json!({"participant_id":pid,"expected_generation":1})
        )
        .unwrap_err()
        .code,
        "Unavailable"
    );
    assert_eq!(f.call(&unavailable,"ack","participants.receipt",json!({"participant_id":pid,"binding_generation":1,"event_id":page["events"][0]["event_id"],"state":"delivered"})).unwrap_err().code,"Unavailable");
    let paused = f
        .call(
            &unavailable,
            "pause",
            "participants.pause",
            json!({"participant_id":pid,"expected_generation":1}),
        )
        .unwrap();
    assert_eq!(paused["provider_turn_cancelled"], false);
    let unknown = ParticipantService::open(
        f.dir.path(),
        Engine::open(f.dir.path()).unwrap(),
        Arc::new(UnreconciledHost),
    )
    .unwrap();
    assert_eq!(
        f.call(
            &unknown,
            "resume-unknown",
            "participants.resume",
            json!({"participant_id":pid,"expected_generation":2})
        )
        .unwrap_err()
        .code,
        "Conflict"
    );
    let state = f
        .call(
            &unknown,
            "state",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    assert_eq!(state["paused"], true);
    assert_eq!(state["binding_generation"], 2);
}

#[test]
fn operation_lookup_never_changes_durable_intents_and_credentials_are_not_saved() {
    let f = Fixture::new();
    let s = f.service();
    let b = f.bind(&s);
    let pid = b["participant_id"].as_str().unwrap();
    f.engine
        .execute(
            &f.token,
            &req(
                "t",
                "topics.create",
                &f.sid,
                json!({"title":"task","body":"work","kind":"task"}),
            ),
        )
        .unwrap();
    let page = f
        .call(&s, "r", "participants.read", json!({"participant_id":pid}))
        .unwrap();
    let db = rusqlite::Connection::open(f.dir.path().join("v2-participants.sqlite")).unwrap();
    let saved = || {
        db.query_row(
            "SELECT json FROM participant_ledger WHERE id=1",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    let before = saved();
    f.call(&s,"lookup","operations.get",json!({"participant_id":pid,"operation_request_id":page["events"][0]["operation_request_id"]})).unwrap();
    assert_eq!(before, saved());
    assert!(!before.contains(&f.token));
    assert_eq!(f.call(&s,"leak","participants.receipt",json!({"participant_id":pid,"binding_generation":1,"event_id":page["events"][0]["event_id"],"state":"handled","evidence_ref":f.token})).unwrap_err().code,"Invalid");
    assert_eq!(before, saved());
    assert_eq!(f.call(&s,"self-capability","participants.bind",json!({"endpoint_id":f.eid,"session_id":"fake","attachment_id":f.attachment,"adapter_kind":"test-owned","initial_cursor":"beginning","expected_generation":0,"capabilities":["shell"]})).unwrap_err().code,"Invalid");
}

#[test]
fn ledger_rejects_an_unrelated_storage_directory() {
    let f = Fixture::new();
    let unrelated = tempfile::tempdir().unwrap();
    let result = ParticipantService::open(unrelated.path(), f.engine.clone(), Arc::new(Host));
    assert!(result.is_err());
    assert!(!unrelated.path().join("v2-participants.sqlite").exists());
}

#[test]
fn source_projection_envelopes_preserve_each_version_and_do_not_relabel_human_comments() {
    use linkstart::{
        runtime_source::{RepositoryPolicy, SourceConfig},
        runtime_source_jobs::SourceJobs,
    };
    use std::time::{Duration, Instant};
    let f = Fixture::new();
    let service = f.service();
    let binding = f.bind(&service);
    let pid = binding["participant_id"].as_str().unwrap();
    let endpoint=f.engine.execute(&f.token,&req("github-endpoint","endpoints.register",&f.sid,json!({"name":"approved source","kind":"github","config":{"repository":"acme/project","allow_write_back":false}}))).unwrap();
    let remote = tempfile::tempdir().unwrap();
    let script = remote.path().join("fake-gh.rs");
    let program = remote.path().join(if cfg!(windows) {
        "fake-gh.exe"
    } else {
        "fake-gh"
    });
    let responses = remote.path().join("issues.json");
    std::fs::write(
        &script,
        r#"
fn main() {
    let args:Vec<String>=std::env::args().skip(1).collect();
    assert_eq!(args[1],"api","unexpected command");
    if let Some(index)=args.iter().position(|a|a=="--method") {
        assert_eq!(args[index+1],"GET","external mutations are forbidden in this fixture");
    }
    if args[2].contains("/issues?") {print!("{}",std::fs::read_to_string(&args[0]).unwrap());}
    else if args[2].contains("/pulls?") {print!("[[]]");}
    else {panic!("unexpected endpoint");}
}
"#,
    )
    .unwrap();
    // Compile a std-only executable with the Rust toolchain already required by
    // the suite; the source boundary remains a real subprocess on every OS.
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let built = std::process::Command::new(compiler)
        .args(["--edition=2021", "--crate-name", "participant_fake_gh"])
        .arg(&script)
        .arg("-o")
        .arg(&program)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "fake gh fixture compilation failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let snapshot = |version: &str, login: &str, id: u64| json!([[{"node_id":"I_PARTICIPANT","number":1,"title":"versioned source","body":format!("source {version}"),"state":"open","html_url":"https://github.com/acme/project/issues/1","user":{"id":id,"login":login,"node_id":format!("U_{login}")},"created_at":"2026-10-20T00:00:00Z","updated_at":version}]]);
    std::fs::write(
        &responses,
        snapshot("2026-10-21T01:00:00Z", "author-v1", 11).to_string(),
    )
    .unwrap();
    let mut config = SourceConfig::new(remote.path().join("source.sqlite"));
    config.repositories = vec![RepositoryPolicy {
        repository: "acme/project".into(),
        allow_write_back: false,
    }];
    config.gh_program = program;
    config.gh_prefix_args = vec![responses.to_string_lossy().into_owned()];
    let jobs = SourceJobs::open_with_config_factory(
        &f.dir.path().join("jobs"),
        f.engine.clone(),
        Arc::new(move |_, _| Ok(config.clone())),
    )
    .unwrap();
    let sync = |id: &str| {
        jobs.start(
            &f.token,
            &req(
                id,
                "sources.sync",
                &f.sid,
                json!({"endpoint_id":endpoint["id"]}),
            ),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let job = jobs.get(&f.token, id).unwrap();
            if !["running", "applying"].contains(&job.status.as_str()) {
                assert_eq!(job.status, "completed", "{job:?}");
                return job;
            }
            assert!(
                Instant::now() < deadline,
                "source job did not complete: {job:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let first = sync("source-v1");
    assert_eq!(first.topic_ids.len(), 1);
    let topic = &first.topic_ids[0];
    std::fs::write(
        &responses,
        snapshot("2026-10-21T02:00:00Z", "author-v2", 22).to_string(),
    )
    .unwrap();
    let second = sync("source-v2");
    assert_eq!(second.topic_ids, first.topic_ids);
    f.engine.execute(&f.token,&req("human-comment","task.act",&f.sid,json!({"topic_id":topic,"action":"comment","comment":"Human follow-up","expected_revision":1}))).unwrap();
    let topic_view = f
        .engine
        .execute(
            &f.token,
            &req("origin", "topics.read", &f.sid, json!({"topic_id":topic})),
        )
        .unwrap();
    assert_eq!(
        topic_view["topic"]["provenance"]["source_version"],
        "2026-10-21T01:00:00Z"
    );
    let page = f
        .call(
            &service,
            "inbox",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    let events = page["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0]["activity"]["provenance"]["source_version"],
        "2026-10-21T01:00:00Z"
    );
    assert_eq!(
        events[1]["activity"]["provenance"]["source_version"],
        "2026-10-21T02:00:00Z"
    );
    assert_eq!(
        events[1]["provenance"], events[1]["activity"]["provenance"],
        "envelope must use immutable V2 event origin, not original V1 topic origin"
    );
    assert_eq!(events[0]["provenance"], events[0]["activity"]["provenance"]);
    assert_eq!(
        events[1]["provenance"]["original_author"]["login"],
        "author-v2"
    );
    assert_eq!(
        events[1]["provenance"]["fetched_at"],
        events[1]["activity"]["provenance"]["fetched_at"]
    );
    assert_eq!(events[2]["activity"]["request_id"], "human-comment");
    assert_eq!(
        events[2]["provenance"],
        json!({"kind":"unknown"}),
        "missing event provenance must never inherit external topic origin"
    );
    // Historical cache fixture only: all forum/source events above were produced by
    // the real public SourceJobs/Engine paths. Do not forge or rewrite core events.
    let cache = rusqlite::Connection::open(f.dir.path().join("v2-participants.sqlite")).unwrap();
    let raw = || {
        cache
            .query_row(
                "SELECT json FROM participant_ledger WHERE id=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    let mut legacy: Value = serde_json::from_str(&raw()).unwrap();
    legacy["intents"][1]["envelope"]["provenance"] = events[0]["provenance"].clone();
    cache
        .execute(
            "UPDATE participant_ledger SET json=?1 WHERE id=1",
            [legacy.to_string()],
        )
        .unwrap();
    let before_lookup = raw();
    let lookup = f
        .call(
            &service,
            "cached-operation",
            "operations.get",
            json!({"participant_id":pid,"operation_request_id":events[1]["operation_request_id"]}),
        )
        .unwrap();
    assert_eq!(
        lookup["envelope"]["provenance"],
        events[1]["activity"]["provenance"]
    );
    assert_eq!(
        raw(),
        before_lookup,
        "operation lookup must not rewrite legacy metadata or delivery state"
    );
    struct EventOriginHost;
    impl TrustedHost for EventOriginHost {
        fn admit(&self, context: &BindingContext) -> Result<OwnedAdmission, ApiError> {
            for intent in &context.pending_intents {
                let expected = intent["envelope"]["activity"]
                    .get("provenance")
                    .cloned()
                    .unwrap_or(json!({"kind":"unknown"}));
                if intent["envelope"]["provenance"] != expected {
                    return Err(ApiError {
                        code: "Invalid".into(),
                        message: "trusted host received stale cached event provenance".into(),
                    });
                }
            }
            Host.admit(context)
        }
    }
    let host_view =
        ParticipantService::open(f.dir.path(), f.engine.clone(), Arc::new(EventOriginHost))
            .unwrap();
    f.call(
        &host_view,
        "normalize-pending-for-host",
        "participants.resume",
        json!({"participant_id":pid,"expected_generation":1}),
    )
    .unwrap();
    drop(service);
    let reopened = f.service();
    let replay = f
        .call(
            &reopened,
            "reread",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    assert_eq!(replay["events"], page["events"]);
    let repaired: Value = serde_json::from_str(&raw()).unwrap();
    assert_eq!(
        repaired["intents"][1]["envelope"]["provenance"],
        events[1]["activity"]["provenance"]
    );
    assert_eq!(
        repaired["intents"][1]["state"],
        legacy["intents"][1]["state"]
    );
}

#[test]
fn resume_admits_prospective_generation_and_rolls_back_failed_admission() {
    struct StrictGenerationHost {
        expected: AtomicU64,
        calls: std::sync::Mutex<Vec<u64>>,
    }
    impl TrustedHost for StrictGenerationHost {
        fn admit(&self, c: &BindingContext) -> Result<OwnedAdmission, ApiError> {
            self.calls.lock().unwrap().push(c.binding_generation);
            if c.binding_generation != self.expected.load(Ordering::SeqCst) {
                return Err(ApiError {
                    code: "Denied".into(),
                    message: "owned registry requires exact prospective generation".into(),
                });
            }
            let mut admission = Host.admit(c)?;
            admission.host_generation = c.binding_generation;
            Ok(admission)
        }
    }
    let f = Fixture::new();
    let host = Arc::new(StrictGenerationHost {
        expected: AtomicU64::new(1),
        calls: std::sync::Mutex::new(vec![]),
    });
    let s = ParticipantService::open(f.dir.path(), f.engine.clone(), host.clone()).unwrap();
    let b = f.bind(&s);
    let pid = b["participant_id"].as_str().unwrap();
    let topic = f
        .engine
        .execute(
            &f.token,
            &req(
                "task",
                "topics.create",
                &f.sid,
                json!({"title":"task","body":"work","kind":"task"}),
            ),
        )
        .unwrap();
    let events = f
        .call(
            &s,
            "inbox",
            "participants.read",
            json!({"participant_id":pid}),
        )
        .unwrap();
    f.call(
        &s,
        "pause",
        "participants.pause",
        json!({"participant_id":pid,"expected_generation":1}),
    )
    .unwrap();
    let db = rusqlite::Connection::open(f.dir.path().join("v2-participants.sqlite")).unwrap();
    let raw = || {
        db.query_row("SELECT json FROM participant_ledger WHERE id=1", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
    };
    let paused = raw();
    let args = json!({"participant_id":pid,"expected_generation":2});
    host.expected.store(4, Ordering::SeqCst);
    assert_eq!(
        f.call(&s, "resume", "participants.resume", args.clone())
            .unwrap_err()
            .code,
        "Denied"
    );
    assert_eq!(
        raw(),
        paused,
        "failed admission must not change binding, cursor, or receipts"
    );
    assert_eq!(
        *host.calls.lock().unwrap().last().unwrap(),
        3,
        "resume must present its prospective generation, like rebind"
    );
    host.expected.store(3, Ordering::SeqCst);
    let resumed = f
        .call(&s, "resume", "participants.resume", args.clone())
        .unwrap();
    assert_eq!(resumed["binding_generation"], 3);
    assert_eq!(resumed["admission"]["host_generation"], 3);
    assert_eq!(
        f.call(&s, "resume", "participants.resume", args).unwrap(),
        resumed,
        "same-request replay must remain admitted by exact-generation registry"
    );
    f.call(&s,"handled","participants.receipt",json!({"participant_id":pid,"binding_generation":3,"event_id":events["events"][0]["event_id"],"state":"handled"})).unwrap();
    let checkpoint=f.call(&s,"checkpoint","work.checkpoint.set",json!({"participant_id":pid,"binding_generation":3,"work_id":"work","expected_revision":0,"topic_id":topic["id"],"assignment_revision":0,"completed":[],"pending":[],"unknown":[],"artifacts":[]})).unwrap();
    assert_eq!(checkpoint["revision"], 1);
    assert_eq!(host.calls.lock().unwrap().as_slice(), [1, 3, 3, 3, 3, 3]);
}

#[test]
fn binding_generation_exhaustion_rejects_each_transition_without_changing_ledger() {
    let f = Fixture::new();
    let s = f.service();
    let b = f.bind(&s);
    let pid = b["participant_id"].as_str().unwrap();
    let db = rusqlite::Connection::open(f.dir.path().join("v2-participants.sqlite")).unwrap();
    let raw = || {
        db.query_row("SELECT json FROM participant_ledger WHERE id=1", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
    };
    // Boundary fixture only: reaching u64::MAX by normal requests is impractical.
    let mut exhausted: Value = serde_json::from_str(&raw()).unwrap();
    exhausted["bindings"][0]["binding_generation"] = json!(u64::MAX);
    db.execute(
        "UPDATE participant_ledger SET json=?1 WHERE id=1",
        [exhausted.to_string()],
    )
    .unwrap();
    let before = raw();
    for tool in [
        "participants.pause",
        "participants.resume",
        "participants.bind",
    ] {
        let mut args = json!({"participant_id":pid,"expected_generation":u64::MAX});
        if tool == "participants.bind" {
            args["endpoint_id"] = json!(f.eid);
            args["session_id"] = json!("replacement-session");
            args["attachment_id"] = json!(f.attachment);
            args["adapter_kind"] = json!("test-owned");
        }
        let error = f.call(&s, tool, tool, args).unwrap_err();
        assert_eq!(error.code, "Conflict");
        assert!(error.message.contains("generation exhausted"));
        assert_eq!(raw(), before);
    }
}
