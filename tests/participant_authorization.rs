use linkstart::runtime_core::{owner_token_path, Engine, ToolRequest};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::Duration;

fn call(e: &Engine, c: &str, id: &str, tool: &str, sid: Option<&str>, args: Value) -> Value {
    e.execute(
        c,
        &ToolRequest {
            request_id: id.into(),
            tool: tool.into(),
            space_id: sid.map(str::to_owned),
            args,
        },
    )
    .unwrap()
}

#[test]
fn admission_fences_other_engine_revocation_and_rechecks_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let other = Engine::open(dir.path()).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
    let sp = call(
        &e,
        &owner,
        "space",
        "spaces.create",
        None,
        json!({"name":"test","manager_name":"manager"}),
    );
    let sid = sp["space"]["id"].as_str().unwrap().to_owned();
    let admin = sp["administrator"]["credential"]
        .as_str()
        .unwrap()
        .to_owned();
    let invite = call(
        &e,
        &admin,
        "invite",
        "members.invite",
        Some(&sid),
        json!({"roles":["implementer"]}),
    );
    let worker = call(
        &e,
        invite["invite_token"].as_str().unwrap(),
        "join",
        "members.register",
        Some(&sid),
        json!({"name":"worker"}),
    );
    let credential = worker["credential"].as_str().unwrap().to_owned();
    let ep = call(
        &e,
        &admin,
        "ep",
        "endpoints.register",
        Some(&sid),
        json!({"name":"owned","kind":"terminal"}),
    );
    let eid = ep["id"].as_str().unwrap().to_owned();
    call(
        &e,
        &admin,
        "grant",
        "endpoints.grant",
        Some(&sid),
        json!({"endpoint_id":eid,"member_id":worker["id"],"operations":["send"]}),
    );
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let admission = e.clone();
    let c = credential.clone();
    let s = sid.clone();
    let endpoint = eid.clone();
    let reader = std::thread::spawn(move || {
        admission.with_endpoint_authorization(&c, &s, &endpoint, "send", |snapshot| {
            assert_eq!(snapshot["identity_id"], snapshot["identity"]["id"]);
            assert!(snapshot["identity"].get("credential_hash").is_none());
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let s = sid.clone();
    let writer = std::thread::spawn(move || {
        let result = call(
            &other,
            &admin,
            "revoke",
            "members.revoke_credential",
            Some(&s),
            json!({"member_id":worker["id"],"expected_epoch":0}),
        );
        done_tx.send(result).unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(150)).is_err());
    release_tx.send(()).unwrap();
    reader.join().unwrap().unwrap();
    done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    writer.join().unwrap();
    assert_eq!(
        e.with_endpoint_authorization::<()>(&credential, &sid, &eid, "send", |_| panic!(
            "revoked callback ran"
        ))
        .unwrap_err()
        .code,
        "Unauthenticated"
    );
}
