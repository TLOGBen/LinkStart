use linkstart::runtime_core::{owner_token_path, Engine, ToolRequest};
use serde_json::{json, Value};

fn req(id: &str, tool: &str, sid: Option<&str>, args: Value) -> ToolRequest {
    ToolRequest { request_id: id.into(), tool: tool.into(), space_id: sid.map(str::to_owned), args }
}
fn create(e: &Engine, owner: &str, id: &str) -> Value {
    e.execute(owner, &req(id, "spaces.create", None, json!({"name":id,"manager_name":"manager"}))).unwrap()
}
fn register(e: &Engine, admin: &str, sid: &str) -> (Value, String) {
    let invite = e.execute(admin, &req("invite", "members.invite", Some(sid), json!({"roles":["implementer"]}))).unwrap();
    let token = invite["invite_token"].as_str().unwrap().to_owned();
    (e.execute(&token, &req("join", "members.register", Some(sid), json!({"name":"worker"}))).unwrap(), token)
}

#[test]
fn rotation_fences_successful_replay_and_preserves_identity_and_topic() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let sp = create(&e, &owner, "space");
    let sid = sp["space"]["id"].as_str().unwrap();
    let admin = sp["administrator"]["credential"].as_str().unwrap();
    let (worker, _) = register(&e, admin, sid);
    let old = worker["credential"].as_str().unwrap();
    let write = req("write", "topics.create", Some(sid), json!({"title":"retain","body":"body","kind":"task"}));
    let topic = e.execute(old, &write).unwrap();
    e.execute(old, &req("claim", "task.act", Some(sid), json!({"topic_id":topic["id"],"action":"claim","expected_revision":0,"comment":"working"}))).unwrap();
    let rotated = e.execute(admin, &req("rotate", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":0}))).unwrap();
    assert_eq!(e.execute(old, &write).unwrap_err().code, "Unauthenticated");
    let fresh = rotated["credential"].as_str().unwrap();
    assert_ne!(old, fresh);
    assert_eq!(rotated["credential_epoch"], 1);
    assert_eq!(e.execute(fresh, &req("who", "identity.current", Some(sid), json!({}))).unwrap()["id"], worker["id"]);
    let read = e.execute(fresh, &req("read", "topics.read", Some(sid), json!({"topic_id":topic["id"]}))).unwrap();
    assert_eq!(read["topic"]["id"], topic["id"]);
    assert_eq!(read["topic"]["assignee"], worker["id"]);
    assert!(read.to_string().contains("working"));
}

#[test]
fn legacy_members_and_receipt_markers_default_to_epoch_zero_without_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let sp = create(&e, &owner, "legacy");
    let sid = sp["space"]["id"].as_str().unwrap();
    let admin = sp["administrator"]["credential"].as_str().unwrap();
    drop(e);
    let db = rusqlite::Connection::open(dir.path().join("v2.sqlite3")).unwrap();
    let encoded: String = db.query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0)).unwrap();
    let mut state: Value = serde_json::from_str(&encoded).unwrap();
    for member in state["members"].as_array_mut().unwrap() {
        for key in ["credential_epoch", "credential_expires_at", "credential_revoked"] {
            member.as_object_mut().unwrap().remove(key);
        }
    }
    for key in ["administrator", "manager"] {
        state["receipts"][0]["result"][key]["credential"].as_object_mut().unwrap().remove("epoch");
    }
    db.execute("UPDATE core_state SET json=?1 WHERE id=1", [state.to_string()]).unwrap();
    drop(db);
    let e = Engine::open(dir.path()).unwrap();
    let who = e.execute(admin, &req("who", "identity.current", Some(sid), json!({}))).unwrap();
    assert_eq!(who["credential_epoch"], 0);
    assert!(who["credential_expires_at"].is_null());
    assert_eq!(e.execute(&owner, &req("legacy", "spaces.create", None, json!({"name":"legacy","manager_name":"manager"}))).unwrap()["administrator"]["credential"], admin);
    let rotated = e.execute(admin, &req("rotate", "members.rotate_credential", Some(sid), json!({"member_id":sp["manager"]["id"],"expected_epoch":0}))).unwrap();
    assert_eq!(rotated["credential_epoch"], 1);
}

