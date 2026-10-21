//! Trusted local launch installation and protected, immutable worker credentials.
//! HTTP may select an installed plan; it cannot construct an ApprovedLaunch.
use crate::runtime_codex_host::{
    EngineForumFacade, ExecutionProfile, HostScope, OwnedCodexRegistry, OwnedHostConfig,
    RuntimeToolFacade,
};
use crate::runtime_core::{ApiError, Engine};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
fn error(code: &str, message: &str) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
    }
}
fn internal(_: impl std::fmt::Display) -> ApiError {
    error("Internal", "launch policy storage unavailable")
}
fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 256 && !id.contains('\0')
}
fn valid_timeout(timeout: Duration) -> bool {
    timeout.as_millis() > 0
        && timeout.as_millis() <= u64::MAX as u128
        && timeout.subsec_nanos() % 1_000_000 == 0
        && Instant::now().checked_add(timeout).is_some()
}
const SCHEMA_V2: &str = "
CREATE TABLE worker_slots(space_id TEXT NOT NULL,endpoint_id TEXT NOT NULL,id TEXT NOT NULL,binding TEXT NOT NULL,credential TEXT NOT NULL,PRIMARY KEY(space_id,endpoint_id,id));
CREATE TABLE launch_plans(space_id TEXT NOT NULL,endpoint_id TEXT NOT NULL,id TEXT NOT NULL,revision INTEGER NOT NULL,revoked INTEGER NOT NULL DEFAULT 0,document TEXT NOT NULL,PRIMARY KEY(space_id,endpoint_id,id));";

