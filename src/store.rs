use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
};

use crate::util::{hash, now, PROTOCOL, VERSION};

#[derive(Clone, Serialize)]
pub(crate) struct Notification {
    pub(crate) cursor: i64,
    pub(crate) app_id: String,
    pub(crate) kind: String,
    pub(crate) data: Value,
}
#[derive(Clone)]
pub(crate) struct Store {
    db: Arc<Mutex<Connection>>,
    state_dir: PathBuf,
}
impl Store {
    pub(crate) fn open(dir: &FsPath) -> Result<Self> {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join("linkstart.sqlite3");
        let c = Connection::open(&path)?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
 CREATE TABLE IF NOT EXISTS connections(id TEXT PRIMARY KEY, capability_hash TEXT NOT NULL, callsign TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS apps(id TEXT PRIMARY KEY, connection_id TEXT NOT NULL REFERENCES connections(id), capability_hash TEXT NOT NULL, origin TEXT NOT NULL, app_id TEXT NOT NULL, display_name TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY, app_id TEXT NOT NULL REFERENCES apps(id), sequence INTEGER NOT NULL, payload TEXT NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL, reason TEXT, receipt_id TEXT NOT NULL, created_at INTEGER NOT NULL, UNIQUE(app_id,id), UNIQUE(app_id,sequence));
 CREATE TABLE IF NOT EXISTS feedback(id TEXT NOT NULL UNIQUE, app_id TEXT NOT NULL REFERENCES apps(id), in_reply_to TEXT, payload TEXT NOT NULL, cursor INTEGER PRIMARY KEY AUTOINCREMENT, created_at INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS launch_grants(id TEXT PRIMARY KEY, capability_hash TEXT NOT NULL, connection_id TEXT NOT NULL REFERENCES connections(id), manifest TEXT NOT NULL, expires_at INTEGER NOT NULL, redeemed_at INTEGER);
 CREATE TABLE IF NOT EXISTS launch_pages(locator TEXT PRIMARY KEY, grant_id TEXT NOT NULL UNIQUE REFERENCES launch_grants(id), html TEXT NOT NULL, created_at INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS notifications(cursor INTEGER PRIMARY KEY AUTOINCREMENT, app_id TEXT NOT NULL REFERENCES apps(id), kind TEXT NOT NULL, data TEXT NOT NULL, created_at INTEGER NOT NULL);")?;
        Ok(Self {
            db: Arc::new(Mutex::new(c)),
            state_dir: dir.to_path_buf(),
        })
    }
    pub(crate) fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let c = self.db.lock().unwrap();
        f(&c)
    }
    pub(crate) fn status(&self) -> Result<Value> {
        self.with(|c|{let con:i64=c.query_row("SELECT count(*) FROM connections WHERE status='online'",[],|r|r.get(0))?;let apps:i64=c.query_row("SELECT count(*) FROM apps WHERE status='connected'",[],|r|r.get(0))?;let rec:i64=c.query_row("SELECT count(*) FROM events WHERE status='received'",[],|r|r.get(0))?;Ok(json!({"runtime":"linkstart","version":VERSION,"protocolMajor":PROTOCOL,"state":"ready","onlineConnections":con,"connectedApps":apps,"receivedEvents":rec,"stateDir":self.state_dir}))})
    }
    pub(crate) fn doctor(&self) -> Result<Value> {
        self.with(|c|{let wal:String=c.query_row("PRAGMA journal_mode",[],|r|r.get(0))?;let sync:i64=c.query_row("PRAGMA synchronous",[],|r|r.get(0))?;Ok(json!({"ok":wal.eq_ignore_ascii_case("wal") && sync>=2,"sqliteJournal":wal,"synchronous":"FULL","stateDir":self.state_dir}))})
    }
    pub(crate) fn connections(&self) -> Result<Value> {
        self.with(|c|{let mut q=c.prepare("SELECT id,callsign,status FROM connections ORDER BY created_at")?;let rows=q.query_map([],|r|Ok(json!({"connectionId":r.get::<_,String>(0)?,"callsign":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;Ok(json!({"connections":rows}))})
    }
    pub(crate) fn apps(&self) -> Result<Value> {
        self.with(|c|{let mut q=c.prepare("SELECT id,connection_id,origin,app_id,display_name,status FROM apps ORDER BY created_at")?;let rows=q.query_map([],|r|Ok(json!({"instanceId":r.get::<_,String>(0)?,"connectionId":r.get::<_,String>(1)?,"origin":r.get::<_,String>(2)?,"appId":r.get::<_,String>(3)?,"displayName":r.get::<_,String>(4)?,"status":r.get::<_,String>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;Ok(json!({"apps":rows}))})
    }
    pub(crate) fn ps(&self) -> Result<Value> {
        Ok(json!({
            "connections": self.connections()?["connections"].clone(),
            "apps": self.apps()?["apps"].clone(),
        }))
    }
    pub(crate) fn auth_connection(&self, id: &str, bearer: &str) -> Result<bool> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT capability_hash FROM connections WHERE id=? AND status='online'",
                [id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .is_some_and(|h| h == hash(bearer)))
        })
    }
    pub(crate) fn auth_app(&self, id: &str, bearer: &str) -> Result<Option<(String, String)>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT capability_hash,origin FROM apps WHERE id=? AND status='connected'",
                [id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .filter(|(h, _)| h == &hash(bearer))
            .map(|(_, o)| (id.to_string(), o)))
        })
    }
    pub(crate) fn next_received(&self, connection_id: &str) -> Result<Option<Value>> {
        self.with(|c| Ok(c.query_row("SELECT e.id,e.app_id,e.sequence,e.payload,e.receipt_id FROM events e JOIN apps a ON a.id=e.app_id WHERE a.connection_id=? AND e.status='received' AND e.id=(SELECT e2.id FROM events e2 WHERE e2.app_id=e.app_id AND e2.status='received' ORDER BY e2.sequence LIMIT 1) ORDER BY e.created_at LIMIT 1", [connection_id], |r| { let payload: String=r.get(3)?; Ok(json!({"eventId":r.get::<_,String>(0)?,"appInstanceId":r.get::<_,String>(1)?,"sequence":r.get::<_,i64>(2)?,"payload":serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),"receiptId":r.get::<_,String>(4)?,"status":"received"})) }).optional()?))
    }
    pub(crate) fn ack_event(&self, connection_id: &str, event_id: &str) -> Result<Value> {
        self.with(|c| { let app: String=c.query_row("SELECT e.app_id FROM events e JOIN apps a ON a.id=e.app_id WHERE a.connection_id=? AND e.id=? AND e.status='received'",params![connection_id,event_id],|r|r.get(0)).optional()?.context("event_not_received")?; let first: Option<String>=c.query_row("SELECT id FROM events WHERE app_id=? AND status='received' ORDER BY sequence LIMIT 1",[&app],|r|r.get(0)).optional()?; if first.as_deref()!=Some(event_id){bail!("event_not_inflight")}; c.execute("UPDATE events SET status='delivered' WHERE app_id=? AND id=?",params![app,event_id])?; let out=json!({"eventId":event_id,"status":"delivered","deliveryAck":true}); Self::insert_notification(c,&app,"delivery_ack",&out)?; Ok(out) })
    }
    pub(crate) fn insert_feedback(
        &self,
        connection_id: &str,
        app_id: &str,
        feedback_id: &str,
        in_reply_to: Option<&str>,
        payload: Value,
    ) -> Result<Value> {
        self.with(|c| {
            let belongs: i64 = c.query_row(
                "SELECT count(*) FROM apps WHERE id=? AND connection_id=?",
                params![app_id, connection_id],
                |r| r.get(0),
            )?;
            if belongs != 1 {
                bail!("app_not_bound_to_connection")
            };
            c.execute(
                "INSERT INTO feedback(id,app_id,in_reply_to,payload,created_at) VALUES(?,?,?,?,?)",
                params![
                    feedback_id,
                    app_id,
                    in_reply_to,
                    serde_json::to_string(&payload)?,
                    now()
                ],
            )?;
            let out =
                json!({"feedbackId":feedback_id,"inReplyToEventId":in_reply_to,"payload":payload});
            Self::insert_notification(c, app_id, "feedback", &out)?;
            Ok(out)
        })
    }
    pub(crate) fn insert_notification(
        c: &Connection,
        app_id: &str,
        kind: &str,
        data: &Value,
    ) -> Result<i64> {
        c.execute(
            "INSERT INTO notifications(app_id,kind,data,created_at) VALUES(?,?,?,?)",
            params![app_id, kind, serde_json::to_string(data)?, now()],
        )?;
        Ok(c.last_insert_rowid())
    }
    pub(crate) fn replay(&self, app_id: &str, cursor: i64) -> Result<Vec<Notification>> {
        self.with(|c| { let mut q=c.prepare("SELECT cursor,kind,data FROM notifications WHERE app_id=? AND cursor>? ORDER BY cursor")?; let rows=q.query_map(params![app_id,cursor],|r| Ok(Notification{cursor:r.get(0)?,app_id:app_id.to_string(),kind:r.get(1)?,data:serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(Value::Null)}))?.collect::<rusqlite::Result<Vec<_>>>()?; Ok(rows) })
    }
}
