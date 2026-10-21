//! Explicit GitHub sources via gh CLI. External writes require repository opt-in.
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepositoryPolicy {
    pub repository: String,
    #[serde(default)]
    pub allow_write_back: bool,
}
#[derive(Clone, Debug)]
pub struct SourceConfig {
    pub ledger_path: PathBuf,
    pub repositories: Vec<RepositoryPolicy>,
    pub gh_program: PathBuf,
    pub gh_prefix_args: Vec<String>,
    pub timeout: Duration,
}
impl SourceConfig {
    pub fn new(ledger_path: PathBuf) -> Self {
        Self {
            ledger_path,
            repositories: vec![],
            gh_program: "gh".into(),
            gh_prefix_args: vec![],
            timeout: Duration::from_secs(30),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Issue,
    PullRequest,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceAuthor {
    pub id: u64,
    pub login: String,
    pub node_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub source_id: String,
    pub node_id: String,
    pub repository: String,
    pub number: u64,
    pub kind: SourceKind,
    pub title: String,
    pub body: String,
    pub state: String,
    pub url: String,
    pub author: Option<SourceAuthor>,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IngestReport {
    pub snapshots: Vec<SourceSnapshot>,
    pub changed: Vec<String>,
    pub unchanged: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteState {
    Unknown,
    Confirmed,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WriteReceipt {
    pub repository: String,
    pub number: u64,
    pub request_id: String,
    pub state: WriteState,
    pub comment_id: Option<u64>,
    pub url: Option<String>,
    pub author: Option<SourceAuthor>,
}
#[derive(Clone)]
pub struct SourceAdapter {
    config: SourceConfig,
    db: Arc<Mutex<Connection>>,
    authorization: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    cancel: Option<Arc<AtomicBool>>,
}
impl SourceAdapter {
    pub(crate) fn with_cancellation(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }
    pub(crate) fn with_authorization_guard(
        mut self,
        guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Self {
        self.authorization = Some(guard);
        self
    }
    pub fn reconcile_comment(
        &self,
        repository: &str,
        number: u64,
        request_id: &str,
    ) -> Result<WriteReceipt, String> {
        let repository = self.repository(repository)?;
        let (body,marker,receipt):(String,String,String)=self.db.lock().unwrap().query_row("SELECT body,marker,receipt FROM writes WHERE repository=?1 AND number=?2 AND request_id=?3",params![repository,number,request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(failure)?.ok_or("RequestNotFound: no durable external-write intent")?;
        let receipt: WriteReceipt = serde_json::from_str(&receipt).map_err(failure)?;
        if receipt.state == WriteState::Confirmed {
            return Ok(receipt);
        }
        let rendered = format!("{body}\n\n{marker}");
        let comments = self.read_pages(&format!(
            "repos/{repository}/issues/{number}/comments?per_page=100"
        ))?;
        let matching: Vec<_> = comments
            .iter()
            .filter(|c| c["body"].as_str() == Some(&rendered))
            .collect();
        if matching.len() > 1 {
            return Err("UncertainWrite: multiple remote comments match the intent; human reconciliation required".into());
        }
        if let Some(comment) = matching.first() {
            let confirmed = confirmed_comment(&receipt, &rendered, comment)?;
            self.save_receipt(&confirmed)?;
            return Ok(confirmed);
        }
        // Absence, edit, deletion, eventual consistency or visibility limits do
        // not establish that a previous network mutation never happened.
        Ok(receipt)
    }
    pub fn post_comment(
        &self,
        repository: &str,
        number: u64,
        request_id: &str,
        body: &str,
    ) -> Result<WriteReceipt, String> {
        let repository = self.repository(repository)?;
        if !self
            .config
            .repositories
            .iter()
            .any(|p| p.repository == repository && p.allow_write_back)
        {
            return Err(
                "WriteBackDisabled: repository has no explicit external-write grant".into(),
            );
        }
        if number == 0
            || number > i64::MAX as u64
            || request_id.trim().is_empty()
            || request_id.len() > 200
            || body.trim().is_empty()
            || body.len() > 60000
        {
            return Err("invalid comment target, request id, or body (60000 byte limit)".into());
        }
        let marker = format!("<!-- linkstart-write:{} -->", uuid::Uuid::new_v4());
        let mut receipt = WriteReceipt {
            repository: repository.clone(),
            number,
            request_id: request_id.into(),
            state: WriteState::Unknown,
            comment_id: None,
            url: None,
            author: None,
        };
        {
            let mut db = self.db.lock().unwrap();
            let transaction = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(failure)?;
            let previous:Option<(String,String)>=transaction.query_row("SELECT body,receipt FROM writes WHERE repository=?1 AND number=?2 AND request_id=?3",params![repository,number,request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(failure)?;
            if let Some((old_body, old_receipt)) = previous {
                if old_body != body {
                    return Err(
                        "RequestConflict: request id already names different comment content"
                            .into(),
                    );
                }
                let previous: WriteReceipt = serde_json::from_str(&old_receipt).map_err(failure)?;
                return if previous.state == WriteState::Confirmed {
                    Ok(previous)
                } else {
                    Err("UncertainWrite: request may already have reached GitHub; reconcile without repeating POST".into())
                };
            }
            transaction.execute("INSERT INTO writes(repository,number,request_id,body,marker,receipt) VALUES(?1,?2,?3,?4,?5,?6)",params![repository,number,request_id,body,marker,serde_json::to_string(&receipt).map_err(failure)?]).map_err(failure)?;
            transaction.commit().map_err(failure)?;
        }
        // The intent is durable before the external effect. Any process or
        // response failure leaves Unknown, including a crash after remote commit.
        let rendered = format!("{body}\n\n{marker}");
        let input = serde_json::to_vec(&serde_json::json!({"body":rendered})).map_err(failure)?;
        let endpoint = format!("repos/{repository}/issues/{number}/comments");
        let output = match self.run_gh(
            &[
                "api",
                &endpoint,
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "--input",
                "-",
            ],
            Some(input),
        ) {
            Ok(v) => v,
            Err(_) => return Ok(receipt),
        };
        if let Ok(comment) = serde_json::from_slice::<Value>(&output) {
            if let Ok(confirmed) = confirmed_comment(&receipt, &rendered, &comment) {
                receipt = confirmed;
                self.save_receipt(&receipt)?;
            }
        }
        Ok(receipt)
    }
    fn save_receipt(&self, receipt: &WriteReceipt) -> Result<(), String> {
        self.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE writes SET receipt=?1 WHERE repository=?2 AND number=?3 AND request_id=?4",
                params![
                    serde_json::to_string(receipt).map_err(failure)?,
                    receipt.repository,
                    receipt.number,
                    receipt.request_id
                ],
            )
            .map_err(failure)?;
        Ok(())
    }
    pub fn snapshots(&self, repository: &str) -> Result<Vec<SourceSnapshot>, String> {
        let repository = self.repository(repository)?;
        let db = self.db.lock().unwrap();
        let mut statement = db
            .prepare("SELECT payload FROM sources WHERE repository=?1 ORDER BY source_id")
            .map_err(failure)?;
        let values = statement
            .query_map([repository], |r| r.get::<_, String>(0))
            .map_err(failure)?;
        values
            .map(|v| serde_json::from_str(&v.map_err(failure)?).map_err(failure))
            .collect()
    }
    pub fn new(mut config: SourceConfig) -> Result<Self, String> {
        if config.timeout.is_zero() || config.timeout > Duration::from_secs(300) {
            return Err("source process timeout must be within 1ns..300s".into());
        }
        let mut seen = std::collections::HashSet::new();
        for p in &mut config.repositories {
            p.repository = normalize_repository(&p.repository)?;
            if !seen.insert(p.repository.clone()) {
                return Err("duplicate GitHub repository policies are ambiguous".into());
            }
        }
        let db = Connection::open(&config.ledger_path).map_err(failure)?;
        db.busy_timeout(Duration::from_secs(2)).map_err(failure)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS sources(source_id TEXT PRIMARY KEY,repository TEXT NOT NULL,fingerprint TEXT NOT NULL,payload TEXT NOT NULL); CREATE INDEX IF NOT EXISTS sources_repository ON sources(repository); CREATE TABLE IF NOT EXISTS writes(repository TEXT NOT NULL,number INTEGER NOT NULL,request_id TEXT NOT NULL,body TEXT NOT NULL,marker TEXT NOT NULL,receipt TEXT NOT NULL,PRIMARY KEY(repository,number,request_id));").map_err(failure)?;
        Ok(Self {
            config,
            db: Arc::new(Mutex::new(db)),
            authorization: None,
            cancel: None,
        })
    }
    fn repository(&self, repository: &str) -> Result<String, String> {
        let repository = normalize_repository(repository)?;
        if !self
            .config
            .repositories
            .iter()
            .any(|p| p.repository == repository)
        {
            return Err("GitHub repository is not an explicitly configured source".into());
        }
        Ok(repository)
    }
    fn read_pages(&self, endpoint: &str) -> Result<Vec<Value>, String> {
        let output = self.run_gh(
            &[
                "api",
                endpoint,
                "--hostname",
                "github.com",
                "--method",
                "GET",
                "--paginate",
                "--slurp",
            ],
            None,
        )?;
        let pages: Vec<Vec<Value>> = serde_json::from_slice(&output)
            .map_err(|e| format!("invalid gh paginated JSON: {e}"))?;
        Ok(pages.into_iter().flatten().collect())
    }
    fn run_gh(&self, args: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>, String> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Acquire))
        {
            return Err("Cancelled: source process was not launched".into());
        }
        if let Some(guard) = &self.authorization {
            guard()?;
        }
        let mut command = Command::new(&self.config.gh_program);
        command.args(&self.config.gh_prefix_args).args(args);
        command
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_HOST", "github.com");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let output = run_bounded_command(
            &mut command,
            input,
            self.config.timeout,
            8 * 1024 * 1024,
            self.cancel.as_deref(),
        )?;
        if !output.status.success() {
            return Err(format!("gh request failed with {:?}", output.status.code()));
        }
        Ok(output.stdout)
    }
    pub fn ingest_repository(&self, repository: &str) -> Result<IngestReport, String> {
        let repository = self.repository(repository)?;
        let mut snapshots = vec![];
        for (path, kind) in [
            ("issues", SourceKind::Issue),
            ("pulls", SourceKind::PullRequest),
        ] {
            for item in
                self.read_pages(&format!("repos/{repository}/{path}?state=all&per_page=100"))?
            {
                if kind == SourceKind::Issue && item.get("pull_request").is_some() {
                    continue;
                }
                let node_id = text(&item, "node_id")?;
                snapshots.push(SourceSnapshot {
                    source_id: format!("github.com:node:{node_id}"),
                    node_id,
                    repository: repository.clone(),
                    number: item["number"]
                        .as_u64()
                        .ok_or("GitHub source lacks number")?,
                    kind: kind.clone(),
                    title: text(&item, "title")?,
                    body: item["body"].as_str().unwrap_or_default().into(),
                    state: text(&item, "state")?,
                    url: text(&item, "html_url")?,
                    author: if item["user"].is_null() {
                        None
                    } else {
                        Some(
                            serde_json::from_value(item["user"].clone())
                                .map_err(|e| format!("invalid source author: {e}"))?,
                        )
                    },
                    created_at: text(&item, "created_at")?,
                    updated_at: text(&item, "updated_at")?,
                });
            }
        }
        let mut db = self.db.lock().unwrap();
        let transaction = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failure)?;
        let mut changed = vec![];
        let mut unchanged = 0;
        for snapshot in &snapshots {
            let payload = serde_json::to_string(snapshot).map_err(failure)?;
            let fingerprint = format!("{:x}", Sha256::digest(payload.as_bytes()));
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT fingerprint FROM sources WHERE source_id=?1",
                    [&snapshot.source_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(failure)?;
            if previous.as_deref() == Some(&fingerprint) {
                unchanged += 1;
                continue;
            }
            transaction.execute("INSERT INTO sources(source_id,repository,fingerprint,payload) VALUES(?1,?2,?3,?4) ON CONFLICT(source_id) DO UPDATE SET repository=excluded.repository,fingerprint=excluded.fingerprint,payload=excluded.payload",params![snapshot.source_id,repository,fingerprint,payload]).map_err(failure)?;
            changed.push(snapshot.source_id.clone());
        }
        transaction.commit().map_err(failure)?;
        Ok(IngestReport {
            changed,
            snapshots,
            unchanged,
        })
    }
}
/// Process boundary shared by source sync and bounded provider jobs. Neither
/// timeout nor cancellation establishes whether an external effect happened.
pub(crate) struct BoundedProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
pub(crate) fn run_bounded_command(
    command: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    max_bytes: usize,
    cancel: Option<&AtomicBool>,
) -> Result<BoundedProcessOutput, String> {
    let deadline = Instant::now() + timeout;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("ProcessNotStarted: {e}"))?;
    let stdout = read_limited(
        child
            .stdout
            .take()
            .ok_or("ProcessPipe: stdout unavailable")?,
        max_bytes,
    );
    let stderr = read_limited(
        child
            .stderr
            .take()
            .ok_or("ProcessPipe: stderr unavailable")?,
        max_bytes.min(65536),
    );
    let input_result = if let Some(input) = input {
        let mut stdin = child.stdin.take().ok_or("ProcessPipe: stdin unavailable")?;
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = tx.send(stdin.write_all(&input).map_err(failure));
        });
        Some(rx)
    } else {
        None
    };
    let mut out = None;
    let mut err = None;
    let status = loop {
        if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
            stop_child(&mut child);
            return Err("Cancelled: owned process interrupted; effects may be unknown".into());
        }
        if Instant::now() >= deadline {
            stop_child(&mut child);
            return Err(
                "Timeout: owned process exceeded its budget; effects may be unknown".into(),
            );
        }
        if out.is_none() {
            if let Ok(v) = stdout.try_recv() {
                out = Some(v);
            }
        }
        if err.is_none() {
            if let Ok(v) = stderr.try_recv() {
                err = Some(v);
            }
        }
        if out.as_ref().is_some_and(Result::is_err) || err.as_ref().is_some_and(Result::is_err) {
            stop_child(&mut child);
            return Err("OutputLimit: process output exceeded the retained byte limit".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                stop_child(&mut child);
                return Err(format!("ProcessWait: {e}"));
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let stdout = match out {
        Some(v) => v,
        None => stdout
            .recv_timeout(remaining())
            .map_err(|_| "Timeout: stdout pipe did not close")?,
    }?;
    let stderr = match err {
        Some(v) => v,
        None => stderr
            .recv_timeout(remaining())
            .map_err(|_| "Timeout: stderr pipe did not close")?,
    }?;
    if let Some(input_result) = input_result {
        input_result
            .recv_timeout(remaining())
            .map_err(|_| "Timeout: stdin delivery did not complete")??;
    }
    Ok(BoundedProcessOutput {
        status,
        stdout,
        stderr,
    })
}
fn read_limited(
    mut pipe: impl Read + Send + 'static,
    limit: usize,
) -> mpsc::Receiver<Result<Vec<u8>, String>> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = vec![];
        let mut buffer = [0; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(Ok(bytes));
                    return;
                }
                Ok(n) => {
                    if bytes.len().saturating_add(n) > limit {
                        let _ = tx.send(Err("OutputLimit: native pipe overflow".into()));
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("ProcessPipe: {e}")));
                    return;
                }
            }
        }
    });
    rx
}
fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn failure(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn confirmed_comment(
    receipt: &WriteReceipt,
    rendered: &str,
    comment: &Value,
) -> Result<WriteReceipt, String> {
    if comment["body"].as_str() != Some(rendered) {
        return Err("remote comment content does not match write intent".into());
    }
    let id = comment["id"]
        .as_u64()
        .filter(|i| *i > 0)
        .ok_or("remote comment lacks id")?;
    let url = text(comment, "html_url")?;
    let lower = url.to_ascii_lowercase();
    let prefix = format!("https://github.com/{}/", receipt.repository);
    if lower != format!("{prefix}issues/{}#issuecomment-{id}", receipt.number)
        && lower != format!("{prefix}pull/{}#issuecomment-{id}", receipt.number)
    {
        return Err("remote comment locator does not match repository and target".into());
    }
    let mut confirmed = receipt.clone();
    confirmed.state = WriteState::Confirmed;
    confirmed.comment_id = Some(id);
    confirmed.url = Some(url);
    confirmed.author = Some(serde_json::from_value(comment["user"].clone()).map_err(failure)?);
    Ok(confirmed)
}
fn text(item: &Value, key: &str) -> Result<String, String> {
    item[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| format!("GitHub source lacks {key}"))
}
/// Shared validated GitHub owner/name identity; display configuration remains unchanged.
pub(crate) fn normalize_repository(repository: &str) -> Result<String, String> {
    let parts: Vec<_> = repository.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || !p
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        })
    {
        return Err("repository must be explicit owner/name on github.com".into());
    }
    Ok(repository.to_ascii_lowercase())
}

