use linkstart::{
    runtime_codex_host::*,
    runtime_core::{owner_token_path, Engine, ToolRequest},
    runtime_participants::{BindingContext, ParticipantService, TrustedHost},
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
fn python_executable() -> PathBuf {
    #[cfg(windows)]
    return PathBuf::from("python");
    #[cfg(not(windows))]
    PathBuf::from("python3")
}
struct Calls(AtomicUsize);
impl RuntimeToolFacade for Calls {
    fn call(&self, _: &HostScope, _: &Value) -> Result<Value, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ok":true}))
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    registry: OwnedCodexRegistry,
    token: String,
    scope: HostScope,
    calls: Arc<Calls>,
    engine: Engine,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let execute = |credential: &str, id: &str, tool: &str, sid: Option<&str>, args: Value| {
            engine
                .execute(
                    credential,
                    &ToolRequest {
                        request_id: id.into(),
                        tool: tool.into(),
                        space_id: sid.map(str::to_owned),
                        args,
                    },
                )
                .unwrap()
        };
        let s = execute(
            owner.trim(),
            "space",
            "spaces.create",
            None,
            json!({"name":"host"}),
        );
        let sid = s["space"]["id"].as_str().unwrap();
        let token = s["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_string();
        let endpoint = execute(
            &token,
            "endpoint",
            "endpoints.register",
            Some(sid),
            json!({"name":"host","kind":"terminal"}),
        );
        let hb = execute(&token, "hb", "members.heartbeat", Some(sid), json!({}));
        let scope = HostScope {
            identity_id: hb["identity_id"].as_str().unwrap().into(),
            space_id: sid.into(),
            endpoint_id: endpoint["id"].as_str().unwrap().into(),
            attachment_id: hb["attachment_id"].as_str().unwrap().into(),
            session_id: "session-1".into(),
            binding_generation: 1,
        };
        let registry = OwnedCodexRegistry::open(dir.path(), engine.clone()).unwrap();
        Self {
            _dir: dir,
            registry,
            token,
            scope,
            calls: Arc::new(Calls(AtomicUsize::new(0))),
            engine,
        }
    }
    fn spawn(&self, mode: &str) -> OwnedCodexHost {
        self.registry
            .spawn(
                OwnedHostConfig {
                    executable: python_executable(),
                    args: vec![
                        format!(
                            "{}/tests/fixtures/owned_codex_server.py",
                            env!("CARGO_MANIFEST_DIR")
                        ),
                        mode.into(),
                    ],
                    workspace: self._dir.path().into(),
                    model: "gpt-6-luna".into(),
                    scope: self.scope.clone(),
                    timeout: Duration::from_millis(500),
                    profile: ExecutionProfile::ReadOnlyForum,
                },
                &self.token,
                self.calls.clone(),
            )
            .unwrap()
    }
    fn config(&self, mode: &str, profile: ExecutionProfile) -> OwnedHostConfig {
        OwnedHostConfig {
            executable: python_executable(),
            args: vec![
                format!(
                    "{}/tests/fixtures/owned_codex_server.py",
                    env!("CARGO_MANIFEST_DIR")
                ),
                mode.into(),
            ],
            workspace: self._dir.path().into(),
            model: "gpt-6-luna".into(),
            scope: self.scope.clone(),
            timeout: Duration::from_millis(500),
            profile,
        }
    }
    fn context(&self) -> BindingContext {
        BindingContext {
            identity_id: self.scope.identity_id.clone(),
            space_id: self.scope.space_id.clone(),
            endpoint_id: self.scope.endpoint_id.clone(),
            attachment_id: self.scope.attachment_id.clone(),
            session_id: self.scope.session_id.clone(),
            adapter_kind: "codex-app-server-0.163".into(),
            provider_thread_id: Some("owned-thread".into()),
            pending_intents: vec![],
            binding_generation: 1,
            previous_binding: None,
        }
    }
}

#[test]
fn unverified_provider_version_and_effective_sandbox_are_unsupported() {
    let f = Fixture::new();
    let mut host = f.spawn("bad-version");
    assert!(matches!(host.initialize(), Err(HostError::Unsupported(_))));
    host.close().unwrap();
    drop(host);
    let mut host = f.spawn("bad-sandbox");
    host.initialize().unwrap();
    assert!(matches!(
        host.start_owned_thread(),
        Err(HostError::Unsupported(_))
    ));
    assert!(f.registry.admit(&f.context()).is_err());
    host.close().unwrap();
}

#[test]
#[cfg(windows)]
fn approved_workspace_profile_preserves_request_effective_policy_and_native_resume() {
    let f = Fixture::new();
    assert!(matches!(
        f.registry.approve_workspace(
            &f.token,
            &f.scope.space_id,
            &f.scope.endpoint_id,
            f._dir.path(),
            "other",
            "proof"
        ),
        Err(HostError::Unsupported(_))
    ));
    let approval = f
        .registry
        .approve_workspace(
            &f.token,
            &f.scope.space_id,
            &f.scope.endpoint_id,
            f._dir.path(),
            "0.163.0",
            "fake-capability-boundary-only",
        )
        .unwrap();
    let profile = ExecutionProfile::WorkspaceWrite(approval);
    let mut config = f.config("workspace", profile.clone());
    config.workspace = f._dir.path().parent().unwrap().into();
    assert!(f.registry.spawn(config, &f.token, f.calls.clone()).is_err());
    let mut host = f
        .registry
        .spawn(
            f.config("workspace", profile.clone()),
            &f.token,
            f.calls.clone(),
        )
        .unwrap();
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    host.close().unwrap();
    drop(host);
    let mut wrong = f.spawn("normal");
    wrong.initialize().unwrap();
    assert!(matches!(
        wrong.resume_owned_thread("owned-thread"),
        Err(HostError::Denied(_))
    ));
    wrong.close().unwrap();
    drop(wrong);
    let mut resumed = f
        .registry
        .spawn(f.config("workspace", profile), &f.token, f.calls.clone())
        .unwrap();
    resumed.initialize().unwrap();
    assert_eq!(
        resumed.resume_owned_thread("owned-thread").unwrap(),
        "owned-thread"
    );
    resumed.close().unwrap();
    assert!(resumed.is_exited().unwrap());
}

#[test]
fn actual_forum_facade_rechecks_credentials_and_rejects_admin_tools() {
    let f = Fixture::new();
    let facade = EngineForumFacade::new(f.engine.clone(), f.token.clone());
    let read = json!({"request_id":"same-read","tool":"identity.current","space_id":f.scope.space_id,"args":{}});
    assert!(facade.call(&f.scope, &read).is_ok());
    let write = json!({"request_id":"forbidden","tool":"members.invite","space_id":f.scope.space_id,"args":{"roles":["manager"]}});
    assert!(facade.call(&f.scope, &write).is_err());
    let owner = std::fs::read_to_string(owner_token_path(f._dir.path())).unwrap();
    f.engine
        .execute(
            owner.trim(),
            &ToolRequest {
                request_id: "revoke".into(),
                tool: "members.revoke_credential".into(),
                space_id: Some(f.scope.space_id.clone()),
                args: json!({"member_id":f.scope.identity_id,"expected_epoch":0}),
            },
        )
        .unwrap();
    assert!(facade.call(&f.scope, &read).is_err());
}

#[test]
fn binding_fence_rejects_all_late_publication_and_admission() {
    let f = Fixture::new();
    let mut host = f.spawn("normal");
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    for field in [
        "identity",
        "space",
        "endpoint",
        "attachment",
        "session",
        "thread",
        "adapter",
    ] {
        let mut c = f.context();
        match field {
            "identity" => c.identity_id = "other".into(),
            "space" => c.space_id = "other".into(),
            "endpoint" => c.endpoint_id = "other".into(),
            "attachment" => c.attachment_id = "other".into(),
            "session" => c.session_id = "other".into(),
            "thread" => c.provider_thread_id = Some("other".into()),
            _ => c.adapter_kind = "other".into(),
        };
        assert!(f.registry.admit(&c).is_err());
    }
    f.registry.fence_binding(&f.scope.session_id, 1).unwrap();
    assert!(f.registry.admit(&f.context()).is_err());
    assert!(host.start_turn("late", "work").is_err());
    assert_eq!(f.calls.0.load(Ordering::SeqCst), 0);
    host.close().unwrap();
}
#[test]
fn request_id_collision_and_notifications_are_routed_before_response() {
    let f = Fixture::new();
    let mut host = f.spawn("normal");
    host.initialize().unwrap();
    assert_eq!(host.start_owned_thread().unwrap(), "owned-thread");
    assert!(f.registry.admit(&f.context()).is_ok());
    assert_eq!(host.start_turn("op", "work").unwrap(), "turn-1");
    assert_eq!(f.calls.0.load(Ordering::SeqCst), 1);
    let mut old = f.context();
    old.binding_generation = 2;
    assert!(f.registry.admit(&old).is_err());
    assert_eq!(
        host.pump(Duration::from_millis(20)).unwrap()["method"],
        "turn/started"
    );
    assert!(matches!(
        host.pump(Duration::from_millis(20)),
        Err(HostError::Idle)
    ));
    assert!(f.registry.admit(&f.context()).is_ok());
    let pid = host.pid();
    std::thread::sleep(Duration::from_millis(25));
    assert_eq!(host.pid(), pid);
    host.interrupt().unwrap();
    host.close().unwrap();
    assert!(host.is_exited().unwrap());
    assert!(f.registry.admit(&f.context()).is_err());
}

#[test]
fn owned_eof_preserves_unknown_and_close_observes_exit() {
    let f = Fixture::new();
    let mut host = f.spawn("eof");
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    assert!(
        matches!(host.start_turn("eof-operation", "work"), Err(HostError::Unknown{operation_id}) if operation_id=="eof-operation")
    );
    assert!(f.registry.admit(&f.context()).is_err());
    host.close().unwrap();
    assert!(host.is_exited().unwrap());
}
#[test]
fn exact_owned_thread_recovers_in_new_process_but_external_thread_is_rejected() {
    let f = Fixture::new();
    let mut first = f.spawn("normal");
    first.initialize().unwrap();
    first.start_owned_thread().unwrap();
    let mut other_config = f.config("normal", ExecutionProfile::ReadOnlyForum);
    other_config.scope.session_id = "another-session".into();
    let mut concurrent = f
        .registry
        .spawn(other_config, &f.token, f.calls.clone())
        .unwrap();
    concurrent.initialize().unwrap();
    assert!(matches!(
        concurrent.resume_owned_thread("owned-thread"),
        Err(HostError::Denied(_))
    ));
    concurrent.close().unwrap();
    drop(concurrent);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.spawn("normal"))).is_err());
    first.close().unwrap();
    drop(first);
    let mut next = f.spawn("normal");
    next.initialize().unwrap();
    assert!(matches!(
        next.resume_owned_thread("external"),
        Err(HostError::Unsupported(_))
    ));
    assert_eq!(
        next.resume_owned_thread("owned-thread").unwrap(),
        "owned-thread"
    );
    next.close().unwrap();
}
#[test]
fn lost_response_keeps_operation_unknown_and_never_repeats_turn() {
    let f = Fixture::new();
    let mut host = f.spawn("lost");
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    assert!(
        matches!(host.start_turn("lost-op","work"),Err(HostError::Unknown{operation_id}) if operation_id=="lost-op")
    );
    assert!(host.start_turn("replacement", "work").is_err());
    assert!(f.registry.admit(&f.context()).is_err());
    host.close().unwrap();
    drop(host);
    let mut next = f.spawn("normal");
    next.initialize().unwrap();
    next.resume_owned_thread("owned-thread").unwrap();
    assert!(matches!(
        next.start_turn("replacement", "work"),
        Err(HostError::Unknown { operation_id }) if operation_id=="lost-op"
    ));
    assert_eq!(
        next.read_exact_thread().unwrap()["thread"]["id"],
        "owned-thread"
    );
    assert_eq!(next.reconcile_operation("lost-op").unwrap(), "turn-1");
    assert!(f.registry.admit(&f.context()).is_ok());
    next.close().unwrap();
}

