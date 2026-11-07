//! Durable Agent Trial metadata.
//!
//! Trial records stay deliberately small. Candidate sessions are ordinary AoE
//! sessions; this store records their shared checkpoint and prompt, stable
//! contender identities, launch state, and comparable terminal facts. All disk
//! access is owned by the lazy TUI trial worker.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

pub const TRIAL_SCHEMA_VERSION: u32 = 1;
pub const MAX_TRIAL_PROMPT_BYTES: usize = 16 * 1024;
pub const MAX_TRIAL_CONTENDERS: usize = 8;
pub const MAX_VERIFICATION_COMMAND_BYTES: usize = 4096;
pub const DEFAULT_VERIFICATION_TIMEOUT_SECS: u64 = 600;
pub const MAX_VERIFICATION_TIMEOUT_SECS: u64 = 3600;

const TRIALS_SUBDIR: &str = "trials";
const MANIFESTS_SUBDIR: &str = "manifests";
const STORE_LOCK_FILE: &str = ".lock";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_MANIFESTS_LOADED: usize = 2048;
const MAX_PRIOR_ATTEMPTS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialState {
    Launching,
    Running,
    Partial,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContenderState {
    Pending,
    Creating,
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ContenderState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrialContenderSpec {
    pub id: String,
    pub tool: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationRecipe {
    pub command: String,
    pub timeout_secs: u64,
}

impl VerificationRecipe {
    pub fn new(command: impl Into<String>, timeout_secs: u64) -> Result<Self> {
        let command = command.into().trim().to_string();
        if command.is_empty() || command.len() > MAX_VERIFICATION_COMMAND_BYTES {
            bail!("verification command is empty or exceeds 4 KiB");
        }
        if !(1..=MAX_VERIFICATION_TIMEOUT_SECS).contains(&timeout_secs) {
            bail!(
                "verification timeout must be between 1 and {} seconds",
                MAX_VERIFICATION_TIMEOUT_SECS
            );
        }
        Ok(Self {
            command,
            timeout_secs,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    Passed,
    Failed,
    TimedOut,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationResult {
    pub state: VerificationState,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub observed_at_ms: u64,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrialAttemptRecord {
    pub state: ContenderState,
    pub session_id: Option<String>,
    pub started_at_ms: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub reported_tokens: Option<u64>,
    pub verification: Option<VerificationResult>,
    pub changed_files: Option<u32>,
    pub insertions: Option<u32>,
    pub deletions: Option<u32>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrialContenderRecord {
    pub spec: TrialContenderSpec,
    pub state: ContenderState,
    pub session_id: Option<String>,
    pub started_at_ms: Option<u64>,
    pub elapsed_ms: Option<u64>,
    /// Remains absent until an adapter reports real usage.
    pub reported_tokens: Option<u64>,
    #[serde(default)]
    pub verification: Option<VerificationResult>,
    #[serde(default)]
    pub changed_files: Option<u32>,
    #[serde(default)]
    pub insertions: Option<u32>,
    #[serde(default)]
    pub deletions: Option<u32>,
    #[serde(default)]
    pub prior_attempts: Vec<TrialAttemptRecord>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrialManifest {
    pub schema_version: u32,
    pub id: String,
    pub checkpoint_id: String,
    #[serde(default)]
    pub checkpoint_source_session_id: Option<String>,
    pub prompt: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub state: TrialState,
    #[serde(default)]
    pub verification_recipe: Option<VerificationRecipe>,
    pub contenders: Vec<TrialContenderRecord>,
    pub retained_winner_session_id: Option<String>,
}

impl TrialManifest {
    pub fn summary(&self) -> TrialSummary {
        summary(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrialCreateRequest {
    pub manifest: TrialManifest,
}

impl TrialCreateRequest {
    pub fn new(
        checkpoint_id: impl Into<String>,
        prompt: impl Into<String>,
        tools: impl IntoIterator<Item = String>,
        permission_mode: impl Into<String>,
    ) -> Result<Self> {
        let checkpoint_id = checkpoint_id.into();
        validate_id(&checkpoint_id, "checkpoint")?;
        let prompt = prompt.into().trim().to_string();
        if prompt.is_empty() {
            bail!("trial prompt cannot be empty");
        }
        if prompt.len() > MAX_TRIAL_PROMPT_BYTES {
            bail!("trial prompt exceeds the 16 KiB limit");
        }
        let tools: Vec<String> = tools
            .into_iter()
            .map(|tool| tool.trim().to_string())
            .filter(|tool| !tool.is_empty())
            .collect();
        if !(2..=MAX_TRIAL_CONTENDERS).contains(&tools.len()) {
            bail!(
                "a trial requires 2 through {} contenders",
                MAX_TRIAL_CONTENDERS
            );
        }
        let permission_mode = permission_mode.into();
        let contenders = tools
            .into_iter()
            .map(|tool| TrialContenderRecord {
                spec: TrialContenderSpec {
                    id: uuid::Uuid::new_v4().to_string(),
                    tool,
                    model: None,
                    effort: None,
                    permission_mode: permission_mode.clone(),
                },
                state: ContenderState::Pending,
                session_id: None,
                started_at_ms: None,
                elapsed_ms: None,
                reported_tokens: None,
                verification: None,
                changed_files: None,
                insertions: None,
                deletions: None,
                prior_attempts: Vec::new(),
                error: None,
            })
            .collect();
        let now = now_ms();
        Ok(Self {
            manifest: TrialManifest {
                schema_version: TRIAL_SCHEMA_VERSION,
                id: uuid::Uuid::new_v4().to_string(),
                checkpoint_id,
                checkpoint_source_session_id: None,
                prompt,
                created_at_ms: now,
                updated_at_ms: now,
                state: TrialState::Launching,
                verification_recipe: None,
                contenders,
                retained_winner_session_id: None,
            },
        })
    }

    pub fn trial_id(&self) -> &str {
        &self.manifest.id
    }

    pub fn with_verification_recipe(mut self, recipe: VerificationRecipe) -> Self {
        self.manifest.verification_recipe = Some(recipe);
        self
    }

    pub fn with_checkpoint_source_session(mut self, session_id: impl Into<String>) -> Self {
        self.manifest.checkpoint_source_session_id = Some(session_id.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrialUpdate {
    Creating,
    SessionStarting {
        session_id: String,
    },
    Running {
        started_at_ms: u64,
    },
    Completed {
        elapsed_ms: u64,
    },
    Verified {
        result: VerificationResult,
        changed_files: Option<u32>,
        insertions: Option<u32>,
        deletions: Option<u32>,
    },
    ReportedTokens {
        tokens: u64,
    },
    Failed {
        error: String,
    },
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrialSummary {
    pub id: String,
    pub checkpoint_id: String,
    pub checkpoint_source_session_id: Option<String>,
    pub prompt: String,
    pub state: TrialState,
    pub completed: usize,
    pub failed: usize,
    pub total: usize,
    pub session_ids: Vec<String>,
    pub contenders: Vec<TrialContenderSummary>,
    pub verification_recipe: Option<VerificationRecipe>,
    pub retained_winner_session_id: Option<String>,
    pub ranking: Vec<TrialRank>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrialContenderSummary {
    pub id: String,
    pub tool: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: String,
    pub state: ContenderState,
    pub session_id: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub reported_tokens: Option<u64>,
    pub started_at_ms: Option<u64>,
    pub verification: Option<VerificationResult>,
    pub changed_files: Option<u32>,
    pub insertions: Option<u32>,
    pub deletions: Option<u32>,
    pub prior_attempts: usize,
    pub retained: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrialRank {
    pub rank: usize,
    pub contender_id: String,
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationExecution {
    pub result: VerificationResult,
    pub changed_files: Option<u32>,
    pub insertions: Option<u32>,
    pub deletions: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrialLoadReport {
    pub trials: Vec<TrialSummary>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TrialStore {
    root: PathBuf,
}

impl TrialStore {
    pub fn open() -> Result<Self> {
        Self::at_root(crate::session::get_app_dir()?.join(TRIALS_SUBDIR))
    }

    pub fn at_root(root: PathBuf) -> Result<Self> {
        let store = Self { root };
        fs::create_dir_all(store.manifests_dir())?;
        Ok(store)
    }

    pub fn create(&self, request: TrialCreateRequest) -> Result<TrialManifest> {
        let _lock = self.exclusive_lock()?;
        validate_manifest(&request.manifest)?;
        let path = self.manifest_path(&request.manifest.id);
        if path.exists() {
            let existing = self.read_manifest_path(&path)?;
            if existing == request.manifest {
                return Ok(existing);
            }
            bail!("trial id already exists with different content");
        }
        self.write_manifest(&request.manifest)?;
        Ok(request.manifest)
    }

    pub fn inspect(&self, trial_id: &str) -> Result<TrialManifest> {
        validate_id(trial_id, "trial")?;
        self.read_manifest_path(&self.manifest_path(trial_id))
    }

    pub fn update(
        &self,
        trial_id: &str,
        contender_id: &str,
        update: TrialUpdate,
    ) -> Result<TrialManifest> {
        let _lock = self.exclusive_lock()?;
        let mut manifest = self.inspect(trial_id)?;
        let contender = manifest
            .contenders
            .iter_mut()
            .find(|contender| contender.spec.id == contender_id)
            .with_context(|| format!("trial contender does not exist: {contender_id}"))?;
        apply_contender_update(contender, update)?;
        manifest.updated_at_ms = now_ms();
        manifest.state = derive_state(&manifest.contenders);
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn retry(&self, trial_id: &str, contender_id: &str) -> Result<TrialManifest> {
        let _lock = self.exclusive_lock()?;
        let mut manifest = self.inspect(trial_id)?;
        let contender = manifest
            .contenders
            .iter_mut()
            .find(|contender| contender.spec.id == contender_id)
            .with_context(|| format!("trial contender does not exist: {contender_id}"))?;
        if !matches!(
            contender.state,
            ContenderState::Failed | ContenderState::Cancelled
        ) {
            bail!("only a failed or cancelled contender can be retried");
        }
        contender.prior_attempts.push(TrialAttemptRecord {
            state: contender.state,
            session_id: contender.session_id.take(),
            started_at_ms: contender.started_at_ms.take(),
            elapsed_ms: contender.elapsed_ms.take(),
            reported_tokens: contender.reported_tokens.take(),
            verification: contender.verification.take(),
            changed_files: contender.changed_files.take(),
            insertions: contender.insertions.take(),
            deletions: contender.deletions.take(),
            error: contender.error.take(),
        });
        contender.state = ContenderState::Pending;
        manifest.updated_at_ms = now_ms();
        manifest.state = derive_state(&manifest.contenders);
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn cancel(&self, trial_id: &str) -> Result<TrialManifest> {
        let _lock = self.exclusive_lock()?;
        let mut manifest = self.inspect(trial_id)?;
        for contender in &mut manifest.contenders {
            if !contender.state.is_terminal() {
                apply_contender_update(contender, TrialUpdate::Cancelled)?;
            }
        }
        manifest.updated_at_ms = now_ms();
        manifest.state = derive_state(&manifest.contenders);
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn retain_winner(&self, trial_id: &str, contender_id: &str) -> Result<TrialManifest> {
        let _lock = self.exclusive_lock()?;
        let mut manifest = self.inspect(trial_id)?;
        let contender = manifest
            .contenders
            .iter()
            .find(|contender| contender.spec.id == contender_id)
            .with_context(|| format!("trial contender does not exist: {contender_id}"))?;
        if contender.state != ContenderState::Completed {
            bail!("only a completed contender can be retained");
        }
        let session_id = contender
            .session_id
            .clone()
            .context("completed contender has no session")?;
        manifest.retained_winner_session_id = Some(session_id);
        manifest.updated_at_ms = now_ms();
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn load(&self) -> Result<TrialLoadReport> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(self.manifests_dir())? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(target: "trial.store", %error, "trial entry unreadable");
                    continue;
                }
            };
            if entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(UNIX_EPOCH);
                paths.push((modified, entry.path()));
            }
        }
        paths.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        let mut report = TrialLoadReport::default();
        if paths.len() > MAX_MANIFESTS_LOADED {
            report.errors.push(format!(
                "trial history exceeds the {}-manifest load limit",
                MAX_MANIFESTS_LOADED
            ));
            paths.truncate(MAX_MANIFESTS_LOADED);
        }
        for (_, path) in paths {
            match self.read_manifest_path(&path) {
                Ok(manifest) => report.trials.push(summary(&manifest)),
                Err(error) => report.errors.push(format!(
                    "{}: {error}",
                    path.file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_default()
                )),
            }
        }
        report
            .trials
            .sort_by_key(|trial| std::cmp::Reverse(trial.updated_at_ms));
        Ok(report)
    }

    fn write_manifest(&self, manifest: &TrialManifest) -> Result<()> {
        validate_manifest(manifest)?;
        let bytes = serde_json::to_vec_pretty(manifest)?;
        crate::session::atomic_write(&self.manifest_path(&manifest.id), &bytes)
    }

    fn read_manifest_path(&self, path: &Path) -> Result<TrialManifest> {
        let metadata = fs::metadata(path)?;
        if metadata.len() > MAX_MANIFEST_BYTES {
            bail!("trial manifest exceeds {} bytes", MAX_MANIFEST_BYTES);
        }
        let manifest: TrialManifest = serde_json::from_slice(&fs::read(path)?)?;
        validate_manifest(&manifest)?;
        if path
            .file_stem()
            .is_some_and(|stem| stem != manifest.id.as_str())
        {
            bail!("trial manifest id does not match its filename");
        }
        Ok(manifest)
    }

    fn manifests_dir(&self) -> PathBuf {
        self.root.join(MANIFESTS_SUBDIR)
    }

    fn manifest_path(&self, trial_id: &str) -> PathBuf {
        self.manifests_dir().join(format!("{trial_id}.json"))
    }

    fn exclusive_lock(&self) -> Result<fs::File> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.root.join(STORE_LOCK_FILE))?;
        file.lock_exclusive()?;
        Ok(file)
    }
}

fn apply_contender_update(contender: &mut TrialContenderRecord, update: TrialUpdate) -> Result<()> {
    match update {
        TrialUpdate::Creating => {
            if contender.state == ContenderState::Pending {
                contender.state = ContenderState::Creating;
            }
        }
        TrialUpdate::SessionStarting { session_id } => {
            if let Some(existing) = &contender.session_id {
                if existing != &session_id {
                    bail!("trial contender is already linked to session {existing}");
                }
            }
            if contender.state.is_terminal() && contender.session_id.is_none() {
                bail!("cannot link a terminal trial contender to a new session");
            }
            contender.session_id = Some(session_id);
            if !contender.state.is_terminal() {
                contender.state = ContenderState::Starting;
            }
        }
        TrialUpdate::Running { started_at_ms } => {
            if !contender.state.is_terminal() {
                if contender.session_id.is_none() {
                    bail!("cannot run a trial contender before its session is linked");
                }
                contender.state = ContenderState::Running;
                contender.started_at_ms.get_or_insert(started_at_ms);
                contender.error = None;
            }
        }
        TrialUpdate::Completed { elapsed_ms } => match contender.state {
            ContenderState::Completed => {
                if contender.elapsed_ms != Some(elapsed_ms) {
                    bail!("trial contender completion cannot be rewritten");
                }
            }
            ContenderState::Running => {
                contender.state = ContenderState::Completed;
                contender.elapsed_ms = Some(elapsed_ms);
                contender.error = None;
            }
            ContenderState::Failed | ContenderState::Cancelled => {
                bail!("terminal trial contender cannot become completed");
            }
            _ => bail!("trial contender cannot complete before it is running"),
        },
        TrialUpdate::Verified {
            result,
            changed_files,
            insertions,
            deletions,
        } => {
            if contender.state != ContenderState::Completed {
                bail!("trial contender must complete before verification is recorded");
            }
            if let Some(existing) = &contender.verification {
                if existing != &result
                    || contender.changed_files != changed_files
                    || contender.insertions != insertions
                    || contender.deletions != deletions
                {
                    bail!("trial contender verification cannot be rewritten");
                }
            } else {
                contender.verification = Some(result);
                contender.changed_files = changed_files;
                contender.insertions = insertions;
                contender.deletions = deletions;
            }
        }
        TrialUpdate::ReportedTokens { tokens } => {
            if let Some(existing) = contender.reported_tokens {
                if existing != tokens {
                    bail!("reported trial usage cannot be rewritten");
                }
            } else {
                contender.reported_tokens = Some(tokens);
            }
        }
        TrialUpdate::Failed { error } => match contender.state {
            ContenderState::Completed | ContenderState::Cancelled => {
                bail!("terminal trial contender cannot become failed");
            }
            ContenderState::Failed => {
                if contender.error.as_deref() != Some(error.as_str()) {
                    bail!("trial contender failure cannot be rewritten");
                }
            }
            _ => {
                contender.state = ContenderState::Failed;
                contender.error = Some(error);
            }
        },
        TrialUpdate::Cancelled => {
            if !contender.state.is_terminal() {
                contender.state = ContenderState::Cancelled;
            }
        }
    }
    Ok(())
}

fn derive_state(contenders: &[TrialContenderRecord]) -> TrialState {
    let completed = contenders
        .iter()
        .filter(|contender| contender.state == ContenderState::Completed)
        .count();
    let failed = contenders
        .iter()
        .filter(|contender| contender.state == ContenderState::Failed)
        .count();
    if completed == contenders.len() {
        TrialState::Completed
    } else if failed == contenders.len() {
        TrialState::Failed
    } else if contenders
        .iter()
        .all(|contender| contender.state == ContenderState::Cancelled)
    {
        TrialState::Cancelled
    } else if contenders
        .iter()
        .all(|contender| contender.state.is_terminal())
    {
        TrialState::Partial
    } else if contenders.iter().any(|contender| {
        matches!(
            contender.state,
            ContenderState::Starting | ContenderState::Running
        )
    }) {
        TrialState::Running
    } else {
        TrialState::Launching
    }
}

fn summary(manifest: &TrialManifest) -> TrialSummary {
    let ranking = rank_contenders(manifest);
    TrialSummary {
        id: manifest.id.clone(),
        checkpoint_id: manifest.checkpoint_id.clone(),
        checkpoint_source_session_id: manifest.checkpoint_source_session_id.clone(),
        prompt: manifest.prompt.clone(),
        state: manifest.state,
        completed: manifest
            .contenders
            .iter()
            .filter(|contender| contender.state == ContenderState::Completed)
            .count(),
        failed: manifest
            .contenders
            .iter()
            .filter(|contender| contender.state == ContenderState::Failed)
            .count(),
        total: manifest.contenders.len(),
        session_ids: manifest
            .contenders
            .iter()
            .filter_map(|contender| contender.session_id.clone())
            .collect(),
        contenders: manifest
            .contenders
            .iter()
            .map(|contender| TrialContenderSummary {
                id: contender.spec.id.clone(),
                tool: contender.spec.tool.clone(),
                model: contender.spec.model.clone(),
                effort: contender.spec.effort.clone(),
                permission_mode: contender.spec.permission_mode.clone(),
                state: contender.state,
                session_id: contender.session_id.clone(),
                elapsed_ms: contender.elapsed_ms,
                reported_tokens: contender.reported_tokens,
                started_at_ms: contender.started_at_ms,
                verification: contender.verification.clone(),
                changed_files: contender.changed_files,
                insertions: contender.insertions,
                deletions: contender.deletions,
                prior_attempts: contender.prior_attempts.len(),
                retained: contender.session_id.is_some()
                    && contender.session_id == manifest.retained_winner_session_id,
                error: contender.error.clone(),
            })
            .collect(),
        verification_recipe: manifest.verification_recipe.clone(),
        retained_winner_session_id: manifest.retained_winner_session_id.clone(),
        ranking,
        updated_at_ms: manifest.updated_at_ms,
    }
}

fn rank_contenders(manifest: &TrialManifest) -> Vec<TrialRank> {
    let mut candidates: Vec<&TrialContenderRecord> = manifest
        .contenders
        .iter()
        .filter(|contender| {
            contender.state == ContenderState::Completed
                && contender.session_id.is_some()
                && contender.verification.is_some()
        })
        .collect();
    candidates.sort_by(|left, right| {
        verification_order(left.verification.as_ref().expect("filtered verification"))
            .cmp(&verification_order(
                right.verification.as_ref().expect("filtered verification"),
            ))
            .then_with(|| {
                left.changed_files
                    .unwrap_or(u32::MAX)
                    .cmp(&right.changed_files.unwrap_or(u32::MAX))
            })
            .then_with(|| {
                left.elapsed_ms
                    .unwrap_or(u64::MAX)
                    .cmp(&right.elapsed_ms.unwrap_or(u64::MAX))
            })
            .then_with(|| {
                left.reported_tokens
                    .unwrap_or(u64::MAX)
                    .cmp(&right.reported_tokens.unwrap_or(u64::MAX))
            })
            .then_with(|| left.spec.id.cmp(&right.spec.id))
    });
    candidates
        .into_iter()
        .enumerate()
        .map(|(index, contender)| TrialRank {
            rank: index + 1,
            contender_id: contender.spec.id.clone(),
            session_id: contender
                .session_id
                .clone()
                .expect("filtered contender session"),
        })
        .collect()
}

fn verification_order(result: &VerificationResult) -> u8 {
    match result.state {
        VerificationState::Passed => 0,
        VerificationState::Failed => 1,
        VerificationState::TimedOut => 2,
        VerificationState::Error => 3,
    }
}

fn validate_manifest(manifest: &TrialManifest) -> Result<()> {
    if manifest.schema_version != TRIAL_SCHEMA_VERSION {
        bail!("unsupported trial schema {}", manifest.schema_version);
    }
    validate_id(&manifest.id, "trial")?;
    validate_id(&manifest.checkpoint_id, "checkpoint")?;
    if manifest
        .checkpoint_source_session_id
        .as_deref()
        .is_some_and(str::is_empty)
    {
        bail!("trial checkpoint source session id is empty");
    }
    if manifest.prompt.trim().is_empty() || manifest.prompt.len() > MAX_TRIAL_PROMPT_BYTES {
        bail!("trial prompt is empty or exceeds the 16 KiB limit");
    }
    if !(2..=MAX_TRIAL_CONTENDERS).contains(&manifest.contenders.len()) {
        bail!("trial contender count is outside the supported range");
    }
    if let Some(recipe) = &manifest.verification_recipe {
        VerificationRecipe::new(recipe.command.clone(), recipe.timeout_secs)?;
    }
    let mut contender_ids = std::collections::BTreeSet::new();
    let mut session_ids = std::collections::BTreeSet::new();
    for contender in &manifest.contenders {
        validate_id(&contender.spec.id, "contender")?;
        if contender.spec.tool.trim().is_empty() {
            bail!("trial contender tool cannot be empty");
        }
        if !contender_ids.insert(&contender.spec.id) {
            bail!("trial contender id is duplicated");
        }
        if let Some(session_id) = &contender.session_id {
            if session_id.trim().is_empty() || !session_ids.insert(session_id) {
                bail!("trial contender session id is empty or duplicated");
            }
        }
        if matches!(
            contender.state,
            ContenderState::Starting | ContenderState::Running | ContenderState::Completed
        ) && contender.session_id.is_none()
        {
            bail!("started trial contender has no session id");
        }
        if matches!(
            contender.state,
            ContenderState::Running | ContenderState::Completed
        ) && contender.started_at_ms.is_none()
        {
            bail!("running trial contender has no start timestamp");
        }
        if contender.state == ContenderState::Completed && contender.elapsed_ms.is_none() {
            bail!("completed trial contender has no elapsed time");
        }
        if contender.state == ContenderState::Failed
            && contender.error.as_deref().is_none_or(str::is_empty)
        {
            bail!("failed trial contender has no error");
        }
        if contender.prior_attempts.len() > MAX_PRIOR_ATTEMPTS {
            bail!("trial contender retry history exceeds the supported limit");
        }
        if contender
            .prior_attempts
            .iter()
            .any(|attempt| !attempt.state.is_terminal())
        {
            bail!("trial retry history contains a non-terminal attempt");
        }
        if contender.verification.is_some() && contender.state != ContenderState::Completed {
            bail!("only a completed contender may have verification");
        }
        if (contender.changed_files.is_some()
            || contender.insertions.is_some()
            || contender.deletions.is_some())
            && contender.verification.is_none()
        {
            bail!("trial diff facts require a verification result");
        }
    }
    if let Some(winner) = manifest.retained_winner_session_id.as_deref() {
        if !manifest.contenders.iter().any(|contender| {
            contender.state == ContenderState::Completed
                && contender.session_id.as_deref() == Some(winner)
        }) {
            bail!("retained winner is not a completed trial contender");
        }
    }
    if manifest.state != derive_state(&manifest.contenders) {
        bail!("trial state does not match its contender states");
    }
    Ok(())
}

fn validate_id(id: &str, kind: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(id).with_context(|| format!("invalid {kind} id {id}"))?;
    if parsed.to_string() != id.to_ascii_lowercase() {
        bail!("{kind} id is not canonical");
    }
    Ok(())
}

/// Execute an explicitly confirmed verification recipe in one contender's
/// worktree. The caller must run this on a background worker. Output is never
/// persisted because it may contain secrets; the durable fact is the bounded
/// exit summary plus repository diff counters.
pub fn run_verification(worktree: &Path, recipe: &VerificationRecipe) -> VerificationExecution {
    let started = std::time::Instant::now();
    let mut command = std::process::Command::new(crate::session::user_shell());
    command
        .arg("-c")
        .arg(&recipe.command)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("SSH_ASKPASS", "true");
    let outcome = crate::process::run_with_timeout_process_group(
        &mut command,
        Duration::from_secs(recipe.timeout_secs),
    );
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let result = match outcome {
        Ok(Some(output)) if output.status.success() => VerificationResult {
            state: VerificationState::Passed,
            exit_code: output.status.code(),
            duration_ms,
            observed_at_ms: now_ms(),
            summary: "verification command passed".to_string(),
        },
        Ok(Some(output)) => VerificationResult {
            state: VerificationState::Failed,
            exit_code: output.status.code(),
            duration_ms,
            observed_at_ms: now_ms(),
            summary: match output.status.code() {
                Some(code) => format!("verification command exited with status {code}"),
                None => "verification command ended without an exit code".to_string(),
            },
        },
        Ok(None) => VerificationResult {
            state: VerificationState::TimedOut,
            exit_code: None,
            duration_ms,
            observed_at_ms: now_ms(),
            summary: format!(
                "verification command timed out after {} seconds",
                recipe.timeout_secs
            ),
        },
        Err(error) => VerificationResult {
            state: VerificationState::Error,
            exit_code: None,
            duration_ms,
            observed_at_ms: now_ms(),
            summary: format!("verification command could not run: {error}"),
        },
    };
    let (changed_files, insertions, deletions) = diff_stats(worktree)
        .map(|stats| (Some(stats.0), Some(stats.1), Some(stats.2)))
        .unwrap_or((None, None, None));
    VerificationExecution {
        result,
        changed_files,
        insertions,
        deletions,
    }
}

fn diff_stats(worktree: &Path) -> Result<(u32, u32, u32)> {
    let repository = crate::git::open_repo_at(worktree)?;
    let tree = repository.head()?.peel_to_tree()?;
    let mut options = git2::DiffOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    let diff = repository.diff_tree_to_workdir_with_index(Some(&tree), Some(&mut options))?;
    let stats = diff.stats()?;
    Ok((
        stats.files_changed().min(u32::MAX as usize) as u32,
        stats.insertions().min(u32::MAX as usize) as u32,
        stats.deletions().min(u32::MAX as usize) as u32,
    ))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> TrialCreateRequest {
        TrialCreateRequest::new(
            uuid::Uuid::new_v4().to_string(),
            "implement the parser",
            ["claude".to_string(), "codex".to_string()],
            "default",
        )
        .unwrap()
    }

    #[test]
    fn updates_are_idempotent_and_derive_trial_state() {
        let root = tempfile::tempdir().unwrap();
        let store = TrialStore::at_root(root.path().join("trials")).unwrap();
        let manifest = store.create(request()).unwrap();
        let first = manifest.contenders[0].spec.id.clone();
        let second = manifest.contenders[1].spec.id.clone();
        store
            .update(&manifest.id, &first, TrialUpdate::Creating)
            .unwrap();
        store
            .update(
                &manifest.id,
                &first,
                TrialUpdate::SessionStarting {
                    session_id: "session-a".to_string(),
                },
            )
            .unwrap();
        let running = store
            .update(
                &manifest.id,
                &first,
                TrialUpdate::Running { started_at_ms: 10 },
            )
            .unwrap();
        assert_eq!(running.state, TrialState::Running);
        store
            .update(
                &manifest.id,
                &first,
                TrialUpdate::Completed { elapsed_ms: 20 },
            )
            .unwrap();
        let partial = store
            .update(
                &manifest.id,
                &second,
                TrialUpdate::Failed {
                    error: "launch failed".to_string(),
                },
            )
            .unwrap();
        assert_eq!(partial.state, TrialState::Partial);
        assert_eq!(summary(&partial).completed, 1);
    }

    #[test]
    fn refuses_to_relink_a_contender_to_another_session() {
        let root = tempfile::tempdir().unwrap();
        let store = TrialStore::at_root(root.path().join("trials")).unwrap();
        let manifest = store.create(request()).unwrap();
        let contender = &manifest.contenders[0].spec.id;
        store
            .update(
                &manifest.id,
                contender,
                TrialUpdate::SessionStarting {
                    session_id: "session-a".to_string(),
                },
            )
            .unwrap();
        let error = store
            .update(
                &manifest.id,
                contender,
                TrialUpdate::SessionStarting {
                    session_id: "session-b".to_string(),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("already linked"));
    }

    #[test]
    fn load_keeps_valid_trials_when_one_manifest_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let store = TrialStore::at_root(root.path().join("trials")).unwrap();
        store.create(request()).unwrap();
        fs::write(store.manifests_dir().join("broken.json"), "{").unwrap();

        let report = store.load().unwrap();

        assert_eq!(report.trials.len(), 1);
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn terminal_contender_state_cannot_regress() {
        let root = tempfile::tempdir().unwrap();
        let store = TrialStore::at_root(root.path().join("trials")).unwrap();
        let manifest = store.create(request()).unwrap();
        let contender = manifest.contenders[0].spec.id.clone();
        store
            .update(
                &manifest.id,
                &contender,
                TrialUpdate::SessionStarting {
                    session_id: "session-a".to_string(),
                },
            )
            .unwrap();
        store
            .update(
                &manifest.id,
                &contender,
                TrialUpdate::Running { started_at_ms: 10 },
            )
            .unwrap();
        store
            .update(
                &manifest.id,
                &contender,
                TrialUpdate::Completed { elapsed_ms: 20 },
            )
            .unwrap();

        assert!(store
            .update(
                &manifest.id,
                &contender,
                TrialUpdate::Failed {
                    error: "late failure".to_string(),
                },
            )
            .is_err());
        let persisted = store.inspect(&manifest.id).unwrap();
        assert_eq!(persisted.contenders[0].state, ContenderState::Completed);
        assert_eq!(persisted.contenders[0].elapsed_ms, Some(20));
    }

    #[test]
    fn verification_ranking_and_winner_retention_are_durable() {
        let root = tempfile::tempdir().unwrap();
        let store = TrialStore::at_root(root.path().join("trials")).unwrap();
        let request = request()
            .with_verification_recipe(VerificationRecipe::new("cargo test", 30).unwrap())
            .with_checkpoint_source_session("source-session");
        let manifest = store.create(request).unwrap();
        let contenders: Vec<_> = manifest
            .contenders
            .iter()
            .map(|contender| contender.spec.id.clone())
            .collect();
        for (index, contender_id) in contenders.iter().enumerate() {
            store
                .update(
                    &manifest.id,
                    contender_id,
                    TrialUpdate::SessionStarting {
                        session_id: format!("session-{index}"),
                    },
                )
                .unwrap();
            store
                .update(
                    &manifest.id,
                    contender_id,
                    TrialUpdate::Running { started_at_ms: 10 },
                )
                .unwrap();
            store
                .update(
                    &manifest.id,
                    contender_id,
                    TrialUpdate::Completed {
                        elapsed_ms: 100 + index as u64,
                    },
                )
                .unwrap();
        }
        store
            .update(
                &manifest.id,
                &contenders[0],
                TrialUpdate::Verified {
                    result: VerificationResult {
                        state: VerificationState::Failed,
                        exit_code: Some(1),
                        duration_ms: 10,
                        observed_at_ms: 20,
                        summary: "verification failed".to_string(),
                    },
                    changed_files: Some(1),
                    insertions: Some(1),
                    deletions: Some(0),
                },
            )
            .unwrap();
        store
            .update(
                &manifest.id,
                &contenders[1],
                TrialUpdate::Verified {
                    result: VerificationResult {
                        state: VerificationState::Passed,
                        exit_code: Some(0),
                        duration_ms: 11,
                        observed_at_ms: 21,
                        summary: "verification passed".to_string(),
                    },
                    changed_files: Some(4),
                    insertions: Some(8),
                    deletions: Some(2),
                },
            )
            .unwrap();

        let ranked = store.inspect(&manifest.id).unwrap().summary();
        assert_eq!(ranked.ranking[0].contender_id, contenders[1]);
        assert_eq!(
            ranked.checkpoint_source_session_id.as_deref(),
            Some("source-session")
        );

        let retained = store
            .retain_winner(&manifest.id, &contenders[1])
            .unwrap()
            .summary();
        assert!(retained.contenders[1].retained);
        assert!(!retained.contenders[0].retained);
    }

    #[test]
    fn cancel_and_retry_archive_only_the_failed_attempt() {
        let root = tempfile::tempdir().unwrap();
        let store = TrialStore::at_root(root.path().join("trials")).unwrap();
        let manifest = store.create(request()).unwrap();
        let failed_id = manifest.contenders[0].spec.id.clone();
        let untouched_id = manifest.contenders[1].spec.id.clone();
        store
            .update(
                &manifest.id,
                &failed_id,
                TrialUpdate::Failed {
                    error: "launch failed".to_string(),
                },
            )
            .unwrap();
        let cancelled = store.cancel(&manifest.id).unwrap();
        assert_eq!(
            cancelled
                .contenders
                .iter()
                .find(|contender| contender.spec.id == untouched_id)
                .unwrap()
                .state,
            ContenderState::Cancelled
        );

        let retried = store.retry(&manifest.id, &failed_id).unwrap();
        let failed = retried
            .contenders
            .iter()
            .find(|contender| contender.spec.id == failed_id)
            .unwrap();
        assert_eq!(failed.state, ContenderState::Pending);
        assert_eq!(failed.prior_attempts.len(), 1);
        assert_eq!(
            failed.prior_attempts[0].error.as_deref(),
            Some("launch failed")
        );
        assert_eq!(
            retried
                .contenders
                .iter()
                .find(|contender| contender.spec.id == untouched_id)
                .unwrap()
                .state,
            ContenderState::Cancelled,
            "retry must not duplicate or reset another contender"
        );
    }
}