/// Schema changes, row copying and version publication share one SQLite transaction.
/// Legacy JSON and credential bytes are preserved; migration never reauthorizes or repairs them.
fn migrate(db: &mut Connection) -> anyhow::Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: u64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    anyhow::ensure!(version <= 2, "unsupported launch policy schema version");
    if version == 2 {
        tx.prepare("SELECT space_id,endpoint_id,id,binding,credential FROM worker_slots LIMIT 0")?;
        tx.prepare(
            "SELECT space_id,endpoint_id,id,revision,revoked,document FROM launch_plans LIMIT 0",
        )?;
        tx.commit()?;
        return Ok(());
    }
    let tables: u64 = tx.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('worker_slots','launch_plans')", [], |r| r.get(0))?;
    if tables == 0 {
        tx.execute_batch(SCHEMA_V2)?;
    } else {
        anyhow::ensure!(tables == 2, "incomplete legacy launch policy schema");
        tx.execute_batch("ALTER TABLE worker_slots RENAME TO worker_slots_v1; ALTER TABLE launch_plans RENAME TO launch_plans_v1;")?;
        tx.execute_batch(SCHEMA_V2)?;
        {
            let mut statement = tx.prepare("SELECT id,binding,credential FROM worker_slots_v1")?;
            let rows = statement.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (id, encoded, credential) = row?;
                let b: WorkerBinding = serde_json::from_str(&encoded)
                    .map_err(|_| anyhow::anyhow!("invalid legacy worker binding"))?;
                anyhow::ensure!(
                    valid_id(&id)
                        && [
                            &b.space_id,
                            &b.endpoint_id,
                            &b.identity_id,
                            &b.attachment_id
                        ]
                        .iter()
                        .all(|id| valid_id(id))
                        && !credential.is_empty(),
                    "invalid legacy worker slot"
                );
                tx.execute(
                    "INSERT INTO worker_slots VALUES(?1,?2,?3,?4,?5)",
                    params![b.space_id, b.endpoint_id, id, encoded, credential],
                )?;
            }
        }
        {
            let mut statement =
                tx.prepare("SELECT id,revision,revoked,document FROM launch_plans_v1")?;
            let rows = statement.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (id, revision, revoked, encoded) = row?;
                let p: StoredPlan = serde_json::from_str(&encoded)
                    .map_err(|_| anyhow::anyhow!("invalid legacy launch document"))?;
                anyhow::ensure!(
                    valid_id(&id)
                        && valid_id(&p.binding.space_id)
                        && valid_id(&p.binding.endpoint_id)
                        && revision > 0
                        && [0, 1].contains(&revoked),
                    "invalid legacy launch row"
                );
                tx.execute(
                    "INSERT INTO launch_plans VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        p.binding.space_id,
                        p.binding.endpoint_id,
                        id,
                        revision,
                        revoked,
                        encoded
                    ],
                )?;
            }
        }
        tx.execute_batch("DROP TABLE worker_slots_v1; DROP TABLE launch_plans_v1;")?;
    }
    tx.execute_batch("PRAGMA user_version=2;")?;
    tx.commit()?;
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerBinding {
    pub space_id: String,
    pub endpoint_id: String,
    pub identity_id: String,
    pub attachment_id: String,
}
/// In-process trusted installation input, never a wire request.
pub struct TrustedLaunchPlan {
    pub binding: WorkerBinding,
    pub worker_slot: String,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub workspace: PathBuf,
    pub model: String,
    pub provider_version: String,
    pub capability_evidence: String,
    pub timeout: Duration,
    pub profile: ExecutionProfile,
}
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct StoredPlan {
    binding: WorkerBinding,
    worker_slot: String,
    executable: PathBuf,
    args: Vec<String>,
    workspace: PathBuf,
    model: String,
    provider_version: String,
    capability_evidence: String,
    timeout_ms: u64,
    profile: Value,
}
/// No Debug/Serialize/Deserialize: contains a private worker bearer.
pub struct ApprovedLaunch {
    config: OwnedHostConfig,
    credential: String,
    facade: Arc<dyn RuntimeToolFacade>,
    plan_id: String,
    revision: u64,
}
impl ApprovedLaunch {
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn profile(&self) -> &ExecutionProfile {
        &self.config.profile
    }
    pub(crate) fn into_parts(
        mut self,
        session_id: String,
        binding_generation: u64,
    ) -> (OwnedHostConfig, String, Arc<dyn RuntimeToolFacade>) {
        self.config.scope.session_id = session_id;
        self.config.scope.binding_generation = binding_generation;
        (self.config, self.credential, self.facade)
    }
}
pub struct LaunchPolicyStore {
    path: PathBuf,
    engine: Engine,
    registry: OwnedCodexRegistry,
}
impl LaunchPolicyStore {
    pub fn open(dir: &Path, engine: Engine, registry: OwnedCodexRegistry) -> anyhow::Result<Self> {
        anyhow::ensure!(
            dir.canonicalize()? == engine.private_storage_directory()?,
            "launch storage must match Engine"
        );
        crate::runtime_core::protect(dir)?;
        let path = dir.join("v2-launch-policy.sqlite");
        if path.exists() {
            crate::runtime_core::protect(&path)?;
        }
        let mut db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        // DELETE journal stays inside the protected parent, avoiding persistent WAL secrets.
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        migrate(&mut db)?;
        crate::runtime_core::protect(&path)?;
        Ok(Self {
            path,
            engine,
            registry,
        })
    }
    fn db(&self) -> Result<Connection, ApiError> {
        let db = Connection::open(&self.path).map_err(internal)?;
        db.busy_timeout(Duration::from_secs(5)).map_err(internal)?;
        Ok(db)
    }
    fn admin<T>(
        &self,
        actor: &str,
        b: &WorkerBinding,
        f: impl FnOnce() -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        self.engine.with_endpoint_authorization(
            actor,
            &b.space_id,
            &b.endpoint_id,
            "manage",
            |auth| {
                if auth["identity_id"] != "owner"
                    && !auth["identity"]["roles"]
                        .as_array()
                        .is_some_and(|r| r.iter().any(|r| r == "manager" || r == "human_admin"))
                {
                    return Err(error("Denied", "trusted administrator required"));
                }
                f()
            },
        )
    }
    fn worker(&self, b: &WorkerBinding, credential: &str) -> Result<(), ApiError> {
        self.engine.with_endpoint_authorization(
            credential,
            &b.space_id,
            &b.endpoint_id,
            "send",
            |auth| {
                if b.identity_id == "owner"
                    || auth["identity_id"] != b.identity_id
                    || !auth["identity"]["attachments"].as_array().is_some_and(|a| {
                        a.iter()
                            .any(|a| a["attachment_id"] == b.attachment_id && a["state"] == "alive")
                    })
                {
                    return Err(error("Denied", "exact live worker attachment required"));
                }
                Ok(())
            },
        )
    }
    fn slot(&self, id: &str, b: &WorkerBinding) -> Result<String, ApiError> {
        let row: Option<(String, String)> = self
            .db()?
            .query_row(
                "SELECT binding,credential FROM worker_slots WHERE space_id=?1 AND endpoint_id=?2 AND id=?3",
                params![b.space_id,b.endpoint_id,id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        let (binding, credential) = row.ok_or_else(|| {
            error(
                "WorkerCredentialRequired",
                "provision independent worker credential first",
            )
        })?;
        if serde_json::from_str::<WorkerBinding>(&binding).map_err(internal)? != *b {
            return Err(error("Denied", "worker slot linkage mismatch"));
        }
        self.worker(b, &credential).map_err(|_| {
            error(
                "WorkerCredentialRequired",
                "stored worker credential or attachment is no longer valid",
            )
        })?;
        Ok(credential)
    }
    /// Installs an already-provisioned credential; never invites, rotates or repairs it.
    pub fn provision_worker_slot(
        &self,
        actor: &str,
        id: &str,
        binding: WorkerBinding,
        credential: &str,
    ) -> Result<(), ApiError> {
        if !valid_id(id)
            || credential.is_empty()
            || [
                &binding.space_id,
                &binding.endpoint_id,
                &binding.identity_id,
                &binding.attachment_id,
            ]
            .iter()
            .any(|id| !valid_id(id))
        {
            return Err(error("Invalid", "nonempty slot and credential required"));
        }
        self.worker(&binding, credential)?;
        self.admin(actor, &binding, || {
            let mut db = self.db()?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let encoded = serde_json::to_string(&binding).map_err(internal)?;
            let old: Option<(String, String)> = tx
                .query_row(
                    "SELECT binding,credential FROM worker_slots WHERE space_id=?1 AND endpoint_id=?2 AND id=?3",
                    params![binding.space_id,binding.endpoint_id,id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(internal)?;
            if let Some((b, c)) = old {
                if b != encoded || c != credential {
                    return Err(error("Conflict", "existing worker slot is immutable"));
                }
                return Ok(());
            }
            tx.execute(
                "INSERT INTO worker_slots VALUES(?1,?2,?3,?4,?5)",
                params![binding.space_id,binding.endpoint_id,id, encoded, credential],
            )
            .map_err(internal)?;
            tx.commit().map_err(internal)
        })
    }
    pub fn install(
        &self,
        actor: &str,
        id: &str,
        expected_previous_revision: Option<u64>,
        plan: TrustedLaunchPlan,
    ) -> Result<u64, ApiError> {
        if !valid_id(id)
            || !valid_id(&plan.worker_slot)
            || plan.model.trim().is_empty()
            || plan.provider_version.trim().is_empty()
            || plan.capability_evidence.trim().is_empty()
            || !valid_timeout(plan.timeout)
            || plan.args.iter().any(|a| a.contains('\0'))
        {
            return Err(error("Invalid", "invalid trusted launch plan"));
        }
        let worker_credential = self.slot(&plan.worker_slot, &plan.binding)?;
        let executable = plan
            .executable
            .canonicalize()
            .map_err(|_| error("Invalid", "canonical executable required"))?;
        let workspace = plan
            .workspace
            .canonicalize()
            .map_err(|_| error("Invalid", "canonical workspace required"))?;
        if !executable.is_file() || !workspace.is_dir() {
            return Err(error(
                "Invalid",
                "executable file and workspace directory required",
            ));
        }
        let stored = StoredPlan {
            binding: plan.binding,
            worker_slot: plan.worker_slot,
            executable,
            args: plan.args,
            workspace,
            model: plan.model,
            provider_version: plan.provider_version,
            capability_evidence: plan.capability_evidence,
            timeout_ms: plan.timeout.as_millis() as u64,
            profile: serde_json::to_value(plan.profile).map_err(internal)?,
        };
        self.restore_profile(&stored)?;
        let encoded = serde_json::to_string(&stored).map_err(internal)?;
        if encoded.contains(&worker_credential) || encoded.contains(actor) {
            return Err(error(
                "Invalid",
                "credential must not appear in public plan or argv",
            ));
        }
        self.admin(actor, &stored.binding, || {
            let document = serde_json::to_string(&stored).map_err(internal)?;
            let mut db = self.db()?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(internal)?;
            let old: Option<(u64, bool, String)> = tx.query_row(
                "SELECT revision,revoked,document FROM launch_plans WHERE space_id=?1 AND endpoint_id=?2 AND id=?3", params![stored.binding.space_id,stored.binding.endpoint_id,id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ).optional().map_err(internal)?;
            if let Some((_, _, ref prior)) = old {
                let prior: StoredPlan = serde_json::from_str(prior).map_err(internal)?;
                if prior.binding.space_id != stored.binding.space_id || prior.binding.endpoint_id != stored.binding.endpoint_id {
                    return Err(error("Denied", "plan target is immutable"));
                }
            }
            let revision = match old {
                None if expected_previous_revision.is_none() => 1,
                Some((rev, false, ref doc)) if expected_previous_revision.is_none() && *doc == document => return Ok(rev),
                Some((rev, _, _)) if expected_previous_revision == Some(rev) => rev.checked_add(1)
                    .filter(|n| *n <= i64::MAX as u64)
                    .ok_or_else(|| error("Conflict", "revision exhausted"))?,
                _ => return Err(error("Conflict", "exact previous plan revision required")),
            };
            tx.execute(
                "INSERT INTO launch_plans VALUES(?1,?2,?3,?4,0,?5) ON CONFLICT(space_id,endpoint_id,id) DO UPDATE SET revision=excluded.revision,revoked=0,document=excluded.document",
                params![stored.binding.space_id,stored.binding.endpoint_id,id, revision, document],
            ).map_err(internal)?;
            tx.commit().map_err(internal)?;
            Ok(revision)
        })
    }
    fn restore_profile(&self, p: &StoredPlan) -> Result<ExecutionProfile, ApiError> {
        if p.profile == Value::String("ReadOnlyForum".into()) {
            return Ok(ExecutionProfile::ReadOnlyForum);
        }
        let approval = p
            .profile
            .as_object()
            .filter(|o| o.len() == 1)
            .and_then(|o| o.get("WorkspaceWrite"))
            .ok_or_else(|| error("Invalid", "invalid persisted profile"))?;
        if approval["codex_version"] != p.provider_version
            || approval["capability_evidence"] != p.capability_evidence
        {
            return Err(error("Invalid", "profile provider evidence mismatch"));
        }
        self.registry
            .restore_workspace_approval(
                &p.binding.space_id,
                &p.binding.endpoint_id,
                &p.workspace,
                approval,
            )
            .map(ExecutionProfile::WorkspaceWrite)
            .map_err(|e| error("Unsupported", &e.to_string()))
    }
    pub fn approve(
        &self,
        actor: &str,
        space: &str,
        endpoint: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<ApprovedLaunch, ApiError> {
        if !valid_id(id) {
            return Err(error("Invalid", "plan ID must be 1..=256 bytes"));
        }
        // Authorize before lookup, keeping target existence private.
        self.engine
            .with_endpoint_authorization(actor, space, endpoint, "manage", |_| Ok(()))?;
        let row: Option<(u64, bool, String)> = self
            .db()?
            .query_row(
                "SELECT revision,revoked,document FROM launch_plans WHERE space_id=?1 AND endpoint_id=?2 AND id=?3",
                params![space,endpoint,id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(internal)?;
        let (revision, revoked, document) =
            row.ok_or_else(|| error("LaunchApprovalRequired", "trusted launch plan required"))?;
        let p: StoredPlan = serde_json::from_str(&document)
            .map_err(|_| error("Invalid", "malformed persisted launch plan"))?;
        if p.binding.space_id != space || p.binding.endpoint_id != endpoint {
            return Err(error("Denied", "plan target mismatch"));
        }
        if revision != expected_revision || revoked {
            return Err(error("Conflict", "plan revision stale or revoked"));
        }
        if !valid_timeout(Duration::from_millis(p.timeout_ms))
            || !valid_id(&p.worker_slot)
            || p.model.trim().is_empty()
            || p.provider_version.trim().is_empty()
            || p.capability_evidence.trim().is_empty()
            || p.args.iter().any(|a| a.contains('\0'))
            || !p.executable.is_file()
            || !p.workspace.is_dir()
            || p.executable.canonicalize().map_err(internal)? != p.executable
            || p.workspace.canonicalize().map_err(internal)? != p.workspace
        {
            return Err(error(
                "Invalid",
                "persisted plan constraints no longer hold",
            ));
        }
        let credential = self.slot(&p.worker_slot, &p.binding)?;
        let profile = self.restore_profile(&p)?;
        if document.contains(&credential) {
            return Err(error(
                "Invalid",
                "credential present in persisted public plan",
            ));
        }
        let config = OwnedHostConfig {
            executable: p.executable,
            args: p.args,
            workspace: p.workspace,
            model: p.model,
            timeout: Duration::from_millis(p.timeout_ms),
            profile,
            scope: HostScope {
                identity_id: p.binding.identity_id,
                space_id: space.into(),
                endpoint_id: endpoint.into(),
                attachment_id: p.binding.attachment_id,
                session_id: String::new(),
                binding_generation: 0,
            },
        };
        // Check current actor once more after resolving worker and disk state.
        self.engine
            .with_endpoint_authorization(actor, space, endpoint, "manage", |_| {
                let current: Option<(u64, bool, String)> = self
                    .db()?
                    .query_row(
                        "SELECT revision,revoked,document FROM launch_plans WHERE space_id=?1 AND endpoint_id=?2 AND id=?3",
                        params![space,endpoint,id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(internal)?;
                if current != Some((revision, false, document.clone())) {
                    return Err(error("Conflict", "plan changed while resolving"));
                }
                Ok(())
            })?;
        Ok(ApprovedLaunch {
            config,
            facade: Arc::new(EngineForumFacade::new(
                self.engine.clone(),
                credential.clone(),
            )),
            credential,
            plan_id: id.into(),
            revision,
        })
    }
    /// Revocation advances the revision and cannot be undone by replaying installation.
    pub fn revoke(
        &self,
        actor: &str,
        space: &str,
        endpoint: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<u64, ApiError> {
        if !valid_id(id) {
            return Err(error("Invalid", "plan ID must be 1..=256 bytes"));
        }
        let b = WorkerBinding {
            space_id: space.into(),
            endpoint_id: endpoint.into(),
            identity_id: String::new(),
            attachment_id: String::new(),
        };
        self.admin(actor, &b, || {
            let db = self.db()?;
            let row: Option<String> = db.query_row("SELECT document FROM launch_plans WHERE space_id=?1 AND endpoint_id=?2 AND id=?3", params![space,endpoint,id], |r| r.get(0))
                .optional().map_err(internal)?;
            let p: StoredPlan = serde_json::from_str(&row.ok_or_else(|| error("LaunchApprovalRequired", "plan missing"))?).map_err(internal)?;
            if p.binding.space_id != space || p.binding.endpoint_id != endpoint {
                return Err(error("Denied", "plan target mismatch"));
            }
            let next = expected_revision.checked_add(1).filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| error("Conflict", "revision exhausted"))?;
            let changed = db.execute("UPDATE launch_plans SET revision=?1,revoked=1 WHERE space_id=?2 AND endpoint_id=?3 AND id=?4 AND revision=?5 AND revoked=0",
                params![next,space,endpoint,id,expected_revision]).map_err(internal)?;
            if changed != 1 {
                return Err(error("Conflict", "exact current revision required"));
            }
            Ok(next)
        })
    }
}