#[test]
fn fake_provider_callbacks_execute_real_claim_comment_submission_and_independent_review() {
    let f = Fixture::new();
    let facade = Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone()));
    let envelope = |id: &str, tool: &str, args: Value| json!({"request_id":id,"tool":tool,"space_id":f.scope.space_id,"args":args});
    let topic = facade
        .call(
            &f.scope,
            &envelope(
                "create",
                "topics.create",
                json!({"title":"real core work","body":"fixture","kind":"task"}),
            ),
        )
        .unwrap();
    let tid = topic["id"].as_str().unwrap();
    let mut host = f
        .registry
        .spawn(
            f.config("forum", ExecutionProfile::ReadOnlyForum),
            &f.token,
            facade.clone(),
        )
        .unwrap();
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    let run = |host: &mut OwnedCodexHost, id: &str, request: Value| {
        host.start_turn(id, &request.to_string()).unwrap();
        loop {
            if host.pump(Duration::from_secs(1)).unwrap()["method"] == "turn/completed" {
                break;
            }
        }
    };
    run(
        &mut host,
        "claim-op",
        envelope(
            "claim",
            "task.act",
            json!({"topic_id":tid,"action":"claim","comment":"taking responsibility","expected_revision":0}),
        ),
    );
    let exec = |credential: &str, id: &str, tool: &str, args: Value| {
        f.engine
            .execute(
                credential,
                &ToolRequest {
                    request_id: id.into(),
                    tool: tool.into(),
                    space_id: Some(f.scope.space_id.clone()),
                    args,
                },
            )
            .unwrap()
    };
    let invite = exec(
        &f.token,
        "invite",
        "members.invite",
        json!({"roles":["implementer","reviewer"]}),
    );
    let member = exec(
        invite["invite_token"].as_str().unwrap(),
        "register",
        "members.register",
        json!({"name":"independent"}),
    );
    let token = member["credential"].as_str().unwrap();
    exec(
        &f.token,
        "grant",
        "endpoints.grant",
        json!({"endpoint_id":f.scope.endpoint_id,"member_id":member["id"],"operations":["observe","send"]}),
    );
    let heartbeat = exec(token, "reviewer-hb", "members.heartbeat", json!({}));
    let mut scope = f.scope.clone();
    scope.identity_id = member["id"].as_str().unwrap().into();
    scope.attachment_id = heartbeat["attachment_id"].as_str().unwrap().into();
    scope.session_id = "reviewer-session".into();
    let reviewer = EngineForumFacade::new(f.engine.clone(), token.into());
    assert!(reviewer
        .call(
            &scope,
            &envelope(
                "competing-claim",
                "task.act",
                json!({"topic_id":tid,"action":"claim","comment":"compete","expected_revision":0})
            )
        )
        .is_err());
    run(
        &mut host,
        "comment-op",
        envelope(
            "comment",
            "task.act",
            json!({"topic_id":tid,"action":"comment","comment":"implemented fixture","expected_revision":1}),
        ),
    );
    run(
        &mut host,
        "submit-op",
        envelope(
            "submit",
            "task.act",
            json!({"topic_id":tid,"action":"submit_review","comment":"immutable artifact","artifact_ref":"git:fixture-immutable","expected_revision":2}),
        ),
    );
    let submitted = facade
        .call(
            &f.scope,
            &envelope("read-submission", "topics.read", json!({"topic_id":tid})),
        )
        .unwrap();
    let submission = &submitted["topic"]["current_submission"];
    assert_eq!(submitted["topic"]["state"], "pending_review");
    assert_eq!(submission["author"], f.scope.identity_id);
    reviewer.call(&scope,&envelope("review","task.act",json!({"topic_id":tid,"action":"approve_review","comment":"reviewed exact fixture artifact","expected_revision":3,"expected_submission":submission["id"]}))).unwrap();
    let read = facade
        .call(
            &f.scope,
            &envelope("read-reviewed", "topics.read", json!({"topic_id":tid})),
        )
        .unwrap();
    assert_eq!(read["topic"]["state"], "pending_test");
    host.close().unwrap();
}

