use linkstart::{
    runtime_codex_host::{ExecutionProfile, OwnedCodexRegistry},
    runtime_core::{owner_token_path, Engine, ToolRequest},
    runtime_launch_policy::*,
};
use serde_json::{json, Value};
use std::time::Duration;

fn rejected_installer(f: &Fixture, case: &str) -> String {
    match case {
        "invalid" => "invalid-actor".into(),
        "cross-space" => alternate_scope(f, true).0,
        "no-manage" => f.worker.clone(),
        "revoked" => {
            let invite = call(
                &f.e,
                &f.admin,
                Some(&f.binding.space_id),
                "members.invite",
                json!({"roles":["manager"]}),
            );
            let manager = call(
                &f.e,
                invite["invite_token"].as_str().unwrap(),
                Some(&f.binding.space_id),
                "members.register",
                json!({"name":"revoked-installer"}),
            );
            call(
                &f.e,
                &f.admin,
                Some(&f.binding.space_id),
                "endpoints.grant",
                json!({"endpoint_id":f.binding.endpoint_id,"member_id":manager["id"],"operations":["manage"]}),
            );
            call(
                &f.e,
                &f.admin,
                Some(&f.binding.space_id),
                "members.revoke_credential",
                json!({"member_id":manager["id"],"expected_epoch":0}),
            );
            manager["credential"].as_str().unwrap().into()
        }
        _ => unreachable!(),
    }
}

#[test]
fn installer_admission_precedes_private_slot_and_preflight_observation() {
    for case in ["invalid", "cross-space", "no-manage", "revoked"] {
        let f = Fixture::new();
        f.provision();
        f.store
            .install(&f.admin, "existing-plan", None, f.plan())
            .unwrap();
        let actor = rejected_installer(&f, case);
        let expected =
            f.e.with_endpoint_authorization(
                &actor,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "manage",
                |_| Ok(()),
            )
            .unwrap_err();
        let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE unrelated_state(id INTEGER PRIMARY KEY,value TEXT NOT NULL); INSERT INTO unrelated_state VALUES(17,'must remain unchanged');").unwrap();
        let before = storage_snapshot(&db);
        for slot in [
            "absent",
            "existing-wrong-linkage",
            "existing-exact-linkage",
            "invalid-preflight",
        ] {
            let mut plan = f.plan();
            match slot {
                "absent" => plan.worker_slot = "absent-slot".into(),
                "existing-wrong-linkage" => {
                    plan.binding.identity_id = "probe-identity".into();
                    plan.binding.attachment_id = "probe-attachment".into();
                }
                "invalid-preflight" => plan.workspace = f.dir.path().join("absent-directory"),
                _ => {}
            }
            let observed = f
                .store
                .install(&actor, "existing-plan", None, plan)
                .unwrap_err();
            assert_eq!(
                (&observed.code, &observed.message),
                (&expected.code, &expected.message),
                "{case}/{slot}"
            );
            assert!(
                before == storage_snapshot(&db),
                "write after rejected {case}/{slot}"
            );
        }
    }
}

#[test]
fn provisioning_admission_precedes_worker_validation() {
    for case in ["invalid", "cross-space", "no-manage", "revoked"] {
        let f = Fixture::new();
        f.provision();
        let actor = rejected_installer(&f, case);
        let expected =
            f.e.with_endpoint_authorization(
                &actor,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "manage",
                |_| Ok(()),
            )
            .unwrap_err();
        let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE unrelated_state(id INTEGER PRIMARY KEY,value TEXT NOT NULL); INSERT INTO unrelated_state VALUES(17,'must remain unchanged');").unwrap();
        let before = storage_snapshot(&db);
        for id in ["absent-slot", "worker-slot"] {
            for credential in [&f.worker[..], "invalid-worker"] {
                let observed = f
                    .store
                    .provision_worker_slot(&actor, id, f.binding.clone(), credential)
                    .unwrap_err();
                assert_eq!(
                    (&observed.code, &observed.message),
                    (&expected.code, &expected.message),
                    "{case}/{id}"
                );
                assert!(
                    before == storage_snapshot(&db),
                    "write after rejected {case}/{id}"
                );
            }
        }
    }
}

