use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use clap::{Args, Parser, Subcommand};
use futures_util::{stream as future_stream, Stream};
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

const PROTOCOL: &str = "v1";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_BODY: usize = 64 * 1024;

#[derive(Parser)]
#[command(name="linkstart", version=VERSION, about="LinkStart v1 Preview 本機 Runtime", disable_help_subcommand=true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Help(HelpArgs),
    Version(JsonFlag),
    Status(StateArgs),
    Ps(StateArgs),
    Doctor(StateArgs),
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Connections {
        #[command(subcommand)]
        command: ListCommand,
    },
    Apps {
        #[command(subcommand)]
        command: ListCommand,
    },
    Monitor {
        #[command(subcommand)]
        command: MonitorCommand,
    },
    Feedback {
        #[command(subcommand)]
        command: FeedbackCommand,
    },
}
#[derive(Args)]
struct JsonFlag {
    #[arg(long)]
    json: bool,
}
#[derive(Args)]
struct HelpArgs {
    command: Option<String>,
    #[arg(long)]
    json: bool,
}
#[derive(Args, Clone)]
struct StateArgs {
    #[arg(long, default_value_os_t=default_state_dir())]
    state_dir: PathBuf,
    #[arg(long)]
    json: bool,
}
#[derive(Subcommand)]
enum DaemonCommand {
    Start(StartArgs),
    Stop(StateArgs),
    Restart(StartArgs),
    #[command(hide = true)]
    Run(StartArgs),
}
#[derive(Args, Clone)]
struct StartArgs {
    #[command(flatten)]
    state: StateArgs,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 45831)]
    port: u16,
}
#[derive(Deserialize)]
struct DaemonRecord {
    pid: u32,
    address: String,
    version: String,
    #[serde(rename = "protocolMajor")]
    protocol_major: String,
}
#[derive(Subcommand)]
enum ListCommand {
    List(StateArgs),
}
#[derive(Subcommand)]
enum MonitorCommand {
    Wait(MonitorWaitArgs),
    Ack(MonitorAckArgs),
}
#[derive(Args)]
struct MonitorWaitArgs {
    #[command(flatten)]
    state: StateArgs,
    #[arg(long)]
    connection_id: String,
    #[arg(long)]
    capability: String,
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
}
#[derive(Args)]
struct MonitorAckArgs {
    #[command(flatten)]
    state: StateArgs,
    #[arg(long)]
    connection_id: String,
    #[arg(long)]
    capability: String,
    #[arg(long)]
    event_id: String,
}
#[derive(Subcommand)]
enum FeedbackCommand {
    Send(FeedbackSendArgs),
}
#[derive(Args)]
struct FeedbackSendArgs {
    #[command(flatten)]
    state: StateArgs,
    #[arg(long)]
    connection_id: String,
    #[arg(long)]
    capability: String,
    #[arg(long)]
    app_instance_id: String,
    #[arg(long)]
    feedback_id: String,
    #[arg(long)]
    payload: String,
    #[arg(long)]
    in_reply_to_event_id: Option<String>,
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("LINKSTART_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("linkstart"))
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn token() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|v| format!("{v:02x}")).collect()
}
fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn json_line(v: Value) {
    println!("{}", serde_json::to_string(&v).unwrap());
}
fn main_help_json() -> Value {
    json!({
        "name":"linkstart",
        "version":VERSION,
        "protocolMajor":PROTOCOL,
        "commands":["version","status","ps","doctor","daemon start|stop|restart","connections list","apps list","monitor wait|ack","feedback send"],
        "operations":{
            "attach":{"endpoint":"POST /v1/connections","requiredFields":["protocolMajor"],"optionalFields":["callsign"],"responseIdentities":["connectionId","callsign","capability","status"]},
            "register":{"endpoint":"POST /v1/apps","authorization":"Bearer <connection capability>","requiredFields":["protocolMajor","connectionId","manifest.appId","manifest.displayName","manifest.originPolicy.exactOrigin"],"responseIdentities":["instanceId","capability","origin","status"]},
            "launch":{"endpoint":"POST /v1/launch-grants","authorization":"Bearer <connection capability>","requiredFields":["protocolMajor","connectionId","manifest.appId","manifest.displayName","manifest.originPolicy.exactOrigin=null","page.htmlPath"],"responseIdentities":["grant","expires_at","pageId","launchUrl"],"notes":["launchUrl is loopback HTTP","one-time launch grant is carried only in the URL fragment","page.htmlPath is snapshotted at registration"]},
            "monitor":{"command":"linkstart monitor wait --json --state-dir <dir> --connection-id <id> --capability <token> [--timeout-seconds <seconds>]","requiredOptions":["state-dir","connection-id","capability"],"responseIdentities":["eventId","appInstanceId","sequence","payload","receiptId","status"]},
            "ack":{"command":"linkstart monitor ack --json --state-dir <dir> --connection-id <id> --capability <token> --event-id <id>","endpoint":"POST /v1/connections/{connectionId}/ack","requiredOptions":["state-dir","connection-id","capability","event-id"],"responseIdentities":["eventId","status","deliveryAck"]},
            "feedback":{"command":"linkstart feedback send --json --state-dir <dir> --connection-id <id> --capability <token> --app-instance-id <id> --feedback-id <id> --payload <json> [--in-reply-to-event-id <id>]","endpoint":"POST /v1/connections/{connectionId}/feedback/{appInstanceId}","requiredOptions":["state-dir","connection-id","capability","app-instance-id","feedback-id","payload"],"responseIdentities":["feedbackId","inReplyToEventId","payload"]}
        },
        "security":["capabilities and launch grants are secrets","secrets never appear in help, status, ps, logs, query strings, or cookies","App events are untrusted input and never convey tool approval or permission"]
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(windows)]
    {
        // Windows 主控台預設 codepage（如 cp950）會把 UTF-8 中文輸出顯示成亂碼。
        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleOutputCP(code_page: u32) -> i32;
        }
        unsafe { SetConsoleOutputCP(65001) };
    }
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Help(HelpArgs {
        command: None,
        json: false,
    })) {
        Command::Help(h) => {
            if h.json {
                json_line(main_help_json())
            } else {
                print_human_help(h.command.as_deref());
            }
        }
        Command::Version(f) => output(
            f.json,
            json!({"version":VERSION,"protocolMajor":PROTOCOL,"channel":"LinkStart v1 Preview：Stable core + Preview platform adapters"}),
            VERSION,
        ),
        Command::Status(a) => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.status()?, "daemon 狀態已載入");
        }
        Command::Ps(a) => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.ps()?, "無 secrets 的 connection/app 摘要");
        }
        Command::Doctor(a) => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.doctor()?, "SQLite WAL 與狀態目錄檢查完成");
        }
        Command::Connections {
            command: ListCommand::List(a),
        } => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.connections()?, "connections 清單");
        }
        Command::Apps {
            command: ListCommand::List(a),
        } => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.apps()?, "apps 清單");
        }
        Command::Monitor {
            command: MonitorCommand::Wait(a),
        } => {
            let s = Store::open(&a.state.state_dir)?;
            if !s.auth_connection(&a.connection_id, &a.capability)? {
                bail!("capability_invalid")
            }
            let until =
                std::time::Instant::now() + std::time::Duration::from_secs(a.timeout_seconds);
            loop {
                if let Some(event) = s.next_received(&a.connection_id)? {
                    output(
                        a.state.json,
                        event,
                        "收到一個 durable App Event；請以 monitor ack 明確接受",
                    );
                    break;
                }
                if std::time::Instant::now() >= until {
                    output(
                        a.state.json,
                        json!({"status":"timeout"}),
                        "等待逾時，請重新 arm monitor wait",
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        Command::Monitor {
            command: MonitorCommand::Ack(a),
        } => {
            let s = Store::open(&a.state.state_dir)?;
            if !s.auth_connection(&a.connection_id, &a.capability)? {
                bail!("capability_invalid")
            }
            output(
                a.state.json,
                s.ack_event(&a.connection_id, &a.event_id)?,
                "已記錄 Delivery Ack；不代表模型完成或工具核准",
            );
        }
        Command::Feedback {
            command: FeedbackCommand::Send(a),
        } => {
            let s = Store::open(&a.state.state_dir)?;
            if !s.auth_connection(&a.connection_id, &a.capability)? {
                bail!("capability_invalid")
            }
            let payload = canonical(
                serde_json::from_str(&a.payload).context("feedback payload must be JSON")?,
            );
            output(
                a.state.json,
                s.insert_feedback(
                    &a.connection_id,
                    &a.app_instance_id,
                    &a.feedback_id,
                    a.in_reply_to_event_id.as_deref(),
                    payload,
                )?,
                "Agent Feedback 已 durable 寫入",
            );
        }
        Command::Daemon {
            command: DaemonCommand::Start(a),
        } => {
            daemon_control(&a, start_daemon(&a), "daemon 已啟動或重用")?;
        }
        Command::Daemon {
            command: DaemonCommand::Restart(a),
        } => {
            let result = (|| -> Result<Value> {
                let stopped = stop_daemon(&a.state.state_dir)?;
                let mut result = start_daemon(&a)?;
                result["stoppedBeforeRestart"] = json!(stopped);
                Ok(result)
            })();
            daemon_control(&a, result, "daemon 已重啟")?;
        }
        Command::Daemon {
            command: DaemonCommand::Run(a),
        } => serve(a).await?,
        Command::Daemon {
            command: DaemonCommand::Stop(a),
        } => {
            let result = stop_daemon(&a.state_dir).map(|stopped| json!({"stopped":stopped}));
            let start = StartArgs {
                state: a,
                host: "127.0.0.1".into(),
                port: 45831,
            };
            daemon_control(&start, result, "daemon 已停止或沒有可停止的 daemon")?;
        }
    };
    Ok(())
}
fn daemon_control(a: &StartArgs, result: Result<Value>, human: &str) -> Result<()> {
    match result {
        Ok(value) => {
            output(a.state.json, value, human);
            Ok(())
        }
        Err(error) => {
            if a.state.json {
                json_line(json!({"state":"failed","error":error.to_string()}));
            }
            Err(error)
        }
    }
}
fn start_daemon(a: &StartArgs) -> Result<Value> {
    if a.host != "127.0.0.1" {
        bail!("loopback_only: host must be 127.0.0.1")
    }
    let record_path = a.state.state_dir.join("daemon.json");
    if let Ok(bytes) = fs::read(&record_path) {
        let record: DaemonRecord =
            serde_json::from_slice(&bytes).context("invalid daemon discovery record")?;
        if process_alive(record.pid) {
            if record.version != VERSION || record.protocol_major != PROTOCOL {
                bail!("daemon_version_or_protocol_conflict: explicit drain/restart required")
            }
            return Ok(
                json!({"state":"reused","pid":record.pid,"address":record.address,"version":VERSION,"protocolMajor":PROTOCOL}),
            );
        }
    }
    fs::create_dir_all(&a.state.state_dir)?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "daemon",
            "run",
            "--state-dir",
            a.state.state_dir.to_str().context("non-utf8 state dir")?,
            "--host",
            &a.host,
            "--port",
            &a.port.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // 沒有 CREATE_NO_WINDOW 時，console-subsystem 的 daemon run 從無 console 的
        // parent（DETACHED_PROCESS）被生出來，Windows 會配一個常駐的空白 console 視窗。
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn()?;
    let address = format!("{}:{}", a.host, a.port);
    for _ in 0..100 {
        if child.try_wait()?.is_some() {
            bail!("daemon_replacement_exited")
        }
        let record = fs::read(&record_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<DaemonRecord>(&bytes).ok());
        if let Some(record) = record.filter(|record| {
            record.pid == child.id()
                && record.address == address
                && record.version == VERSION
                && record.protocol_major == PROTOCOL
        }) {
            if process_alive(record.pid) && health_ready(&address) {
                return Ok(
                    json!({"state":"started","pid":child.id(),"address":address,"version":VERSION,"protocolMajor":PROTOCOL}),
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("daemon_start_timeout")
}
fn health_ready(address: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(address) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(100)));
    let _ = stream.write_all(
        format!("GET /v1/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    );
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200")
        && response.contains("\"status\":\"ready\"")
}
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        FsPath::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(unix))]
    {
        pid != 0
    }
}
fn stop_daemon(state_dir: &FsPath) -> Result<bool> {
    let record_path = state_dir.join("daemon.json");
    let Ok(bytes) = fs::read(&record_path) else {
        return Ok(false);
    };
    let record: DaemonRecord =
        serde_json::from_slice(&bytes).context("invalid daemon discovery record")?;
    if record.protocol_major != PROTOCOL {
        bail!("protocol_mismatch: explicit compatible runtime required")
    }
    if process_alive(record.pid) {
        #[cfg(unix)]
        {
            let status = std::process::Command::new("kill")
                .args(["-TERM", &record.pid.to_string()])
                .status()?;
            if !status.success() {
                bail!("failed to stop recorded daemon")
            }
        }
        #[cfg(not(unix))]
        {
            bail!("daemon stop requires the platform current-user process adapter")
        }
    }
    for _ in 0..100 {
        if !process_alive(record.pid) && TcpStream::connect(&record.address).is_err() {
            let _ = fs::remove_file(record_path);
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("daemon_stop_timeout_or_listener_still_accepting")
}
fn output(as_json: bool, v: Value, human: &str) {
    if as_json {
        json_line(v)
    } else {
        println!("{human}")
    }
}
fn print_human_help(cmd: Option<&str>) {
    match cmd { Some(c)=>println!("linkstart {c}\n使用 `linkstart {c} --json` 取得機器可讀結果。"), None=>println!("LinkStart v1 Preview 本機 Runtime\n\n用法：linkstart <命令>\n\n命令：version、status、ps、doctor、daemon start|stop|restart、connections list、apps list、monitor wait|ack、feedback send\n\n所有狀態輸出都會遮蔽 capability。使用 `linkstart help --json` 取得機器可讀說明。") }
}

#[derive(Clone)]
struct AppState {
    store: Store,
    host: String,
}
#[derive(Clone, Serialize)]
struct Notification {
    cursor: i64,
    app_id: String,
    kind: String,
    data: Value,
}
#[derive(Clone)]
struct Store {
    db: Arc<Mutex<Connection>>,
    state_dir: PathBuf,
}
impl Store {
    fn open(dir: &FsPath) -> Result<Self> {
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
    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let c = self.db.lock().unwrap();
        f(&c)
    }
    fn status(&self) -> Result<Value> {
        self.with(|c|{let con:i64=c.query_row("SELECT count(*) FROM connections WHERE status='online'",[],|r|r.get(0))?;let apps:i64=c.query_row("SELECT count(*) FROM apps WHERE status='connected'",[],|r|r.get(0))?;let rec:i64=c.query_row("SELECT count(*) FROM events WHERE status='received'",[],|r|r.get(0))?;Ok(json!({"runtime":"linkstart","version":VERSION,"protocolMajor":PROTOCOL,"state":"ready","onlineConnections":con,"connectedApps":apps,"receivedEvents":rec,"stateDir":self.state_dir}))})
    }
    fn doctor(&self) -> Result<Value> {
        self.with(|c|{let wal:String=c.query_row("PRAGMA journal_mode",[],|r|r.get(0))?;let sync:i64=c.query_row("PRAGMA synchronous",[],|r|r.get(0))?;Ok(json!({"ok":wal.eq_ignore_ascii_case("wal") && sync>=2,"sqliteJournal":wal,"synchronous":"FULL","stateDir":self.state_dir}))})
    }
    fn connections(&self) -> Result<Value> {
        self.with(|c|{let mut q=c.prepare("SELECT id,callsign,status FROM connections ORDER BY created_at")?;let rows=q.query_map([],|r|Ok(json!({"connectionId":r.get::<_,String>(0)?,"callsign":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;Ok(json!({"connections":rows}))})
    }
    fn apps(&self) -> Result<Value> {
        self.with(|c|{let mut q=c.prepare("SELECT id,connection_id,origin,app_id,display_name,status FROM apps ORDER BY created_at")?;let rows=q.query_map([],|r|Ok(json!({"instanceId":r.get::<_,String>(0)?,"connectionId":r.get::<_,String>(1)?,"origin":r.get::<_,String>(2)?,"appId":r.get::<_,String>(3)?,"displayName":r.get::<_,String>(4)?,"status":r.get::<_,String>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;Ok(json!({"apps":rows}))})
    }
    fn ps(&self) -> Result<Value> {
        Ok(json!({
            "connections": self.connections()?["connections"].clone(),
            "apps": self.apps()?["apps"].clone(),
        }))
    }
    fn auth_connection(&self, id: &str, bearer: &str) -> Result<bool> {
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
    fn auth_app(&self, id: &str, bearer: &str) -> Result<Option<(String, String)>> {
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
    fn next_received(&self, connection_id: &str) -> Result<Option<Value>> {
        self.with(|c| Ok(c.query_row("SELECT e.id,e.app_id,e.sequence,e.payload,e.receipt_id FROM events e JOIN apps a ON a.id=e.app_id WHERE a.connection_id=? AND e.status='received' AND e.id=(SELECT e2.id FROM events e2 WHERE e2.app_id=e.app_id AND e2.status='received' ORDER BY e2.sequence LIMIT 1) ORDER BY e.created_at LIMIT 1", [connection_id], |r| { let payload: String=r.get(3)?; Ok(json!({"eventId":r.get::<_,String>(0)?,"appInstanceId":r.get::<_,String>(1)?,"sequence":r.get::<_,i64>(2)?,"payload":serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),"receiptId":r.get::<_,String>(4)?,"status":"received"})) }).optional()?))
    }
    fn ack_event(&self, connection_id: &str, event_id: &str) -> Result<Value> {
        self.with(|c| { let app: String=c.query_row("SELECT e.app_id FROM events e JOIN apps a ON a.id=e.app_id WHERE a.connection_id=? AND e.id=? AND e.status='received'",params![connection_id,event_id],|r|r.get(0)).optional()?.context("event_not_received")?; let first: Option<String>=c.query_row("SELECT id FROM events WHERE app_id=? AND status='received' ORDER BY sequence LIMIT 1",[&app],|r|r.get(0)).optional()?; if first.as_deref()!=Some(event_id){bail!("event_not_inflight")}; c.execute("UPDATE events SET status='delivered' WHERE app_id=? AND id=?",params![app,event_id])?; let out=json!({"eventId":event_id,"status":"delivered","deliveryAck":true}); Self::insert_notification(c,&app,"delivery_ack",&out)?; Ok(out) })
    }
    fn insert_feedback(
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
    fn insert_notification(c: &Connection, app_id: &str, kind: &str, data: &Value) -> Result<i64> {
        c.execute(
            "INSERT INTO notifications(app_id,kind,data,created_at) VALUES(?,?,?,?)",
            params![app_id, kind, serde_json::to_string(data)?, now()],
        )?;
        Ok(c.last_insert_rowid())
    }
    fn replay(&self, app_id: &str, cursor: i64) -> Result<Vec<Notification>> {
        self.with(|c| { let mut q=c.prepare("SELECT cursor,kind,data FROM notifications WHERE app_id=? AND cursor>? ORDER BY cursor")?; let rows=q.query_map(params![app_id,cursor],|r| Ok(Notification{cursor:r.get(0)?,app_id:app_id.to_string(),kind:r.get(1)?,data:serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(Value::Null)}))?.collect::<rusqlite::Result<Vec<_>>>()?; Ok(rows) })
    }
}

#[derive(Deserialize)]
struct RegisterConnection {
    #[serde(rename = "protocolMajor")]
    protocol_major: String,
    callsign: Option<String>,
}
#[derive(Serialize)]
struct ConnectionCreated {
    #[serde(rename = "connectionId")]
    connection_id: String,
    callsign: String,
    #[serde(rename = "capability")]
    capability: String,
    status: String,
}
#[derive(Deserialize)]
struct RegisterApp {
    #[serde(rename = "protocolMajor")]
    protocol_major: String,
    #[serde(rename = "connectionId")]
    connection_id: String,
    manifest: Manifest,
    page: Option<LaunchPage>,
}
#[derive(Deserialize)]
struct LaunchPage {
    #[serde(rename = "htmlPath")]
    html_path: PathBuf,
}
#[derive(Deserialize, Serialize)]
struct Manifest {
    #[serde(rename = "appId")]
    app_id: String,
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "originPolicy")]
    origin_policy: OriginPolicy,
}
#[derive(Deserialize, Serialize)]
struct OriginPolicy {
    #[serde(rename = "exactOrigin")]
    exact_origin: String,
}
#[derive(Serialize)]
struct AppCreated {
    #[serde(rename = "instanceId")]
    instance_id: String,
    capability: String,
    origin: String,
    status: String,
}
#[derive(Serialize)]
struct GrantCreated {
    grant: String,
    expires_at: i64,
    #[serde(rename = "pageId")]
    page_id: String,
    #[serde(rename = "launchUrl")]
    launch_url: String,
}
#[derive(Deserialize)]
struct EventInput {
    #[serde(rename = "eventId")]
    event_id: String,
    payload: Value,
}
#[derive(Deserialize)]
struct AckInput {
    #[serde(rename = "eventId")]
    event_id: String,
}
#[derive(Deserialize)]
struct FailInput {
    #[serde(rename = "eventId")]
    event_id: String,
    reason: String,
}
#[derive(Deserialize)]
struct FeedbackInput {
    #[serde(rename = "feedbackId")]
    feedback_id: String,
    #[serde(rename = "inReplyToEventId")]
    in_reply_to: Option<String>,
    payload: Value,
}
#[derive(Deserialize)]
struct StreamQuery {
    cursor: Option<i64>,
}
fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
fn reject(code: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::FORBIDDEN, Json(json!({"error":code})))
}
fn origin_gate(
    headers: &HeaderMap,
    expected_host: &str,
    expected_origin: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    if headers.get(header::HOST).and_then(|h| h.to_str().ok()) != Some(expected_host) {
        return Err(reject("host_mismatch"));
    };
    let actual_origin = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok());
    let daemon_same_origin = expected_origin == format!("http://{expected_host}");
    if actual_origin != Some(expected_origin) && !(daemon_same_origin && actual_origin.is_none()) {
        return Err(reject("origin_mismatch"));
    };
    Ok(())
}
fn protocol(v: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if v == PROTOCOL {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"protocol_mismatch","expected":PROTOCOL})),
        ))
    }
}
async fn serve(a: StartArgs) -> Result<()> {
    if a.host != "127.0.0.1" {
        bail!("loopback_only: host must be 127.0.0.1")
    }
    let record_path = a.state.state_dir.join("daemon.json");
    if let Ok(bytes) = fs::read(&record_path) {
        let record: DaemonRecord =
            serde_json::from_slice(&bytes).context("invalid daemon discovery record")?;
        if process_alive(record.pid) {
            if record.version != VERSION || record.protocol_major != PROTOCOL {
                bail!("daemon_version_or_protocol_conflict: explicit drain/restart required")
            }
            output(
                a.state.json,
                json!({"reused":true,"version":VERSION,"protocolMajor":PROTOCOL}),
                "相容 daemon 已在執行，安全重用",
            );
            return Ok(());
        }
    }
    let store = Store::open(&a.state.state_dir)?;
    let addr: SocketAddr = format!("{}:{}", a.host, a.port).parse()?;
    let host = addr.to_string();
    fs::write(
        &record_path,
        serde_json::to_vec(
            &json!({"address":host,"pid":std::process::id(),"version":VERSION,"protocolMajor":PROTOCOL}),
        )?,
    )?;
    let state = AppState { store, host };
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/connections", post(register_connection))
        .route("/v1/apps", post(register_app))
        .route("/v1/launch-grants", post(create_grant))
        .route("/v1/launch-pages/:locator", get(serve_launch_page))
        .route(
            "/v1/launch-grants/redeem",
            post(redeem_grant).options(preflight_redeem),
        )
        .route(
            "/v1/apps/:id/events",
            post(receive_event).options(preflight_app_post),
        )
        .route(
            "/v1/apps/:id/cancel",
            post(cancel_event).options(preflight_app_post),
        )
        .route(
            "/v1/apps/:id/stream",
            get(stream).options(preflight_app_get),
        )
        .route("/v1/connections/:id/ack", post(ack))
        .route("/v1/connections/:id/fail", post(fail))
        .route("/v1/connections/:id/feedback/:app", post(feedback))
        .layer(axum::middleware::from_fn_with_state(state.clone(), cors))
        .layer(RequestBodyLimitLayer::new(MAX_BODY))
        .with_state(state);
    println!("LinkStart daemon listening on http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}
async fn health(State(_s): State<AppState>) -> Json<Value> {
    Json(json!({"version":VERSION,"protocolMajor":PROTOCOL,"status":"ready"}))
}
async fn cors(
    State(s): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        == Some(&s.host);
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = req.uri().path();
    let app_id = path
        .strip_prefix("/v1/apps/")
        .and_then(|p| p.split('/').next());
    let allowed = host_ok
        && match (app_id, origin.as_deref()) {
            (Some(id), Some(o)) => app_origin(&s.store, id).ok().flatten().as_deref() == Some(o),
            (None, Some(o)) if path == "/v1/launch-grants/redeem" => {
                o == format!("http://{}", s.host)
            }
            _ => false,
        };
    let mut response = next.run(req).await;
    if allowed {
        if let Some(origin) = origin {
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_str(&origin).unwrap(),
            );
            response.headers_mut().insert(
                header::VARY,
                HeaderValue::from_static("Origin, Access-Control-Request-Private-Network"),
            );
        }
    }
    response
}
fn app_origin(store: &Store, id: &str) -> Result<Option<String>> {
    store.with(|c| {
        Ok(c.query_row(
            "SELECT origin FROM apps WHERE id=? AND status='connected'",
            [id],
            |r| r.get(0),
        )
        .optional()?)
    })
}
fn preflight_headers(
    headers: &HeaderMap,
    method: &str,
    required: &[&str],
) -> Result<HeaderMap, StatusCode> {
    if headers
        .get("access-control-request-method")
        .and_then(|v| v.to_str().ok())
        != Some(method)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let requested = headers
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|x| x.trim().to_ascii_lowercase())
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>();
    if requested
        .iter()
        .any(|x| x != "authorization" && x != "content-type")
        || required.iter().any(|x| !requested.iter().any(|v| v == x))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut out = HeaderMap::new();
    out.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_str(&required.join(", ")).unwrap(),
    );
    out.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_str(method).unwrap(),
    );
    out.insert(
        header::VARY,
        HeaderValue::from_static("Origin, Access-Control-Request-Private-Network"),
    );
    if headers
        .get("access-control-request-private-network")
        .and_then(|v| v.to_str().ok())
        == Some("true")
    {
        out.insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
    }
    Ok(out)
}
fn exact_preflight(
    headers: &HeaderMap,
    host: &str,
    origin: &str,
    method: &str,
    required: &[&str],
) -> Result<HeaderMap, StatusCode> {
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some(host)
        || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(origin)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut out = preflight_headers(headers, method, required)?;
    out.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(origin).map_err(|_| StatusCode::FORBIDDEN)?,
    );
    Ok(out)
}
async fn preflight_app_post(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap), StatusCode> {
    let origin = app_origin(&s.store, &id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::FORBIDDEN)?;
    Ok((
        StatusCode::NO_CONTENT,
        exact_preflight(
            &headers,
            &s.host,
            &origin,
            "POST",
            &["authorization", "content-type"],
        )?,
    ))
}
async fn preflight_app_get(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap), StatusCode> {
    let origin = app_origin(&s.store, &id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::FORBIDDEN)?;
    Ok((
        StatusCode::NO_CONTENT,
        exact_preflight(&headers, &s.host, &origin, "GET", &["authorization"])?,
    ))
}
async fn preflight_redeem(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap), StatusCode> {
    Ok((
        StatusCode::NO_CONTENT,
        exact_preflight(
            &headers,
            &s.host,
            &format!("http://{}", s.host),
            "POST",
            &["authorization"],
        )?,
    ))
}
async fn register_connection(
    State(s): State<AppState>,
    Json(x): Json<RegisterConnection>,
) -> Result<Json<ConnectionCreated>, (StatusCode, Json<Value>)> {
    protocol(&x.protocol_major)?;
    let id = Uuid::new_v4().to_string();
    let cap = token();
    let callsign = x.callsign.unwrap_or_else(|| format!("LS-{}", &id[..6]));
    s.store
        .with(|c| {
            c.execute(
                "INSERT INTO connections VALUES(?,?,?,?,?)",
                params![id, hash(&cap), callsign, "online", now()],
            )?;
            Ok(())
        })
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(ConnectionCreated {
        connection_id: id,
        callsign,
        capability: cap,
        status: "online".into(),
    }))
}
async fn register_app(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(x): Json<RegisterApp>,
) -> Result<Json<AppCreated>, (StatusCode, Json<Value>)> {
    protocol(&x.protocol_major)?;
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&x.connection_id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    };
    if x.manifest.origin_policy.exact_origin == "null" {
        return Err(reject("opaque_origin_requires_launch_grant"));
    };
    let id = Uuid::new_v4().to_string();
    let app_cap = token();
    s.store
        .with(|c| {
            c.execute(
                "INSERT INTO apps VALUES(?,?,?,?,?,?,?,?)",
                params![
                    id,
                    x.connection_id,
                    hash(&app_cap),
                    x.manifest.origin_policy.exact_origin,
                    x.manifest.app_id,
                    x.manifest.display_name,
                    "connected",
                    now()
                ],
            )?;
            Ok(())
        })
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(AppCreated {
        instance_id: id,
        capability: app_cap,
        origin: x.manifest.origin_policy.exact_origin,
        status: "connected".into(),
    }))
}
fn host_gate(headers: &HeaderMap, expected: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if headers.get(header::HOST).and_then(|h| h.to_str().ok()) == Some(expected) {
        Ok(())
    } else {
        Err(reject("host_mismatch"))
    }
}
async fn create_grant(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(x): Json<RegisterApp>,
) -> Result<Json<GrantCreated>, (StatusCode, Json<Value>)> {
    protocol(&x.protocol_major)?;
    host_gate(&headers, &s.host)?;
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&x.connection_id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    }
    if x.manifest.origin_policy.exact_origin != "null" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"launch_grant_requires_opaque_manifest"})),
        ));
    }
    let page = x.page.as_ref().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"launch_page_required"})),
        )
    })?;
    let html = fs::read_to_string(&page.html_path).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"launch_page_unreadable"})),
        )
    })?;
    if html.len() > 2 * 1024 * 1024 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error":"launch_page_too_large"})),
        ));
    }
    let grant = token();
    let expires = now() + 300;
    let grant_id = Uuid::new_v4().to_string();
    let page_id = Uuid::new_v4().to_string();
    let manifest = serde_json::to_string(&x.manifest).map_err(|_| reject("storage_error"))?;
    s.store
        .with(|c| {
            c.execute(
                "INSERT INTO launch_grants VALUES(?,?,?,?,?,NULL)",
                params![grant_id, hash(&grant), x.connection_id, manifest, expires],
            )?;
            c.execute(
                "INSERT INTO launch_pages VALUES(?,?,?,?)",
                params![page_id, grant_id, html, now()],
            )?;
            Ok(())
        })
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(GrantCreated {
        launch_url: format!(
            "http://{}/v1/launch-pages/{}#daemon=http%3A%2F%2F{}&grant={}",
            s.host, page_id, s.host, grant
        ),
        page_id,
        expires_at: expires,
        grant,
    }))
}
// LINK START 進場動畫；附加在文件尾端，因為 App html 不保證有 <body> 標籤。
// pointer-events:none —— 動畫絕不能攔截使用者對 App 的第一個互動。
// 隧道段等待 App 發出的 `linkstart:connected` 事件才收尾；等不到由 MAX_TUNNEL/MAXTOTAL 保險絲結束。
const LINK_START_BOOT: &str = r##"
<style>
#linkstart-boot{position:fixed;inset:0;z-index:2147483647;pointer-events:none;background:#05070d;overflow:hidden;opacity:1;transition:opacity .35s ease}
#linkstart-boot.lsboot-done{opacity:0}
#linkstart-boot canvas{position:absolute;inset:0;width:100%;height:100%}
#linkstart-boot .lsboot-text{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;font:italic 800 clamp(2.4rem,9vw,5.5rem)/1 "Segoe UI",system-ui,sans-serif;letter-spacing:.16em;color:#f2f6ff;text-shadow:0 0 14px rgba(90,200,255,.9),0 0 46px rgba(90,160,255,.5);opacity:0;transform:scale(.85)}
@media (prefers-reduced-motion: reduce){#linkstart-boot canvas{display:none}}
</style>
<div id="linkstart-boot" aria-hidden="true"><canvas></canvas><div class="lsboot-text">LINK START</div></div>
<script>
(function(){try{
var boot=document.getElementById("linkstart-boot");if(!boot)return;
var reduced=false;try{reduced=window.matchMedia("(prefers-reduced-motion: reduce)").matches}catch(_){}
var text=boot.querySelector(".lsboot-text");
var canvas=boot.querySelector("canvas");
var ctx=canvas&&canvas.getContext?canvas.getContext("2d"):null;
var T_TEXT=520,T_FLASH=700,MIN_TUNNEL=1900,MAX_TUNNEL=3600,BURST=420,MAXTOTAL=4600;
var start=null,raf=0,finished=false,connected=false,burstAt=null;
var colors=["#e3173c","#ff9d00","#ffe600","#7ed321","#00c9d7","#2f6bff","#8c2bd9","#e326b8","#15151a","#8a8a92"];
var wedges=[],rot=0;
function spawn(t){return{a:Math.random()*Math.PI*2,w:.05+Math.random()*.16,c:colors[Math.floor(Math.random()*colors.length)],born:t,life:260+Math.random()*520,r0:.02+Math.random()*.1}}
function onConnect(){connected=true}
window.addEventListener("linkstart:connected",onConnect);
function finish(){if(finished)return;finished=true;
if(raf)cancelAnimationFrame(raf);
window.removeEventListener("keydown",finish,true);
window.removeEventListener("linkstart:connected",onConnect);
boot.classList.add("lsboot-done");
setTimeout(function(){if(boot.parentNode)boot.parentNode.removeChild(boot)},400)}
function frame(now){
if(finished)return;
if(start===null)start=now;
var t=now-start,dpr=window.devicePixelRatio||1;
var w=canvas.width=canvas.clientWidth*dpr,h=canvas.height=canvas.clientHeight*dpr;
var cx=w/2,cy=h/2,m=Math.hypot(cx,cy)||1;
if(t<T_TEXT){
ctx.fillStyle="#05070d";ctx.fillRect(0,0,w,h);
text.style.opacity=Math.min(1,t/180);
text.style.transform="scale("+(.85+.15*Math.min(1,t/T_TEXT))+")";
}else if(t<T_FLASH){
text.style.opacity=Math.max(0,1-(t-T_TEXT)/120);
ctx.fillStyle="#fff";ctx.fillRect(0,0,w,h);
}else if(burstAt===null){
text.style.opacity=0;
if((connected&&t>MIN_TUNNEL)||t>MAX_TUNNEL){burstAt=t}
else{
var tt=t-T_FLASH;
ctx.fillStyle="#fff";ctx.fillRect(0,0,w,h);
rot+=.0016;
var target=Math.min(44,8+Math.floor(tt/60));
while(wedges.length<target)wedges.push(spawn(t));
for(var i=0;i<wedges.length;i++){var p=wedges[i];
if(t-p.born>p.life){wedges[i]=p=spawn(t)}
var a=p.a+rot,r0=p.r0*m,r1=1.6*m;
ctx.fillStyle=p.c;
ctx.beginPath();
ctx.moveTo(cx+Math.cos(a)*r0,cy+Math.sin(a)*r0);
ctx.lineTo(cx+Math.cos(a-p.w/2)*r1,cy+Math.sin(a-p.w/2)*r1);
ctx.lineTo(cx+Math.cos(a+p.w/2)*r1,cy+Math.sin(a+p.w/2)*r1);
ctx.closePath();ctx.fill()}
var vg=ctx.createRadialGradient(cx,cy,0,cx,cy,.3*m);
vg.addColorStop(0,"rgba(8,10,16,.5)");vg.addColorStop(1,"rgba(8,10,16,0)");
ctx.fillStyle=vg;ctx.fillRect(0,0,w,h);
}
}
if(burstAt!==null){
var tb=(t-burstAt)/BURST;
if(tb>=1){finish();return}
ctx.fillStyle="#fff";ctx.fillRect(0,0,w,h);
for(var j=0;j<90;j++){var ba=j/90*Math.PI*2+rot;
var g=ctx.createLinearGradient(cx,cy,cx+Math.cos(ba)*1.4*m,cy+Math.sin(ba)*1.4*m);
g.addColorStop(0,"rgba(120,225,255,"+.55*(1-tb)+")");
g.addColorStop(1,"rgba(160,235,255,0)");
ctx.strokeStyle=g;ctx.lineWidth=(1+j%4)*dpr;
ctx.beginPath();ctx.moveTo(cx,cy);ctx.lineTo(cx+Math.cos(ba)*1.4*m,cy+Math.sin(ba)*1.4*m);ctx.stroke()}
var core=ctx.createRadialGradient(cx,cy,0,cx,cy,(.25+.95*tb)*m);
core.addColorStop(0,"#fff");core.addColorStop(.55,"rgba(255,255,255,.92)");core.addColorStop(1,"rgba(220,246,255,0)");
ctx.fillStyle=core;ctx.fillRect(0,0,w,h);
}
raf=requestAnimationFrame(frame)}
window.addEventListener("keydown",finish,true);
if(reduced||!ctx){text.style.opacity=1;text.style.transform="scale(1)";setTimeout(finish,700)}
else{raf=requestAnimationFrame(frame)}
setTimeout(finish,MAXTOTAL)
}catch(_){}})();
</script>
"##;
fn with_link_start_boot(mut html: String) -> String {
    if html.contains("data-linkstart-boot=\"off\"") {
        return html;
    }
    html.push_str(LINK_START_BOOT);
    html
}
async fn serve_launch_page(
    State(s): State<AppState>,
    Path(locator): Path<String>,
    headers: HeaderMap,
) -> Result<(HeaderMap, String), (StatusCode, Json<Value>)> {
    host_gate(&headers, &s.host)?;
    let html = s
        .store
        .with(|c| {
            Ok(c.query_row(
                "SELECT p.html FROM launch_pages p JOIN launch_grants g ON g.id=p.grant_id WHERE p.locator=? AND g.expires_at>=?",
                params![locator, now()],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
        .map_err(|_| reject("storage_error"))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"launch_page_not_found"})),
            )
        })?;
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response_headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok((response_headers, with_link_start_boot(html)))
}
async fn redeem_grant(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AppCreated>, (StatusCode, Json<Value>)> {
    host_gate(&headers, &s.host)?;
    let launch_origin = format!("http://{}", s.host);
    if headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) != Some(&launch_origin) {
        return Err(reject("origin_mismatch"));
    }
    let grant = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    let app_cap = token();
    let app_id = Uuid::new_v4().to_string();
    let result=s.store.with(|c|{let row:Option<(String,String,i64,Option<i64>)>=c.query_row("SELECT connection_id,manifest,expires_at,redeemed_at FROM launch_grants WHERE capability_hash=?",[hash(grant)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;let(conn,manifest,expires,redeemed)=row.context("launch_grant_invalid")?;if redeemed.is_some()||expires<now(){bail!("launch_grant_expired_or_redeemed")};let m:Manifest=serde_json::from_str(&manifest)?;let status:String=c.query_row("SELECT status FROM connections WHERE id=?",[&conn],|r|r.get(0))?;if status!="online"{bail!("origin_offline")};c.execute("INSERT INTO apps VALUES(?,?,?,?,?,?,?,?)",params![app_id,conn,hash(&app_cap),launch_origin,m.app_id,m.display_name,"connected",now()])?;c.execute("UPDATE launch_grants SET redeemed_at=? WHERE capability_hash=? AND redeemed_at IS NULL",params![now(),hash(grant)])?;Ok(AppCreated{instance_id:app_id.clone(),capability:app_cap.clone(),origin:launch_origin.clone(),status:"connected".into()})}).map_err(|_|reject("launch_grant_invalid_or_expired"))?;
    Ok(Json(result))
}
async fn receive_event(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(x): Json<EventInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    let (_, origin) = s
        .store
        .auth_app(&id, cap)
        .map_err(|_| reject("storage_error"))?
        .ok_or_else(|| reject("capability_invalid"))?;
    origin_gate(&headers, &s.host, &origin)?;
    if x.event_id.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_event_id"})),
        ));
    };
    let payload = canonical(x.payload);
    let payload_s = serde_json::to_string(&payload).unwrap();
    let h = hash(&payload_s);
    let reply=s.store.with(|c|{let conn_status: String=c.query_row("SELECT c.status FROM connections c JOIN apps a ON a.connection_id=c.id WHERE a.id=?",[&id],|r|r.get(0))?;if conn_status!="online"{return Ok(json!({"error":"origin_offline"}))};let old:Option<(String,String,String,i64)>=c.query_row("SELECT receipt_id,payload_hash,status,sequence FROM events WHERE app_id=? AND id=?",params![id,x.event_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;if let Some((receipt,old_hash,status,seq))=old{return Ok(if old_hash==h {json!({"receiptId":receipt,"eventId":x.event_id,"sequence":seq,"status":status,"duplicate":true})}else{json!({"error":"event_id_conflict"})})};let seq:i64=c.query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM events WHERE app_id=?",[&id],|r|r.get(0))?;let receipt=format!("rcpt-{}",Uuid::new_v4());c.execute("INSERT INTO events VALUES(?,?,?,?,?,?,?,?,?)",params![x.event_id,id,seq,payload_s,h,"received",Option::<String>::None,receipt,now()])?;Ok(json!({"receiptId":receipt,"eventId":x.event_id,"sequence":seq,"status":"received","untrustedInput":true}))}).map_err(|_|reject("storage_error"))?;
    if reply.get("error").is_none() {
        s.store
            .with(|c| Store::insert_notification(c, &id, "receipt", &reply))
            .map_err(|_| reject("storage_error"))?;
    }
    Ok(Json(reply))
}
async fn cancel_event(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(x): Json<AckInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    let (_, o) = s
        .store
        .auth_app(&id, cap)
        .map_err(|_| reject("storage_error"))?
        .ok_or_else(|| reject("capability_invalid"))?;
    origin_gate(&headers, &s.host, &o)?;
    let n=s.store.with(|c|Ok(c.execute("UPDATE events SET status='cancelled' WHERE app_id=? AND id=? AND status='received'",params![id,x.event_id])?)).map_err(|_|reject("storage_error"))?;
    if n == 0 {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error":"event_not_received"})),
        ));
    };
    let d = json!({"eventId":x.event_id,"status":"cancelled"});
    s.store
        .with(|c| Store::insert_notification(c, &id, "event", &d))
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(d))
}
async fn ack(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(x): Json<AckInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    };
    let out = s.store.ack_event(&id, &x.event_id).map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":"event_not_received_or_not_inflight"})),
        )
    })?;
    Ok(Json(out))
}
async fn fail(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(x): Json<FailInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    };
    let n=s.store.with(|c|Ok(c.execute("UPDATE events SET status='failed',reason=? WHERE id=? AND status='received' AND app_id IN (SELECT id FROM apps WHERE connection_id=?)",params![x.reason,x.event_id,id])?)).map_err(|_|reject("storage_error"))?;
    if n == 0 {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error":"event_not_received"})),
        ));
    };
    Ok(Json(json!({"eventId":x.event_id,"status":"failed"})))
}
async fn feedback(
    State(s): State<AppState>,
    Path((id, app)): Path<(String, String)>,
    headers: HeaderMap,
    Json(x): Json<FeedbackInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    if !s
        .store
        .auth_connection(&id, cap)
        .map_err(|_| reject("storage_error"))?
    {
        return Err(reject("capability_invalid"));
    };
    let data = canonical(x.payload);
    let out = s
        .store
        .insert_feedback(&id, &app, &x.feedback_id, x.in_reply_to.as_deref(), data)
        .map_err(|_| reject("storage_error"))?;
    Ok(Json(out))
}
async fn stream(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    let cap = bearer(&headers).ok_or_else(|| reject("capability_missing"))?;
    let (_, o) = s
        .store
        .auth_app(&id, cap)
        .map_err(|_| reject("storage_error"))?
        .ok_or_else(|| reject("capability_invalid"))?;
    origin_gate(&headers, &s.host, &o)?;
    let cursor = query.cursor.unwrap_or(0);
    let replay = s
        .store
        .replay(&id, cursor)
        .map_err(|_| reject("storage_error"))?;
    let durable = future_stream::unfold(
        (s.store.clone(), id, cursor, VecDeque::from(replay)),
        |(store, app_id, mut cursor, mut pending)| async move {
            loop {
                if let Some(notification) = pending.pop_front() {
                    cursor = notification.cursor;
                    let event = Event::default()
                        .id(notification.cursor.to_string())
                        .event(notification.kind)
                        .data(notification.data.to_string());
                    return Some((Ok(event), (store, app_id, cursor, pending)));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                pending = VecDeque::from(store.replay(&app_id, cursor).unwrap_or_default());
            }
        },
    );
    let st: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(durable);
    let mut response = Sse::new(st)
        .keep_alive(KeepAlive::default())
        .into_response();
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(&o).unwrap(),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Origin"));
    Ok(response)
}
fn canonical(v: Value) -> Value {
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