#[test]
fn reconciling_completed_a_preserves_active_b_and_its_durable_state() {
    let f = Fixture::new();
    let mut host = f.spawn("history");
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    host.start_turn("A", "work A").unwrap();
    while host.pump(Duration::from_secs(1)).unwrap()["method"] != "turn/completed" {}
    assert_eq!(host.start_turn("B", "work B").unwrap(), "turn-2");
    assert_eq!(host.reconcile_operation("A").unwrap(), "turn-1");
    assert_eq!(host.interrupt().unwrap()["interruptedTurn"], "turn-2");
    while f.calls.0.load(Ordering::SeqCst) == 0 {
        host.pump(Duration::from_secs(1)).unwrap();
    }
    assert_eq!(f.calls.0.load(Ordering::SeqCst), 1);
    let db = rusqlite::Connection::open(f._dir.path().join("v2-codex-host.sqlite")).unwrap();
    let state = |id: &str| {
        db.query_row("SELECT state FROM host_operations WHERE id=?1", [id], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
    };
    assert_eq!(state("A"), "completed");
    assert_eq!(state("B"), "accepted");
    host.close().unwrap();
}

struct BarrierFacade {
    inner: EngineForumFacade,
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    panic_after_release: bool,
}
impl RuntimeToolFacade for BarrierFacade {
    fn call(&self, scope: &HostScope, request: &Value) -> Result<Value, String> {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        if self.panic_after_release {
            panic!("injected facade panic");
        }
        self.inner.call(scope, request)
    }
}

#[test]
fn binding_fence_does_not_return_while_admitted_real_mutation_is_blocked() {
    let f = Fixture::new();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let facade = Arc::new(BarrierFacade {
        inner: EngineForumFacade::new(f.engine.clone(), f.token.clone()),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
        panic_after_release: false,
    });
    let mut config = f.config("forum", ExecutionProfile::ReadOnlyForum);
    config.timeout = Duration::from_secs(5);
    let mut host = f.registry.spawn(config, &f.token, facade).unwrap();
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    let request = json!({"request_id":"barrier-mutation","tool":"topics.create","space_id":f.scope.space_id,"args":{"title":"admitted callback","body":"must finish before fence returns","kind":"task"}});
    let worker = std::thread::spawn(move || {
        let result = host.start_turn("barrier-op", &request.to_string());
        (host, result)
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let registry = f.registry.clone();
    let session = f.scope.session_id.clone();
    let (returned_tx, returned_rx) = std::sync::mpsc::channel();
    let fence = std::thread::spawn(move || {
        returned_tx
            .send(registry.fence_binding(&session, 1))
            .unwrap();
    });
    let early = returned_rx.recv_timeout(Duration::from_millis(150));
    release_tx.send(()).unwrap();
    let (mut host, _) = worker.join().unwrap();
    let early_returned = early.is_ok();
    if let Ok(result) = early {
        result.unwrap();
    } else {
        returned_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
    }
    fence.join().unwrap();
    host.close().unwrap();
    let page = f
        .engine
        .execute(
            &f.token,
            &ToolRequest {
                request_id: "after-drain".into(),
                tool: "topics.list".into(),
                space_id: Some(f.scope.space_id.clone()),
                args: json!({}),
            },
        )
        .unwrap();
    assert_eq!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["title"] == "admitted callback")
            .count(),
        1,
        "barrier must exercise an actual committed Engine mutation"
    );
    assert!(
        !early_returned,
        "fence returned before the already admitted facade mutation drained"
    );
}

#[test]
fn fence_timeout_stays_fenced_and_same_original_generation_can_drain_on_retry() {
    for panic_after_release in [false, true] {
        let f = Fixture::new();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let facade = Arc::new(BarrierFacade {
            inner: EngineForumFacade::new(f.engine.clone(), f.token.clone()),
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
            panic_after_release,
        });
        let mut config = f.config("forum-double", ExecutionProfile::ReadOnlyForum);
        config.timeout = Duration::from_secs(5);
        let mut host = f.registry.spawn(config, &f.token, facade).unwrap();
        host.initialize().unwrap();
        host.start_owned_thread().unwrap();
        let request = json!({"request_id":"timeout-mutation","tool":"topics.create","space_id":f.scope.space_id,"args":{"title":"callback lease","body":"fixture","kind":"task"}});
        let worker = std::thread::spawn(move || {
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                host.start_turn("timeout-op", &request.to_string())
            }))
            .is_err();
            (host, panicked)
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                f.registry
                    .fence_binding_timeout(&f.scope.session_id, 1, Duration::ZERO),
                Err(HostError::Pending {
                    active_callbacks: 1
                })
            ));
        }
        assert!(f
            .registry
            .fence_binding_timeout(&f.scope.session_id, 2, Duration::ZERO)
            .is_err());
        assert!(f.registry.admit(&f.context()).is_err());
        release_tx.send(()).unwrap();
        let (mut host, panicked) = worker.join().unwrap();
        assert_eq!(panicked, panic_after_release);
        f.registry
            .fence_binding_timeout(&f.scope.session_id, 1, Duration::ZERO)
            .unwrap();
        // A second server callback was queued before the first was released.
        // Draining the old notification and callback must never enter the facade.
        for _ in 0..2 {
            let _ = host.pump(Duration::from_millis(50));
        }
        assert!(matches!(
            entered_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(f.registry.admit(&f.context()).is_err());
        assert!(host.start_turn("late", "late").is_err());
        host.close().unwrap();
    }
}

