//! Durable v2 application commands. All read/write authorization happens at dispatch.
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRequest {
    pub request_id: String,
    pub tool: String,
    pub space_id: Option<String>,
    pub args: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ApiError {}
fn err(code: &str, message: impl Into<String>) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
    }
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    err("Internal", e.to_string())
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    spaces: Vec<Value>,
    members: Vec<Value>,
    topics: Vec<Value>,
    #[serde(default)]
    events: Vec<Value>,
    #[serde(default)]
    invites: Vec<Value>,
    #[serde(default)]
    endpoints: Vec<Value>,
    #[serde(default)]
    receipts: Vec<Value>,
    #[serde(default)]
    settings: Value,
    #[serde(default, skip_serializing)]
    attachments: Vec<Value>,
}
struct Inner {
    db: Connection,
    secret: String,
}
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}
pub fn owner_token_path(dir: &Path) -> PathBuf {
    dir.join("v2-owner.token")
}
fn protect(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if path.is_dir() { 0o700 } else { 0o600 }),
        )?;
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("whoami").output()?;
        anyhow::ensure!(output.status.success(), "cannot determine local owner");
        let user = String::from_utf8(output.stdout)?.trim().to_string();
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args([
                "/inheritance:r",
                "/grant:r",
                &format!("{}:{}", user, if path.is_dir() { "(OI)(CI)F" } else { "F" }),
            ])
            .output()?;
        anyhow::ensure!(
            status.status.success(),
            "cannot protect runtime storage ACL"
        );
    }
    Ok(())
}
fn derive(secret: &str, kind: &str, key: &str) -> String {
    hash(&format!("linkstart-v2\0{secret}\0{kind}\0{key}"))
}
fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str, ApiError> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|x| !x.trim().is_empty())
        .ok_or_else(|| err("Invalid", format!("{k} must be a nonempty string")))
}
fn admin(roles: &Value) -> bool {
    roles
        .as_array()
        .is_some_and(|rs| rs.iter().any(|r| r == "human_admin" || r == "manager"))
}
const ROLES: &[&str] = &[
    "human_admin",
    "session_maintainer",
    "manager",
    "implementer",
    "reviewer",
    "consultant",
    "tester",
    "observer",
    "task_decomposer",
];
fn validate_args(r: &ToolRequest) -> Result<(), ApiError> {
    let allowed: &[&str] = match r.tool.as_str() {
        "spaces.list" | "members.list" | "topics.list" | "activity.read" | "endpoints.list" => {
            &["cursor", "limit"]
        }
        "spaces.create" => &["name", "administrator_name", "manager_name"],
        "members.invite" => &["roles"],
        "members.heartbeat" => &["attachment_id"],
        "members.register" => &["name"],
        "members.set_roles" => &["member_id", "roles"],
        "topics.create" => &["title", "body", "kind", "parent_topic_id", "dependencies"],
        "topics.read" => &["topic_id", "cursor", "limit"],
        "task.act" => &[
            "topic_id",
            "action",
            "comment",
            "expected_revision",
            "artifact_ref",
            "expected_submission",
            "reply_to",
            "artifact_refs",
        ],
        "settings.get" | "identity.current" => &[],
        "settings.set" => &["settings"],
        "endpoints.register" => &["name", "kind", "config"],
        "endpoints.grant" => &["endpoint_id", "member_id", "operations"],
        "endpoint.authorize" => &["endpoint_id", "operation"],
        _ => return Err(err("Unsupported", format!("unsupported tool {}", r.tool))),
    };
    let args = r
        .args
        .as_object()
        .ok_or_else(|| err("Invalid", "args must be an object"))?;
    if let Some(key) = args.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(err("Invalid", format!("unsupported argument {key}")));
    }
    Ok(())
}
fn roles_valid(v: &Value) -> Result<(), ApiError> {
    let roles = v
        .as_array()
        .ok_or_else(|| err("Invalid", "roles must be an array"))?;
    if roles.is_empty()
        || roles
            .iter()
            .any(|r| !r.as_str().is_some_and(|r| ROLES.contains(&r)))
    {
        return Err(err("Invalid", "unknown or empty role set"));
    }
    Ok(())
}
fn has(roles: &Value, role: &str) -> bool {
    roles
        .as_array()
        .is_some_and(|a| a.iter().any(|r| r == role))
}
fn posting(roles: &Value) -> bool {
    roles.as_array().is_some_and(|a| {
        a.iter()
            .any(|r| r != "observer" && r != "session_maintainer")
    })
}
fn public_member(m: &Value) -> Value {
    let mut m = m.clone();
    m.as_object_mut().unwrap().remove("credential_hash");
    m
}
fn member_view(s: &State, member: &Value, time: u64) -> Value {
    let mut view = public_member(member);
    let ttl = s.settings["heartbeat_seconds"]
        .as_u64()
        .unwrap_or(30)
        .saturating_mul(1000);
    let mut status = "disconnected";
    let mut last_seen = None;
    let attachments = s
        .attachments
        .iter()
        .filter(|a| a["identity_id"] == member["id"])
        .map(|a| {
            let seen = a["last_seen"].as_u64().unwrap_or(0);
            last_seen = Some(last_seen.unwrap_or(0).max(seen));
            let age = time.saturating_sub(seen);
            let state = if age <= ttl {
                "alive"
            } else if age <= ttl.saturating_mul(3) {
                "suspect"
            } else {
                "disconnected"
            };
            if state == "alive" || (state == "suspect" && status == "disconnected") {
                status = state;
            }
            json!({"attachment_id":a["id"],"last_seen":seen,"state":state})
        })
        .collect::<Vec<_>>();
    view["connection_state"] = json!(status);
    view["last_seen"] = json!(last_seen);
    view["attachments"] = json!(attachments);
    view["liveness_kind"] = json!("connection");
    view
}
fn permission(owner: bool, member: Option<&Value>, r: &ToolRequest) -> Result<(), ApiError> {
    if owner {
        return Ok(());
    }
    let m = member.ok_or_else(|| err("Unauthenticated", "invalid credential"))?;
    if r.tool == "spaces.list" {
        return Ok(());
    }
    if matches!(r.tool.as_str(), "settings.get" | "settings.set") {
        return Err(err("Denied", "global runtime settings require local owner"));
    }
    if r.space_id.as_deref() != m["space_id"].as_str() {
        return Err(err("Denied", "space membership required"));
    }
    let roles = &m["roles"];
    let manage = admin(roles);
    let allowed = match r.tool.as_str() {
        "spaces.create" => false,
        "members.invite" | "members.set_roles" | "settings.set" | "endpoints.grant" => manage,
        "topics.create" => manage || posting(roles),
        "task.act" => match r.args["action"].as_str().unwrap_or("") {
            "comment" => manage || posting(roles),
            "claim" | "block" | "resume" | "submit_review" => manage || has(roles, "implementer"),
            "approve_review" | "request_changes" => manage || has(roles, "reviewer"),
            "pass_test" | "fail_test" => manage || has(roles, "tester"),
            "reclaim" => manage,
            _ => true,
        },
        "endpoints.register" => manage || has(roles, "session_maintainer"),
        _ => true,
    };
    if allowed {
        Ok(())
    } else {
        Err(err("Denied", "current roles do not permit this operation"))
    }
}
fn make_member(s: &mut State, secret: &str, space: &str, name: &str, roles: Value) -> Value {
    let key = id();
    let token = derive(secret, "member", &key);
    let m = json!({"id":key,"space_id":space,"name":name,"kind":if has(&roles,"human_admin"){"human"}else{"agent"},"roles":roles,"credential_hash":hash(&token),"created_at":now()});
    s.members.push(m.clone());
    let mut public = m;
    public.as_object_mut().unwrap().remove("credential_hash");
    public["credential"] = json!({"$credential":key});
    public
}
// Only server-owned response slots may materialize secrets. Never traverse caller JSON.
fn hydrate(v: &mut Value, secret: &str) {
    if let Some(key) = v
        .get("credential")
        .and_then(|m| m.get("$credential"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        if v.get("id").and_then(Value::as_str) == Some(&key) && v.get("roles").is_some() {
            v["credential"] = json!(derive(secret, "member", &key));
        }
    }
    if let Some(key) = v
        .get("invite_token")
        .and_then(|m| m.get("$invite"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        v["invite_token"] = json!(derive(secret, "invite", &key));
    }
    for key in ["administrator", "manager"] {
        if let Some(member) = v.get_mut(key) {
            if member.is_object() {
                hydrate(member, secret);
            }
        }
    }
}
fn readonly_tool(tool: &str) -> bool {
    matches!(
        tool,
        "topics.read"
            | "topics.list"
            | "spaces.list"
            | "members.list"
            | "activity.read"
            | "settings.get"
            | "endpoints.list"
            | "endpoint.authorize"
            | "identity.current"
    )
}
impl Engine {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        Self::open_with_clock(dir, Arc::new(now))
    }
    /// Clock returns Unix epoch milliseconds; injectable only at the system time boundary.
    pub fn open_with_clock(
        dir: &Path,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        protect(dir)?;
        let token_path = owner_token_path(dir);
        let secret = match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&token_path)
        {
            Ok(mut f) => {
                let secret = format!("{}{}", id(), id());
                f.write_all(secret.as_bytes())?;
                f.sync_all()?;
                secret
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::read_to_string(&token_path)?
            }
            Err(e) => return Err(e.into()),
        };
        protect(&token_path)?;
        anyhow::ensure!(secret.len() >= 32, "owner token invalid");
        let mut db = Connection::open(dir.join("v2.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS core_state (id INTEGER PRIMARY KEY CHECK(id=1), json TEXT NOT NULL); CREATE TABLE IF NOT EXISTS core_attachments(id TEXT PRIMARY KEY,space_id TEXT NOT NULL,identity_id TEXT NOT NULL,last_seen INTEGER NOT NULL);")?;
        db.execute(
            "INSERT OR IGNORE INTO core_state(id,json) VALUES(1,?1)",
            [serde_json::to_string(&State::default())?],
        )?;
        let legacy: String = db.query_row("SELECT json FROM core_state WHERE id=1", [], |row| {
            row.get(0)
        })?;
        let legacy: State = serde_json::from_str(&legacy)?;
        if !legacy.attachments.is_empty() {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for a in legacy.attachments {
                tx.execute("INSERT OR IGNORE INTO core_attachments(id,space_id,identity_id,last_seen) VALUES(?1,?2,?3,?4)",rusqlite::params![a["id"].as_str().unwrap_or(""),a["space_id"].as_str().unwrap_or(""),a["identity_id"].as_str().unwrap_or(""),a["last_seen"].as_u64().unwrap_or(0)])?;
            }
            tx.commit()?;
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { db, secret })),
            clock,
        })
    }
    pub fn is_owner(&self, credential: &str) -> bool {
        self.inner
            .lock()
            .map(|i| hash(credential) == hash(&i.secret))
            .unwrap_or(false)
    }
    pub fn execute_requiring_endpoint(
        &self,
        credential: &str,
        request: &ToolRequest,
        endpoint_id: &str,
        operation: &str,
    ) -> Result<Value, ApiError> {
        self.execute_inner(credential, request, None, Some((endpoint_id, operation)))
    }
    pub fn execute_requiring_role(
        &self,
        credential: &str,
        request: &ToolRequest,
        required_role: &str,
    ) -> Result<Value, ApiError> {
        self.execute_inner(credential, request, Some(required_role), None)
    }
    pub fn execute(&self, credential: &str, request: &ToolRequest) -> Result<Value, ApiError> {
        self.execute_inner(credential, request, None, None)
    }
    fn execute_inner(
        &self,
        credential: &str,
        request: &ToolRequest,
        required_role: Option<&str>,
        required_endpoint: Option<(&str, &str)>,
    ) -> Result<Value, ApiError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| err("Internal", "runtime lock poisoned"))?;
        let secret = inner.secret.clone();
        let tx = inner
            .db
            .transaction_with_behavior(if readonly_tool(&request.tool) {
                TransactionBehavior::Deferred
            } else {
                TransactionBehavior::Immediate
            })
            .map_err(internal)?;
        let encoded: String = tx
            .query_row("SELECT json FROM core_state WHERE id=1", [], |r| r.get(0))
            .map_err(internal)?;
        let mut s: State = serde_json::from_str(&encoded).map_err(internal)?;
        if matches!(
            request.tool.as_str(),
            "members.heartbeat" | "members.list" | "identity.current"
        ) {
            let mut statement = tx
                .prepare("SELECT id,space_id,identity_id,last_seen FROM core_attachments")
                .map_err(internal)?;
            s.attachments=statement.query_map([],|row|Ok(json!({"id":row.get::<_,String>(0)?,"space_id":row.get::<_,String>(1)?,"identity_id":row.get::<_,String>(2)?,"last_seen":row.get::<_,u64>(3)?}))).map_err(internal)?.collect::<Result<Vec<_>,_>>().map_err(internal)?;
        }
        let owner = hash(credential) == hash(&secret);
        let member = s
            .members
            .iter()
            .find(|m| m["credential_hash"] == hash(credential))
            .cloned();
        let invitation = if request.tool == "members.register" {
            s.invites
                .iter()
                .find(|i| i["token_hash"] == hash(credential))
                .cloned()
        } else {
            None
        };
        let identity = if let Some(invite) = &invitation {
            format!("invite:{}", invite["id"].as_str().unwrap())
        } else if owner {
            "owner".to_string()
        } else {
            member
                .as_ref()
                .and_then(|m| m["id"].as_str())
                .ok_or_else(|| err("Unauthenticated", "invalid credential"))?
                .to_string()
        };
        validate_args(request)?;
        if request.request_id.trim().is_empty() {
            return Err(err("Invalid", "request_id required"));
        }
        if invitation.is_none() {
            permission(owner, member.as_ref(), request)?;
        }
        if let Some(role) = required_role {
            if !ROLES.contains(&role) || !member.as_ref().is_some_and(|m| has(&m["roles"], role)) {
                return Err(err(
                    "Denied",
                    "current identity lacks required dispatch role",
                ));
            }
        }
        if let Some(sid) = request.space_id.as_deref() {
            if invitation.is_none()
                && !owner
                && !member.as_ref().is_some_and(|m| m["space_id"] == sid)
            {
                return Err(err("Denied", "space membership required"));
            }
        }
        if let Some((eid, operation)) = required_endpoint {
            if !["observe", "send", "control", "manage"].contains(&operation) {
                return Err(err("Invalid", "invalid endpoint dispatch operation"));
            }
            let endpoint = s
                .endpoints
                .iter()
                .find(|e| e["id"] == eid && e["space_id"].as_str() == request.space_id.as_deref())
                .ok_or_else(|| err("NotFound", "dispatch endpoint not found in requested space"))?;
            if endpoint["kind"] == "github"
                && (endpoint["source_authority"]["space_id"].as_str()
                    != request.space_id.as_deref()
                    || !endpoint["source_authority"]["approved_by"].is_string())
            {
                return Err(err("Denied", "legacy GitHub endpoint lacks repository approval; an administrator must register an approved endpoint"));
            }
            let manage = owner || member.as_ref().is_some_and(|m| admin(&m["roles"]));
            if !manage
                && !endpoint["grants"][&identity]
                    .as_array()
                    .is_some_and(|ops| ops.iter().any(|op| op == operation))
            {
                return Err(err("Denied", "current endpoint dispatch grant required"));
            }
        }
        let fingerprint = serde_json::to_string(request).map_err(internal)?;
        let renewal =
            request.tool == "members.heartbeat" && request.args.get("attachment_id").is_some();
        if let Some(receipt) = s.receipts.iter().find(|x| {
            !renewal && x["identity"] == identity && x["request_id"] == request.request_id
        }) {
            if receipt["fingerprint"] != fingerprint {
                return Err(err(
                    "RequestConflict",
                    "request id was already used with different content",
                ));
            }
            let mut result = receipt["result"].clone();
            hydrate(&mut result, &secret);
            return Ok(result);
        }
        let mut result = if let Some(invite) = invitation {
            if request.space_id.as_deref() != invite["space_id"].as_str() {
                return Err(err("Denied", "invite belongs to another space"));
            }
            if invite["used"] == true {
                return Err(err("RequestConflict", "invitation already used"));
            }
            if request.args.get("roles").is_some() {
                return Err(err("Invalid", "registration cannot choose roles"));
            }
            let name = text(&request.args, "name")?;
            let result = make_member(
                &mut s,
                &secret,
                invite["space_id"].as_str().unwrap(),
                name,
                invite["roles"].clone(),
            );
            s.invites
                .iter_mut()
                .find(|i| i["id"] == invite["id"])
                .unwrap()["used"] = json!(true);
            result
        } else {
            dispatch(
                &mut s,
                &secret,
                owner,
                member.as_ref(),
                request,
                (self.clock)(),
            )?
        };
        if request.tool == "members.heartbeat" {
            tx.execute("INSERT INTO core_attachments(id,space_id,identity_id,last_seen) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET last_seen=excluded.last_seen",rusqlite::params![result["attachment_id"].as_str().ok_or_else(||err("Internal","heartbeat attachment missing"))?,result["space_id"].as_str().unwrap_or(""),result["identity_id"].as_str().unwrap_or(""),result["last_seen"].as_u64().unwrap_or(0)]).map_err(internal)?;
            if renewal {
                tx.commit().map_err(internal)?;
                return Ok(result);
            }
        }
        if readonly_tool(&request.tool) {
            tx.commit().map_err(internal)?;
            hydrate(&mut result, &secret);
            return Ok(result);
        }
        if !renewal {
            s.receipts.push(json!({"identity":identity,"request_id":request.request_id,"fingerprint":fingerprint,"result":result}));
        }
        let encoded = serde_json::to_string(&s).map_err(internal)?;
        tx.execute("UPDATE core_state SET json=?1 WHERE id=1", [encoded])
            .map_err(internal)?;
        tx.commit().map_err(internal)?;
        hydrate(&mut result, &secret);
        Ok(result)
    }
}
fn page(items: Vec<Value>, args: &Value) -> Result<Value, ApiError> {
    let start = match args.get("cursor") {
        None | Some(Value::Null) => 0,
        Some(Value::String(c)) => c
            .parse::<usize>()
            .map_err(|_| err("Invalid", "invalid cursor"))?,
        _ => return Err(err("Invalid", "cursor must be a string")),
    };
    let limit = match args.get("limit") {
        None => 50,
        Some(v) => v
            .as_u64()
            .filter(|x| *x >= 1 && *x <= 100)
            .ok_or_else(|| err("Invalid", "limit must be 1..100"))? as usize,
    };
    if start > items.len() {
        return Err(err("Invalid", "cursor beyond available history"));
    }
    let end = start.saturating_add(limit).min(items.len());
    Ok(
        json!({"items":items[start..end],"next_cursor":if end<items.len(){Some(end.to_string())}else{None}}),
    )
}
fn dispatch(
    s: &mut State,
    secret: &str,
    owner: bool,
    member: Option<&Value>,
    r: &ToolRequest,
    runtime_now: u64,
) -> Result<Value, ApiError> {
    if r.tool == "identity.current" {
        return Ok(if owner {
            json!({"id":"owner","kind":"owner","roles":[],"space_id":null})
        } else {
            member_view(
                s,
                member.ok_or_else(|| err("Unauthenticated", "invalid credential"))?,
                runtime_now,
            )
        });
    }
    if matches!(r.tool.as_str(), "settings.get" | "settings.set") {
        if !owner {
            return Err(err("Denied", "global runtime settings require local owner"));
        }
        if r.space_id.is_some() {
            return Err(err("Invalid", "runtime settings require space_id null"));
        }
        let mut settings = if s.settings.is_object() {
            s.settings.clone()
        } else {
            json!({"discovery_enabled":false,"heartbeat_seconds":30,"reconnect_seconds":5,"history_limit":10000,"sources":[]})
        };
        if r.tool == "settings.set" {
            let input = r.args["settings"]
                .as_object()
                .ok_or_else(|| err("Invalid", "settings must be an object"))?;
            for (key, value) in input {
                let valid = match key.as_str() {
                    "discovery_enabled" => value.is_boolean(),
                    "heartbeat_seconds" => value.as_u64().is_some_and(|n| (5..=3600).contains(&n)),
                    "reconnect_seconds" => value.as_u64().is_some_and(|n| (1..=3600).contains(&n)),
                    "history_limit" => value.as_u64().is_some_and(|n| (1..=100000).contains(&n)),
                    "sources" => value.as_array().is_some_and(|a| {
                        a.iter().all(|v| {
                            v["locator"].as_str().is_some_and(|s| !s.trim().is_empty())
                                && v["autojoin"].is_boolean()
                        })
                    }),
                    _ => false,
                };
                if !valid {
                    return Err(err(
                        "Invalid",
                        format!("invalid or unsupported setting {key}"),
                    ));
                }
                settings[key] = value.clone();
            }
            s.settings = settings.clone();
        }
        return Ok(
            json!({"settings":settings,"scope":"runtime","requires_restart":true,"effect":"saved runtime defaults; adapter application must be verified"}),
        );
    }
    if r.tool == "spaces.list" {
        return page(
            s.spaces
                .iter()
                .filter(|sp| owner || member.is_some_and(|m| m["space_id"] == sp["id"]))
                .cloned()
                .collect(),
            &r.args,
        );
    }
    if r.tool == "spaces.create" {
        if !owner {
            return Err(err("Denied", "local owner required"));
        }
        let name = text(&r.args, "name")?;
        let sid = id();
        let space = json!({"id":sid,"name":name,"created_at":now()});
        s.spaces.push(space.clone());
        let administrator = make_member(
            s,
            secret,
            &sid,
            r.args["administrator_name"]
                .as_str()
                .unwrap_or("Human administrator"),
            json!(["human_admin"]),
        );
        let manager = r.args["manager_name"]
            .as_str()
            .map(|name| make_member(s, secret, &sid, name, json!(["manager"])));
        return Ok(json!({"space":space,"administrator":administrator,"manager":manager}));
    }
    let sid = r
        .space_id
        .as_deref()
        .ok_or_else(|| err("Invalid", "space_id required"))?;
    if !s.spaces.iter().any(|sp| sp["id"] == sid) {
        return Err(err("NotFound", "space not found"));
    }
    if !owner && !member.is_some_and(|m| m["space_id"] == sid) {
        return Err(err("Denied", "space membership required"));
    }
    let roles = member.map(|m| m["roles"].clone()).unwrap_or(json!([]));
    let manager = owner || admin(&roles);
    let author = if owner {
        "owner"
    } else {
        member.unwrap()["id"].as_str().unwrap()
    };
    match r.tool.as_str() {
        "endpoints.register" => {
            let name = text(&r.args, "name")?;
            let kind = text(&r.args, "kind")?;
            if kind == "github" && !manager {
                return Err(err("Denied", "GitHub repository access requires explicit space administrator or manager authorization"));
            }
            let mut grants = serde_json::Map::new();
            if !manager {
                grants.insert(author.into(), json!(["manage"]));
            }
            let mut ep = json!({"id":id(),"space_id":sid,"name":name,"kind":kind,"config":r.args.get("config").cloned().unwrap_or(json!({})),"created_at":now(),"grants":grants});
            if kind == "github" {
                ep["source_authority"] = json!({"approved_by":author,"space_id":sid,"approved_at":now()});
            }
            s.endpoints.push(ep.clone());
            Ok(ep)
        }
        "endpoints.grant" => {
            let eid = text(&r.args, "endpoint_id")?;
            let mid = text(&r.args, "member_id")?;
            if !s
                .members
                .iter()
                .any(|m| m["id"] == mid && m["space_id"] == sid)
            {
                return Err(err("NotFound", "member not found in space"));
            }
            let ops = r.args["operations"]
                .as_array()
                .ok_or_else(|| err("Invalid", "operations must be an array"))?;
            if ops.iter().any(|op| {
                !op.as_str()
                    .is_some_and(|x| ["observe", "send", "control", "manage"].contains(&x))
            }) {
                return Err(err("Invalid", "unsupported endpoint operation"));
            }
            let ep = s
                .endpoints
                .iter_mut()
                .find(|e| e["id"] == eid && e["space_id"] == sid)
                .ok_or_else(|| err("NotFound", "endpoint not found"))?;
            ep["grants"][mid] = json!(ops);
            Ok(ep.clone())
        }
        "endpoints.list" => page(
            s.endpoints
                .iter()
                .filter(|e| {
                    e["space_id"] == sid
                        && (manager
                            || e["grants"][author]
                                .as_array()
                                .is_some_and(|a| !a.is_empty()))
                })
                .cloned()
                .collect(),
            &r.args,
        ),
        "endpoint.authorize" => {
            let eid = text(&r.args, "endpoint_id")?;
            let op = text(&r.args, "operation")?;
            if !["observe", "send", "control", "manage"].contains(&op) {
                return Err(err("Invalid", "unsupported operation"));
            }
            let ep = s
                .endpoints
                .iter()
                .find(|e| e["id"] == eid && e["space_id"] == sid)
                .ok_or_else(|| err("NotFound", "endpoint not found in space"))?;
            if ep["kind"] == "github" && (ep["source_authority"]["space_id"] != sid || !ep["source_authority"]["approved_by"].is_string()) {
                return Err(err("Denied", "legacy GitHub endpoint lacks repository approval; an administrator must register an approved endpoint"));
            }
            if !manager
                && !ep["grants"][author]
                    .as_array()
                    .is_some_and(|ops| ops.iter().any(|o| o == op))
            {
                return Err(err("Denied", "explicit endpoint grant required"));
            }
            Ok(json!({"identity_id":author,"owner":owner,"endpoint":ep,"operation":op}))
        }
        "members.list" => page(
            s.members
                .iter()
                .filter(|m| m["space_id"] == sid)
                .map(|m| {
                    let mut view = member_view(s, m, runtime_now);
                    let mid = m["id"].as_str().unwrap_or("");
                    let grants: Vec<Value> = s.endpoints.iter()
                        .filter(|ep| ep["space_id"] == sid)
                        .filter(|ep| manager || ep["grants"][author].as_array().is_some_and(|ops| !ops.is_empty()))
                        .flat_map(|ep| {
                            let mut sources = Vec::new();
                            if admin(&m["roles"]) {
                                sources.push(json!({"endpoint_id":ep["id"],"endpoint_name":ep["name"],"operations":["observe","send","control","manage"],"source":"space_manager_role"}));
                            }
                            if let Some(explicit) = ep["grants"][mid].as_array().filter(|ops| !ops.is_empty()) {
                                sources.push(json!({"endpoint_id":ep["id"],"endpoint_name":ep["name"],"operations":explicit,"source":"explicit_endpoint_grant"}));
                            }
                            sources
                        }).collect();
                    view["grants"] = json!(grants);
                    view
                })
                .collect(),
            &r.args,
        ),
        "members.heartbeat" => {
            if owner {
                return Err(err("Denied", "owner is not a persistent member attachment"));
            }
            let attachment = if let Some(value) = r.args.get("attachment_id") {
                let aid = value
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| err("Invalid", "attachment_id must be a nonempty string"))?;
                let a = s
                    .attachments
                    .iter_mut()
                    .find(|a| a["id"] == aid)
                    .ok_or_else(|| err("NotFound", "attachment not found"))?;
                if a["identity_id"] != author || a["space_id"] != sid {
                    return Err(err("Denied", "attachment belongs to another identity"));
                }
                a["last_seen"] = json!(runtime_now);
                a.clone()
            } else {
                let a =
                    json!({"id":id(),"identity_id":author,"space_id":sid,"last_seen":runtime_now});
                s.attachments.push(a.clone());
                a
            };
            Ok(
                json!({"attachment_id":attachment["id"],"identity_id":author,"space_id":sid,"last_seen":runtime_now,"state":"alive","liveness_kind":"connection","heartbeat_seconds":s.settings["heartbeat_seconds"].as_u64().unwrap_or(30)}),
            )
        }
        "members.invite" => {
            roles_valid(&r.args["roles"])?;
            if has(&r.args["roles"], "human_admin") {
                return Err(err(
                    "Denied",
                    "human administrators require local owner bootstrap, not agent registration",
                ));
            }
            let key = id();
            let token = derive(secret, "invite", &key);
            s.invites.push(json!({"id":key,"space_id":sid,"roles":r.args["roles"],"token_hash":hash(&token),"used":false,"created_at":now()}));
            Ok(json!({"invite_token":{"$invite":key},"space_id":sid,"roles":r.args["roles"]}))
        }
        "members.set_roles" => {
            roles_valid(&r.args["roles"])?;
            let mid = text(&r.args, "member_id")?;
            let existing = s
                .members
                .iter()
                .find(|m| m["space_id"] == sid && m["id"] == mid)
                .ok_or_else(|| err("NotFound", "member not found"))?;
            if has(&r.args["roles"], "human_admin") && existing["kind"] != "human" {
                return Err(err(
                    "Denied",
                    "agent identity cannot become a human administrator",
                ));
            }
            if admin(&existing["roles"])
                && !admin(&r.args["roles"])
                && s.members
                    .iter()
                    .filter(|m| m["space_id"] == sid && admin(&m["roles"]))
                    .count()
                    == 1
            {
                return Err(err(
                    "Conflict",
                    "cannot remove the final human_admin or manager",
                ));
            }
            let m = s
                .members
                .iter_mut()
                .find(|m| m["space_id"] == sid && m["id"] == mid)
                .ok_or_else(|| err("NotFound", "member not found"))?;
            m["roles"] = r.args["roles"].clone();
            Ok(public_member(m))
        }
        "topics.create" => {
            if !(manager || posting(&roles)) {
                return Err(err("Denied", "posting role required"));
            }
            let title = text(&r.args, "title")?;
            let body = text(&r.args, "body")?;
            let kind = r.args["kind"].as_str().unwrap_or("discussion");
            if !["task", "plan", "question", "review", "discussion"].contains(&kind) {
                return Err(err("Invalid", "unsupported topic kind"));
            }
            let parent = r
                .args
                .get("parent_topic_id")
                .cloned()
                .unwrap_or(Value::Null);
            if !parent.is_null()
                && !s
                    .topics
                    .iter()
                    .any(|t| t["id"] == parent && t["space_id"] == sid)
            {
                return Err(err("NotFound", "parent topic not found in space"));
            }
            let dependencies = r.args.get("dependencies").cloned().unwrap_or(json!([]));
            let deps = dependencies
                .as_array()
                .ok_or_else(|| err("Invalid", "dependencies must be an array"))?;
            if deps.iter().any(|d| {
                !d.is_string()
                    || !s
                        .topics
                        .iter()
                        .any(|t| t["id"] == *d && t["space_id"] == sid)
            }) {
                return Err(err("NotFound", "dependency not found in space"));
            }
            let topic = json!({"id":id(),"space_id":sid,"title":title,"body":body,"kind":kind,"author":author,"created_at":now(),"updated_at":now(),"revision":0,"state":if kind=="task"{"unclaimed"}else{"discussion"},"assignee":null,"current_submission":null,"parent_topic_id":parent,"dependencies":dependencies});
            s.topics.push(topic.clone());
            s.events.push(json!({"id":id(),"sequence":s.events.len()+1,"space_id":sid,"topic_id":topic["id"],"author":author,"created_at":topic["created_at"],"request_id":r.request_id,"action":"create_topic","comment":body,"reply_to":null,"artifact_refs":[],"assignee_before":null,"revision_before":0,"revision_after":0,"state_before":null,"state_after":topic["state"],"assignee":null,"submission":null,"self_review":false}));
            Ok(topic)
        }
        "topics.list" => page(
            s.topics
                .iter()
                .filter(|t| t["space_id"] == sid)
                .cloned()
                .collect(),
            &r.args,
        ),
        "activity.read" => page(
            s.events
                .iter()
                .filter(|e| e["space_id"] == sid)
                .cloned()
                .collect(),
            &r.args,
        ),
        "topics.read" => {
            let tid = text(&r.args, "topic_id")?;
            let topic = s
                .topics
                .iter()
                .find(|t| t["id"] == tid && t["space_id"] == sid)
                .ok_or_else(|| err("NotFound", "topic not found"))?;
            let comments = page(
                s.events
                    .iter()
                    .filter(|e| {
                        e["space_id"] == sid
                            && e["topic_id"] == tid
                            && e["action"] != "create_topic"
                    })
                    .cloned()
                    .collect(),
                &r.args,
            )?;
            Ok(
                json!({"topic":topic,"comments":comments["items"],"next_cursor":comments["next_cursor"]}),
            )
        }
        "task.act" => {
            let tid = text(&r.args, "topic_id")?;
            let action = text(&r.args, "action")?;
            let comment = text(&r.args, "comment")?;
            let reply = r.args.get("reply_to").cloned().unwrap_or(Value::Null);
            if !reply.is_null()
                && !s
                    .events
                    .iter()
                    .any(|e| e["id"] == reply && e["space_id"] == sid && e["topic_id"] == tid)
            {
                return Err(err("NotFound", "reply target not found in topic"));
            }
            let artifact_refs = r.args.get("artifact_refs").cloned().unwrap_or(json!([]));
            if !artifact_refs.as_array().is_some_and(|a| {
                a.iter()
                    .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
            }) {
                return Err(err("Invalid", "artifact_refs must be nonempty strings"));
            }
            let topic = s
                .topics
                .iter_mut()
                .find(|t| t["id"] == tid && t["space_id"] == sid)
                .ok_or_else(|| err("NotFound", "topic not found"))?;
            let revision = topic["revision"].as_u64().unwrap();
            if r.args["expected_revision"].as_u64() != Some(revision) {
                return Err(err(
                    "TopicConflict",
                    format!(
                        "current revision {revision}, assignee {}",
                        topic["assignee"]
                    ),
                ));
            }
            let before = topic["state"].clone();
            let old_assignee = topic["assignee"].clone();
            let owned = manager || topic["assignee"] == author;
            if action != "comment" && topic["kind"] != "task" {
                return Err(err(
                    "TopicConflict",
                    "only task topics have task transitions",
                ));
            }
            if matches!(
                action,
                "approve_review" | "request_changes" | "pass_test" | "fail_test"
            ) {
                let expected = text(&r.args, "expected_submission")?;
                if topic["current_submission"]["id"] != expected {
                    return Err(err(
                        "TopicConflict",
                        "current submission differs from expected_submission",
                    ));
                }
            }
            match action {
                "comment" => {}
                "claim" if before == "unclaimed" => {
                    topic["state"] = json!("in_progress");
                    topic["assignee"] = json!(author);
                }
                "block" if before == "in_progress" && owned => topic["state"] = json!("blocked"),
                "resume" if before == "blocked" && owned => topic["state"] = json!("in_progress"),
                "submit_review" if before == "in_progress" && owned => {
                    let artifact = text(&r.args, "artifact_ref")?;
                    topic["current_submission"] = json!({"id":id(),"artifact_ref":artifact,"author":author,"created_at":now()});
                    topic["state"] = json!("pending_review");
                }
                "approve_review" if before == "pending_review" => {
                    topic["state"] = json!("pending_test")
                }
                "request_changes" if before == "pending_review" => {
                    topic["state"] = json!("in_progress")
                }
                "pass_test" if before == "pending_test" => topic["state"] = json!("done"),
                "fail_test" if before == "pending_test" => topic["state"] = json!("in_progress"),
                "reclaim" if manager => {
                    topic["state"] = json!("unclaimed");
                    topic["assignee"] = Value::Null;
                    topic["current_submission"] = Value::Null;
                }
                _ => {
                    return Err(err(
                        "TopicConflict",
                        "action is not allowed for assignee/current state",
                    ))
                }
            };
            topic["revision"] = json!(revision + 1);
            topic["updated_at"] = json!(now());
            let event = json!({"id":id(),"sequence":s.events.len()+1,"space_id":sid,"topic_id":tid,"author":author,"created_at":now(),"request_id":r.request_id,"action":action,"comment":comment,"revision_before":revision,"revision_after":revision+1,"state_before":before,"state_after":topic["state"],"assignee":topic["assignee"],"submission":topic["current_submission"],"assignee_before":old_assignee,"reply_to":reply,"artifact_refs":artifact_refs,"self_review":matches!(action,"approve_review"|"request_changes"|"pass_test"|"fail_test")&&topic["current_submission"]["author"]==author});
            s.events.push(event.clone());
            Ok(json!({"topic":topic,"event":event}))
        }
        _ => Err(err("Unsupported", format!("unsupported tool {}", r.tool))),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_maintainer_cannot_mint_github_authority() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let sp = e
            .execute(
                &owner,
                &req(
                    "s",
                    "spaces.create",
                    None,
                    json!({"name":"source authority"}),
                ),
            )
            .unwrap();
        let sid = sp["space"]["id"].as_str().unwrap();
        let admin = sp["administrator"]["credential"].as_str().unwrap();
        let invite = e
            .execute(
                admin,
                &req(
                    "i",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["session_maintainer"]}),
                ),
            )
            .unwrap();
        let member = e
            .execute(
                invite["invite_token"].as_str().unwrap(),
                &req(
                    "j",
                    "members.register",
                    Some(sid),
                    json!({"name":"session operator"}),
                ),
            )
            .unwrap();
        let input = json!({"name":"repo","kind":"github","config":{"repository":"example/repo","allow_write_back":true}});
        let result = e.execute(
            member["credential"].as_str().unwrap(),
            &req(
                "self-approve",
                "endpoints.register",
                Some(sid),
                input.clone(),
            ),
        );
        assert_eq!(result.unwrap_err().code, "Denied");
        let ep = e
            .execute(
                admin,
                &req("approved", "endpoints.register", Some(sid), input),
            )
            .unwrap();
        assert_eq!(
            ep["source_authority"]["approved_by"],
            sp["administrator"]["id"]
        );
        let terminal = e
            .execute(
                member["credential"].as_str().unwrap(),
                &req(
                    "terminal",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"owned shell","kind":"terminal"}),
                ),
            )
            .unwrap();
        assert_eq!(terminal["kind"], "terminal");
    }
    #[test]
    fn member_grant_view_tracks_authoritative_endpoint_changes() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let sp = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"grants"})),
            )
            .unwrap();
        let sid = sp["space"]["id"].as_str().unwrap();
        let admin = sp["administrator"]["credential"].as_str().unwrap();
        let invite = e
            .execute(
                admin,
                &req(
                    "i",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["observer"]}),
                ),
            )
            .unwrap();
        let member = e
            .execute(
                invite["invite_token"].as_str().unwrap(),
                &req(
                    "join",
                    "members.register",
                    Some(sid),
                    json!({"name":"observer"}),
                ),
            )
            .unwrap();
        let endpoint = e
            .execute(
                admin,
                &req(
                    "ep",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"shell", "kind":"terminal"}),
                ),
            )
            .unwrap();
        e.execute(admin, &req("grant", "endpoints.grant", Some(sid), json!({"endpoint_id":endpoint["id"],"member_id":member["id"],"operations":["observe"]}))).unwrap();
        let read = || {
            let list = e
                .execute(admin, &req("list", "members.list", Some(sid), json!({})))
                .unwrap();
            list["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == member["id"])
                .unwrap()["grants"]
                .clone()
        };
        assert_eq!(
            read(),
            json!([{"endpoint_id":endpoint["id"],"endpoint_name":"shell","operations":["observe"],"source":"explicit_endpoint_grant"}])
        );
        e.execute(
            admin,
            &req(
                "revoke",
                "endpoints.grant",
                Some(sid),
                json!({"endpoint_id":endpoint["id"],"member_id":member["id"],"operations":[]}),
            ),
        )
        .unwrap();
        assert_eq!(read(), json!([]));
    }
    #[test]
    fn member_projection_preserves_role_and_explicit_grant_sources() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let sp = e
            .execute(
                &owner,
                &req(
                    "s",
                    "spaces.create",
                    None,
                    json!({"name":"sources","manager_name":"manager"}),
                ),
            )
            .unwrap();
        let sid = sp["space"]["id"].as_str().unwrap();
        let admin = sp["administrator"]["credential"].as_str().unwrap();
        let manager = &sp["manager"];
        let ep = e
            .execute(
                admin,
                &req(
                    "ep",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"shell","kind":"terminal"}),
                ),
            )
            .unwrap();
        e.execute(
            admin,
            &req(
                "g",
                "endpoints.grant",
                Some(sid),
                json!({"endpoint_id":ep["id"],"member_id":manager["id"],"operations":["observe"]}),
            ),
        )
        .unwrap();
        let list = e
            .execute(admin, &req("read", "members.list", Some(sid), json!({})))
            .unwrap();
        let grants = &list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == manager["id"])
            .unwrap()["grants"];
        assert_eq!(
            grants,
            &json!([
                {"endpoint_id":ep["id"],"endpoint_name":"shell","operations":["observe","send","control","manage"],"source":"space_manager_role"},
                {"endpoint_id":ep["id"],"endpoint_name":"shell","operations":["observe"],"source":"explicit_endpoint_grant"}
            ])
        );
    }
    #[test]
    fn member_projection_hides_endpoints_the_reader_cannot_discover() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let sp = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"visibility"})),
            )
            .unwrap();
        let sid = sp["space"]["id"].as_str().unwrap();
        let admin = sp["administrator"]["credential"].as_str().unwrap();
        let invite = e
            .execute(
                admin,
                &req(
                    "i",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["observer"]}),
                ),
            )
            .unwrap();
        let reader = e
            .execute(
                invite["invite_token"].as_str().unwrap(),
                &req(
                    "join",
                    "members.register",
                    Some(sid),
                    json!({"name":"reader"}),
                ),
            )
            .unwrap();
        let token = reader["credential"].as_str().unwrap();
        let hidden = e
            .execute(
                admin,
                &req(
                    "secret",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"private-shell","kind":"terminal"}),
                ),
            )
            .unwrap();
        let visible = e
            .execute(
                admin,
                &req(
                    "visible",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"shared-shell","kind":"terminal"}),
                ),
            )
            .unwrap();
        e.execute(admin, &req("g", "endpoints.grant", Some(sid), json!({"endpoint_id":visible["id"],"member_id":reader["id"],"operations":["observe"]}))).unwrap();
        let list = e
            .execute(token, &req("read", "members.list", Some(sid), json!({})))
            .unwrap();
        let encoded = list.to_string();
        assert!(
            !encoded.contains(hidden["id"].as_str().unwrap()),
            "member projection leaked hidden endpoint id"
        );
        assert!(
            !encoded.contains("private-shell"),
            "member projection leaked hidden endpoint name"
        );
        assert!(encoded.contains(visible["id"].as_str().unwrap()));
    }
    #[test]
    fn legacy_github_without_server_approval_cannot_dispatch_even_as_manager() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let sp = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"legacy"})),
            )
            .unwrap();
        let sid = sp["space"]["id"].as_str().unwrap();
        let admin = sp["administrator"]["credential"].as_str().unwrap();
        let ep = e
            .execute(
                admin,
                &req(
                    "ep",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"old repository","kind":"terminal"}),
                ),
            )
            .unwrap();
        drop(e);
        // Migration input only: old persisted endpoints predate server-owned authority.
        // All behavior assertions below go through the public Engine interface.
        let db = Connection::open(dir.path().join("v2.sqlite3")).unwrap();
        let encoded: String = db
            .query_row("SELECT json FROM core_state WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        let mut legacy: Value = serde_json::from_str(&encoded).unwrap();
        legacy["endpoints"][0]["kind"] = json!("github");
        legacy["endpoints"][0]["config"] = json!({"repository":"example/repo","source_authority":{"approved_by":"caller-forged","space_id":sid}});
        db.execute(
            "UPDATE core_state SET json=?1 WHERE id=1",
            [legacy.to_string()],
        )
        .unwrap();
        drop(db);
        let e = Engine::open(dir.path()).unwrap();
        let auth = req(
            "auth",
            "endpoint.authorize",
            Some(sid),
            json!({"endpoint_id":ep["id"],"operation":"observe"}),
        );
        assert_eq!(e.execute(admin, &auth).unwrap_err().code, "Denied");
        let create = req(
            "apply",
            "topics.create",
            Some(sid),
            json!({"title":"must not import","body":"legacy input"}),
        );
        let result =
            e.execute_requiring_endpoint(admin, &create, ep["id"].as_str().unwrap(), "observe");
        assert_eq!(result.unwrap_err().code, "Denied");
        let topics = e
            .execute(admin, &req("read", "topics.list", Some(sid), json!({})))
            .unwrap();
        assert_eq!(topics["items"], json!([]));
    }
    fn req(id: &str, tool: &str, space: Option<&str>, args: Value) -> ToolRequest {
        ToolRequest {
            request_id: id.into(),
            tool: tool.into(),
            space_id: space.map(str::to_string),
            args,
        }
    }
    #[test]
    fn durable_space_and_topic_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req(
                    "space",
                    "spaces.create",
                    None,
                    json!({"name":"durable","administrator_name":"human"}),
                ),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        let topic = e
            .execute(
                admin,
                &req(
                    "topic",
                    "topics.create",
                    Some(sid),
                    json!({"title":"plan","body":"persistent first post","kind":"task"}),
                ),
            )
            .unwrap();
        drop(e);
        let reopened = Engine::open(dir.path()).unwrap();
        let read = reopened
            .execute(
                admin,
                &req(
                    "read",
                    "topics.read",
                    Some(sid),
                    json!({"topic_id":topic["id"]}),
                ),
            )
            .unwrap();
        assert_eq!(read["topic"]["body"], "persistent first post");
        assert_eq!(read["topic"]["revision"], 0);
    }
    #[test]
    fn claim_is_atomic_and_receipt_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req(
                    "s",
                    "spaces.create",
                    None,
                    json!({"name":"race","manager_name":"manager"}),
                ),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap().to_string();
        let admin = created["administrator"]["credential"]
            .as_str()
            .unwrap()
            .to_string();
        let manager = created["manager"]["credential"]
            .as_str()
            .unwrap()
            .to_string();
        let t = e
            .execute(
                &admin,
                &req(
                    "t",
                    "topics.create",
                    Some(&sid),
                    json!({"title":"claim me","body":"work","kind":"task"}),
                ),
            )
            .unwrap();
        let tid = t["id"].as_str().unwrap();
        let r = req(
            "claim",
            "task.act",
            Some(&sid),
            json!({"topic_id":tid,"action":"claim","comment":"taking responsibility","expected_revision":0}),
        );
        let other = Engine::open(dir.path()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b = barrier.clone();
        let r2 = r.clone();
        let e2 = other.clone();
        let manager2 = manager.clone();
        let join = std::thread::spawn(move || {
            b.wait();
            e2.execute(&manager2, &r2)
        });
        barrier.wait();
        let a = e.execute(&admin, &r);
        let b = join.join().unwrap();
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        let (credential, receipt) = if let Ok(v) = a {
            (admin, v)
        } else {
            assert_eq!(a.unwrap_err().code, "TopicConflict");
            (manager, b.unwrap())
        };
        assert_eq!(receipt["topic"]["revision"], 1);
        drop(e);
        drop(other);
        let reopened = Engine::open(dir.path()).unwrap();
        assert_eq!(reopened.execute(&credential, &r).unwrap(), receipt);
        let mut changed = r.clone();
        changed.args["comment"] = json!("different");
        assert_eq!(
            reopened.execute(&credential, &changed).unwrap_err().code,
            "RequestConflict"
        );
        let view = reopened
            .execute(
                &credential,
                &req("read", "topics.read", Some(&sid), json!({"topic_id":tid})),
            )
            .unwrap();
        assert_eq!(view["comments"].as_array().unwrap().len(), 1);
        assert_eq!(view["comments"][0]["state_after"], "in_progress");
    }
    #[test]
    fn revoked_implementer_cannot_update_owned_task_or_replay_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"roles"})),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        let invite = e
            .execute(
                admin,
                &req(
                    "invite",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["implementer"]}),
                ),
            )
            .unwrap();
        let token = invite["invite_token"].as_str().unwrap();
        let worker = e
            .execute(
                token,
                &req(
                    "join",
                    "members.register",
                    Some(sid),
                    json!({"name":"implementer"}),
                ),
            )
            .unwrap();
        let credential = worker["credential"].as_str().unwrap();
        let t = e
            .execute(
                credential,
                &req(
                    "t",
                    "topics.create",
                    Some(sid),
                    json!({"title":"task","body":"do","kind":"task"}),
                ),
            )
            .unwrap();
        let claim = req(
            "claim",
            "task.act",
            Some(sid),
            json!({"topic_id":t["id"],"action":"claim","comment":"start","expected_revision":0}),
        );
        e.execute(credential, &claim).unwrap();
        e.execute(
            admin,
            &req(
                "revoke",
                "members.set_roles",
                Some(sid),
                json!({"member_id":worker["id"],"roles":["observer"]}),
            ),
        )
        .unwrap();
        assert_eq!(e.execute(credential, &claim).unwrap_err().code, "Denied");
        assert_eq!(e.execute(credential,&req("block","task.act",Some(sid),json!({"topic_id":t["id"],"action":"block","comment":"blocked","expected_revision":1}))).unwrap_err().code,"Denied");
        let view = e
            .execute(
                credential,
                &req(
                    "read",
                    "topics.read",
                    Some(sid),
                    json!({"topic_id":t["id"]}),
                ),
            )
            .unwrap();
        assert_eq!(view["topic"]["assignee"], worker["id"]);
        assert_eq!(view["comments"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn review_and_test_bind_immutable_submission_and_reject_old_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"submissions"})),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        let t = e
            .execute(
                admin,
                &req(
                    "t",
                    "topics.create",
                    Some(sid),
                    json!({"title":"deliver","body":"work","kind":"task"}),
                ),
            )
            .unwrap();
        let tid = t["id"].as_str().unwrap();
        let act = |n: u64, action: &str, artifact: Option<&str>, submission: Option<&str>| {
            e.execute(admin,&req(&format!("act{n}"),"task.act",Some(sid),json!({"topic_id":tid,"action":action,"comment":"evidence","expected_revision":n,"artifact_ref":artifact,"expected_submission":submission})))
        };
        act(0, "claim", None, None).unwrap();
        let first = act(1, "submit_review", Some("git:first"), None).unwrap();
        let first_id = first["topic"]["current_submission"]["id"].as_str().unwrap();
        assert_eq!(first["topic"]["state"], "pending_review");
        act(2, "request_changes", None, Some(first_id)).unwrap();
        let second = act(3, "submit_review", Some("git:second"), None).unwrap();
        let second_id = second["topic"]["current_submission"]["id"]
            .as_str()
            .unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(
            act(4, "approve_review", None, Some(first_id))
                .unwrap_err()
                .code,
            "TopicConflict"
        );
        let reviewed = act(4, "approve_review", None, Some(second_id)).unwrap();
        assert_eq!(reviewed["event"]["self_review"], true);
        act(5, "pass_test", None, Some(second_id)).unwrap();
        let view = e
            .execute(
                admin,
                &req("read", "topics.read", Some(sid), json!({"topic_id":tid})),
            )
            .unwrap();
        assert_eq!(view["topic"]["state"], "done");
        assert_eq!(view["comments"][2]["submission"]["id"], first_id);
        assert_eq!(view["comments"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn invitation_cannot_self_elevate_and_last_administrator_requires_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"handoff"})),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        assert_eq!(
            e.execute(
                admin,
                &req(
                    "demote",
                    "members.set_roles",
                    Some(sid),
                    json!({"member_id":created["administrator"]["id"],"roles":["observer"]})
                )
            )
            .unwrap_err()
            .code,
            "Conflict"
        );
        let invited = e
            .execute(
                admin,
                &req(
                    "i",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["session_maintainer"]}),
                ),
            )
            .unwrap();
        let token = invited["invite_token"].as_str().unwrap();
        assert_eq!(
            e.execute(
                token,
                &req(
                    "join",
                    "members.register",
                    Some(sid),
                    json!({"name":"admin","roles":["manager"]})
                )
            )
            .unwrap_err()
            .code,
            "Invalid"
        );
        let maintainer = e
            .execute(
                token,
                &req(
                    "join",
                    "members.register",
                    Some(sid),
                    json!({"name":"admin"}),
                ),
            )
            .unwrap();
        let credential = maintainer["credential"].as_str().unwrap();
        assert_eq!(
            e.execute(
                credential,
                &req(
                    "elevate",
                    "members.set_roles",
                    Some(sid),
                    json!({"member_id":maintainer["id"],"roles":["manager"]})
                )
            )
            .unwrap_err()
            .code,
            "Denied"
        );
        e.execute(
            admin,
            &req(
                "promote",
                "members.set_roles",
                Some(sid),
                json!({"member_id":maintainer["id"],"roles":["manager"]}),
            ),
        )
        .unwrap();
        e.execute(
            admin,
            &req(
                "demote",
                "members.set_roles",
                Some(sid),
                json!({"member_id":created["administrator"]["id"],"roles":["observer"]}),
            ),
        )
        .unwrap();
        let members = e
            .execute(
                credential,
                &req("members", "members.list", Some(sid), json!({})),
            )
            .unwrap();
        assert_eq!(members["items"].as_array().unwrap().len(), 2);
        assert!(members["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("credential_hash").is_none() && m.get("credential").is_none()));
    }

    #[test]
    fn endpoint_grants_are_scoped_and_rechecked_after_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"endpoint"})),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        let i = e
            .execute(
                admin,
                &req(
                    "i",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["observer"]}),
                ),
            )
            .unwrap();
        let observer = e
            .execute(
                i["invite_token"].as_str().unwrap(),
                &req(
                    "j",
                    "members.register",
                    Some(sid),
                    json!({"name":"read only"}),
                ),
            )
            .unwrap();
        let credential = observer["credential"].as_str().unwrap();
        let ep = e
            .execute(
                admin,
                &req(
                    "ep",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"terminal","kind":"terminal"}),
                ),
            )
            .unwrap();
        let read = req(
            "observe",
            "endpoint.authorize",
            Some(sid),
            json!({"endpoint_id":ep["id"],"operation":"observe"}),
        );
        assert_eq!(e.execute(credential, &read).unwrap_err().code, "Denied");
        assert_eq!(
            e.execute(
                credential,
                &req("l", "endpoints.list", Some(sid), json!({}))
            )
            .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        e.execute(
            admin,
            &req(
                "g",
                "endpoints.grant",
                Some(sid),
                json!({"endpoint_id":ep["id"],"member_id":observer["id"],"operations":["observe"]}),
            ),
        )
        .unwrap();
        assert_eq!(
            e.execute(credential, &read).unwrap()["identity_id"],
            observer["id"]
        );
        assert_eq!(
            e.execute(
                credential,
                &req(
                    "send",
                    "endpoint.authorize",
                    Some(sid),
                    json!({"endpoint_id":ep["id"],"operation":"send"})
                )
            )
            .unwrap_err()
            .code,
            "Denied"
        );
        e.execute(
            admin,
            &req(
                "revoke",
                "endpoints.grant",
                Some(sid),
                json!({"endpoint_id":ep["id"],"member_id":observer["id"],"operations":[]}),
            ),
        )
        .unwrap();
        assert_eq!(e.execute(credential, &read).unwrap_err().code, "Denied");
        let other = e
            .execute(
                &owner,
                &req("s2", "spaces.create", None, json!({"name":"isolated"})),
            )
            .unwrap();
        assert_eq!(
            e.execute(
                credential,
                &req(
                    "cross",
                    "endpoint.authorize",
                    Some(other["space"]["id"].as_str().unwrap()),
                    json!({"endpoint_id":ep["id"],"operation":"observe"})
                )
            )
            .unwrap_err()
            .code,
            "Denied"
        );
    }

    #[test]
    fn decomposer_builds_scoped_child_tasks_and_offline_activity_paginates() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"project"})),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        let i = e
            .execute(
                admin,
                &req(
                    "i",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["task_decomposer"]}),
                ),
            )
            .unwrap();
        let d = e
            .execute(
                i["invite_token"].as_str().unwrap(),
                &req(
                    "j",
                    "members.register",
                    Some(sid),
                    json!({"name":"planner"}),
                ),
            )
            .unwrap();
        let token = d["credential"].as_str().unwrap();
        let mission = e
            .execute(
                token,
                &req(
                    "mission",
                    "topics.create",
                    Some(sid),
                    json!({"title":"mission","body":"parent","kind":"plan"}),
                ),
            )
            .unwrap();
        let child=e.execute(token,&req("child","topics.create",Some(sid),json!({"title":"child","body":"step","kind":"task","parent_topic_id":mission["id"],"dependencies":[mission["id"]]}))).unwrap();
        assert_eq!(child["parent_topic_id"], mission["id"]);
        assert_eq!(e.execute(token,&req("claim","task.act",Some(sid),json!({"topic_id":child["id"],"action":"claim","comment":"no","expected_revision":0}))).unwrap_err().code,"Denied");
        e.execute(token,&req("comment","task.act",Some(sid),json!({"topic_id":child["id"],"action":"comment","comment":"clarification","expected_revision":0}))).unwrap();
        let other = e
            .execute(
                &owner,
                &req("other", "spaces.create", None, json!({"name":"private"})),
            )
            .unwrap();
        assert_eq!(e.execute(token,&req("invalidchild","topics.create",Some(sid),json!({"title":"bad","body":"bad","kind":"task","parent_topic_id":other["space"]["id"]}))).unwrap_err().code,"NotFound");
        let spaces = e
            .execute(token, &req("spaces", "spaces.list", None, json!({})))
            .unwrap();
        assert_eq!(spaces["items"].as_array().unwrap().len(), 1);
        let topics = e
            .execute(
                token,
                &req("topics", "topics.list", Some(sid), json!({"limit":1})),
            )
            .unwrap();
        assert_eq!(topics["items"].as_array().unwrap().len(), 1);
        assert!(topics["next_cursor"].is_string());
        let first = e
            .execute(
                token,
                &req("activity", "activity.read", Some(sid), json!({"limit":2})),
            )
            .unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        drop(e);
        let reopened = Engine::open(dir.path()).unwrap();
        let second = reopened
            .execute(
                token,
                &req(
                    "activity2",
                    "activity.read",
                    Some(sid),
                    json!({"cursor":first["next_cursor"]}),
                ),
            )
            .unwrap();
        assert_eq!(second["items"].as_array().unwrap().len(), 1);
        assert_eq!(second["items"][0]["comment"], "clarification");
        assert_eq!(second["items"][0]["author"], d["id"]);
    }

    #[test]
    fn runtime_settings_require_owner_validate_and_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let space = e
            .execute(
                &owner,
                &req(
                    "s",
                    "spaces.create",
                    None,
                    json!({"name":"settings","manager_name":"manager"}),
                ),
            )
            .unwrap();
        let manager = space["manager"]["credential"].as_str().unwrap();
        let request = req(
            "settings",
            "settings.set",
            None,
            json!({"settings":{"history_limit":1234,"heartbeat_seconds":15,"discovery_enabled":false}}),
        );
        assert_eq!(e.execute(manager, &request).unwrap_err().code, "Denied");
        e.execute(&owner, &request).unwrap();
        assert_eq!(
            e.execute(
                &owner,
                &req(
                    "invalid",
                    "settings.set",
                    None,
                    json!({"settings":{"heartbeat_seconds":0}})
                )
            )
            .unwrap_err()
            .code,
            "Invalid"
        );
        drop(e);
        let reopened = Engine::open(dir.path()).unwrap();
        let settings = reopened
            .execute(&owner, &req("get", "settings.get", None, json!({})))
            .unwrap();
        assert_eq!(settings["settings"]["history_limit"], 1234);
        assert_eq!(settings["settings"]["heartbeat_seconds"], 15);
        assert_eq!(settings["settings"]["discovery_enabled"], false);
    }

    #[test]
    fn human_administrator_cannot_be_forged_by_agent_invitation_or_role_change() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let s = e
            .execute(
                &owner,
                &req(
                    "s",
                    "spaces.create",
                    None,
                    json!({"name":"human","manager_name":"agent"}),
                ),
            )
            .unwrap();
        let sid = s["space"]["id"].as_str().unwrap();
        let admin = s["administrator"]["credential"].as_str().unwrap();
        assert_eq!(s["administrator"]["kind"], "human");
        assert_eq!(s["manager"]["kind"], "agent");
        assert_eq!(
            e.execute(
                admin,
                &req(
                    "humaninvite",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["human_admin"]})
                )
            )
            .unwrap_err()
            .code,
            "Denied"
        );
        assert_eq!(
            e.execute(
                admin,
                &req(
                    "humanrole",
                    "members.set_roles",
                    Some(sid),
                    json!({"member_id":s["manager"]["id"],"roles":["human_admin"]})
                )
            )
            .unwrap_err()
            .code,
            "Denied"
        );
        let members = e
            .execute(admin, &req("l", "members.list", Some(sid), json!({})))
            .unwrap();
        assert_eq!(members["items"][1]["roles"], json!(["manager"]));
    }

    #[test]
    fn caller_configuration_cannot_expand_private_credential_receipt_markers() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req(
                    "s",
                    "spaces.create",
                    None,
                    json!({"name":"no credential oracle"}),
                ),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        let marker = json!({"$credential":created["administrator"]["id"]});
        let ep = e
            .execute(
                admin,
                &req(
                    "ep",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"config","kind":"terminal","config":marker}),
                ),
            )
            .unwrap();
        assert_eq!(ep["config"], marker);
        assert_ne!(ep["config"], admin);
    }

    #[test]
    fn rejected_server_fields_leave_no_partial_comment_or_revision() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"atomic failure"})),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        let t = e
            .execute(
                admin,
                &req(
                    "t",
                    "topics.create",
                    Some(sid),
                    json!({"title":"test","body":"start","kind":"task"}),
                ),
            )
            .unwrap();
        let request = req(
            "act",
            "task.act",
            Some(sid),
            json!({"topic_id":t["id"],"action":"claim","comment":"start","expected_revision":0,"author":"someone else"}),
        );
        assert_eq!(e.execute(admin, &request).unwrap_err().code, "Invalid");
        let read = e
            .execute(
                admin,
                &req(
                    "read",
                    "topics.read",
                    Some(sid),
                    json!({"topic_id":t["id"]}),
                ),
            )
            .unwrap();
        assert_eq!(read["topic"]["revision"], 0);
        assert_eq!(read["comments"], json!([]));
        let mut corrected = request;
        corrected.args.as_object_mut().unwrap().remove("author");
        assert_eq!(
            e.execute(admin, &corrected).unwrap()["event"]["author"],
            created["administrator"]["id"]
        );
    }

    #[test]
    fn comment_reply_keeps_same_topic_target_and_artifact_references() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"reply"})),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let admin = created["administrator"]["credential"].as_str().unwrap();
        let t = e
            .execute(
                admin,
                &req(
                    "t",
                    "topics.create",
                    Some(sid),
                    json!({"title":"plan","body":"start","kind":"plan"}),
                ),
            )
            .unwrap();
        let first=e.execute(admin,&req("first","task.act",Some(sid),json!({"topic_id":t["id"],"action":"comment","comment":"question","expected_revision":0}))).unwrap();
        let second=e.execute(admin,&req("second","task.act",Some(sid),json!({"topic_id":t["id"],"action":"comment","comment":"answer","expected_revision":1,"reply_to":first["event"]["id"],"artifact_refs":["git:proof"]}))).unwrap();
        assert_eq!(second["event"]["reply_to"], first["event"]["id"]);
        assert_eq!(second["event"]["artifact_refs"], json!(["git:proof"]));
        let other = e
            .execute(
                admin,
                &req(
                    "other",
                    "topics.create",
                    Some(sid),
                    json!({"title":"other","body":"start","kind":"plan"}),
                ),
            )
            .unwrap();
        assert_eq!(e.execute(admin,&req("bad","task.act",Some(sid),json!({"topic_id":other["id"],"action":"comment","comment":"bad reference","expected_revision":0,"reply_to":first["event"]["id"]}))).unwrap_err().code,"NotFound");
        let view = e
            .execute(
                admin,
                &req(
                    "view",
                    "topics.read",
                    Some(sid),
                    json!({"topic_id":other["id"]}),
                ),
            )
            .unwrap();
        assert_eq!(view["topic"]["revision"], 0);
    }

    #[test]
    fn current_identity_comes_from_credential_and_tracks_revoked_roles() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let created = e
            .execute(
                &owner,
                &req("s", "spaces.create", None, json!({"name":"identity"})),
            )
            .unwrap();
        let sid = created["space"]["id"].as_str().unwrap();
        let token = created["administrator"]["credential"].as_str().unwrap();
        let who = e
            .execute(token, &req("who", "identity.current", Some(sid), json!({})))
            .unwrap();
        assert_eq!(who["id"], created["administrator"]["id"]);
        assert_eq!(who["roles"], json!(["human_admin"]));
        assert!(who.get("credential_hash").is_none());
        assert_eq!(
            e.execute(&owner, &req("owner", "identity.current", None, json!({})))
                .unwrap()["kind"],
            "owner"
        );
    }

    #[test]
    fn read_tools_do_not_request_sqlite_writer_lock() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        e.execute(
            &owner,
            &req("space", "spaces.create", None, json!({"name":"read only"})),
        )
        .unwrap();
        // SQLite is the system boundary. Inject writer contention; assert only public API behavior.
        let writer = Connection::open(dir.path().join("v2.sqlite3")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = e.clone();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(reader.execute(&owner, &req("read", "spaces.list", None, json!({}))));
        });
        let early = rx.recv_timeout(std::time::Duration::from_millis(300));
        writer.execute_batch("ROLLBACK").unwrap();
        worker.join().unwrap();
        assert!(
            early.is_ok(),
            "read-only tool must finish while another connection holds the writer reservation"
        );
        assert_eq!(
            early.unwrap().unwrap()["items"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn independent_attachment_heartbeats_aggregate_connection_liveness_without_releasing_claim() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let time = Arc::new(AtomicU64::new(1_000_000));
        let clock = time.clone();
        let e = Engine::open_with_clock(dir.path(), Arc::new(move || clock.load(Ordering::SeqCst)))
            .unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        e.execute(
            &owner,
            &req(
                "settings",
                "settings.set",
                None,
                json!({"settings":{"heartbeat_seconds":5}}),
            ),
        )
        .unwrap();
        let s = e
            .execute(
                &owner,
                &req("space", "spaces.create", None, json!({"name":"liveness"})),
            )
            .unwrap();
        let sid = s["space"]["id"].as_str().unwrap();
        let admin = s["administrator"]["credential"].as_str().unwrap();
        let topic = e
            .execute(
                admin,
                &req(
                    "topic",
                    "topics.create",
                    Some(sid),
                    json!({"title":"work","body":"remain claimed","kind":"task"}),
                ),
            )
            .unwrap();
        e.execute(admin,&req("claim","task.act",Some(sid),json!({"topic_id":topic["id"],"action":"claim","comment":"working","expected_revision":0}))).unwrap();
        let first = e
            .execute(
                admin,
                &req("beat1", "members.heartbeat", Some(sid), json!({})),
            )
            .unwrap();
        let second = e
            .execute(
                admin,
                &req("beat2", "members.heartbeat", Some(sid), json!({})),
            )
            .unwrap();
        assert_ne!(first["attachment_id"], second["attachment_id"]);
        assert_eq!(first["last_seen"], 1_000_000);
        time.store(1_006_000, Ordering::SeqCst);
        let who = e
            .execute(
                admin,
                &req("who1", "identity.current", Some(sid), json!({})),
            )
            .unwrap();
        assert_eq!(who["connection_state"], "suspect");
        e.execute(
            admin,
            &req(
                "beat3",
                "members.heartbeat",
                Some(sid),
                json!({"attachment_id":second["attachment_id"]}),
            ),
        )
        .unwrap();
        let who = e
            .execute(
                admin,
                &req("who2", "identity.current", Some(sid), json!({})),
            )
            .unwrap();
        assert_eq!(who["connection_state"], "alive");
        assert_eq!(who["attachments"].as_array().unwrap().len(), 2);
        time.store(1_016_000, Ordering::SeqCst);
        assert_eq!(
            e.execute(
                admin,
                &req("who3", "identity.current", Some(sid), json!({}))
            )
            .unwrap()["connection_state"],
            "suspect"
        );
        time.store(1_022_000, Ordering::SeqCst);
        let list = e
            .execute(admin, &req("list", "members.list", Some(sid), json!({})))
            .unwrap();
        assert_eq!(list["items"][0]["connection_state"], "disconnected");
        assert_eq!(list["items"][0]["liveness_kind"], "connection");
        let read = e
            .execute(
                admin,
                &req(
                    "read",
                    "topics.read",
                    Some(sid),
                    json!({"topic_id":topic["id"]}),
                ),
            )
            .unwrap();
        assert_eq!(read["topic"]["assignee"], s["administrator"]["id"]);
        assert_eq!(read["topic"]["revision"], 1);
        let invitation = e
            .execute(
                admin,
                &req(
                    "invite",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["observer"]}),
                ),
            )
            .unwrap();
        let observer = e
            .execute(
                invitation["invite_token"].as_str().unwrap(),
                &req(
                    "join",
                    "members.register",
                    Some(sid),
                    json!({"name":"observer"}),
                ),
            )
            .unwrap();
        assert_eq!(
            e.execute(
                observer["credential"].as_str().unwrap(),
                &req(
                    "steal",
                    "members.heartbeat",
                    Some(sid),
                    json!({"attachment_id":first["attachment_id"]})
                )
            )
            .unwrap_err()
            .code,
            "Denied"
        );
    }

    #[test]
    fn restrictive_role_gate_rechecks_current_role_even_when_general_posting_and_receipt_exist() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let s = e
            .execute(
                &owner,
                &req(
                    "space",
                    "spaces.create",
                    None,
                    json!({"name":"restricted dispatch"}),
                ),
            )
            .unwrap();
        let sid = s["space"]["id"].as_str().unwrap();
        let admin = s["administrator"]["credential"].as_str().unwrap();
        let i = e
            .execute(
                admin,
                &req(
                    "invite",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["task_decomposer","implementer"]}),
                ),
            )
            .unwrap();
        let worker = e
            .execute(
                i["invite_token"].as_str().unwrap(),
                &req(
                    "join",
                    "members.register",
                    Some(sid),
                    json!({"name":"worker"}),
                ),
            )
            .unwrap();
        let token = worker["credential"].as_str().unwrap();
        let create = req(
            "child",
            "topics.create",
            Some(sid),
            json!({"title":"child","body":"acceptance","kind":"task"}),
        );
        e.execute_requiring_role(token, &create, "task_decomposer")
            .unwrap();
        e.execute(
            admin,
            &req(
                "revoke",
                "members.set_roles",
                Some(sid),
                json!({"member_id":worker["id"],"roles":["implementer"]}),
            ),
        )
        .unwrap();
        assert_eq!(
            e.execute_requiring_role(token, &create, "task_decomposer")
                .unwrap_err()
                .code,
            "Denied"
        );
        let ordinary = req(
            "ordinary",
            "topics.create",
            Some(sid),
            json!({"title":"ordinary","body":"still allowed","kind":"task"}),
        );
        e.execute(token, &ordinary).unwrap();
        assert_eq!(
            e.execute_requiring_role(token, &ordinary, "task_decomposer")
                .unwrap_err()
                .code,
            "Denied"
        );
    }

    #[test]
    fn endpoint_dispatch_gate_rechecks_revoked_grant_before_replaying_forum_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path()).unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let s = e
            .execute(
                &owner,
                &req(
                    "space",
                    "spaces.create",
                    None,
                    json!({"name":"endpoint dispatch"}),
                ),
            )
            .unwrap();
        let sid = s["space"]["id"].as_str().unwrap();
        let admin = s["administrator"]["credential"].as_str().unwrap();
        let i = e
            .execute(
                admin,
                &req(
                    "invite",
                    "members.invite",
                    Some(sid),
                    json!({"roles":["implementer"]}),
                ),
            )
            .unwrap();
        let worker = e
            .execute(
                i["invite_token"].as_str().unwrap(),
                &req(
                    "join",
                    "members.register",
                    Some(sid),
                    json!({"name":"worker"}),
                ),
            )
            .unwrap();
        let token = worker["credential"].as_str().unwrap();
        let endpoint = e
            .execute(
                admin,
                &req(
                    "endpoint",
                    "endpoints.register",
                    Some(sid),
                    json!({"name":"source","kind":"github"}),
                ),
            )
            .unwrap();
        let eid = endpoint["id"].as_str().unwrap();
        e.execute(
            admin,
            &req(
                "grant",
                "endpoints.grant",
                Some(sid),
                json!({"endpoint_id":eid,"member_id":worker["id"],"operations":["observe"]}),
            ),
        )
        .unwrap();
        let create = req(
            "source-topic",
            "topics.create",
            Some(sid),
            json!({"title":"snapshot","body":"source","kind":"question"}),
        );
        e.execute_requiring_endpoint(token, &create, eid, "observe")
            .unwrap();
        e.execute(
            admin,
            &req(
                "revoke",
                "endpoints.grant",
                Some(sid),
                json!({"endpoint_id":eid,"member_id":worker["id"],"operations":[]}),
            ),
        )
        .unwrap();
        assert_eq!(
            e.execute_requiring_endpoint(token, &create, eid, "observe")
                .unwrap_err()
                .code,
            "Denied"
        );
        assert!(e.execute(token, &create).is_ok());
    }

    #[test]
    fn heartbeat_renewal_refreshes_latest_attachment_even_when_pulse_id_is_reused() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let time = Arc::new(AtomicU64::new(100000));
        let c = time.clone();
        let e = Engine::open_with_clock(dir.path(), Arc::new(move || c.load(Ordering::SeqCst)))
            .unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let s = e
            .execute(
                &owner,
                &req("space", "spaces.create", None, json!({"name":"pulse"})),
            )
            .unwrap();
        let sid = s["space"]["id"].as_str().unwrap();
        let admin = s["administrator"]["credential"].as_str().unwrap();
        let initial = req("initial", "members.heartbeat", Some(sid), json!({}));
        let attachment = e.execute(admin, &initial).unwrap();
        assert_eq!(
            e.execute(admin, &initial).unwrap()["attachment_id"],
            attachment["attachment_id"]
        );
        let renewal = req(
            "pulse",
            "members.heartbeat",
            Some(sid),
            json!({"attachment_id":attachment["attachment_id"]}),
        );
        time.store(101000, Ordering::SeqCst);
        e.execute(admin, &renewal).unwrap();
        time.store(102000, Ordering::SeqCst);
        let latest = e.execute(admin, &renewal).unwrap();
        assert_eq!(latest["last_seen"], 102000);
        assert_eq!(latest["heartbeat_seconds"], 30);
        let who = e
            .execute(admin, &req("who", "identity.current", Some(sid), json!({})))
            .unwrap();
        assert_eq!(who["attachments"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn heartbeat_renewal_does_not_rewrite_the_forum_snapshot() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let time = Arc::new(AtomicU64::new(100000));
        let c = time.clone();
        let e = Engine::open_with_clock(dir.path(), Arc::new(move || c.load(Ordering::SeqCst)))
            .unwrap();
        let owner = std::fs::read_to_string(owner_token_path(dir.path())).unwrap();
        let s = e
            .execute(
                &owner,
                &req(
                    "space",
                    "spaces.create",
                    None,
                    json!({"name":"bounded pulse write"}),
                ),
            )
            .unwrap();
        let sid = s["space"]["id"].as_str().unwrap();
        let admin = s["administrator"]["credential"].as_str().unwrap();
        e.execute(
            admin,
            &req(
                "large",
                "topics.create",
                Some(sid),
                json!({"title":"existing discussion","body":"x".repeat(65536),"kind":"plan"}),
            ),
        )
        .unwrap();
        let a = e
            .execute(
                admin,
                &req("initial", "members.heartbeat", Some(sid), json!({})),
            )
            .unwrap();
        let wal = dir.path().join("v2.sqlite3-wal");
        let before = std::fs::metadata(&wal).unwrap().len();
        time.store(101000, Ordering::SeqCst);
        e.execute(
            admin,
            &req(
                "pulse",
                "members.heartbeat",
                Some(sid),
                json!({"attachment_id":a["attachment_id"]}),
            ),
        )
        .unwrap();
        let after = std::fs::metadata(&wal).unwrap().len();
        assert!(
            after.saturating_sub(before) < 32768,
            "one attachment pulse must write bounded row pages, not existing forum content"
        );
    }
}
