use linkstart::{
    runtime_codex_host::{ExecutionProfile, OwnedCodexRegistry},
    runtime_core::{owner_token_path, Engine, ToolRequest},
    runtime_launch_policy::*,
};
use serde_json::{json, Value};
use std::time::Duration;
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
        "Denied"
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