#[cfg(all(test, windows))]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    pub(crate) struct Fixture {
        pub(crate) dir: TempDir,
        pub(crate) config: SourceConfig,
    }
    impl Fixture {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("fake-gh.ps1");
            let base = dir.path().to_string_lossy().replace('\'', "''");
            std::fs::write(&script,format!(r#"
$ErrorActionPreference='Stop'
$base='{base}'
[Console]::OutputEncoding=New-Object System.Text.UTF8Encoding($false)
[Console]::InputEncoding=New-Object System.Text.UTF8Encoding($false)
$entry=ConvertTo-Json -Compress -InputObject @($args)
[IO.File]::AppendAllText((Join-Path $base 'calls.jsonl'),$entry+"`n",(New-Object System.Text.UTF8Encoding($false)))
$endpoint=$args[1]
if(Test-Path (Join-Path $base 'slow-read')){{Start-Sleep -Milliseconds 2000}}
if($endpoint -match '/comments'){{
    $methodIndex=[Array]::IndexOf($args,'--method')
    $method=$args[$methodIndex+1]
    if($method -eq 'POST'){{
        $payload=ConvertFrom-Json ([Console]::In.ReadToEnd())
        $comment=@{{id=901;node_id='IC_REMOTE';html_url='https://github.com/acme/project/issues/1#issuecomment-901';body=$payload.body;user=@{{id=22;login='writer';node_id='U_WRITER'}}}}
        $raw=ConvertTo-Json -Compress -Depth 10 $comment
        [IO.File]::WriteAllText((Join-Path $base 'remote-comment.json'),$raw,(New-Object System.Text.UTF8Encoding($false)))
        if(Test-Path (Join-Path $base 'fail-after-write')){{exit 9}}
        [Console]::Out.Write($raw)
    }}else{{
        if(Test-Path (Join-Path $base 'remote-comment.json')){{[Console]::Out.Write('[['+[IO.File]::ReadAllText((Join-Path $base 'remote-comment.json'))+']]')}}else{{[Console]::Out.Write('[[]]')}}
    }}
    exit 0
}}
if($endpoint -match '/issues\?'){{$file='issues.json'}}
elseif($endpoint -match '/pulls\?'){{$file='pulls.json'}}
else{{throw ('unexpected fake endpoint '+$endpoint)}}
[Console]::Out.Write([IO.File]::ReadAllText((Join-Path $base $file)))
"#)).unwrap();
            let item = |node: &str, n: u64, title: &str| json!({"node_id":node,"number":n,"title":title,"body":"Original text","state":"open","html_url":format!("https://github.com/acme/project/issues/{n}"),"user":{"id":11,"login":"author","node_id":"U_AUTHOR"},"created_at":"2026-10-20T00:00:00Z","updated_at":"2026-10-21T00:00:00Z"});
            let issue = item("I_STABLE", 1, "Issue snapshot");
            let mut issue_pr = item("I_PR_WRONG_NODE", 2, "PR through issue listing");
            issue_pr["pull_request"] =
                json!({"url":"https://api.github.com/repos/acme/project/pulls/2"});
            std::fs::write(
                dir.path().join("issues.json"),
                serde_json::to_vec(&json!([[issue, issue_pr]])).unwrap(),
            )
            .unwrap();
            std::fs::write(
                dir.path().join("pulls.json"),
                serde_json::to_vec(&json!([[item("PR_STABLE", 2, "PR snapshot")]])).unwrap(),
            )
            .unwrap();
            let mut config = SourceConfig::new(dir.path().join("source.sqlite"));
            config.repositories = vec![RepositoryPolicy {
                repository: "acme/project".into(),
                allow_write_back: false,
            }];
            config.gh_program = "pwsh.exe".into();
            config.gh_prefix_args = vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-File".into(),
                script.to_string_lossy().into_owned(),
            ];
            Self { dir, config }
        }
        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            std::fs::read_to_string(self.dir.path().join("calls.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect()
        }
    }
    #[test]
    fn explicit_repository_ingests_distinct_issue_and_pr_with_source_metadata() {
        let fixture = Fixture::new();
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        let report = adapter
            .ingest_repository("Acme/Project")
            .expect("configured GitHub source can be ingested through process boundary");
        assert_eq!(report.snapshots.len(), 2);
        assert_eq!(report.changed.len(), 2);
        let issue = report
            .snapshots
            .iter()
            .find(|s| s.kind == SourceKind::Issue)
            .unwrap();
        assert_eq!(issue.source_id, "github.com:node:I_STABLE");
        assert_eq!(issue.author.as_ref().unwrap().login, "author");
        assert_eq!(issue.updated_at, "2026-10-21T00:00:00Z");
        let pr = report
            .snapshots
            .iter()
            .find(|s| s.kind == SourceKind::PullRequest)
            .unwrap();
        assert_eq!(pr.source_id, "github.com:node:PR_STABLE");
        assert_eq!(fixture.calls().len(), 2);
    }
    #[test]
    fn snapshots_dedupe_across_restart_and_changed_issue_keeps_identity() {
        let fixture = Fixture::new();
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        adapter.ingest_repository("acme/project").unwrap();
        drop(adapter);
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        let repeated = adapter.ingest_repository("acme/project").unwrap();
        assert!(
            repeated.changed.is_empty(),
            "unchanged source snapshots must not create duplicate imports"
        );
        assert_eq!(repeated.unchanged, 2);
        let path = fixture.dir.path().join("issues.json");
        let mut issues: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        issues[0][0]["title"] = json!("Revised issue");
        issues[0][0]["updated_at"] = json!("2026-10-21T01:00:00Z");
        std::fs::write(path, serde_json::to_vec(&issues).unwrap()).unwrap();
        let changed = adapter.ingest_repository("acme/project").unwrap();
        assert_eq!(changed.changed, vec!["github.com:node:I_STABLE"]);
        let persisted = adapter.snapshots("acme/project").unwrap();
        assert_eq!(persisted.len(), 2);
        assert_eq!(
            persisted
                .iter()
                .find(|s| s.source_id == "github.com:node:I_STABLE")
                .unwrap()
                .title,
            "Revised issue"
        );
    }
    #[test]
    fn external_write_is_disabled_by_default_and_repository_scoped_before_process_start() {
        let fixture = Fixture::new();
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        let error = adapter
            .post_comment("acme/project", 1, "write-1", "Comment")
            .unwrap_err();
        assert!(
            error.contains("WriteBackDisabled"),
            "must report disabled capability: {error}"
        );
        assert!(adapter
            .post_comment("other/private", 1, "write-1", "Comment")
            .is_err());
        assert!(adapter.ingest_repository("other/private").is_err());
        assert!(
            fixture.calls().is_empty(),
            "unauthorized repository must never reach gh"
        );
        let policy: RepositoryPolicy =
            serde_json::from_value(json!({"repository":"acme/project"})).unwrap();
        assert!(!policy.allow_write_back);
    }
    #[test]
    fn explicit_comment_write_is_durable_idempotent_and_conflicts_do_not_reach_gh() {
        let mut fixture = Fixture::new();
        fixture.config.repositories[0].allow_write_back = true;
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        let receipt = adapter
            .post_comment(
                "acme/project",
                1,
                "request-a",
                "Line one\n繁體中文 \"quoted\" $literal `code`",
            )
            .expect("explicit repository write grant permits one comment");
        assert_eq!(receipt.state, WriteState::Confirmed);
        assert_eq!(receipt.comment_id, Some(901));
        assert_eq!(receipt.author.as_ref().unwrap().login, "writer");
        drop(adapter);
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        assert_eq!(
            adapter
                .post_comment(
                    "acme/project",
                    1,
                    "request-a",
                    "Line one\n繁體中文 \"quoted\" $literal `code`"
                )
                .unwrap(),
            receipt
        );
        assert!(adapter
            .post_comment("acme/project", 1, "request-a", "Changed comment")
            .unwrap_err()
            .contains("RequestConflict"));
        assert_eq!(
            fixture.calls().len(),
            1,
            "durable replay must not POST twice"
        );
        let raw: Value = serde_json::from_slice(
            &std::fs::read(fixture.dir.path().join("remote-comment.json")).unwrap(),
        )
        .unwrap();
        assert!(raw["body"]
            .as_str()
            .unwrap()
            .starts_with("Line one\n繁體中文 \"quoted\" $literal `code`\n\n<!-- linkstart-write:"));
        assert!(fixture.calls()[0].iter().any(|a| a == "--input"));
    }
    #[test]
    fn canonical_repository_does_not_confirm_foreign_or_malicious_remote_locators() {
        for locator in [
            "https://github.com/acme/other/issues/1#issuecomment-901",
            "https://github.com.evil/acme/project/issues/1#issuecomment-901",
            "https://github.com/acme/project/../other/issues/1#issuecomment-901",
        ] {
            let mut fixture = Fixture::new();
            fixture.config.repositories[0].repository = "Acme/Project".into();
            fixture.config.repositories[0].allow_write_back = true;
            let script = fixture.dir.path().join("fake-gh.ps1");
            let text = std::fs::read_to_string(&script).unwrap().replace(
                "https://github.com/acme/project/issues/1#issuecomment-901",
                locator,
            );
            std::fs::write(script, text).unwrap();
            let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
            let receipt = adapter
                .post_comment("ACME/PROJECT", 1, "foreign-locator", "approved content")
                .unwrap();
            assert_eq!(
                receipt.state,
                WriteState::Unknown,
                "foreign locator must not become a confirmed receipt: {locator}"
            );
            assert_eq!(receipt.repository, "acme/project");
            assert!(adapter
                .reconcile_comment("Acme/Project", 1, "foreign-locator")
                .unwrap_err()
                .contains("remote comment locator does not match repository and target"));
            assert!(adapter
                .post_comment("acme/project", 1, "foreign-locator", "approved content")
                .unwrap_err()
                .contains("UncertainWrite"));
            assert_eq!(
                fixture
                    .calls()
                    .iter()
                    .filter(|args| args.iter().any(|a| a == "POST"))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn unknown_write_never_reposts_and_reconciliation_requires_matching_remote_evidence() {
        let mut fixture = Fixture::new();
        fixture.config.repositories[0].allow_write_back = true;
        std::fs::write(fixture.dir.path().join("fail-after-write"), b"yes").unwrap();
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        let unknown = adapter
            .post_comment("acme/project", 1, "uncertain-a", "One external effect")
            .unwrap();
        assert_eq!(unknown.state, WriteState::Unknown);
        assert!(adapter
            .post_comment("acme/project", 1, "uncertain-a", "One external effect")
            .unwrap_err()
            .contains("UncertainWrite"));
        assert_eq!(fixture.calls().len(), 1);
        drop(adapter);
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        let remote_path = fixture.dir.path().join("remote-comment.json");
        let saved = std::fs::read(&remote_path).unwrap();
        std::fs::remove_file(&remote_path).unwrap();
        let unresolved = adapter
            .reconcile_comment("acme/project", 1, "uncertain-a")
            .expect("read-only reconciliation is available after restart");
        assert_eq!(
            unresolved.state,
            WriteState::Unknown,
            "absence is not proof that POST was never accepted"
        );
        assert!(adapter
            .post_comment("acme/project", 1, "uncertain-a", "One external effect")
            .is_err());
        std::fs::write(remote_path, saved).unwrap();
        let resolved = adapter
            .reconcile_comment("acme/project", 1, "uncertain-a")
            .unwrap();
        assert_eq!(resolved.state, WriteState::Confirmed);
        assert_eq!(resolved.comment_id, Some(901));
        assert_eq!(
            adapter
                .post_comment("acme/project", 1, "uncertain-a", "One external effect")
                .unwrap(),
            resolved
        );
        let calls = fixture.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls
                .iter()
                .filter(|args| args.iter().any(|a| a == "POST"))
                .count(),
            1,
            "reconciliation may only GET"
        );
    }
    #[test]
    fn hung_source_process_respects_budget_and_does_not_publish_partial_snapshots() {
        let mut fixture = Fixture::new();
        fixture.config.timeout = Duration::from_millis(150);
        std::fs::write(fixture.dir.path().join("slow-read"), b"yes").unwrap();
        let adapter = SourceAdapter::new(fixture.config.clone()).unwrap();
        let start = std::time::Instant::now();
        let result = adapter.ingest_repository("acme/project");
        assert!(
            result.is_err(),
            "hung gh must fail within the configured process budget"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "timeout must bound caller wait"
        );
        assert!(
            adapter.snapshots("acme/project").unwrap().is_empty(),
            "partial fetches must not commit source state"
        );
    }
}
