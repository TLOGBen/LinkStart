use rand::{rngs::OsRng, RngCore};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const PROTOCOL: &str = "v1";
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const MAX_BODY: usize = 64 * 1024;

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub(crate) fn token() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|v| format!("{v:02x}")).collect()
}
pub(crate) fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
pub(crate) fn json_line(v: Value) {
    println!("{}", serde_json::to_string(&v).unwrap());
}

pub(crate) fn output(as_json: bool, v: Value, human: &str) {
    if as_json {
        json_line(v)
    } else {
        println!("{human}")
    }
}

pub(crate) fn canonical(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut b = BTreeMap::new();
            for (k, v) in m {
                b.insert(k, canonical(v));
            }
            Value::Object(b.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(canonical).collect()),
        x => x,
    }
}
