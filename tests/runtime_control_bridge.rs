use linkstart::{
    runtime_codex_host::*,
    runtime_control_bridge::OwnedControlBridge,
    runtime_core::{owner_token_path, Engine, ToolRequest},
    runtime_launch_policy::*,
    runtime_supervisor::*,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

fn exec(e: &Engine, a: &str, s: Option<&str>, t: &str, args: Value) -> Value {
    e.execute(
        a,
        &ToolRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            tool: t.into(),
            space_id: s.map(Into::into),
            args,
        },
    )
    .unwrap()
}
struct Fixture {
    dir: tempfile::TempDir,
    engine: Engine,
    admin: String,
    worker: String,
    binding: WorkerBinding,
    plans: Arc<LaunchPolicyStore>,
    supervisor: Arc<OwnedSessionSupervisor>,
    bridge: Arc<OwnedControlBridge>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let space = exec(
            &engine,
            &owner,
            None,
            "spaces.create",
            json!({"name":"bridge"}),
        );
        let sid = space["space"]["id"].as_str().unwrap();
        let admin = space["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_owned();
        let ep = exec(
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
            json!({"name":"worker"}),
        );
        let worker = member["credential"].as_str().unwrap().to_owned();
        exec(
            &engine,
            &admin,
            Some(sid),
            "endpoints.grant",
            json!({"endpoint_id":ep["id"],"member_id":member["id"],"operations":["send","observe","control"]}),
        );
        let hb = exec(&engine, &worker, Some(sid), "members.heartbeat", json!({}));
        let binding = WorkerBinding {
            space_id: sid.into(),
            endpoint_id: ep["id"].as_str().unwrap().into(),
            identity_id: member["id"].as_str().unwrap().into(),
            attachment_id: hb["attachment_id"].as_str().unwrap().into(),
        };
        let registry = OwnedCodexRegistry::open(dir.path(), engine.clone()).unwrap();
        let plans = Arc::new(
            LaunchPolicyStore::open(dir.path(), engine.clone(), registry.clone()).unwrap(),
        );
        plans
            .provision_worker_slot(&admin, "slot", binding.clone(), &worker)
            .unwrap();
        let script = dir.path().join("fake.py");
        std::fs::write(
            &script,
            include_str!("fixtures/owned_codex_server.py").replace(
                "thread='owned-thread'",
                "import os\nthread='owned-thread-'+str(os.getpid())",
            ),
        )
        .unwrap();
        let python = std::process::Command::new(if cfg!(windows) { "python" } else { "python3" })
            .args(["-c", "import sys;print(sys.executable)"])
            .output()
            .unwrap();
        plans
            .install(
                &admin,
                "plan",
                None,
                TrustedLaunchPlan {
                    binding: binding.clone(),
                    worker_slot: "slot".into(),
                    executable: PathBuf::from(String::from_utf8(python.stdout).unwrap().trim()),
                    args: vec![script.to_str().unwrap().into(), "history".into()],
                    workspace: dir.path().into(),
                    model: "gpt-6-luna".into(),
                    provider_version: "0.163.0".into(),
                    capability_evidence: "controlled fake provider".into(),
                    timeout: Duration::from_secs(3),
                    profile: ExecutionProfile::ReadOnlyForum,
                },
            )
            .unwrap();
        let supervisor = Arc::new(
            OwnedSessionSupervisor::new(
                engine.clone(),
                registry.clone(),
                SupervisorOptions::default(),
            )
            .unwrap(),
        );
        let participants = Arc::new(
            linkstart::runtime_participants::ParticipantService::open(
                dir.path(),
                engine.clone(),
                Arc::new(registry.clone()),
            )
            .unwrap(),
        );
        let bridge = Arc::new(
            OwnedControlBridge::open(
                dir.path(),
                engine.clone(),
                supervisor.clone(),
                registry,
                participants,
                plans.clone(),
            )
            .unwrap(),
        );
        Self {
            dir,
            engine,
            admin,
            worker,
            binding,
            plans,
            supervisor,
            bridge,
        }
    }
    fn request(&self, id: &str, tool: &str, args: Value) -> ToolRequest {
        ToolRequest {
            request_id: id.into(),
            tool: tool.into(),
            space_id: Some(self.binding.space_id.clone()),
            args,
        }
    }
    fn launch_request(&self) -> ToolRequest {
        self.request("launch","owned.launch",json!({"endpoint_id":self.binding.endpoint_id,"plan_id":"plan","expected_plan_revision":1}))
    }
    fn launch(&self) -> Value {
        self.bridge
            .execute(&self.admin, &self.launch_request())
            .unwrap()
    }
    fn target(&self, v: &Value) -> Value {
        json!({"endpoint_id":self.binding.endpoint_id,"session_id":v["session_id"],"expected_generation":v["binding_generation"]})
    }
    fn running(&self, v: &Value) {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let status = self
                .bridge
                .execute(
                    &self.admin,
                    &self.request("get", "owned.get", self.target(v)),
                )
                .unwrap();
            if status["phase"] == "Running" {
                return;
            }
            assert!(Instant::now() < until, "{status}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn actor(&self, grants: &[&str]) -> (String, String) {
        let invite = exec(
            &self.engine,
            &self.admin,
            Some(&self.binding.space_id),
            "members.invite",
            json!({"roles":["observer"]}),
        );
        let member = exec(
            &self.engine,
            invite["invite_token"].as_str().unwrap(),
            Some(&self.binding.space_id),
            "members.register",
            json!({"name":"actor"}),
        );
        let id = member["id"].as_str().unwrap().to_owned();
        self.grants(&id, grants);
        (member["credential"].as_str().unwrap().into(), id)
    }
    fn grants(&self, id: &str, grants: &[&str]) {
        exec(
            &self.engine,
            &self.admin,
            Some(&self.binding.space_id),
            "endpoints.grant",
            json!({"endpoint_id":self.binding.endpoint_id,"member_id":id,"operations":grants}),
        );
    }
    fn gate_reads(&self) {
        let path = self.dir.path().join("fake.py");
        let script=std::fs::read_to_string(&path).unwrap().replace("elif method=='thread/read':","elif method=='thread/read':\n  import pathlib,time\n  pathlib.Path('read-entered').touch()\n  while not pathlib.Path('read-release').exists():time.sleep(0.005)").replace("elif method=='turn/start':","elif method=='turn/start':\n  open('turn-count','a').write('sent\\n')");
        std::fs::write(path, script).unwrap();
    }
    fn gate_interrupt(&self) {
        let path = self.dir.path().join("fake.py");
        let script=std::fs::read_to_string(&path).unwrap().replace(
            "elif method=='turn/interrupt':send({'id':rid,'result':{'interruptedTurn':p['turnId']}})",
            "elif method=='turn/interrupt':\n  import pathlib,time\n  open('interrupt-count','a').write('sent\\n')\n  pathlib.Path('interrupt-effect').touch()\n  while not pathlib.Path('interrupt-release').exists():time.sleep(0.005)\n  send({'id':rid,'result':{'interruptedTurn':p['turnId'],'privateReply':'controlled-sensitive-reply'}})"
        );
        std::fs::write(path, script).unwrap();
    }
    fn active_turn(&self, launch: &Value) {
        for (request_id, state) in [("active-first", "confirmed"), ("active-second", "accepted")] {
            let mut args = self.target(launch);
            args["text"] = json!(request_id);
            let receipt = self
                .bridge
                .execute(&self.admin, &self.request(request_id, "owned.turn", args))
                .unwrap();
            let until = Instant::now() + Duration::from_secs(4);
            loop {
                let mut args = self.target(launch);
                args["operation_id"] = receipt["operation_id"].clone();
                let status = self
                    .bridge
                    .execute(
                        &self.admin,
                        &self.request("wait-active", "owned.operation.get", args),
                    )
                    .unwrap();
                if status["state"] == state {
                    break;
                }
                assert!(Instant::now() < until, "{status}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    fn await_file(&self, name: &str) {
        let until = Instant::now() + Duration::from_secs(4);
        while !self.dir.path().join(name).exists() {
            assert!(Instant::now() < until, "missing {name}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn queued_turn_and_read_delivery_keep_original_actor_authority() {
    let f = Fixture::new();
    f.gate_reads();
    let launched = f.launch();
    f.running(&launched);
    let (actor, id) = f.actor(&["observe", "send"]);
    let bridge = f.bridge.clone();
    let request = f.request("read", "owned.read", {
        let mut v = f.target(&launched);
        v["wait_ms"] = json!(1000);
        v
    });
    let credential = actor.clone();
    let read = std::thread::spawn(move || bridge.execute(&credential, &request));
    f.await_file("read-entered");
    let mut args = f.target(&launched);
    args["text"] = json!("must never send");
    let turn = f
        .bridge
        .execute(&actor, &f.request("queued", "owned.turn", args))
        .unwrap();
    f.grants(&id, &[]);
    std::fs::write(f.dir.path().join("read-release"), "").unwrap();
    assert_eq!(read.join().unwrap().unwrap_err().code, "Denied");
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let mut args = f.target(&launched);
        args["operation_id"] = turn["operation_id"].clone();
        let op = f
            .bridge
            .execute(&f.admin, &f.request("lookup", "owned.operation.get", args))
            .unwrap();
        if op["state"] == "rejected" {
            break;
        }
        assert!(Instant::now() < until, "{op}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!f.dir.path().join("turn-count").exists());
    assert!(f
        .bridge
        .execute(&actor, &f.request("get", "owned.get", f.target(&launched)))
        .is_err());
}

#[test]
fn grant_matrix_and_explicit_operation_store_routing() {
    let f = Fixture::new();
    let launched = f.launch();
    f.running(&launched);
    for grant in ["observe", "send", "control", "manage"] {
        let (actor, _) = f.actor(&[grant]);
        let get = f
            .bridge
            .execute(&actor, &f.request("get", "owned.get", f.target(&launched)));
        assert_eq!(get.is_ok(), grant == "observe");
        let list = f
            .bridge
            .execute(&actor, &f.request("list", "owned.list", json!({"limit":1})))
            .unwrap();
        assert_eq!(
            list["items"].as_array().unwrap().len(),
            if grant == "observe" { 1 } else { 0 }
        );
    }
    let mut args = f.target(&launched);
    args["source_kind"] = json!("owned_session");
    args["operation_id"] = launched["operation_id"].clone();
    let op = f
        .bridge
        .execute(&f.admin, &f.request("routing", "operations.get", args))
        .unwrap();
    assert_eq!(op["state"], "confirmed");
    assert_eq!(
        f.bridge
            .execute(
                &f.admin,
                &f.request("missing", "operations.get", json!({"operation_id":"x"}))
            )
            .unwrap_err()
            .code,
        "Unsupported"
    );
    assert_eq!(
        f.bridge
            .execute(
                &f.admin,
                &f.request("pause", "owned.pause", f.target(&launched))
            )
            .unwrap_err()
            .code,
        "Unsupported"
    );
}

#[test]
fn crash_reopen_intent_never_spawns_or_resends_and_unknown_blocks_new_turn() {
    let f = Fixture::new();
    let launch = f.launch();
    f.running(&launch);
    let mut args = f.target(&launch);
    args["text"] = json!("work");
    let r = f.request("turn", "owned.turn", args);
    let turn = f.bridge.execute(&f.admin, &r).unwrap();
    // A real fake-provider acknowledgement is witnessed before simulating loss of
    // the bridge's final receipt write. Recovery must not resend that effect.
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let mut args = f.target(&launch);
        args["operation_id"] = turn["operation_id"].clone();
        let observed = f
            .bridge
            .execute(&f.admin, &f.request("witness", "owned.operation.get", args))
            .unwrap();
        if observed["snapshot"]["phase"] == "Accepted"
            || observed["snapshot"]["phase"] == "Confirmed"
        {
            break;
        }
        assert!(Instant::now() < until, "{observed}");
        std::thread::sleep(Duration::from_millis(5));
    }
    let db = rusqlite::Connection::open(f.dir.path().join("v2-owned-control.sqlite")).unwrap();
    db.execute(
        "UPDATE requests SET state='intent' WHERE operation=?1",
        [turn["operation_id"].as_str().unwrap()],
    )
    .unwrap();
    f.supervisor.request_shutdown();
    let until = Instant::now() + Duration::from_secs(5);
    while f.supervisor.active_count() > 0 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    let registry = OwnedCodexRegistry::open(f.dir.path(), f.engine.clone()).unwrap();
    let supervisor = Arc::new(
        OwnedSessionSupervisor::new(f.engine.clone(), registry.clone(), Default::default())
            .unwrap(),
    );
    let participants = Arc::new(
        linkstart::runtime_participants::ParticipantService::open(
            f.dir.path(),
            f.engine.clone(),
            Arc::new(registry.clone()),
        )
        .unwrap(),
    );
    let reopened = OwnedControlBridge::open(
        f.dir.path(),
        f.engine.clone(),
        supervisor.clone(),
        registry,
        participants,
        f.plans.clone(),
    )
    .unwrap();
    let result = reopened.execute(&f.admin, &r).unwrap();
    assert_eq!(result["state"], "unknown");
    assert_eq!(result["operation_id"], turn["operation_id"]);
    assert_eq!(supervisor.active_count(), 0);
    assert_eq!(
        reopened
            .execute(
                &f.admin,
                &f.request("restart-read", "owned.read", f.target(&launch))
            )
            .unwrap()["state"],
        "unknown"
    );
    let mut lookup = f.target(&launch);
    lookup["operation_id"] = turn["operation_id"].clone();
    assert_eq!(
        reopened
            .execute(
                &f.admin,
                &f.request("restart-reconcile", "owned.operation.reconcile", lookup)
            )
            .unwrap()["state"],
        "unknown"
    );
    let mut new = r.clone();
    new.request_id = "new-turn".into();
    assert_eq!(
        reopened.execute(&f.admin, &new).unwrap_err().code,
        "Conflict"
    );
    let old = reopened.execute(&f.admin, &f.launch_request()).unwrap();
    assert_eq!(old["state"], "unknown");
    assert_eq!(old["session_id"], launch["session_id"]);
    assert_eq!(supervisor.active_count(), 0);
}

#[test]
fn actor_namespace_and_page_cursor_are_bound_to_current_space_and_identity() {
    let f = Fixture::new();
    let first = f.launch();
    f.running(&first);
    let (manager, _) = f.actor(&["manage"]);
    let second = f.bridge.execute(&manager, &f.launch_request()).unwrap();
    assert_ne!(first["operation_id"], second["operation_id"]);
    assert_ne!(first["session_id"], second["session_id"]);
    f.running(&second);
    let first_page = f
        .bridge
        .execute(
            &f.admin,
            &f.request("page", "owned.list", json!({"limit":1})),
        )
        .unwrap();
    assert_eq!(first_page["items"].as_array().unwrap().len(), 1);
    assert!(first_page["next_cursor"].is_string());
    let second_page = f
        .bridge
        .execute(
            &f.admin,
            &f.request(
                "page2",
                "owned.list",
                json!({"limit":1,"cursor":first_page["next_cursor"]}),
            ),
        )
        .unwrap();
    assert_eq!(second_page["items"].as_array().unwrap().len(), 1);
    assert_ne!(
        first_page["items"][0]["scope"]["session_id"],
        second_page["items"][0]["scope"]["session_id"]
    );
    let (observer, _) = f.actor(&["observe"]);
    assert_eq!(
        f.bridge
            .execute(
                &observer,
                &f.request(
                    "foreign-cursor",
                    "owned.list",
                    json!({"limit":1,"cursor":first_page["next_cursor"]})
                )
            )
            .unwrap_err()
            .code,
        "Invalid"
    );
    let mut foreign = f.request("foreign-space", "owned.list", json!({}));
    foreign.space_id = Some("foreign-space".into());
    assert_eq!(
        f.bridge.execute(&observer, &foreign).unwrap_err().code,
        "Denied"
    );
}

#[test]
fn crash_before_launch_binding_keeps_intent_lookup_without_a_second_admission() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    let request = f.launch_request();
    let operation = uuid::Uuid::new_v4().to_string();
    let session = uuid::Uuid::new_v4().to_string();
    let identity = exec(
        &f.engine,
        &f.admin,
        Some(&f.binding.space_id),
        "identity.current",
        json!({}),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!({"tool":request.tool,"args":request.args})).unwrap()
        )
    );
    let body = json!({"state":"unknown","operation_id":operation,"endpoint_id":f.binding.endpoint_id,"session_id":session,"binding_generation":1,"actor_identity":identity});
    let db = rusqlite::Connection::open(f.dir.path().join("v2-owned-control.sqlite")).unwrap();
    db.execute(
        "INSERT INTO requests(space,actor,request,fingerprint,operation,session,tool,state,body) VALUES(?1,?2,?3,?4,?5,?6,'owned.launch','intent',?7)",
        rusqlite::params![
            f.binding.space_id,
            identity,
            request.request_id,
            fingerprint,
            operation,
            session,
            body.to_string()
        ],
    )
    .unwrap();
    drop(db);
    let registry = OwnedCodexRegistry::open(f.dir.path(), f.engine.clone()).unwrap();
    let participants = Arc::new(
        linkstart::runtime_participants::ParticipantService::open(
            f.dir.path(),
            f.engine.clone(),
            Arc::new(registry.clone()),
        )
        .unwrap(),
    );
    let reopened = OwnedControlBridge::open(
        f.dir.path(),
        f.engine.clone(),
        f.supervisor.clone(),
        registry,
        participants,
        f.plans.clone(),
    )
    .unwrap();
    assert_eq!(
        reopened.execute(&f.admin, &request).unwrap()["state"],
        "unknown"
    );
    let mut args = f.target(&body);
    args["operation_id"] = json!(operation);
    assert_eq!(
        reopened
            .execute(&f.admin, &f.request("lookup", "owned.operation.get", args))
            .unwrap()["state"],
        "unknown"
    );
    assert_eq!(f.supervisor.active_count(), 0);
}

#[test]
fn launch_without_get_recovers_exact_durable_thread_metadata_after_restart() {
    let f = Fixture::new();
    let launch = f.launch();
    // Observe the real host durable boundary directly, without populating the
    // bridge status cache through get/list or the running helper.
    let host = rusqlite::Connection::open(f.dir.path().join("v2-codex-host.sqlite")).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    let expected: String = loop {
        use rusqlite::OptionalExtension;
        let found: Option<String> = host
            .query_row(
                "SELECT thread FROM owned_threads WHERE json_extract(scope,'$.session_id')=?1",
                [launch["session_id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        if let Some(thread) = found {
            break thread;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    };
    let bridge_db =
        rusqlite::Connection::open(f.dir.path().join("v2-owned-control.sqlite")).unwrap();
    let cache: Option<String> = bridge_db
        .query_row(
            "SELECT status FROM sessions WHERE id=?1",
            [launch["session_id"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(cache.is_none());
    drop(bridge_db);
    drop(host);
    f.supervisor.request_shutdown();
    let until = Instant::now() + Duration::from_secs(5);
    while f.supervisor.active_count() > 0 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    let registry = OwnedCodexRegistry::open(f.dir.path(), f.engine.clone()).unwrap();
    let supervisor = Arc::new(
        OwnedSessionSupervisor::new(f.engine.clone(), registry.clone(), Default::default())
            .unwrap(),
    );
    let participants = Arc::new(
        linkstart::runtime_participants::ParticipantService::open(
            f.dir.path(),
            f.engine.clone(),
            Arc::new(registry.clone()),
        )
        .unwrap(),
    );
    let reopened = OwnedControlBridge::open(
        f.dir.path(),
        f.engine.clone(),
        supervisor.clone(),
        registry,
        participants,
        f.plans.clone(),
    )
    .unwrap();
    let result = reopened
        .execute(
            &f.admin,
            &f.request("cold-get", "owned.get", f.target(&launch)),
        )
        .unwrap();
    assert_eq!(result["provider_thread_id"], expected);
    assert_eq!(result["profile"], "ReadOnlyForum");
    assert_eq!(result["phase"], "Unknown");
    assert_eq!(result["scope"]["binding_generation"], 1);
    assert_eq!(supervisor.active_count(), 0);
    let host = rusqlite::Connection::open(f.dir.path().join("v2-codex-host.sqlite")).unwrap();
    let original_scope: String = host
        .query_row(
            "SELECT scope FROM owned_threads WHERE thread=?1",
            [&expected],
            |row| row.get(0),
        )
        .unwrap();
    for corruption in ["scope", "generation", "profile"] {
        match corruption {
            "scope" => {
                host.execute("UPDATE owned_threads SET scope=json_set(scope,'$.attachment_id','foreign') WHERE thread=?1",[&expected]).unwrap();
            }
            "generation" => {
                host.execute(
                    "UPDATE host_generations SET generation=2 WHERE session=?1",
                    [launch["session_id"].as_str().unwrap()],
                )
                .unwrap();
            }
            _ => {
                host.execute(
                    "UPDATE owned_threads SET profile=?1 WHERE thread=?2",
                    rusqlite::params![json!("OtherProfile").to_string(), expected],
                )
                .unwrap();
            }
        }
        assert_eq!(
            reopened
                .execute(
                    &f.admin,
                    &f.request("corrupt-get", "owned.get", f.target(&launch))
                )
                .unwrap_err()
                .code,
            "Conflict"
        );
        host.execute(
            "UPDATE owned_threads SET scope=?1,profile=?2 WHERE thread=?3",
            rusqlite::params![original_scope, json!("ReadOnlyForum").to_string(), expected],
        )
        .unwrap();
        host.execute(
            "UPDATE host_generations SET generation=1 WHERE session=?1",
            [launch["session_id"].as_str().unwrap()],
        )
        .unwrap();
    }
    assert_eq!(supervisor.active_count(), 0);
}

#[test]
fn sealed_launch_revalidates_plan_worker_and_actor() {
    for revoke in ["plan", "worker", "actor"] {
        let f = Fixture::new();
        let (actor, id) = f.actor(&["manage"]);
        let approved = f
            .plans
            .approve(
                &actor,
                &f.binding.space_id,
                &f.binding.endpoint_id,
                "plan",
                1,
            )
            .unwrap();
        match revoke {
            "plan" => {
                f.plans
                    .revoke(
                        &f.admin,
                        &f.binding.space_id,
                        &f.binding.endpoint_id,
                        "plan",
                        1,
                    )
                    .unwrap();
            }
            "worker" => f.grants(&f.binding.identity_id, &[]),
            _ => f.grants(&id, &[]),
        }
        assert!(f
            .supervisor
            .start_as(
                &actor,
                approved,
                "server-owned".into(),
                1,
                ThreadAdmission::New
            )
            .is_err());
        assert_eq!(f.supervisor.active_count(), 0);
    }
}

#[test]
fn shutdown_ticket_detects_reservation_and_preexisting_launch_drains() {
    let f = Fixture::new();
    let before = f.bridge.inventory();
    let db = rusqlite::Connection::open(f.dir.path().join("v2-launch-policy.sqlite")).unwrap();
    db.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let bridge = f.bridge.clone();
    let actor = f.admin.clone();
    let request = f.launch_request();
    let launch = std::thread::spawn(move || bridge.execute(&actor, &request));
    let until = Instant::now() + Duration::from_secs(3);
    while f.bridge.inventory().launch_reservations == 0 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        f.bridge.begin_shutdown(before.epoch).unwrap_err().code,
        "StaleImpact"
    );
    let pending = f.bridge.begin_shutdown(f.bridge.inventory().epoch).unwrap();
    assert_eq!(pending.launch_reservations, 1);
    db.execute_batch("COMMIT").unwrap();
    let _ = launch.join().unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while f.bridge.inventory().active_workers > 0 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(f.bridge.inventory().launch_reservations, 0);
    assert!(f.bridge.inventory().draining);
}

fn f5_durable_body(f: &Fixture, operation: &Value) -> Value {
    let db = rusqlite::Connection::open(f.dir.path().join("v2-owned-control.sqlite")).unwrap();
    let body: String = db.query_row("SELECT body FROM requests WHERE operation=?1", [operation["operation_id"].as_str().unwrap()], |r| r.get(0)).unwrap();
    serde_json::from_str(&body).unwrap()
}

fn f5_get_first_terminal(outcome: &str) {
    let f = Fixture::new();
    f.gate_reads(); // count actual turn/start invocations; release reads initially.
    std::fs::write(f.dir.path().join("read-release"), "").unwrap();
    let path = f.dir.path().join("fake.py");
    let script = std::fs::read_to_string(&path).unwrap().replace("'status':'completed'", &format!("'status':'{outcome}'"))
        .replace("elif method=='thread/read':", &format!("elif method=='thread/read':\n  import pathlib\n  if history:history[0]['status']='{}' if pathlib.Path('contradictory-read').exists() else '{outcome}'", if outcome == "failed" { "completed" } else { "failed" }));
    std::fs::write(path, script).unwrap();
    let launch = f.launch();
    f.running(&launch);
    let mut args = f.target(&launch);
    args["text"] = json!("F5 exact terminal evidence");
    let turn_request = f.request("f5-original-turn", "owned.turn", args);
    let turn = f.bridge.execute(&f.admin, &turn_request).unwrap();
    let db = rusqlite::Connection::open(f.dir.path().join("v2-owned-control.sqlite")).unwrap();
    let encoded: String = db.query_row("SELECT scope FROM sessions WHERE id=?1", [launch["session_id"].as_str().unwrap()], |r| r.get(0)).unwrap();
    let scope: HostScope = serde_json::from_str(&encoded).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let snapshot = f.supervisor.operation_as(&f.admin, &scope, turn["operation_id"].as_str().unwrap()).unwrap();
        if matches!(snapshot.phase, OperationPhase::Confirmed) {
            assert_eq!(serde_json::to_value(snapshot.provider_outcome).unwrap(), outcome);
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut args = f.target(&launch);
    args["operation_id"] = turn["operation_id"].clone();
    let get = f.request("f5-get-first", "owned.operation.get", args.clone());
    let first = f.bridge.execute(&f.admin, &get).unwrap();
    assert_eq!(first["state"], "confirmed");
    assert_eq!(first["snapshot"]["provider_outcome"], outcome);
    assert_eq!(f5_durable_body(&f, &turn), first);
    args["wait_ms"] = json!(1000);
    let reconcile = f.request("f5-reconcile", "owned.operation.reconcile", args);
    let observed = f.bridge.execute(&f.admin, &reconcile).unwrap();
    eprintln!("F5 {outcome} get-first={first}; reconcile={observed}");
    assert_eq!(observed["observation"]["outcome"], outcome, "typed observation suppressed by first snapshot envelope");
    assert_eq!(observed["snapshot"], first["snapshot"]);
    assert_eq!(observed["operation_id"], turn["operation_id"]);
    assert_eq!(observed["observation"]["operation_id"], turn["operation_id"]);
    assert_eq!(observed["observation"]["turn_id"], first["snapshot"]["provider_turn_id"]);
    let thread: String = db.query_row("SELECT json_extract(status,'$.provider_thread_id') FROM sessions WHERE id=?1", [scope.session_id.as_str()], |r| r.get(0)).unwrap();
    assert_eq!(observed["observation"]["thread_id"], thread);
    assert_eq!(f5_durable_body(&f, &turn), observed);
    assert_eq!(f.bridge.execute(&f.admin, &get).unwrap(), observed);
    assert_eq!(f.bridge.execute(&f.admin, &reconcile).unwrap(), observed);
    assert_eq!(f.bridge.execute(&f.admin, &turn_request).unwrap(), observed);

    // A real exact provider read can contradict its prior terminal report. Do
    // not turn that incoming positive evidence into an admitted typed response.
    std::fs::write(f.dir.path().join("contradictory-read"), "").unwrap();
    assert_eq!(f.bridge.execute(&f.admin, &reconcile).unwrap_err().code, "Conflict");
    assert_eq!(f5_durable_body(&f, &turn), observed);
    assert_eq!(f.bridge.execute(&f.admin, &get).unwrap(), observed);
    std::fs::remove_file(f.dir.path().join("contradictory-read")).unwrap();
    assert_eq!(f.bridge.execute(&f.admin, &reconcile).unwrap(), observed);

    // A blocked exact read must still recheck the original observer at delivery.
    std::fs::remove_file(f.dir.path().join("read-entered")).unwrap();
    std::fs::remove_file(f.dir.path().join("read-release")).unwrap();
    let (actor, id) = f.actor(&["observe"]);
    let bridge = f.bridge.clone();
    let request = reconcile.clone();
    let credential = actor.clone();
    let reader = std::thread::spawn(move || bridge.execute(&credential, &request));
    f.await_file("read-entered");
    f.grants(&id, &[]);
    std::fs::write(f.dir.path().join("read-release"), "").unwrap();
    let denied = reader.join().unwrap();
    assert_eq!(denied.as_ref().unwrap_err().code, "Denied");
    assert!(!format!("{denied:?}").contains(&thread));
    assert_eq!(f5_durable_body(&f, &turn), observed);
    f.grants(&id, &["observe"]);
    assert_eq!(f.bridge.execute(&actor, &get).unwrap(), observed);

    f.supervisor.request_shutdown();
    let until = Instant::now() + Duration::from_secs(5);
    while f.supervisor.active_count() > 0 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(f.supervisor.get_as(&f.admin, &scope).unwrap().worker_exited);
    let registry = OwnedCodexRegistry::open(f.dir.path(), f.engine.clone()).unwrap();
    let supervisor = Arc::new(OwnedSessionSupervisor::new(f.engine.clone(), registry.clone(), Default::default()).unwrap());
    let participants = Arc::new(linkstart::runtime_participants::ParticipantService::open(f.dir.path(), f.engine.clone(), Arc::new(registry.clone())).unwrap());
    let cold = OwnedControlBridge::open(f.dir.path(), f.engine.clone(), supervisor.clone(), registry, participants, f.plans.clone()).unwrap();
    assert_eq!(cold.execute(&f.admin, &get).unwrap(), observed);
    assert_eq!(cold.execute(&f.admin, &turn_request).unwrap(), observed);
    // Historical admitted evidence survives; this does not claim a native resume.
    assert_eq!(cold.execute(&f.admin, &reconcile).unwrap(), observed);
    assert_eq!(supervisor.active_count(), 0);
    assert_eq!(f5_durable_body(&f, &turn), observed);
    assert_eq!(std::fs::read_to_string(f.dir.path().join("turn-count")).unwrap().lines().count(), 1);
}

#[test]
fn f5_get_first_completed_preserves_typed_observation() { f5_get_first_terminal("completed"); }
#[test]
fn f5_get_first_interrupted_preserves_typed_observation() { f5_get_first_terminal("interrupted"); }
#[test]
fn f5_get_first_failed_preserves_typed_observation() { f5_get_first_terminal("failed"); }

#[test]
fn f5_pending_without_observation_does_not_conflict_or_invent_terminal_evidence() {
    let f = Fixture::new();
    f.gate_reads();
    let launch = f.launch();
    f.running(&launch);
    let mut args = f.target(&launch);
    args["text"] = json!("pending exact read");
    let turn = f.bridge.execute(&f.admin, &f.request("f5-pending-turn", "owned.turn", args)).unwrap();
    // Deliberately do not get a completion projection before this zero-wait read.
    let before = f5_durable_body(&f, &turn);
    let mut args = f.target(&launch);
    args["operation_id"] = turn["operation_id"].clone();
    args["wait_ms"] = json!(0);
    let pending = f.bridge.execute(&f.admin, &f.request("f5-pending-read", "owned.operation.reconcile", args));
    f.await_file("read-entered");
    std::fs::write(f.dir.path().join("read-release"), "").unwrap();
    // NoObservation preserves V4's retained admission receipt; no Conflict or
    // synthetic observation replaces a real timeout with terminal provider proof.
    assert_eq!(pending.unwrap(), before);
    assert!(before.get("observation").is_none());
    assert_eq!(f5_durable_body(&f, &turn), before);
}

#[test]
fn bridge_typed_reconcile_exact_interrupt_and_manage_only_stop_poll() {
    let f = Fixture::new();
    let launch = f.launch();
    f.running(&launch);
    let turn = |id: &str| {
        let mut args = f.target(&launch);
        args["text"] = json!(id);
        f.bridge
            .execute(&f.admin, &f.request(id, "owned.turn", args))
            .unwrap()
    };
    let first = turn("first");
    let reconcile = |operation: &Value| {
        let mut args = f.target(&launch);
        args["operation_id"] = operation["operation_id"].clone();
        args["wait_ms"] = json!(1000);
        f.bridge
            .execute(
                &f.admin,
                &f.request("reconcile", "owned.operation.reconcile", args),
            )
            .unwrap()
    };
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let observed = reconcile(&first);
        if observed["observation"]["outcome"] == "completed" {
            break;
        }
        assert!(Instant::now() < until, "{observed}");
    }
    let second = turn("second");
    let observed = reconcile(&second);
    assert_eq!(observed["observation"]["outcome"], "in_progress");
    let interrupt = |id: &str, expected: &str| {
        let mut args = f.target(&launch);
        args["provider_turn_id"] = json!(expected);
        args["wait_ms"] = json!(1000);
        f.bridge
            .execute(&f.admin, &f.request(id, "owned.interrupt", args))
    };
    assert!(interrupt("old", "turn-1").is_err());
    assert_eq!(
        interrupt("current", "turn-2").unwrap()["observation"]["interruptedTurn"],
        "turn-2"
    );
    let (maintainer, _) = f.actor(&["manage"]);
    let mut args = f.target(&launch);
    args["wait_ms"] = json!(0);
    let request = f.request("stop", "owned.stop", args);
    let first = f.bridge.execute(&maintainer, &request).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let status = f.bridge.execute(&maintainer, &request).unwrap();
        assert_eq!(status["operation_id"], first["operation_id"]);
        if status["state"] == "exited" {
            break;
        }
        assert!(Instant::now() < until, "{status}");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        f.bridge
            .execute(
                &maintainer,
                &f.request("sensitive", "owned.get", f.target(&launch))
            )
            .unwrap_err()
            .code,
        "Denied"
    );
}

#[cfg(windows)]
#[test]
fn approved_workspace_write_reaches_host_without_profile_downgrade() {
    let f = Fixture::new();
    let registry = OwnedCodexRegistry::open(f.dir.path(), f.engine.clone()).unwrap();
    let approval = registry
        .approve_workspace(
            &f.admin,
            &f.binding.space_id,
            &f.binding.endpoint_id,
            f.dir.path(),
            "0.163.0",
            "controlled fake provider",
        )
        .unwrap();
    let script = f.dir.path().join("fake.py");
    let python = std::process::Command::new("python")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()
        .unwrap();
    f.plans
        .install(
            &f.admin,
            "write",
            None,
            TrustedLaunchPlan {
                binding: f.binding.clone(),
                worker_slot: "slot".into(),
                executable: PathBuf::from(String::from_utf8(python.stdout).unwrap().trim()),
                args: vec![script.to_str().unwrap().into(), "workspace".into()],
                workspace: f.dir.path().into(),
                model: "gpt-6-luna".into(),
                provider_version: "0.163.0".into(),
                capability_evidence: "controlled fake provider".into(),
                timeout: Duration::from_secs(3),
                profile: ExecutionProfile::WorkspaceWrite(approval),
            },
        )
        .unwrap();
    let launched=f.bridge.execute(&f.admin,&f.request("write-launch","owned.launch",json!({"endpoint_id":f.binding.endpoint_id,"plan_id":"write","expected_plan_revision":1}))).unwrap();
    f.running(&launched);
    let status = f
        .bridge
        .execute(
            &f.admin,
            &f.request("write-get", "owned.get", f.target(&launched)),
        )
        .unwrap();
    assert!(status["profile"].get("WorkspaceWrite").is_some());
}

async fn http(
    addr: std::net::SocketAddr,
    host: String,
    actor: String,
    request: Option<ToolRequest>,
) -> (u16, Value) {
    tokio::task::spawn_blocking(move||{use std::io::{Read,Write};let mut stream=std::net::TcpStream::connect(addr).unwrap();stream.set_read_timeout(Some(Duration::from_secs(4))).unwrap();let body=request.as_ref().map(|r|serde_json::to_string(r).unwrap()).unwrap_or_default();let path=if request.is_some(){"/v2/tools"}else{"/v2/health"};let method=if request.is_some(){"POST"}else{"GET"};write!(stream,"{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {actor}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();let mut response=String::new();stream.read_to_string(&mut response).unwrap();let (headers,body)=response.split_once("\r\n\r\n").unwrap();let status=headers.split_whitespace().nth(1).unwrap().parse().unwrap();(status,serde_json::from_str(body).unwrap())}).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_provider_wait_does_not_hold_global_gate_and_shutdown_tracks_worker() {
    let f = Fixture::new();
    f.gate_reads();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let host = addr.to_string();
    let (router, _) =
        linkstart::runtime_http::router_with_bridge(f.dir.path(), &host, f.bridge.clone()).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (status, launch) = http(
        addr,
        host.clone(),
        f.admin.clone(),
        Some(f.launch_request()),
    )
    .await;
    assert_eq!(status, 200);
    f.running(&launch);
    let mut args = f.target(&launch);
    args["wait_ms"] = json!(1000);
    let read = tokio::spawn(http(
        addr,
        host.clone(),
        f.admin.clone(),
        Some(f.request("read", "owned.read", args)),
    ));
    f.await_file("read-entered");
    let other = exec(
        &f.engine,
        &std::fs::read_to_string(owner_token_path(f.dir.path())).unwrap(),
        None,
        "spaces.create",
        json!({"name":"other"}),
    );
    let other_request = ToolRequest {
        request_id: "forum".into(),
        tool: "topics.list".into(),
        space_id: Some(other["space"]["id"].as_str().unwrap().into()),
        args: json!({}),
    };
    for request in [None, Some(other_request)] {
        let actor = if request.is_none() {
            f.admin.clone()
        } else {
            other["administrator"]["credential"]
                .as_str()
                .unwrap()
                .into()
        };
        let (status, _) = tokio::time::timeout(
            Duration::from_millis(750),
            http(addr, host.clone(), actor, request),
        )
        .await
        .expect("provider blocked unrelated HTTP");
        assert_eq!(status, 200);
    }
    let owner = std::fs::read_to_string(owner_token_path(f.dir.path())).unwrap();
    let (_, prepared) = http(
        addr,
        host.clone(),
        owner.clone(),
        Some(ToolRequest {
            request_id: "prepare".into(),
            tool: "runtime.prepare_shutdown".into(),
            space_id: None,
            args: json!({}),
        }),
    )
    .await;
    assert_eq!(prepared["impact"]["owned_sessions"]["active_workers"], 1);
    let (_, shutdown) = http(
        addr,
        host.clone(),
        owner,
        Some(ToolRequest {
            request_id: "shutdown".into(),
            tool: "runtime.confirm_shutdown".into(),
            space_id: None,
            args: json!({"ticket":prepared["ticket"]}),
        }),
    )
    .await;
    assert_eq!(shutdown["status"], "pending");
    assert_eq!(shutdown["owned_sessions"]["active_workers"], 1);
    std::fs::write(f.dir.path().join("read-release"), "").unwrap();
    let _ = read.await.unwrap();
    server.abort();
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.supervisor.request_shutdown();
        let until = Instant::now() + Duration::from_secs(5);
        while self.supervisor.active_count() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn strict_launch_and_durable_identity_scoped_receipts() {
    let f = Fixture::new();
    for key in [
        "credential",
        "identity_id",
        "executable",
        "argv",
        "model",
        "workspace",
        "profile",
        "approved_by",
    ] {
        let mut r = f.launch_request();
        r.args[key] = json!("injected");
        assert_eq!(f.bridge.execute(&f.admin, &r).unwrap_err().code, "Invalid");
    }
    assert_eq!(f.supervisor.active_count(), 0);
    let a = f.launch();
    let b = f.launch();
    assert_eq!(a, b);
    f.running(&a);
    assert_eq!(f.supervisor.active_count(), 1);
    let mut r = f.launch_request();
    r.args["expected_plan_revision"] = json!(2);
    assert_eq!(
        f.bridge.execute(&f.admin, &r).unwrap_err().code,
        "RequestConflict"
    );
    let mut args = f.target(&a);
    args["text"] = json!("first");
    let r = f.request("turn", "owned.turn", args);
    let turn = f.bridge.execute(&f.admin, &r).unwrap();
    assert_eq!(turn, f.bridge.execute(&f.admin, &r).unwrap());
    let mut lookup = f.target(&a);
    lookup["operation_id"] = turn["operation_id"].clone();
    let operation = f
        .bridge
        .execute(
            &f.admin,
            &f.request("lookup", "owned.operation.get", lookup),
        )
        .unwrap();
    assert_eq!(operation["operation_id"], turn["operation_id"]);
    let mut stale = f.target(&a);
    stale["expected_generation"] = json!(2);
    assert_eq!(
        f.bridge
            .execute(&f.admin, &f.request("stale", "owned.get", stale))
            .unwrap_err()
            .code,
        "StaleGeneration"
    );
    assert!(!std::fs::read(f.dir.path().join("v2-owned-control.sqlite"))
        .unwrap()
        .windows(f.worker.len())
        .any(|s| s == f.worker.as_bytes()));
}

#[test]
fn launch_revocation_and_shutdown_reservation_are_admission_boundaries() {
    let f = Fixture::new();
    f.plans
        .revoke(
            &f.admin,
            &f.binding.space_id,
            &f.binding.endpoint_id,
            "plan",
            1,
        )
        .unwrap();
    assert!(f.bridge.execute(&f.admin, &f.launch_request()).is_err());
    assert_eq!(f.supervisor.active_count(), 0);
    let f = Fixture::new();
    let epoch = f.bridge.inventory().epoch;
    f.bridge.begin_shutdown(epoch).unwrap();
    assert_eq!(
        f.bridge
            .execute(&f.admin, &f.launch_request())
            .unwrap_err()
            .code,
        "Draining"
    );
    assert_eq!(f.bridge.inventory().active_workers, 0);
    let approved = f
        .plans
        .approve(
            &f.admin,
            &f.binding.space_id,
            &f.binding.endpoint_id,
            "plan",
            1,
        )
        .unwrap();
    assert!(matches!(
        f.supervisor.start_as(
            &f.admin,
            approved,
            "after-shutdown".into(),
            1,
            ThreadAdmission::New
        ),
        Err(SupervisorError::Closed)
    ));
}

#[test]
fn interrupt_default_wait_settles_original_operation_without_a_second_send() {
    let f = Fixture::new();
    f.gate_interrupt();
    let launch = f.launch();
    f.running(&launch);
    f.active_turn(&launch);
    let mut args = f.target(&launch);
    args["provider_turn_id"] = json!("turn-2");
    let request = f.request("default-interrupt", "owned.interrupt", args);
    let receipt = f.bridge.execute(&f.admin, &request).unwrap();
    assert_eq!(receipt["state"], "pending");
    f.await_file("interrupt-effect");
    std::fs::write(f.dir.path().join("interrupt-release"), "").unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let replay = f.bridge.execute(&f.admin, &request).unwrap();
        assert_eq!(replay["operation_id"], receipt["operation_id"]);
        let mut args = f.target(&launch);
        args["operation_id"] = receipt["operation_id"].clone();
        let observed = f
            .bridge
            .execute(
                &f.admin,
                &f.request("interrupt-get", "owned.operation.get", args),
            )
            .unwrap();
        if observed["state"] == "confirmed" {
            assert_eq!(observed["snapshot"]["result"]["interruptedTurn"], "turn-2");
            break;
        }
        assert!(
            Instant::now() < until,
            "original interrupt receipt never settled: {observed}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("interrupt-count"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn revoked_interrupt_delivery_never_claims_the_applied_effect_was_rejected() {
    let f = Fixture::new();
    f.gate_interrupt();
    let launch = f.launch();
    f.running(&launch);
    f.active_turn(&launch);
    let (actor, id) = f.actor(&["control", "observe"]);
    let mut args = f.target(&launch);
    args["provider_turn_id"] = json!("turn-2");
    args["wait_ms"] = json!(1000);
    let request = f.request("revoked-interrupt", "owned.interrupt", args);
    let bridge = f.bridge.clone();
    let credential = actor.clone();
    let threaded = request.clone();
    let task = std::thread::spawn(move || bridge.execute(&credential, &threaded));
    f.await_file("interrupt-effect");
    f.grants(&id, &[]);
    std::fs::write(f.dir.path().join("interrupt-release"), "").unwrap();
    let response = task.join().unwrap();
    assert!(!format!("{response:?}").contains("controlled-sensitive-reply"));
    assert_eq!(
        f.bridge.execute(&actor, &request).unwrap_err().code,
        "Denied"
    );
    f.grants(&id, &["control", "observe"]);
    let replay = f.bridge.execute(&actor, &request).unwrap();
    assert_ne!(
        replay["state"], "rejected",
        "delivery revocation cannot prove absence of an already applied interrupt"
    );
    let mut args = f.target(&launch);
    args["operation_id"] = replay["operation_id"].clone();
    let observed = f
        .bridge
        .execute(
            &actor,
            &f.request("after-restore", "owned.operation.get", args),
        )
        .unwrap();
    assert_eq!(observed["state"], "confirmed");
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("interrupt-count"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn revoked_stop_delivery_preserves_unknown_until_current_authority_observes_exit() {
    let f = Fixture::new();
    f.gate_reads();
    let launch = f.launch();
    f.running(&launch);
    let mut args = f.target(&launch);
    args["wait_ms"] = json!(1000);
    let read_request = f.request("blocking-read", "owned.read", args.clone());
    let bridge = f.bridge.clone();
    let admin = f.admin.clone();
    let reader = std::thread::spawn(move || bridge.execute(&admin, &read_request));
    f.await_file("read-entered");
    let (actor, id) = f.actor(&["manage", "observe"]);
    let request = f.request("revoked-stop", "owned.stop", args);
    let bridge = f.bridge.clone();
    let credential = actor.clone();
    let threaded = request.clone();
    let task = std::thread::spawn(move || bridge.execute(&credential, &threaded));
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let status = f
            .bridge
            .execute(
                &f.admin,
                &f.request("stopping-witness", "owned.get", f.target(&launch)),
            )
            .unwrap();
        if status["phase"] == "Stopping" {
            break;
        }
        assert!(Instant::now() < until, "{status}");
        std::thread::sleep(Duration::from_millis(5));
    }
    f.grants(&id, &[]);
    std::fs::write(f.dir.path().join("read-release"), "").unwrap();
    let _ = reader.join().unwrap();
    let response = task.join().unwrap();
    assert!(!format!("{response:?}").contains("provider_thread_id"));
    assert_eq!(
        f.bridge.execute(&actor, &request).unwrap_err().code,
        "Denied"
    );
    let db = rusqlite::Connection::open(f.dir.path().join("v2-owned-control.sqlite")).unwrap();
    let body: String = db
        .query_row(
            "SELECT body FROM requests WHERE actor=?1 AND request='revoked-stop'",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    let receipt: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        receipt["state"], "unknown",
        "stop was admitted before delivery permission was revoked"
    );
    f.grants(&id, &["manage", "observe"]);
    let replay = f.bridge.execute(&actor, &request).unwrap();
    assert_eq!(replay["operation_id"], receipt["operation_id"]);
    let mut args = f.target(&launch);
    args["operation_id"] = receipt["operation_id"].clone();
    let observed = f
        .bridge
        .execute(
            &actor,
            &f.request("exit-after-restore", "owned.operation.get", args),
        )
        .unwrap();
    assert_eq!(observed["state"], "exited");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn posix_shutdown_terminal_wait_releases_global_http_gate() {
    let f = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let host = addr.to_string();
    let (router, _) =
        linkstart::runtime_http::router_with_bridge(f.dir.path(), &host, f.bridge.clone()).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let python = std::process::Command::new("python3")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()
        .unwrap();
    let script="import signal,time,pathlib\nsignal.signal(signal.SIGINT,lambda *_:pathlib.Path('shutdown-entered').touch())\npathlib.Path('terminal-ready').touch()\nwhile not pathlib.Path('terminal-release').exists():time.sleep(0.01)";
    let (status,_)=http(addr,host.clone(),f.admin.clone(),Some(f.request("terminal","terminal.create",json!({"command":String::from_utf8(python.stdout).unwrap().trim(),"args":["-u","-c",script],"cwd":f.dir.path()})))).await;
    assert_eq!(status, 200);
    f.await_file("terminal-ready");
    let owner = std::fs::read_to_string(owner_token_path(f.dir.path())).unwrap();
    let other = exec(
        &f.engine,
        &owner,
        None,
        "spaces.create",
        json!({"name":"shutdown-other"}),
    );
    let forum_request = ToolRequest {
        request_id: "shutdown-forum".into(),
        tool: "topics.list".into(),
        space_id: Some(other["space"]["id"].as_str().unwrap().into()),
        args: json!({}),
    };
    let (_, prepared) = http(
        addr,
        host.clone(),
        owner.clone(),
        Some(ToolRequest {
            request_id: "prepare".into(),
            tool: "runtime.prepare_shutdown".into(),
            space_id: None,
            args: json!({}),
        }),
    )
    .await;
    let shutdown = tokio::spawn(http(
        addr,
        host.clone(),
        owner,
        Some(ToolRequest {
            request_id: "confirm".into(),
            tool: "runtime.confirm_shutdown".into(),
            space_id: None,
            args: json!({"ticket":prepared["ticket"]}),
        }),
    ));
    f.await_file("shutdown-entered");
    let health = tokio::time::timeout(
        Duration::from_millis(200),
        http(addr, host.clone(), f.admin.clone(), None),
    )
    .await;
    let forum = tokio::time::timeout(
        Duration::from_millis(200),
        http(
            addr,
            host.clone(),
            other["administrator"]["credential"]
                .as_str()
                .unwrap()
                .into(),
            Some(forum_request),
        ),
    )
    .await;
    std::fs::write(f.dir.path().join("terminal-release"), "").unwrap();
    let _ = shutdown.await.unwrap();
    server.abort();
    assert!(
        health.is_ok(),
        "health waited for native shutdown cleanup under the global HTTP gate"
    );
    assert_eq!(health.unwrap().0, 200);
    assert!(
        forum.is_ok(),
        "another-space forum waited for native cleanup under the global HTTP gate"
    );
    assert_eq!(forum.unwrap().0, 200);
}