#[test]
fn receipts_keep_original_generation_across_rotation_revoke_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let sp = create(&e, &owner, "space");
    let sid = sp["space"]["id"].as_str().unwrap();
    let admin = sp["administrator"]["credential"].as_str().unwrap();
    let (worker, invite) = register(&e, admin, sid);
    let rotation = req("rotate1", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":0}));
    let first = e.execute(admin, &rotation).unwrap();
    let second = e.execute(admin, &req("rotate2", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":1}))).unwrap();
    assert_eq!(e.execute(admin, &rotation).unwrap(), first);
    assert_eq!(e.execute(&invite, &req("join", "members.register", Some(sid), json!({"name":"worker"}))).unwrap(), worker);
    let who = req("who", "identity.current", Some(sid), json!({}));
    assert_eq!(e.execute(first["credential"].as_str().unwrap(), &who).unwrap_err().code, "Unauthenticated");
    assert_eq!(e.execute(worker["credential"].as_str().unwrap(), &who).unwrap_err().code, "Unauthenticated");
    assert_eq!(e.execute(admin, &req("stale", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":1}))).unwrap_err().code, "Conflict");
    let revoked = e.execute(admin, &req("revoke", "members.revoke_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":2}))).unwrap();
    assert_eq!(revoked["credential_epoch"], 3);
    assert!(revoked.get("credential").is_none());
    drop(e);
    let e = Engine::open(dir.path()).unwrap();
    assert_eq!(e.execute(second["credential"].as_str().unwrap(), &who).unwrap_err().code, "Unauthenticated");
    let restored = e.execute(admin, &req("restore", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":3}))).unwrap();
    assert_eq!(restored["credential_epoch"], 4);
    assert_eq!(e.execute(restored["credential"].as_str().unwrap(), &who).unwrap()["id"], worker["id"]);
    assert_eq!(e.execute(admin, &rotation).unwrap(), first);
    let db = rusqlite::Connection::open(dir.path().join("v2.sqlite3")).unwrap();
    let stored: String = db.query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0)).unwrap();
    for result in [&worker, &first, &second, &restored] {
        assert!(!stored.contains(result["credential"].as_str().unwrap()));
    }
    assert!(!stored.contains(&owner));
    let members = e.execute(admin, &req("list", "members.list", Some(sid), json!({}))).unwrap();
    assert!(!members.to_string().contains("credential_hash"));
    assert!(!members.to_string().contains("$credential"));
}

#[test]
fn bounded_expiry_uses_engine_clock_and_heartbeat_cannot_extend_it() {
    use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
    let dir = tempfile::tempdir().unwrap();
    let time = Arc::new(AtomicU64::new(1000));
    let clock_time = time.clone();
    let e = Engine::open_with_clock(dir.path(), Arc::new(move || clock_time.load(Ordering::SeqCst))).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let sp = create(&e, &owner, "space");
    let sid = sp["space"]["id"].as_str().unwrap();
    let admin = sp["administrator"]["credential"].as_str().unwrap();
    let (worker, _) = register(&e, admin, sid);
    for (i, ttl) in [json!(0),json!(-1),json!(1.5),json!("1"),json!(null),json!(2592001)].into_iter().enumerate() {
        assert_eq!(e.execute(admin, &req(&format!("invalid{i}"), "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":0,"ttl_seconds":ttl}))).unwrap_err().code, "Invalid");
    }
    let rotated = e.execute(admin, &req("rotate", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":0,"ttl_seconds":1}))).unwrap();
    assert_eq!(rotated["credential_expires_at"], 2000);
    let token = rotated["credential"].as_str().unwrap();
    let heartbeat = req("hb", "members.heartbeat", Some(sid), json!({}));
    let attachment = e.execute(token, &heartbeat).unwrap();
    time.store(1999, Ordering::SeqCst);
    e.execute(token, &req("renew", "members.heartbeat", Some(sid), json!({"attachment_id":attachment["attachment_id"]}))).unwrap();
    time.store(2000, Ordering::SeqCst);
    for r in [heartbeat, req("who", "identity.current", Some(sid), json!({})), req("renew", "members.heartbeat", Some(sid), json!({"attachment_id":attachment["attachment_id"]}))] {
        assert_eq!(e.execute(token, &r).unwrap_err().code, "Unauthenticated");
    }
    drop(e);
    let reopen_time = time.clone();
    let e = Engine::open_with_clock(dir.path(), Arc::new(move || reopen_time.load(Ordering::SeqCst))).unwrap();
    assert_eq!(e.execute(token, &req("after-reopen", "identity.current", Some(sid), json!({}))).unwrap_err().code, "Unauthenticated");
    let restored = e.execute(admin, &req("restore", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":1}))).unwrap();
    assert!(restored["credential_expires_at"].is_null());
    assert_eq!(e.execute(restored["credential"].as_str().unwrap(), &req("who", "identity.current", Some(sid), json!({}))).unwrap()["id"], worker["id"]);
    let bounded = e.execute(admin, &req("max-ttl", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":2,"ttl_seconds":2592000}))).unwrap();
    assert_eq!(bounded["credential_expires_at"], 2_592_002_000u64);
}

#[test]
fn management_respects_human_identity_space_and_final_active_admin() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let sp = create(&e, &owner, "space");
    let sid = sp["space"]["id"].as_str().unwrap();
    let admin = sp["administrator"]["credential"].as_str().unwrap();
    let manager = sp["manager"]["credential"].as_str().unwrap();
    let (worker, _) = register(&e, admin, sid);
    for tool in ["members.rotate_credential", "members.revoke_credential"] {
        let human = req(tool, tool, Some(sid), json!({"member_id":sp["administrator"]["id"],"expected_epoch":0}));
        assert_eq!(e.execute(manager, &human).unwrap_err().code, "Denied");
        assert_eq!(e.execute(worker["credential"].as_str().unwrap(), &human).unwrap_err().code, "Denied");
    }
    let other = create(&e, &owner, "other");
    assert_eq!(e.execute(admin, &req("cross", "members.rotate_credential", Some(other["space"]["id"].as_str().unwrap()), json!({"member_id":worker["id"],"expected_epoch":0}))).unwrap_err().code, "Denied");
    assert_eq!(e.execute(admin, &req("cross-target", "members.rotate_credential", Some(sid), json!({"member_id":other["administrator"]["id"],"expected_epoch":0}))).unwrap_err().code, "NotFound");
    e.execute(manager, &req("rotate-agent", "members.rotate_credential", Some(sid), json!({"member_id":worker["id"],"expected_epoch":0}))).unwrap();
    e.execute(admin, &req("revoke-manager", "members.revoke_credential", Some(sid), json!({"member_id":sp["manager"]["id"],"expected_epoch":0}))).unwrap();
    for (tool, args) in [("members.revoke_credential", json!({"member_id":sp["administrator"]["id"],"expected_epoch":0})), ("members.rotate_credential", json!({"member_id":sp["administrator"]["id"],"expected_epoch":0,"ttl_seconds":1}))] {
        assert_eq!(e.execute(admin, &req(tool, tool, Some(sid), args)).unwrap_err().code, "Conflict");
    }
    e.execute(&owner, &req("owner-revoke", "members.revoke_credential", Some(sid), json!({"member_id":sp["administrator"]["id"],"expected_epoch":0}))).unwrap();
    let restored = e.execute(&owner, &req("owner-restore", "members.rotate_credential", Some(sid), json!({"member_id":sp["administrator"]["id"],"expected_epoch":1}))).unwrap();
    e.execute(restored["credential"].as_str().unwrap(), &req("list", "members.list", Some(sid), json!({}))).unwrap();
}

#[test]
fn revoked_or_expired_administrator_cannot_enable_final_active_admin_demotion() {
    use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
    for expire in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let time = Arc::new(AtomicU64::new(1000));
        let read_time = time.clone();
        let e = Engine::open_with_clock(dir.path(), Arc::new(move || read_time.load(Ordering::SeqCst))).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let sp = create(&e, &owner, "space");
        let sid = sp["space"]["id"].as_str().unwrap();
        let admin = sp["administrator"]["credential"].as_str().unwrap();
        let manager_id = &sp["manager"]["id"];
        let inactive = if expire {
            e.execute(admin, &req("expire-manager", "members.rotate_credential", Some(sid), json!({"member_id":manager_id,"expected_epoch":0,"ttl_seconds":1}))).unwrap()
        } else {
            e.execute(admin, &req("revoke-manager", "members.revoke_credential", Some(sid), json!({"member_id":manager_id,"expected_epoch":0}))).unwrap()
        };
        time.store(2000, Ordering::SeqCst);
        let demote = req("demote", "members.set_roles", Some(sid), json!({"member_id":sp["administrator"]["id"],"roles":["observer"]}));
        assert_eq!(e.execute(admin, &demote).unwrap_err().code, "Conflict", "expire={expire}");
        let restored = e.execute(admin, &req("handoff", "members.rotate_credential", Some(sid), json!({"member_id":manager_id,"expected_epoch":inactive["credential_epoch"]}))).unwrap();
        e.execute(admin, &demote).unwrap();
        e.execute(restored["credential"].as_str().unwrap(), &req("invite-next", "members.invite", Some(sid), json!({"roles":["manager"]}))).unwrap();
    }
}

#[test]
fn credential_audit_is_durable_secret_free_atomic_and_idempotent() {
    fn ledger(dir: &std::path::Path) -> Value {
        let db = rusqlite::Connection::open(dir.join("v2.sqlite3")).unwrap();
        let stored: String = db.query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0)).unwrap();
        serde_json::from_str::<Value>(&stored).unwrap()["credential_audit_events"].clone()
    }
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let sp = create(&e, &owner, "audit-space");
    let sid = sp["space"]["id"].as_str().unwrap();
    let admin = sp["administrator"]["credential"].as_str().unwrap();
    let rotation = req("rotate", "members.rotate_credential", Some(sid), json!({"member_id":sp["manager"]["id"],"expected_epoch":0}));
    let rotated = e.execute(admin, &rotation).unwrap();
    let event = &rotated["audit_event"];
    assert_eq!(event["actor"], sp["administrator"]["id"]);
    assert_eq!(event["target_member_id"], sp["manager"]["id"]);
    assert_eq!(event["space_id"], sid);
    assert_eq!(event["action"], "rotate_credential");
    assert_eq!(event["epoch_before"], 0);
    assert_eq!(event["epoch_after"], 1);
    assert_eq!(event["request_id"], "rotate");
    assert_eq!(event["owner_recovery"], false);
    assert!(event["created_at"].is_u64());
    assert_eq!(ledger(dir.path()), json!([event]));
    assert_eq!(e.execute(admin, &rotation).unwrap(), rotated);
    let failed = req("stale", "members.revoke_credential", Some(sid), json!({"member_id":sp["manager"]["id"],"expected_epoch":0}));
    assert_eq!(e.execute(admin, &failed).unwrap_err().code, "Conflict");
    assert_eq!(ledger(dir.path()).as_array().unwrap().len(), 1);
    drop(e);
    let e = Engine::open(dir.path()).unwrap();
    assert_eq!(e.execute(admin, &rotation).unwrap(), rotated);
    assert_eq!(ledger(dir.path()).as_array().unwrap().len(), 1);
    let revoke = req("recover-revoke", "members.revoke_credential", Some(sid), json!({"member_id":sp["administrator"]["id"],"expected_epoch":0}));
    let revoked = e.execute(&owner, &revoke).unwrap();
    let restore = req("recover-restore", "members.rotate_credential", Some(sid), json!({"member_id":sp["administrator"]["id"],"expected_epoch":1}));
    let restored = e.execute(&owner, &restore).unwrap();
    for result in [&revoked, &restored] {
        assert_eq!(result["audit_event"]["actor"], "owner");
        assert_eq!(result["audit_event"]["owner_recovery"], true);
    }
    assert_eq!(revoked["audit_event"]["action"], "revoke_credential");
    assert_eq!(e.execute(&owner, &restore).unwrap(), restored);
    let audit = ledger(dir.path());
    assert_eq!(audit.as_array().unwrap().len(), 3);
    let serialized = audit.to_string();
    for secret in [&owner, admin, rotated["credential"].as_str().unwrap(), restored["credential"].as_str().unwrap()] {
        assert!(!serialized.contains(secret));
    }
    for forbidden in ["credential_hash", "$credential", "author", "topic_id"] {
        assert!(!serialized.contains(forbidden));
    }
    let activity = e.execute(restored["credential"].as_str().unwrap(), &req("activity", "activity.read", Some(sid), json!({}))).unwrap();
    assert!(activity["items"].as_array().unwrap().is_empty());
}