fn alternate_scope(f: &Fixture, other_space: bool) -> (String, String, WorkerBinding) {
    let (admin, worker, mut binding) = if other_space {
        let owner = std::fs::read_to_string(owner_token_path(f.dir.path())).unwrap();
        let s = call(&f.e, &owner, None, "spaces.create", json!({"name":"other"}));
        let admin = s["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_owned();
        let sid = s["space"]["id"].as_str().unwrap();
        let hb = call(&f.e, &admin, Some(sid), "members.heartbeat", json!({}));
        let binding = WorkerBinding {
            space_id: sid.into(),
            endpoint_id: String::new(),
            identity_id: hb["identity_id"].as_str().unwrap().into(),
            attachment_id: hb["attachment_id"].as_str().unwrap().into(),
        };
        (admin.clone(), admin, binding)
    } else {
        (f.admin.clone(), f.worker.clone(), f.binding.clone())
    };
    let endpoint = call(
        &f.e,
        &admin,
        Some(&binding.space_id),
        "endpoints.register",
        json!({"name":"alternate","kind":"terminal"}),
    );
    binding.endpoint_id = endpoint["id"].as_str().unwrap().into();
    call(
        &f.e,
        &admin,
        Some(&binding.space_id),
        "endpoints.grant",
        json!({"endpoint_id":binding.endpoint_id,"member_id":binding.identity_id,"operations":["send"]}),
    );
    (admin, worker, binding)
}
#[test]
fn scope_namespaces_cross_space() {
    scope_namespace_case(true);
}
#[test]
fn scope_namespaces_cross_endpoint() {
    scope_namespace_case(false);
}
fn scope_namespace_case(other_space: bool) {
    {
        let f = Fixture::new();
        f.provision();
        let (admin, worker, binding) = alternate_scope(&f, other_space);
        f.store
            .provision_worker_slot(&admin, "other-slot", binding.clone(), &worker)
            .unwrap();
        let mut foreign = f.plan();
        foreign.binding = binding.clone();
        foreign.worker_slot = "other-slot".into();
        f.store
            .install(&admin, "shared-plan", None, foreign)
            .unwrap();
        let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
        let before: (String, u64, bool) = db
            .query_row(
                "SELECT document,revision,revoked FROM launch_plans WHERE id='shared-plan'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let missing = f
            .store
            .approve(
                &f.admin,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "missing",
                1,
            )
            .err()
            .unwrap();
        let foreign = f
            .store
            .approve(
                &f.admin,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "shared-plan",
                1,
            )
            .err()
            .unwrap();
        assert_eq!(
            (&missing.code, &missing.message),
            (&foreign.code, &foreign.message)
        );
        let missing = f
            .store
            .revoke(
                &f.admin,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "missing",
                1,
            )
            .unwrap_err();
        let foreign = f
            .store
            .revoke(
                &f.admin,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "shared-plan",
                1,
            )
            .unwrap_err();
        assert_eq!(
            (&missing.code, &missing.message),
            (&foreign.code, &foreign.message)
        );
        assert_eq!(
            f.store
                .install(&f.admin, "shared-plan", None, f.plan())
                .unwrap(),
            1
        );
        let after: (String, u64, bool) = db
            .query_row(
                "SELECT document,revision,revoked FROM launch_plans WHERE document=?1",
                [&before.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(before, after);
        assert!(f
            .store
            .approve(
                &admin,
                &binding.space_id,
                &binding.endpoint_id,
                "shared-plan",
                1
            )
            .is_ok());
    }
}
#[test]
fn worker_slots_cross_space() {
    worker_slot_case(true);
}
#[test]
fn worker_slots_cross_endpoint() {
    worker_slot_case(false);
}
fn worker_slot_case(other_space: bool) {
    {
        let f = Fixture::new();
        f.provision();
        let (admin, worker, binding) = alternate_scope(&f, other_space);
        f.store
            .provision_worker_slot(&admin, "worker-slot", binding.clone(), &worker)
            .unwrap();
        let mut plan = f.plan();
        plan.binding = binding.clone();
        f.store.install(&admin, "plan", None, plan).unwrap();
        assert!(f
            .store
            .approve(&admin, &binding.space_id, &binding.endpoint_id, "plan", 1)
            .is_ok());
        f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
        assert!(f.approve(1).is_ok());
    }
}
#[test]
fn extreme_timeout_obeys_platform_deadline_without_panic() {
    let f = Fixture::new();
    f.provision();
    let mut p = f.plan();
    p.timeout = Duration::from_millis(u64::MAX);
    let supported = std::time::Instant::now().checked_add(p.timeout).is_some();
    let result = f.store.install(&f.admin, "plan", None, p);
    if supported {
        assert_eq!(result.unwrap(), 1);
        assert!(f.approve(1).is_ok());
    } else {
        assert_eq!(result.unwrap_err().code, "Invalid");
    }
}
fn legacy_fixture(f: &Fixture, version: u64) {
    f.provision();
    f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
    f.store
        .install(&f.admin, "plan", Some(1), f.plan())
        .unwrap();
    f.store
        .install(&f.admin, "revoked", None, f.plan())
        .unwrap();
    f.store
        .revoke(
            &f.admin,
            &f.binding.space_id,
            &f.binding.endpoint_id,
            "revoked",
            1,
        )
        .unwrap();
    let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE;
        ALTER TABLE worker_slots RENAME TO slots_v2_fixture;
        ALTER TABLE launch_plans RENAME TO plans_v2_fixture;
        CREATE TABLE worker_slots(id TEXT PRIMARY KEY,binding TEXT NOT NULL,credential TEXT NOT NULL);
        CREATE TABLE launch_plans(id TEXT PRIMARY KEY,revision INTEGER NOT NULL,revoked INTEGER NOT NULL DEFAULT 0,document TEXT NOT NULL);
        INSERT INTO worker_slots SELECT id,binding,credential FROM slots_v2_fixture;
        INSERT INTO launch_plans SELECT id,revision,revoked,document FROM plans_v2_fixture;
        DROP TABLE slots_v2_fixture; DROP TABLE plans_v2_fixture;
        CREATE TABLE unrelated_state(id INTEGER PRIMARY KEY,value TEXT NOT NULL);
        INSERT INTO unrelated_state VALUES(17,'must remain unchanged'); COMMIT;").unwrap();
    db.pragma_update(None, "user_version", version).unwrap();
}
fn storage_snapshot(
    db: &rusqlite::Connection,
) -> (
    Vec<(String, String, String)>,
    Vec<(String, u64, bool, String)>,
    String,
) {
    let slots = db
        .prepare("SELECT id,binding,credential FROM worker_slots ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let plans = db
        .prepare("SELECT id,revision,revoked,document FROM launch_plans ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let unrelated = db
        .query_row("SELECT value FROM unrelated_state WHERE id=17", [], |r| {
            r.get(0)
        })
        .unwrap();
    (slots, plans, unrelated)
}
#[test]
fn legacy_migration_preserves_rows_secrets_revisions_and_reopen_is_idempotent() {
    for version in [0, 1] {
        let f = Fixture::new();
        legacy_fixture(&f, version);
        let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
        let before = storage_snapshot(&db);
        for _ in 0..2 {
            let reopened = LaunchPolicyStore::open(
                f.dir.path(),
                f.e.clone(),
                OwnedCodexRegistry::open(f.dir.path(), f.e.clone()).unwrap(),
            )
            .unwrap();
            assert!(before == storage_snapshot(&db));
            assert_eq!(
                db.query_row("PRAGMA user_version", [], |r| r.get::<_, u64>(0))
                    .unwrap(),
                2
            );
            assert!(reopened
                .approve(
                    &f.admin,
                    &f.binding.space_id,
                    &f.binding.endpoint_id,
                    "plan",
                    2
                )
                .is_ok());
            assert_eq!(
                reopened
                    .approve(
                        &f.admin,
                        &f.binding.space_id,
                        &f.binding.endpoint_id,
                        "revoked",
                        2
                    )
                    .err()
                    .unwrap()
                    .code,
                "Conflict"
            );
        }
        let worker_columns: u64 = db
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('worker_slots')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(worker_columns, 5);
    }
}
#[test]
fn malformed_legacy_document_rolls_back_both_tables_and_unrelated_state() {
    let f = Fixture::new();
    legacy_fixture(&f, 1);
    let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
    db.execute(
        "UPDATE launch_plans SET document='PRIVATE-MIGRATION-SENTINEL' WHERE id='revoked'",
        [],
    )
    .unwrap();
    let before = storage_snapshot(&db);
    let error = LaunchPolicyStore::open(
        f.dir.path(),
        f.e.clone(),
        OwnedCodexRegistry::open(f.dir.path(), f.e.clone()).unwrap(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(!error.contains(&f.worker) && !error.contains("PRIVATE-MIGRATION-SENTINEL"));
    assert!(before == storage_snapshot(&db));
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('worker_slots')",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        3
    );
    assert_eq!(db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('worker_slots_v1','launch_plans_v1')",[],|r|r.get::<_,u64>(0)).unwrap(),0);
}
#[test]
fn unknown_future_schema_is_rejected_without_mutation() {
    let f = Fixture::new();
    legacy_fixture(&f, 99);
    let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
    let before = storage_snapshot(&db);
    let error = LaunchPolicyStore::open(
        f.dir.path(),
        f.e.clone(),
        OwnedCodexRegistry::open(f.dir.path(), f.e.clone()).unwrap(),
    )
    .err()
    .unwrap()
    .to_string();
    assert_eq!(error, "unsupported launch policy schema version");
    assert!(before == storage_snapshot(&db));
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        99
    );
}
#[test]
fn identifier_and_persisted_timeout_boundaries() {
    let f = Fixture::new();
    f.provision();
    for timeout in [
        Duration::ZERO,
        Duration::from_nanos(1),
        Duration::from_nanos(1_000_001),
        Duration::from_secs(u64::MAX),
    ] {
        let mut p = f.plan();
        p.timeout = timeout;
        assert_eq!(
            f.store.install(&f.admin, "plan", None, p).unwrap_err().code,
            "Invalid"
        );
    }
    assert_eq!(
        f.store
            .install(&f.admin, &"x".repeat(257), None, f.plan())
            .unwrap_err()
            .code,
        "Invalid"
    );
    assert_eq!(
        f.store
            .provision_worker_slot(&f.admin, &"x".repeat(257), f.binding.clone(), &f.worker)
            .unwrap_err()
            .code,
        "Invalid"
    );
    let mut p = f.plan();
    p.timeout = Duration::from_millis(1);
    assert_eq!(f.store.install(&f.admin, "plan", None, p).unwrap(), 1);
    assert!(f.approve(1).is_ok());
}
#[test]
fn exact_space_is_required_even_for_owner() {
    let f = Fixture::new();
    f.provision();
    f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(f.dir.path())).unwrap();
    let other = call(&f.e, &owner, None, "spaces.create", json!({"name":"other"}));
    let sid = other["space"]["id"].as_str().unwrap();
    let token = other["administrator"]["credential"].as_str().unwrap();
    let ep = call(
        &f.e,
        token,
        Some(sid),
        "endpoints.register",
        json!({"name":"other","kind":"terminal"}),
    );
    assert_eq!(
        f.store
            .approve(&owner, sid, ep["id"].as_str().unwrap(), "plan", 1)
            .err()
            .unwrap()
            .code,
        "LaunchApprovalRequired"
    );
}
#[test]
fn expired_worker_remains_expired_after_reopen() {
    let f = Fixture::new();
    let rotated = call(
        &f.e,
        &f.admin,
        Some(&f.binding.space_id),
        "members.rotate_credential",
        json!({"member_id":f.binding.identity_id,"expected_epoch":0,"ttl_seconds":1}),
    );
    f.store
        .provision_worker_slot(
            &f.admin,
            "worker-slot",
            f.binding.clone(),
            rotated["credential"].as_str().unwrap(),
        )
        .unwrap();
    f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
    let future = rotated["credential_expires_at"].as_u64().unwrap();
    let engine =
        Engine::open_with_clock(f.dir.path(), std::sync::Arc::new(move || future)).unwrap();
    let store = LaunchPolicyStore::open(
        f.dir.path(),
        engine.clone(),
        OwnedCodexRegistry::open(f.dir.path(), engine).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store
            .approve(
                &f.admin,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "plan",
                1
            )
            .err()
            .unwrap()
            .code,
        "WorkerCredentialRequired"
    );
}
#[test]
fn workspace_write_keeps_approved_profile_across_reopen() {
    let f = Fixture::new();
    f.provision();
    let registry = OwnedCodexRegistry::open(f.dir.path(), f.e.clone()).unwrap();
    let result = registry.approve_workspace(
        &f.admin,
        &f.binding.space_id,
        &f.binding.endpoint_id,
        f.dir.path(),
        "0.163.0",
        "fake-admission-no-provider",
    );
    #[cfg(not(windows))]
    {
        assert!(matches!(
            result,
            Err(linkstart::runtime_codex_host::HostError::Unsupported(_))
        ));
    }
    #[cfg(windows)]
    {
        let mut p = f.plan();
        p.profile = ExecutionProfile::WorkspaceWrite(result.unwrap());
        p.capability_evidence = "fake-admission-no-provider".into();
        f.store.install(&f.admin, "plan", None, p).unwrap();
        let store = LaunchPolicyStore::open(f.dir.path(), f.e.clone(), registry).unwrap();
        let launch = store
            .approve(
                &f.admin,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "plan",
                1,
            )
            .unwrap();
        assert!(matches!(
            launch.profile(),
            ExecutionProfile::WorkspaceWrite(_)
        ));
    }
}
fn call(e: &Engine, t: &str, s: Option<&str>, tool: &str, args: Value) -> Value {
    e.execute(
        t,
        &ToolRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            tool: tool.into(),
            space_id: s.map(str::to_owned),
            args,
        },
    )
    .unwrap()
}
struct Fixture {
    dir: tempfile::TempDir,
    e: Engine,
    store: LaunchPolicyStore,
    admin: String,
    worker: String,
    binding: WorkerBinding,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let s = call(&e, &owner, None, "spaces.create", json!({"name":"launch"}));
        let sid = s["space"]["id"].as_str().unwrap();
        let admin = s["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_owned();
        let ep = call(
            &e,
            &admin,
            Some(sid),
            "endpoints.register",
            json!({"name":"worker","kind":"terminal"}),
        );
        let inv = call(
            &e,
            &admin,
            Some(sid),
            "members.invite",
            json!({"roles":["implementer"]}),
        );
        let w = call(
            &e,
            inv["invite_token"].as_str().unwrap(),
            Some(sid),
            "members.register",
            json!({"name":"worker"}),
        );
        let worker = w["credential"].as_str().unwrap().to_owned();
        call(
            &e,
            &admin,
            Some(sid),
            "endpoints.grant",
            json!({"endpoint_id":ep["id"],"member_id":w["id"],"operations":["send"]}),
        );
        let hb = call(&e, &worker, Some(sid), "members.heartbeat", json!({}));
        let binding = WorkerBinding {
            space_id: sid.into(),
            endpoint_id: ep["id"].as_str().unwrap().into(),
            identity_id: w["id"].as_str().unwrap().into(),
            attachment_id: hb["attachment_id"].as_str().unwrap().into(),
        };
        let registry = OwnedCodexRegistry::open(dir.path(), e.clone()).unwrap();
        let store = LaunchPolicyStore::open(dir.path(), e.clone(), registry).unwrap();
        Self {
            dir,
            e,
            store,
            admin,
            worker,
            binding,
        }
    }
    fn plan(&self) -> TrustedLaunchPlan {
        TrustedLaunchPlan {
            binding: self.binding.clone(),
            worker_slot: "worker-slot".into(),
            executable: std::env::current_exe().unwrap(),
            args: vec!["fake-no-spawn".into()],
            workspace: self.dir.path().into(),
            model: "fake-model".into(),
            provider_version: "0.163.0".into(),
            capability_evidence: "fake-no-spawn".into(),
            timeout: Duration::from_secs(1),
            profile: ExecutionProfile::ReadOnlyForum,
        }
    }
    fn provision(&self) {
        self.store
            .provision_worker_slot(
                &self.admin,
                "worker-slot",
                self.binding.clone(),
                &self.worker,
            )
            .unwrap();
    }
    fn approve(&self, rev: u64) -> Result<ApprovedLaunch, linkstart::runtime_core::ApiError> {
        self.store.approve(
            &self.admin,
            &self.binding.space_id,
            &self.binding.endpoint_id,
            "plan",
            rev,
        )
    }
}
#[test]
fn durable_exact_plan_and_private_slot() {
    let f = Fixture::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(f.dir.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(f.dir.path().join("v2-launch-policy.sqlite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    f.provision();
    assert_eq!(
        f.store.install(&f.admin, "plan", None, f.plan()).unwrap(),
        1
    );
    assert!(f.approve(1).is_ok());
    assert_eq!(f.approve(2).err().unwrap().code, "Conflict");
    assert_eq!(
        f.store
            .install(&f.admin, "plan", Some(0), f.plan())
            .unwrap_err()
            .code,
        "Conflict"
    );
    assert_eq!(
        f.store.install(&f.admin, "plan", None, f.plan()).unwrap(),
        1
    );
    let reopened = LaunchPolicyStore::open(
        f.dir.path(),
        f.e.clone(),
        OwnedCodexRegistry::open(f.dir.path(), f.e.clone()).unwrap(),
    )
    .unwrap();
    assert!(reopened
        .approve(
            &f.admin,
            &f.binding.space_id,
            &f.binding.endpoint_id,
            "plan",
            1
        )
        .is_ok());
    let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
    let raw: String = db
        .query_row(
            "SELECT document FROM launch_plans WHERE id='plan'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!raw.contains(&f.worker));
    assert!(!raw.contains(&f.admin));
}
#[test]
fn missing_slot_nonadmin_and_borrowed_manager_are_rejected() {
    let f = Fixture::new();
    assert_eq!(
        f.store
            .install(&f.admin, "plan", None, f.plan())
            .unwrap_err()
            .code,
        "WorkerCredentialRequired"
    );
    assert_eq!(
        f.store
            .provision_worker_slot(&f.admin, "wrong", f.binding.clone(), &f.admin)
            .unwrap_err()
            .code,
        "Denied"
    );
    assert_eq!(
        f.store
            .provision_worker_slot(&f.worker, "wrong", f.binding.clone(), &f.worker)
            .unwrap_err()
            .code,
        "Denied"
    );
    f.provision();
    f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
    assert_eq!(
        f.store
            .approve(
                &f.worker,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "plan",
                1
            )
            .err()
            .unwrap()
            .code,
        "Denied"
    );
}
#[test]
fn rotation_never_recreates_or_replaces_private_slot() {
    let f = Fixture::new();
    f.provision();
    f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
    let fresh = call(
        &f.e,
        &f.admin,
        Some(&f.binding.space_id),
        "members.rotate_credential",
        json!({"member_id":f.binding.identity_id,"expected_epoch":0}),
    );
    assert_eq!(f.approve(1).err().unwrap().code, "WorkerCredentialRequired");
    assert_eq!(
        f.store
            .provision_worker_slot(
                &f.admin,
                "worker-slot",
                f.binding.clone(),
                fresh["credential"].as_str().unwrap()
            )
            .unwrap_err()
            .code,
        "Conflict"
    );
}
#[test]
fn revocation_advances_revision_and_replay_does_not_restore_it() {
    let f = Fixture::new();
    f.provision();
    f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
    assert_eq!(
        f.store
            .revoke(
                &f.admin,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "plan",
                1
            )
            .unwrap(),
        2
    );
    assert_eq!(f.approve(1).err().unwrap().code, "Conflict");
    assert_eq!(f.approve(2).err().unwrap().code, "Conflict");
    assert_eq!(
        f.store
            .install(&f.admin, "plan", None, f.plan())
            .unwrap_err()
            .code,
        "Conflict"
    );
    assert_eq!(
        f.store
            .install(&f.admin, "plan", Some(2), f.plan())
            .unwrap(),
        3
    );
    assert!(f.approve(3).is_ok());
}
#[test]
fn malformed_plan_and_unknown_target_fail_closed() {
    let f = Fixture::new();
    f.provision();
    f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
    assert_eq!(
        f.store
            .approve(
                &f.admin,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "missing",
                1
            )
            .err()
            .unwrap()
            .code,
        "LaunchApprovalRequired"
    );
    let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
    db.execute("UPDATE launch_plans SET document='{}'", [])
        .unwrap();
    assert_eq!(f.approve(1).err().unwrap().code, "Invalid");
}
#[test]
fn credential_cannot_leak_through_public_plan_or_fake_argv() {
    let f = Fixture::new();
    f.provision();
    let mut p = f.plan();
    p.args.push(f.worker.clone());
    assert_eq!(
        f.store.install(&f.admin, "plan", None, p).unwrap_err().code,
        "Invalid"
    );
}
#[test]
fn actor_may_use_explicit_manage_without_becoming_installer() {
    let f = Fixture::new();
    f.provision();
    f.store.install(&f.admin, "plan", None, f.plan()).unwrap();
    call(
        &f.e,
        &f.admin,
        Some(&f.binding.space_id),
        "endpoints.grant",
        json!({"endpoint_id":f.binding.endpoint_id,"member_id":f.binding.identity_id,"operations":["send","manage"]}),
    );
    assert!(f
        .store
        .approve(
            &f.worker,
            &f.binding.space_id,
            &f.binding.endpoint_id,
            "plan",
            1
        )
        .is_ok());
    assert_eq!(
        f.store
            .install(&f.worker, "other", None, f.plan())
            .unwrap_err()
            .code,
        "Denied"
    );
    call(
        &f.e,
        &f.admin,
        Some(&f.binding.space_id),
        "endpoints.grant",
        json!({"endpoint_id":f.binding.endpoint_id,"member_id":f.binding.identity_id,"operations":["send"]}),
    );
    assert_eq!(
        f.store
            .approve(
                &f.worker,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "plan",
                1
            )
            .err()
            .unwrap()
            .code,
        "Denied"
    );
}