#[test]
fn actual_participant_resume_admits_prospective_host_generation_then_receipt() {
    let f = Fixture::new();
    let service = ParticipantService::open(
        f._dir.path(),
        f.engine.clone(),
        Arc::new(f.registry.clone()),
    )
    .unwrap();
    let req = |id: &str, tool: &str, args: Value| ToolRequest {
        request_id: id.into(),
        tool: tool.into(),
        space_id: Some(f.scope.space_id.clone()),
        args,
    };
    let mut host = f.spawn("normal");
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    let binding=service.execute(&f.token,&req("binding","participants.bind",json!({"endpoint_id":f.scope.endpoint_id,"session_id":f.scope.session_id,"attachment_id":f.scope.attachment_id,"adapter_kind":"codex-app-server-0.163","provider_thread_id":"owned-thread","expected_generation":0,"initial_cursor":"beginning"}))).unwrap();
    let pid = &binding["participant_id"];
    let topic = f
        .engine
        .execute(
            &f.token,
            &req(
                "topic",
                "topics.create",
                json!({"title":"resume evidence","body":"fixture","kind":"task"}),
            ),
        )
        .unwrap();
    let page = service
        .execute(
            &f.token,
            &req(
                "inbox",
                "participants.read",
                json!({"participant_id":pid,"limit":100}),
            ),
        )
        .unwrap();
    f.registry.fence_binding(&f.scope.session_id, 1).unwrap();
    service
        .execute(
            &f.token,
            &req(
                "pause",
                "participants.pause",
                json!({"participant_id":pid,"expected_generation":1}),
            ),
        )
        .unwrap();
    host.close().unwrap();
    drop(host);
    let mut wrong_config = f.config("normal", ExecutionProfile::ReadOnlyForum);
    wrong_config.scope.binding_generation = 2;
    let mut wrong = f
        .registry
        .spawn(wrong_config, &f.token, f.calls.clone())
        .unwrap();
    wrong.initialize().unwrap();
    wrong.resume_owned_thread("owned-thread").unwrap();
    assert!(service
        .execute(
            &f.token,
            &req(
                "wrong-resume",
                "participants.resume",
                json!({"participant_id":pid,"expected_generation":2})
            )
        )
        .is_err());
    wrong.close().unwrap();
    drop(wrong);
    let mut config = f.config("normal", ExecutionProfile::ReadOnlyForum);
    config.scope.binding_generation = 3;
    let mut resumed = f.registry.spawn(config, &f.token, f.calls.clone()).unwrap();
    resumed.initialize().unwrap();
    resumed.resume_owned_thread("owned-thread").unwrap();
    let result = service
        .execute(
            &f.token,
            &req(
                "resume",
                "participants.resume",
                json!({"participant_id":pid,"expected_generation":2}),
            ),
        )
        .unwrap();
    assert_eq!(result["binding_generation"], 3);
    assert_eq!(
        service
            .execute(
                &f.token,
                &req(
                    "resume",
                    "participants.resume",
                    json!({"participant_id":pid,"expected_generation":2})
                )
            )
            .unwrap(),
        result
    );
    service.execute(&f.token,&req("receipt","participants.receipt",json!({"participant_id":pid,"event_id":page["events"][0]["event_id"],"binding_generation":3,"state":"handled"}))).unwrap();
    service.execute(&f.token,&req("checkpoint","work.checkpoint.set",json!({"participant_id":pid,"work_id":"resume-work","binding_generation":3,"expected_revision":0,"topic_id":topic["id"],"assignment_revision":0,"completed":[],"pending":[],"unknown":[],"artifacts":[]}))).unwrap();
    resumed.close().unwrap();
}

