use linkstart::{
    runtime_codex_host::*,
    runtime_core::{owner_token_path, Engine, ToolRequest},
    runtime_supervisor::{
        OperationPhase, OwnedSessionSupervisor, SessionPhase, SupervisorError, SupervisorOptions,
        ThreadAdmission,
    },
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[test]
fn supervisor_options_are_bounded() {
    let options = SupervisorOptions::default();
    assert!(options.queue_capacity > 0);
    assert!(options.heartbeat_interval.as_secs() < 5);
    let _ = std::mem::size_of::<OwnedSessionSupervisor>();
}

#[test]
fn repair_grant_matrix_admits_only_each_command_authority() {
    let mut observed = Vec::new();
    for grants in [
        vec!["send"],
        vec!["observe"],
        vec!["control"],
        vec!["manage"],
    ] {
        let f = Fixture::new();
        let s = f.supervisor();
        let mut config = f.config(false);
        config.args[1] = "normal".into();
        s.start(
            config,
            f.token.clone(),
            Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
            ThreadAdmission::New,
        )
        .unwrap();
        f.running(&s);
        s.start_turn(&f.token, &f.scope, "active-matrix".into(), "active".into())
            .unwrap();
        f.wait_operation(&s, "active-matrix", OperationPhase::Accepted);
        f.grants(&grants);
        let read = s
            .read_exact_thread(&f.token, &f.scope, Duration::from_millis(300))
            .is_ok();
        let reconcile_allowed = !matches!(
            s.reconcile(
                &f.token,
                &f.scope,
                "active-matrix".into(),
                Duration::from_millis(300)
            ),
            Err(SupervisorError::Denied)
        );
        let interrupt = s
            .interrupt(&f.token, &f.scope, Duration::from_millis(300))
            .is_ok();
        let turn = s
            .start_turn(&f.token, &f.scope, "new-matrix".into(), "queued".into())
            .is_ok();
        let stopped = s.stop(&f.token, &f.scope, Duration::from_secs(3)).is_ok();
        observed.push((grants[0], read, reconcile_allowed, interrupt, turn, stopped));
        s.request_shutdown();
        let deadline = Instant::now() + Duration::from_secs(4);
        while s.active_count() > 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(
        observed,
        vec![
            ("send", false, false, false, true, false),
            ("observe", true, true, false, false, false),
            ("control", false, false, true, false, false),
            ("manage", false, false, false, false, true)
        ]
    );
}

#[test]
fn repair_stop_is_stable_after_join_and_for_concurrent_callers() {
    let f = Fixture::new();
    let s = Arc::new(f.supervisor());
    s.start(
        f.config(false),
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut callers = Vec::new();
    for _ in 0..2 {
        let s = s.clone();
        let token = f.token.clone();
        let scope = f.scope.clone();
        let barrier = barrier.clone();
        callers.push(std::thread::spawn(move || {
            barrier.wait();
            s.stop(&token, &scope, Duration::from_secs(3))
        }));
    }
    barrier.wait();
    let results: Vec<_> = callers.into_iter().map(|c| c.join().unwrap()).collect();
    assert!(
        results.iter().all(|r| r
            .as_ref()
            .is_ok_and(|status| status.worker_exited && status.phase == SessionPhase::Exited)),
        "{results:?}"
    );
    assert_eq!(s.active_count(), 0);
    let repeated = s.stop(&f.token, &f.scope, Duration::ZERO).unwrap();
    assert!(repeated.worker_exited);
    assert_eq!(repeated.phase, SessionPhase::Exited);
}

#[test]
fn repair_exact_reconcile_updates_unknown_snapshot_without_resending() {
    let f = Fixture::new();
    let s = f.supervisor();
    let mut config = f.config(false);
    config.args[1] = "lost".into();
    config.timeout = Duration::from_secs(1);
    let script = std::fs::read_to_string(&config.args[0]).unwrap().replace(
        "elif method=='turn/start':",
        "elif method=='turn/start':\n  open('turn-start-count.txt','a').write('sent\\n')",
    );
    std::fs::write(&config.args[0], script).unwrap();
    s.start(
        config,
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    s.start_turn(&f.token, &f.scope, "lost-op".into(), "work".into())
        .unwrap();
    f.wait_operation(&s, "lost-op", OperationPhase::Unknown);
    assert_eq!(
        s.reconcile(&f.token, &f.scope, "lost-op".into(), Duration::from_secs(2))
            .unwrap(),
        "turn-1"
    );
    let op = s.operation(&f.token, &f.scope, "lost-op").unwrap();
    assert_eq!(op.phase, OperationPhase::Confirmed);
    assert_eq!(op.provider_turn_id.as_deref(), Some("turn-1"));
    assert!(op.error.is_none());
    s.start_turn(&f.token, &f.scope, "lost-op".into(), "work".into())
        .unwrap();
    assert_eq!(
        s.operation(&f.token, &f.scope, "lost-op").unwrap().phase,
        OperationPhase::Confirmed
    );
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("turn-start-count.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    s.stop(&f.token, &f.scope, Duration::from_secs(3)).unwrap();
}

#[test]
fn repair_terminal_reconcile_preserves_failed_and_interrupted_provider_outcomes() {
    for (status, outcome) in [
        ("failed", ProviderTurnOutcome::Failed),
        ("interrupted", ProviderTurnOutcome::Interrupted),
    ] {
        let f = Fixture::new();
        let s = f.supervisor();
        let mut config = f.config(false);
        config.args[1] = "lost".into();
        // This global host budget includes initialize. Lost turn responses use
        // a normal process-start budget; 150ms startup expiry has its own test.
        config.timeout = Duration::from_secs(1);
        let script = std::fs::read_to_string(&config.args[0])
            .unwrap()
            .replace("'status':'completed'", &format!("'status':'{status}'"))
            .replace(
                "if method=='initialize': send(",
                "if method=='initialize':\n  import time; time.sleep(0.3)\n  send(",
            );
        std::fs::write(&config.args[0], script).unwrap();
        s.start(
            config,
            f.token.clone(),
            Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
            ThreadAdmission::New,
        )
        .unwrap();
        f.running(&s);
        s.start_turn(&f.token, &f.scope, "lost-op".into(), "work".into())
            .unwrap();
        f.wait_operation(&s, "lost-op", OperationPhase::Unknown);
        let observed = s
            .reconcile_observation(&f.token, &f.scope, "lost-op".into(), Duration::from_secs(2))
            .unwrap();
        assert_eq!(observed.outcome, outcome);
        let snapshot = s.operation(&f.token, &f.scope, "lost-op").unwrap();
        assert_eq!(snapshot.phase, OperationPhase::Confirmed);
        assert_eq!(snapshot.provider_outcome, Some(outcome));
        assert!(snapshot.error.is_some());
        s.stop(&f.token, &f.scope, Duration::from_secs(3)).unwrap();
    }
}

#[test]
fn repair_reconciling_old_a_does_not_clear_active_b_or_interrupt_route() {
    let f = Fixture::new();
    let s = f.supervisor();
    let mut config = f.config(false);
    config.args[1] = "history".into();
    s.start(
        config,
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    s.start_turn(&f.token, &f.scope, "A".into(), "first".into())
        .unwrap();
    f.wait_operation(&s, "A", OperationPhase::Confirmed);
    s.start_turn(&f.token, &f.scope, "B".into(), "second".into())
        .unwrap();
    f.wait_operation(&s, "B", OperationPhase::Accepted);
    let a = s
        .reconcile_observation(&f.token, &f.scope, "A".into(), Duration::from_secs(2))
        .unwrap();
    assert_eq!(a.outcome, ProviderTurnOutcome::Completed);
    let b = s.operation(&f.token, &f.scope, "B").unwrap();
    assert_eq!(b.phase, OperationPhase::Accepted);
    assert_eq!(b.provider_turn_id.as_deref(), Some("turn-2"));
    assert!(b.error.is_none());
    let interrupted = s
        .interrupt(&f.token, &f.scope, Duration::from_secs(2))
        .unwrap();
    assert_eq!(interrupted["interruptedTurn"], "turn-2");
    s.stop(&f.token, &f.scope, Duration::from_secs(3)).unwrap();
}

#[test]
fn repair_initialization_and_exact_resume_failure_clean_up_and_join() {
    for (mode, admission) in [
        ("bad-version", ThreadAdmission::New),
        ("bad-sandbox", ThreadAdmission::New),
        (
            "normal",
            ThreadAdmission::ExactResume("external-unowned".into()),
        ),
    ] {
        let f = Fixture::new();
        let s = f.supervisor();
        let mut config = f.config(false);
        config.args[1] = mode.into();
        s.start(
            config,
            f.token.clone(),
            Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
            admission,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.active_count() > 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let stopped = s.stop(&f.token, &f.scope, Duration::ZERO).unwrap();
        assert!(stopped.worker_exited);
        assert_eq!(stopped.phase, SessionPhase::Unknown);
        assert!(
            s.stop(&f.token, &f.scope, Duration::ZERO)
                .unwrap()
                .worker_exited
        );
        assert!(f
            .registry
            .renew_owned_attachment(&f.scope, &f.token)
            .is_err());
    }
}

#[test]
fn repair_dispatch_rechecks_queued_read_observe_grant_after_admission() {
    let f = Fixture::new();
    let s = f.supervisor();
    let mut config = f.config(true);
    config.args[1] = "normal".into();
    let script=std::fs::read_to_string(&config.args[0]).unwrap().replace("time.sleep(6.2)","time.sleep(0.35)").replace("method=m['method']; rid=m.get('id'); p=m.get('params',{})","method=m['method']; rid=m.get('id'); p=m.get('params',{})\n open('requests.log','a').write(method+'\\n')");
    std::fs::write(&config.args[0], script).unwrap();
    s.start(
        config,
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    s.start_turn(&f.token, &f.scope, "blocking".into(), "first".into())
        .unwrap();
    f.wait_operation(&s, "blocking", OperationPhase::Dispatching);
    assert!(matches!(
        s.read_exact_thread(&f.token, &f.scope, Duration::ZERO),
        Err(SupervisorError::Pending)
    ));
    f.grants(&["send", "control", "manage"]);
    s.interrupt(&f.token, &f.scope, Duration::from_secs(2))
        .unwrap();
    assert!(!std::fs::read_to_string(f.dir.path().join("requests.log"))
        .unwrap()
        .lines()
        .any(|line| line == "thread/read"));
    s.stop(&f.token, &f.scope, Duration::from_secs(3)).unwrap();
}

#[test]
fn repair_startup_deadline_reports_unknown_initialize_and_cleans_up() {
    use linkstart::runtime_supervisor::{WorkerFailureKind, WorkerStage};
    let f = Fixture::new();
    let s = f.supervisor();
    let mut config = f.config(false);
    config.timeout = Duration::from_millis(150);
    let script=std::fs::read_to_string(&config.args[0]).unwrap().replace("if method=='initialize': send(","if method=='initialize':\n  import time\n  open('startup-trace.txt','a').write('initialize_received '+str(time.time())+'\\n')\n  time.sleep(0.3)\n  send(");
    std::fs::write(&config.args[0], script).unwrap();
    s.start(
        config,
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.active_count() > 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = s.get(&f.token, &f.scope).unwrap();
    let failure = status.worker_failure.as_ref().unwrap();
    eprintln!(
        "startup-deadline observation={status:?}; provider_trace={:?}",
        std::fs::read_to_string(f.dir.path().join("startup-trace.txt"))
    );
    assert_eq!(failure.stage, WorkerStage::Initialize);
    assert_eq!(failure.kind, WorkerFailureKind::UnknownOutcome);
    assert!(status.worker_exited);
    assert!(f
        .registry
        .renew_owned_attachment(&f.scope, &f.token)
        .is_err());
}

fn exec(engine: &Engine, credential: &str, sid: Option<&str>, tool: &str, args: Value) -> Value {
    engine
        .execute(
            credential,
            &ToolRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                tool: tool.into(),
                space_id: sid.map(str::to_owned),
                args,
            },
        )
        .unwrap()
}
struct Fixture {
    dir: tempfile::TempDir,
    engine: Engine,
    token: String,
    admin: String,
    scope: HostScope,
    registry: OwnedCodexRegistry,
}
impl Fixture {
    fn grants(&self, operations: &[&str]) {
        exec(
            &self.engine,
            &self.admin,
            Some(&self.scope.space_id),
            "endpoints.grant",
            json!({"endpoint_id":self.scope.endpoint_id,"member_id":self.scope.identity_id,"operations":operations}),
        );
    }
    fn wait_operation(&self, s: &OwnedSessionSupervisor, id: &str, phase: OperationPhase) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if s.operation(&self.token, &self.scope, id).unwrap().phase == phase {
                return;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let space = exec(
            &engine,
            owner.trim(),
            None,
            "spaces.create",
            json!({"name":"supervision"}),
        );
        let sid = space["space"]["id"].as_str().unwrap();
        let admin = space["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_string();
        exec(
            &engine,
            owner.trim(),
            None,
            "settings.set",
            json!({"settings":{"heartbeat_seconds":5}}),
        );
        let endpoint = exec(
            &engine,
            &admin,
            Some(sid),
            "endpoints.register",
            json!({"name":"owned","kind":"terminal"}),
        );
        let invite = exec(
            &engine,
            &admin,
            Some(sid),
            "members.invite",
            json!({"roles":["implementer"]}),
        );
        let member = exec(
            &engine,
            invite["invite_token"].as_str().unwrap(),
            Some(sid),
            "members.register",
            json!({"name":"agent"}),
        );
        let token = member["credential"].as_str().unwrap().to_string();
        exec(
            &engine,
            &admin,
            Some(sid),
            "endpoints.grant",
            json!({"endpoint_id":endpoint["id"],"member_id":member["id"],"operations":["observe","send","control","manage"]}),
        );
        let hb = exec(&engine, &token, Some(sid), "members.heartbeat", json!({}));
        let scope = HostScope {
            identity_id: member["id"].as_str().unwrap().into(),
            space_id: sid.into(),
            endpoint_id: endpoint["id"].as_str().unwrap().into(),
            attachment_id: hb["attachment_id"].as_str().unwrap().into(),
            session_id: "supervisor-owned".into(),
            binding_generation: 1,
        };
        let registry = OwnedCodexRegistry::open(dir.path(), engine.clone()).unwrap();
        Self {
            dir,
            engine,
            token,
            admin,
            scope,
            registry,
        }
    }
    fn config(&self, slow: bool) -> OwnedHostConfig {
        let fixture = std::fs::read_to_string(format!(
            "{}/tests/fixtures/owned_codex_server.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let fixture = fixture.replace(
            "thread='owned-thread'",
            &format!("thread='{}'", self.scope.session_id),
        );
        let fixture = if slow {
            fixture.replace(
                "elif method=='turn/start':",
                "elif method=='turn/start':\n  import time; time.sleep(6.2)",
            )
        } else {
            fixture
        };
        let path = self
            .dir
            .path()
            .join(format!("{}.py", self.scope.session_id));
        std::fs::write(&path, fixture).unwrap();
        OwnedHostConfig {
            executable: PathBuf::from(if cfg!(windows) { "python" } else { "python3" }),
            args: vec![path.to_str().unwrap().into(), "forum".into()],
            workspace: self.dir.path().into(),
            model: "gpt-6-luna".into(),
            scope: self.scope.clone(),
            timeout: Duration::from_secs(10),
            profile: ExecutionProfile::ReadOnlyForum,
        }
    }
    fn supervisor(&self) -> OwnedSessionSupervisor {
        OwnedSessionSupervisor::new(
            self.engine.clone(),
            self.registry.clone(),
            SupervisorOptions::default(),
        )
        .unwrap()
    }
    fn running(&self, s: &OwnedSessionSupervisor) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = s.get(&self.token, &self.scope).unwrap();
            if status.phase == SessionPhase::Running {
                return;
            }
            assert!(
                status.phase != SessionPhase::Unknown && Instant::now() < deadline,
                "worker did not become running: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn envelope(&self) -> String {
        json!({"request_id":"slow-forum-create","tool":"topics.create","space_id":self.scope.space_id,"args":{"kind":"task","title":"actual callback","body":"owned heartbeat"}}).to_string()
    }
    fn topics(&self) -> Value {
        exec(
            &self.engine,
            &self.token,
            Some(&self.scope.space_id),
            "topics.list",
            json!({}),
        )
    }
}

#[test]
fn baseline_without_supervision_expires_actual_attachment_and_denies_callback() {
    let f = Fixture::new();
    let mut host = f
        .registry
        .spawn(
            f.config(true),
            &f.token,
            Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        )
        .unwrap();
    host.initialize().unwrap();
    host.start_owned_thread().unwrap();
    host.start_turn("baseline", &f.envelope()).unwrap();
    for _ in 0..4 {
        let _ = host.pump(Duration::from_millis(30));
    }
    assert!(!f.topics().to_string().contains("actual callback"));
    host.close().unwrap();
}

#[test]
fn independently_heartbeats_while_provider_request_blocks_beyond_real_ttl() {
    let f = Fixture::new();
    let s = f.supervisor();
    s.start(
        f.config(true),
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    let accepted = s
        .start_turn(&f.token, &f.scope, "slow-op".into(), f.envelope())
        .unwrap();
    assert_eq!(accepted.operation_id.as_deref(), Some("slow-op"));
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if f.topics().to_string().contains("actual callback") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "real Engine forum callback failed after TTL"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    s.stop(&f.token, &f.scope, Duration::from_secs(5)).unwrap();
    assert_eq!(s.active_count(), 0);
}

#[test]
fn current_revocation_stops_owned_worker_and_denies_management() {
    let f = Fixture::new();
    let s = f.supervisor();
    s.start(
        f.config(false),
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    exec(
        &f.engine,
        &f.admin,
        Some(&f.scope.space_id),
        "members.revoke_credential",
        json!({"member_id":f.scope.identity_id,"expected_epoch":0}),
    );
    assert!(matches!(
        s.get(&f.token, &f.scope),
        Err(SupervisorError::Denied)
    ));
    let deadline = Instant::now() + Duration::from_secs(6);
    while s.active_count() > 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn two_sessions_remain_independently_owned_and_stop_pending_counts_live_worker() {
    let mut f = Fixture::new();
    let s = f.supervisor();
    let first = f.scope.clone();
    s.start(
        f.config(true),
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    f.scope.session_id = "second-supervised-session".into();
    s.start(
        f.config(false),
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    assert_eq!(s.active_count(), 2);
    assert_eq!(s.list(&f.token).len(), 2);
    s.start_turn(&f.token, &first, "blocked-stop".into(), f.envelope())
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(
        s.stop(&f.token, &first, Duration::ZERO),
        Err(SupervisorError::Pending)
    ));
    assert_eq!(s.active_count(), 2);
    assert!(matches!(
        s.start_turn(&f.token, &first, "fenced-new".into(), "new".into()),
        Err(SupervisorError::Closed)
    ));
    s.stop(&f.token, &first, Duration::from_secs(12)).unwrap();
    assert_eq!(s.active_count(), 1);
    s.stop(&f.token, &f.scope, Duration::from_secs(5)).unwrap();
    assert_eq!(s.active_count(), 0);
}

#[test]
fn endpoint_revoke_is_not_undone_by_heartbeat() {
    let f = Fixture::new();
    let s = f.supervisor();
    s.start(
        f.config(false),
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    exec(
        &f.engine,
        &f.admin,
        Some(&f.scope.space_id),
        "endpoints.grant",
        json!({"endpoint_id":f.scope.endpoint_id,"member_id":f.scope.identity_id,"operations":[]}),
    );
    let deadline = Instant::now() + Duration::from_secs(6);
    while s.active_count() > 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(s
        .start_turn(&f.token, &f.scope, "revoked".into(), "do not send".into())
        .is_err());
}

#[test]
fn owned_renewal_requires_exact_member_scope_and_never_unfences() {
    let f = Fixture::new();
    let mut host = f
        .registry
        .spawn(
            f.config(false),
            &f.token,
            Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        )
        .unwrap();
    // A real held child can renew before initialize, without accepting another bearer.
    assert!(f
        .registry
        .renew_owned_attachment(&f.scope, &f.admin)
        .is_err());
    f.registry
        .renew_owned_attachment(&f.scope, &f.token)
        .unwrap();
    let mut wrong = f.scope.clone();
    wrong.attachment_id = "foreign".into();
    assert!(f.registry.renew_owned_attachment(&wrong, &f.token).is_err());
    f.registry.fence_binding(&f.scope.session_id, 1).unwrap();
    assert!(f
        .registry
        .renew_owned_attachment(&f.scope, &f.token)
        .is_err());
    host.close().unwrap();
}

#[test]
fn child_exit_is_unknown_without_replay_and_counts_only_actual_exit() {
    let f = Fixture::new();
    let s = f.supervisor();
    let mut config = f.config(false);
    config.args[1] = "eof".into();
    s.start(
        config,
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    s.start_turn(&f.token, &f.scope, "eof-op".into(), "one send".into())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while s.active_count() > 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        s.operation(&f.token, &f.scope, "eof-op").unwrap().phase,
        OperationPhase::Unknown
    );
    s.start_turn(&f.token, &f.scope, "eof-op".into(), "one send".into())
        .unwrap();
    assert!(s
        .start_turn(
            &f.token,
            &f.scope,
            "eof-op".into(),
            "changed payload".into()
        )
        .is_err());
    assert_eq!(s.active_count(), 0);
}

#[test]
fn queue_is_bounded_and_queued_work_has_inspectable_status() {
    let f = Fixture::new();
    let s = OwnedSessionSupervisor::new(
        f.engine.clone(),
        f.registry.clone(),
        SupervisorOptions {
            queue_capacity: 1,
            ..SupervisorOptions::default()
        },
    )
    .unwrap();
    s.start(
        f.config(true),
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    s.start_turn(&f.token, &f.scope, "slow-queue".into(), f.envelope())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while s.operation(&f.token, &f.scope, "slow-queue").unwrap().phase == OperationPhase::Queued {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    s.start_turn(
        &f.token,
        &f.scope,
        "queued".into(),
        "must not duplicate".into(),
    )
    .unwrap();
    assert_eq!(
        s.operation(&f.token, &f.scope, "queued").unwrap().phase,
        OperationPhase::Queued
    );
    assert!(matches!(
        s.start_turn(&f.token, &f.scope, "overflow".into(), "full".into()),
        Err(SupervisorError::QueueFull)
    ));
    s.stop(&f.token, &f.scope, Duration::from_secs(12)).unwrap();
    assert_eq!(
        s.operation(&f.token, &f.scope, "queued").unwrap().phase,
        OperationPhase::Unknown
    );
}

#[test]
fn heartbeat_never_extends_root_credential_expiry() {
    let mut f = Fixture::new();
    let rotated = exec(
        &f.engine,
        &f.admin,
        Some(&f.scope.space_id),
        "members.rotate_credential",
        json!({"member_id":f.scope.identity_id,"expected_epoch":0,"ttl_seconds":2}),
    );
    f.token = rotated["credential"].as_str().unwrap().into();
    let original_expiry = rotated["credential_expires_at"].clone();
    let s = f.supervisor();
    s.start(
        f.config(false),
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    let deadline = Instant::now() + Duration::from_secs(7);
    while s.active_count() > 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(s.get(&f.token, &f.scope).is_err());
    let members = exec(
        &f.engine,
        &f.admin,
        Some(&f.scope.space_id),
        "members.list",
        json!({}),
    );
    assert!(members.to_string().contains(&original_expiry.to_string()));
}

#[test]
fn accepted_queue_exposes_provider_rejection_and_payload_conflict() {
    let f = Fixture::new();
    let s = f.supervisor();
    let mut config = f.config(false);
    config.args[1] = "normal".into();
    s.start(
        config,
        f.token.clone(),
        Arc::new(EngineForumFacade::new(f.engine.clone(), f.token.clone())),
        ThreadAdmission::New,
    )
    .unwrap();
    f.running(&s);
    s.start_turn(&f.token, &f.scope, "active".into(), "remain active".into())
        .unwrap();
    s.start_turn(&f.token, &f.scope, "rejected".into(), "while busy".into())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let op = s.operation(&f.token, &f.scope, "rejected").unwrap();
        if op.phase == OperationPhase::Rejected {
            assert!(op.error.is_some());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(matches!(
        s.start_turn(&f.token, &f.scope, "rejected".into(), "different".into()),
        Err(SupervisorError::Conflict)
    ));
    s.stop(&f.token, &f.scope, Duration::from_secs(5)).unwrap();
}