#[cfg(windows)]
mod startup_descendant {
    use super::*;
    use std::ffi::c_void;
    type Handle = *mut c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetProcessId(process: Handle) -> u32;
        fn GetProcessTimes(
            process: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
        fn TerminateProcess(process: Handle, code: u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    struct HeldProcess(Handle);
    impl HeldProcess {
        fn open(pid: u32) -> Self {
            let raw = unsafe { OpenProcess(0x0010_0000 | 0x1000 | 1, 0, pid) };
            assert!(
                !raw.is_null(),
                "open exact fixture child: {}",
                std::io::Error::last_os_error()
            );
            Self(raw)
        }
        fn created(&self) -> u64 {
            let (mut c, mut e, mut k, mut u) = (
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
            );
            assert_ne!(
                unsafe { GetProcessTimes(self.0, &mut c, &mut e, &mut k, &mut u) },
                0
            );
            (u64::from(c.high) << 32) | u64::from(c.low)
        }
    }
    impl Drop for HeldProcess {
        fn drop(&mut self) {
            unsafe {
                if WaitForSingleObject(self.0, 0) == 258 {
                    TerminateProcess(self.0, 1);
                    WaitForSingleObject(self.0, 5000);
                }
                CloseHandle(self.0);
            }
        }
    }
    #[test]
    fn startup_descendant_is_contained_before_initialize_and_exits_with_owned_job() {
        let f = Fixture::new();
        let mut host = f.spawn("startup-descendant");
        host.initialize().unwrap();
        let proof: Value = serde_json::from_slice(
            &std::fs::read(f._dir.path().join("startup-child.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(proof["root_pid"].as_u64(), Some(u64::from(host.pid())));
        let pid = proof["pid"].as_u64().unwrap() as u32;
        let child = HeldProcess::open(pid);
        assert_eq!(unsafe { GetProcessId(child.0) }, pid);
        assert_eq!(child.created(), proof["creation_time"].as_u64().unwrap());
        assert_eq!(
            unsafe { WaitForSingleObject(child.0, 0) },
            258,
            "startup descendant must actually be live"
        );
        host.close().unwrap();
        assert!(host.is_exited().unwrap());
        assert_eq!(
            unsafe { WaitForSingleObject(child.0, 5000) },
            0,
            "held startup descendant handle must signal exit"
        );
    }
}
