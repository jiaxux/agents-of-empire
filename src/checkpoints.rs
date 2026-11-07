//! Durable, versioned checkpoint capture.
//!
//! The public surface is deliberately small: callers build an immutable
//! [`CaptureRequest`] from an AoE session, then hand it to [`CheckpointStore`]
//! on a background worker. Git inspection, patch materialization, redaction,
//! hashing, and atomic persistence stay behind that boundary.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::{Compression, GzBuilder};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fleet::CheckpointState;
use crate::session::{Instance, View};

pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
pub const CHECKPOINT_BUNDLE_SCHEMA_VERSION: u32 = 1;
pub const MAX_CHECKPOINT_LABEL_CHARS: usize = 80;

const CHECKPOINTS_SUBDIR: &str = "checkpoints";
const MANIFESTS_SUBDIR: &str = "manifests";
const OBJECTS_SUBDIR: &str = "objects/sha256";
const EXPORTS_SUBDIR: &str = "exports";
const STORE_LOCK_FILE: &str = ".lock";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_MANIFESTS_LOADED: usize = 4096;
const MAX_PATCH_BYTES: usize = 64 * 1024 * 1024;
const MAX_UNTRACKED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UNTRACKED_ARCHIVE_BYTES: u64 = 80 * 1024 * 1024;
const MAX_UNTRACKED_FILES: usize = 131_072;
const MAX_BUNDLE_BYTES: u64 = 160 * 1024 * 1024;
const MAX_BUNDLE_ENTRIES: usize = 32;
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestState {
    Materializing,
    Complete,
    Incomplete,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConversationIdentity {
    pub kind: String,
    pub id: String,
    pub event_sequence: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SandboxMetadata {
    pub enabled: bool,
    pub image: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationMetadata {
    pub kind: String,
    pub state: String,
    pub updated_at_ms: u64,
    pub summary: String,
    pub source: String,
}

impl VerificationMetadata {
    pub fn from_evidence(evidence: &crate::fleet::Evidence) -> Self {
        let kind = match evidence.kind {
            crate::fleet::EvidenceKind::Verification => "verification",
            crate::fleet::EvidenceKind::Conflict => "conflict",
            crate::fleet::EvidenceKind::Tests => "tests",
            crate::fleet::EvidenceKind::Format => "format",
            crate::fleet::EvidenceKind::Lint => "lint",
            crate::fleet::EvidenceKind::Review => "review",
            crate::fleet::EvidenceKind::Diff => "diff",
        };
        let state = match evidence.state {
            crate::fleet::EvidenceState::Passed => "passed",
            crate::fleet::EvidenceState::Failed => "failed",
            crate::fleet::EvidenceState::Running => "running",
            crate::fleet::EvidenceState::Missing => "missing",
            crate::fleet::EvidenceState::Stale => "stale",
        };
        Self {
            kind: kind.to_string(),
            state: state.to_string(),
            updated_at_ms: evidence.updated_at_ms,
            summary: evidence.summary.clone(),
            source: "fleet_snapshot".to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    pub title: String,
    pub generation: u64,
    pub agent: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub view: String,
    pub permission_mode: String,
    pub conversation: Option<ConversationIdentity>,
    pub sandbox: Option<SandboxMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub sha256: String,
    pub bytes: u64,
    pub media_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryManifest {
    pub name: String,
    pub identity: String,
    pub base_ref: Option<String>,
    pub base_commit: Option<String>,
    pub head: Option<String>,
    pub changed_paths: usize,
    /// Exact repository-relative paths represented by the patch and archive.
    /// Older manifests omit this additive field and remain inspectable and
    /// forkable, but in-place restore refuses them because scoped rollback
    /// cannot be proven without the path set.
    #[serde(default)]
    pub changed_path_names: Vec<String>,
    pub patch: Option<ArtifactRef>,
    pub untracked_archive: Option<ArtifactRef>,
    pub excluded_paths: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub schema_version: u32,
    pub id: String,
    pub label: String,
    pub created_at_ms: u64,
    pub materialized_at_ms: Option<u64>,
    pub state: ManifestState,
    pub incomplete_reasons: Vec<String>,
    pub session: SessionMetadata,
    pub repositories: Vec<RepositoryManifest>,
    pub campaign_id: Option<String>,
    pub trial_id: Option<String>,
    /// Compact verification facts known by the caller at capture time.
    pub verification: Vec<VerificationMetadata>,
    /// Usage remains absent until an adapter reports real values.
    pub reported_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointSummary {
    pub id: String,
    pub session_id: String,
    pub session_title: String,
    pub label: String,
    pub created_at_ms: u64,
    pub snapshot_ref: String,
    pub state: CheckpointState,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadReport {
    pub checkpoints: Vec<CheckpointSummary>,
    pub errors: Vec<String>,
}

/// Immutable provenance carried from fork preparation through ordinary
/// session creation. The worker revalidates every field against the durable
/// manifest before it mutates the newly-created worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointSeed {
    pub checkpoint_id: String,
    pub repository_head: String,
    pub repository_identity: String,
    pub agent: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: String,
    pub view: String,
    pub warnings: Vec<String>,
}

/// Result of resolving a checkpoint against its still-live source session.
/// This contains no artifact bytes and is safe to move back to the UI thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointForkPlan {
    pub seed: CheckpointSeed,
    pub source_session_id: String,
    pub source_title: String,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckpointExportResult {
    pub checkpoint_id: String,
    pub output_path: PathBuf,
    pub bytes: u64,
    pub artifacts: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckpointDeleteResult {
    pub checkpoint_id: String,
    pub artifacts_removed: usize,
    pub bytes_removed: u64,
    pub artifacts_retained: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckpointImportResult {
    pub checkpoint_id: String,
    pub imported: bool,
    pub artifacts: usize,
}

/// Fully verified, immutable restore intent. Preparation performs all costly
/// artifact and disposable-worktree checks. Execution still revalidates the
/// target HEAD, cleanliness, and repository identity before changing it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointRestorePlan {
    pub seed: CheckpointSeed,
    pub label: String,
    pub target: PathBuf,
    pub original_head: String,
    pub recovery_ref: String,
    pub changed_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckpointRestoreResult {
    pub checkpoint_id: String,
    pub target: PathBuf,
    pub restored_head: String,
    pub recovery_ref: String,
    pub changed_paths: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckpointBundleDescriptor {
    schema_version: u32,
    format: String,
    checkpoint_id: String,
    manifest_path: String,
    artifact_paths: Vec<String>,
    redaction_report_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RedactionReport {
    checkpoint_id: String,
    excluded_paths: Vec<String>,
    incomplete_reasons: Vec<String>,
}

#[derive(Debug)]
struct ArchiveFile {
    path: PathBuf,
    bytes: Vec<u8>,
    mode: u32,
}

#[derive(Clone, Debug)]
struct RepositoryCapture {
    name: String,
    path: PathBuf,
    base_ref: Option<String>,
}

/// Immutable input captured on the UI thread and materialized elsewhere.
#[derive(Clone, Debug)]
pub struct CaptureRequest {
    checkpoint_id: String,
    label: String,
    created_at_ms: u64,
    session: SessionMetadata,
    repositories: Vec<RepositoryCapture>,
    campaign_id: Option<String>,
    trial_id: Option<String>,
    verification: Vec<VerificationMetadata>,
    reported_tokens: Option<u64>,
}

impl CaptureRequest {
    /// Reduce an [`Instance`] to checkpoint-safe metadata. Environment values,
    /// plugin payloads, container ids, commands, and credential paths are not
    /// copied into the request or manifest.
    pub fn from_instance(instance: &Instance, label: impl Into<String>) -> Self {
        let conversation = match instance.view {
            View::Structured => instance
                .acp_session_id
                .as_ref()
                .map(|id| ConversationIdentity {
                    kind: "acp".to_string(),
                    id: id.clone(),
                    event_sequence: None,
                }),
            View::Terminal => instance
                .agent_session_id
                .as_ref()
                .map(|id| ConversationIdentity {
                    kind: "terminal_agent".to_string(),
                    id: id.clone(),
                    event_sequence: None,
                }),
        };
        let permission_mode = instance.acp_mode_id.clone().unwrap_or_else(|| {
            if instance.yolo_mode {
                "yolo".to_string()
            } else {
                "default".to_string()
            }
        });
        let sandbox = instance
            .sandbox_info
            .as_ref()
            .map(|sandbox| SandboxMetadata {
                enabled: sandbox.enabled,
                image: sandbox.image.clone(),
            });
        let session = SessionMetadata {
            id: instance.id.clone(),
            title: instance.title.clone(),
            generation: instance.lifecycle_generation,
            agent: instance
                .agent_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| instance.tool.clone()),
            model: instance.agent_model.clone(),
            effort: instance.acp_effort.clone(),
            view: match instance.view {
                View::Terminal => "terminal",
                View::Structured => "structured",
            }
            .to_string(),
            permission_mode,
            conversation,
            sandbox,
        };
        let repositories = if let Some(workspace) = &instance.workspace_info {
            workspace
                .repos
                .iter()
                .map(|repo| RepositoryCapture {
                    name: repo.name.clone(),
                    path: PathBuf::from(&repo.worktree_path),
                    base_ref: repo
                        .base_branch_override
                        .clone()
                        .or_else(|| repo.base_branch.clone()),
                })
                .collect()
        } else {
            let path = PathBuf::from(&instance.project_path);
            let name = path
                .file_name()
                .map(|part| part.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "repository".to_string());
            vec![RepositoryCapture {
                name,
                path,
                base_ref: instance.base_branch_override.clone().or_else(|| {
                    instance
                        .worktree_info
                        .as_ref()
                        .and_then(|worktree| worktree.base_branch.clone())
                }),
            }]
        };
        Self {
            checkpoint_id: uuid::Uuid::new_v4().to_string(),
            label: normalize_label(&label.into()),
            created_at_ms: now_ms(),
            session,
            repositories,
            campaign_id: None,
            trial_id: None,
            verification: Vec::new(),
            reported_tokens: None,
        }
    }

    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    pub fn with_verification(mut self, verification: Vec<VerificationMetadata>) -> Self {
        self.verification = verification;
        self
    }

    pub fn pending_summary(&self) -> CheckpointSummary {
        CheckpointSummary {
            id: self.checkpoint_id.clone(),
            session_id: self.session.id.clone(),
            session_title: self.session.title.clone(),
            label: self.label.clone(),
            created_at_ms: self.created_at_ms,
            snapshot_ref: "capturing".to_string(),
            state: CheckpointState::Materializing,
            detail: None,
        }
    }
}

/// Durable checkpoint repository. One instance may serve many requests on a
/// worker thread; it owns no mutable in-memory index.
#[derive(Clone, Debug)]
pub struct CheckpointStore {
    root: PathBuf,
}

impl CheckpointStore {
    pub fn open() -> Result<Self> {
        Self::at_root(crate::session::get_app_dir()?.join(CHECKPOINTS_SUBDIR))
    }

    pub fn at_root(root: PathBuf) -> Result<Self> {
        let store = Self { root };
        fs::create_dir_all(store.manifests_dir()).with_context(|| {
            format!(
                "failed to create checkpoint manifests directory at {}",
                store.manifests_dir().display()
            )
        })?;
        fs::create_dir_all(store.objects_dir()).with_context(|| {
            format!(
                "failed to create checkpoint objects directory at {}",
                store.objects_dir().display()
            )
        })?;
        fs::create_dir_all(store.exports_dir()).with_context(|| {
            format!(
                "failed to create checkpoint exports directory at {}",
                store.exports_dir().display()
            )
        })?;
        Ok(store)
    }

    /// Persist an initial materializing manifest, capture every repository,
    /// then atomically replace it with the completed or explicitly incomplete
    /// result. The source worktree is never mutated or paused.
    pub fn capture(&self, request: CaptureRequest) -> Result<CheckpointSummary> {
        let _lock = self.exclusive_lock()?;
        validate_checkpoint_id(&request.checkpoint_id)?;
        let mut manifest = CheckpointManifest {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            id: request.checkpoint_id,
            label: request.label,
            created_at_ms: request.created_at_ms,
            materialized_at_ms: None,
            state: ManifestState::Materializing,
            incomplete_reasons: Vec::new(),
            session: request.session,
            repositories: request
                .repositories
                .iter()
                .map(empty_repository_manifest)
                .collect(),
            campaign_id: request.campaign_id,
            trial_id: request.trial_id,
            verification: request.verification,
            reported_tokens: request.reported_tokens,
        };
        if manifest.session.view == "structured"
            && manifest
                .session
                .conversation
                .as_ref()
                .is_some_and(|conversation| conversation.event_sequence.is_none())
        {
            manifest.incomplete_reasons.push(
                "structured conversation has no stable event sequence; worktree content is still captured"
                    .to_string(),
            );
        }
        if request.repositories.len() > 1 {
            manifest.incomplete_reasons.push(
                "multi-repository content is captured sequentially without a global consistency fence"
                    .to_string(),
            );
        }
        self.write_manifest(&manifest)?;

        manifest.repositories.clear();
        if request.repositories.is_empty() {
            manifest
                .incomplete_reasons
                .push("session has no repository to capture".to_string());
        }
        for repository in &request.repositories {
            let captured = match self.capture_repository(repository) {
                Ok(captured) => captured,
                Err(error) => RepositoryManifest {
                    error: Some(error.to_string()),
                    ..empty_repository_manifest(repository)
                },
            };
            if let Some(error) = &captured.error {
                manifest
                    .incomplete_reasons
                    .push(format!("{}: {error}", captured.name));
            }
            if !captured.excluded_paths.is_empty() {
                manifest.incomplete_reasons.push(format!(
                    "{}: {} sensitive or unsupported path(s) excluded",
                    captured.name,
                    captured.excluded_paths.len()
                ));
            }
            manifest.repositories.push(captured);
        }
        manifest.materialized_at_ms = Some(now_ms());
        manifest.state = if manifest.incomplete_reasons.is_empty() {
            ManifestState::Complete
        } else {
            ManifestState::Incomplete
        };
        self.write_manifest(&manifest)?;
        Ok(summary_from_manifest(&manifest, false))
    }

    /// Load bounded summaries. One malformed manifest does not hide the rest.
    pub fn load(&self) -> Result<LoadReport> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(self.manifests_dir())? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(target: "checkpoint.store", %error, "checkpoint entry unreadable");
                    continue;
                }
            };
            if entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.path().extension().is_some_and(|ext| ext == "json")
            {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(UNIX_EPOCH);
                paths.push((modified, entry.path()));
            }
        }
        paths.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

        let mut report = LoadReport::default();
        if paths.len() > MAX_MANIFESTS_LOADED {
            report.errors.push(format!(
                "checkpoint history exceeds the {}-manifest load limit",
                MAX_MANIFESTS_LOADED
            ));
            paths.truncate(MAX_MANIFESTS_LOADED);
        }
        for (_, path) in paths {
            match self.read_manifest_path(&path) {
                Ok(manifest) => report
                    .checkpoints
                    .push(summary_from_manifest(&manifest, true)),
                Err(error) => report.errors.push(format!(
                    "{}: {error}",
                    path.file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_default()
                )),
            }
        }
        report
            .checkpoints
            .sort_by_key(|checkpoint| std::cmp::Reverse(checkpoint.created_at_ms));
        Ok(report)
    }

    /// Read one manifest for non-mutating inspection.
    pub fn inspect(&self, checkpoint_id: &str) -> Result<CheckpointManifest> {
        validate_checkpoint_id(checkpoint_id)?;
        self.read_manifest_path(&self.manifest_path(checkpoint_id))
    }

    /// Export one checkpoint as a deterministic, versioned gzip tar bundle.
    /// The bundle contains the portable manifest, only the objects it names,
    /// and a redaction report listing excluded paths without their contents.
    /// Existing output is never overwritten.
    pub fn export_to(
        &self,
        checkpoint_id: &str,
        output: impl AsRef<Path>,
    ) -> Result<CheckpointExportResult> {
        let _lock = self.exclusive_lock()?;
        let manifest = self.inspect(checkpoint_id)?;
        let artifacts = manifest_artifacts(&manifest);
        let mut artifact_paths = Vec::with_capacity(artifacts.len());
        let mut verified = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            let max_bytes = artifact_limit(&artifact.media_type)?;
            let path = self.verified_object(artifact, &artifact.media_type, max_bytes)?;
            let bundle_path = format!("objects/sha256/{}", artifact.sha256);
            artifact_paths.push(bundle_path.clone());
            verified.push((bundle_path, path));
        }

        let descriptor = CheckpointBundleDescriptor {
            schema_version: CHECKPOINT_BUNDLE_SCHEMA_VERSION,
            format: "agent-of-empires-checkpoint".to_string(),
            checkpoint_id: manifest.id.clone(),
            manifest_path: "manifest.json".to_string(),
            artifact_paths,
            redaction_report_path: "redaction-report.json".to_string(),
        };
        let redaction = RedactionReport {
            checkpoint_id: manifest.id.clone(),
            excluded_paths: manifest
                .repositories
                .iter()
                .flat_map(|repository| repository.excluded_paths.iter().cloned())
                .collect(),
            incomplete_reasons: manifest.incomplete_reasons.clone(),
        };
        let output = output.as_ref();
        let parent = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        if output.exists() {
            bail!("checkpoint export already exists at {}", output.display());
        }
        let temp = tempfile::NamedTempFile::new_in(parent)?;
        {
            let encoder = GzBuilder::new()
                .mtime(0)
                .write(temp.reopen()?, Compression::default());
            let mut archive = tar::Builder::new(encoder);
            append_bundle_bytes(
                &mut archive,
                "bundle.json",
                &serde_json::to_vec_pretty(&descriptor)?,
            )?;
            append_bundle_bytes(
                &mut archive,
                "manifest.json",
                &serde_json::to_vec_pretty(&manifest)?,
            )?;
            append_bundle_bytes(
                &mut archive,
                "redaction-report.json",
                &serde_json::to_vec_pretty(&redaction)?,
            )?;
            for (bundle_path, object_path) in verified {
                append_bundle_file(&mut archive, &bundle_path, &object_path)?;
            }
            let encoder = archive.into_inner()?;
            encoder.finish()?;
        }
        temp.as_file().sync_all()?;
        temp.persist_noclobber(output)
            .map_err(|error| error.error)
            .with_context(|| {
                format!(
                    "failed to publish checkpoint export at {}",
                    output.display()
                )
            })?;
        let bytes = fs::metadata(output)?.len();
        Ok(CheckpointExportResult {
            checkpoint_id: manifest.id,
            output_path: output.to_path_buf(),
            bytes,
            artifacts: descriptor.artifact_paths.len(),
        })
    }

    /// Default export path under AoE's app data. Intended for the TUI, where a
    /// single key should produce a discoverable bundle without opening a path
    /// editor. The CLI accepts an explicit destination through [`export_to`].
    pub fn export_default(&self, checkpoint_id: &str) -> Result<CheckpointExportResult> {
        let manifest = self.inspect(checkpoint_id)?;
        let label = portable_filename_component(&manifest.label);
        let short_id = manifest.id.get(..8).unwrap_or(manifest.id.as_str());
        let output = self
            .exports_dir()
            .join(format!("{label}-{short_id}.aoe-checkpoint.tar.gz"));
        self.export_to(checkpoint_id, output)
    }

    /// Import a bundle produced by [`export_to`]. Every archive path, manifest
    /// id, media type, byte count, and SHA-256 is checked before anything is
    /// published. Re-importing identical content is idempotent.
    pub fn import_from(&self, input: impl AsRef<Path>) -> Result<CheckpointImportResult> {
        let _lock = self.exclusive_lock()?;
        let bytes = fs::read(input.as_ref())?;
        if bytes.len() as u64 > MAX_BUNDLE_BYTES {
            bail!("checkpoint bundle exceeds the 160 MiB limit");
        }
        let entries = read_bundle_entries(&bytes)?;
        let descriptor: CheckpointBundleDescriptor = serde_json::from_slice(
            entries
                .get("bundle.json")
                .context("checkpoint bundle is missing bundle.json")?,
        )?;
        if descriptor.schema_version != CHECKPOINT_BUNDLE_SCHEMA_VERSION
            || descriptor.format != "agent-of-empires-checkpoint"
        {
            bail!("unsupported checkpoint bundle format");
        }
        if descriptor.manifest_path != "manifest.json"
            || descriptor.redaction_report_path != "redaction-report.json"
        {
            bail!("checkpoint bundle declares unsupported metadata paths");
        }
        let manifest: CheckpointManifest = serde_json::from_slice(
            entries
                .get(&descriptor.manifest_path)
                .context("checkpoint bundle is missing its manifest")?,
        )?;
        validate_manifest(&manifest)?;
        if manifest.id != descriptor.checkpoint_id {
            bail!("checkpoint bundle id does not match its manifest");
        }
        let redaction: RedactionReport = serde_json::from_slice(
            entries
                .get(&descriptor.redaction_report_path)
                .context("checkpoint bundle is missing its redaction report")?,
        )?;
        if redaction.checkpoint_id != manifest.id {
            bail!("checkpoint redaction report belongs to another checkpoint");
        }
        let expected_paths: BTreeSet<String> = manifest_artifacts(&manifest)
            .into_iter()
            .map(|artifact| format!("objects/sha256/{}", artifact.sha256))
            .collect();
        if descriptor
            .artifact_paths
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected_paths
        {
            bail!("checkpoint bundle artifact index does not match its manifest");
        }
        for artifact in manifest_artifacts(&manifest) {
            let path = format!("objects/sha256/{}", artifact.sha256);
            let object = entries
                .get(&path)
                .with_context(|| format!("checkpoint bundle is missing {path}"))?;
            if object.len() as u64 != artifact.bytes || sha256_hex(object) != artifact.sha256 {
                bail!(
                    "checkpoint bundle artifact {} failed verification",
                    artifact.sha256
                );
            }
            if artifact.bytes > artifact_limit(&artifact.media_type)? {
                bail!(
                    "checkpoint bundle artifact {} exceeds its limit",
                    artifact.sha256
                );
            }
        }

        let manifest_path = self.manifest_path(&manifest.id);
        if manifest_path.exists() {
            let existing = self.read_manifest_path(&manifest_path)?;
            if existing != manifest {
                bail!("checkpoint id already exists with different content");
            }
            return Ok(CheckpointImportResult {
                checkpoint_id: manifest.id,
                imported: false,
                artifacts: expected_paths.len(),
            });
        }
        for artifact in manifest_artifacts(&manifest) {
            let path = format!("objects/sha256/{}", artifact.sha256);
            let stored = self.store_object(
                entries.get(&path).expect("artifact paths were verified"),
                &artifact.media_type,
            )?;
            if stored != *artifact {
                bail!("checkpoint artifact metadata changed during import");
            }
        }
        self.write_manifest(&manifest)?;
        Ok(CheckpointImportResult {
            checkpoint_id: manifest.id,
            imported: true,
            artifacts: expected_paths.len(),
        })
    }

    /// Delete one manifest and remove only content-addressed objects that no
    /// remaining readable manifest references. Any unreadable manifest makes
    /// cleanup fail closed, leaving shared objects in place.
    pub fn delete(&self, checkpoint_id: &str) -> Result<CheckpointDeleteResult> {
        let _lock = self.exclusive_lock()?;
        let manifest = self.inspect(checkpoint_id)?;
        let target_artifacts: BTreeSet<String> = manifest_artifacts(&manifest)
            .into_iter()
            .map(|artifact| artifact.sha256.clone())
            .collect();
        let mut referenced = BTreeSet::new();
        for entry in fs::read_dir(self.manifests_dir())? {
            let entry = entry?;
            let path = entry.path();
            if path == self.manifest_path(checkpoint_id)
                || !entry.file_type()?.is_file()
                || path.extension().is_none_or(|extension| extension != "json")
            {
                continue;
            }
            let other = self.read_manifest_path(&path).with_context(|| {
                format!(
                    "cannot prove artifact ownership while {} is unreadable",
                    path.display()
                )
            })?;
            referenced.extend(
                manifest_artifacts(&other)
                    .into_iter()
                    .map(|artifact| artifact.sha256.clone()),
            );
        }
        fs::remove_file(self.manifest_path(checkpoint_id))?;
        let mut artifacts_removed = 0;
        let mut bytes_removed = 0;
        let mut artifacts_retained = 0;
        for sha256 in target_artifacts {
            if referenced.contains(&sha256) {
                artifacts_retained += 1;
                continue;
            }
            let path = self.objects_dir().join(&sha256[..2]).join(&sha256);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    fs::remove_file(&path)?;
                    artifacts_removed += 1;
                    bytes_removed += metadata.len();
                }
                Ok(_) => bail!("checkpoint object path is not a regular file"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(CheckpointDeleteResult {
            checkpoint_id: manifest.id,
            artifacts_removed,
            bytes_removed,
            artifacts_retained,
        })
    }

    /// Resolve and fully verify the artifacts needed for a historical fork.
    /// The source path is supplied by the live session rather than persisted
    /// in the portable manifest. No worktree is changed here.
    pub fn prepare_fork(
        &self,
        checkpoint_id: &str,
        source_session_id: &str,
        source_repo_path: &Path,
    ) -> Result<CheckpointForkPlan> {
        self.prepare_fork_inner(checkpoint_id, source_session_id, source_repo_path, true)
    }

    /// Verify a checkpoint against a known derived session repository and use
    /// that session as the launch template. The caller must prove provenance;
    /// this layer independently verifies repository identity, commit presence,
    /// and every artifact before returning a plan.
    pub fn prepare_retry_fork(
        &self,
        checkpoint_id: &str,
        template_session_id: &str,
        template_repo_path: &Path,
    ) -> Result<CheckpointForkPlan> {
        self.prepare_fork_inner(
            checkpoint_id,
            template_session_id,
            template_repo_path,
            false,
        )
    }

    fn prepare_fork_inner(
        &self,
        checkpoint_id: &str,
        source_session_id: &str,
        source_repo_path: &Path,
        require_original_session: bool,
    ) -> Result<CheckpointForkPlan> {
        let manifest = self.inspect(checkpoint_id)?;
        if require_original_session && manifest.session.id != source_session_id {
            bail!("checkpoint source session no longer matches the selected session");
        }
        let repository = materializable_repository(&manifest)?;
        if !crate::git::GitWorktree::is_git_repo(source_repo_path) {
            bail!("checkpoint source is no longer a git repository");
        }
        if let Some(identity) = crate::git::get_remote_slug(source_repo_path) {
            if identity != repository.identity {
                bail!(
                    "checkpoint repository identity changed from {} to {}",
                    repository.identity,
                    identity
                );
            }
        }
        let head = repository
            .head
            .as_deref()
            .context("checkpoint repository has no recorded HEAD")?;
        let oid = git2::Oid::from_str(head).context("checkpoint HEAD is not a valid object id")?;
        crate::git::open_repo_at(source_repo_path)?
            .find_commit(oid)
            .with_context(|| {
                format!("checkpoint commit {head} is unavailable in the source repo")
            })?;

        if let Some(artifact) = &repository.patch {
            self.verified_object(artifact, "text/x-diff", MAX_PATCH_BYTES as u64)?;
        }
        if let Some(artifact) = &repository.untracked_archive {
            let path =
                self.verified_object(artifact, "application/x-tar", MAX_UNTRACKED_ARCHIVE_BYTES)?;
            let bytes = fs::read(path)?;
            parse_archive(&bytes)?;
        }

        let mut warnings = manifest.incomplete_reasons.clone();
        if let Some(error) = &repository.error {
            warnings.push(error.clone());
        }
        warnings.sort();
        warnings.dedup();
        let repository_identity = repository.identity.clone();
        Ok(CheckpointForkPlan {
            seed: CheckpointSeed {
                checkpoint_id: manifest.id.clone(),
                repository_head: head.to_string(),
                repository_identity,
                agent: manifest.session.agent.clone(),
                model: manifest.session.model.clone(),
                effort: manifest.session.effort.clone(),
                permission_mode: manifest.session.permission_mode.clone(),
                view: manifest.session.view.clone(),
                warnings,
            },
            source_session_id: source_session_id.to_string(),
            source_title: manifest.session.title.clone(),
            label: manifest.label.clone(),
        })
    }

    /// Verify an in-place restore without changing the target. The checkpoint
    /// is materialized in a disposable detached worktree first, so the later
    /// confirmation never discovers an invalid patch after moving the real
    /// checkout. Restore is unavailable for older manifests that do not carry
    /// exact changed-path names because rollback could not be scoped safely.
    pub fn prepare_restore(
        &self,
        checkpoint_id: &str,
        target: impl AsRef<Path>,
    ) -> Result<CheckpointRestorePlan> {
        let target = target.as_ref();
        let manifest = self.inspect(checkpoint_id)?;
        let repository = materializable_repository(&manifest)?;
        if repository.changed_paths != repository.changed_path_names.len() {
            bail!(
                "checkpoint predates scoped restore metadata; fork it instead of restoring in place"
            );
        }
        for path in &repository.changed_path_names {
            if !is_safe_relative_path(path) {
                bail!("checkpoint contains an unsafe changed path");
            }
        }
        let seed = seed_from_manifest(&manifest, repository)?;
        let original_head = validate_clean_restore_target(target, repository, None)?;

        let main_repo = crate::git::GitWorktree::find_main_repo(target)
            .map_err(|error| anyhow!(error.to_string()))?;
        let temp_root = tempfile::tempdir()?;
        let disposable = temp_root.path().join("restore-preflight");
        let head = repository
            .head
            .as_deref()
            .context("checkpoint repository has no recorded HEAD")?;
        run_git_checked(
            &main_repo,
            [
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "--detach",
                disposable
                    .to_str()
                    .context("restore preflight path is not valid UTF-8")?,
                head,
            ],
            "create restore preflight worktree",
        )?;
        let preflight = self.materialize_new_worktree(&seed, &disposable);
        let cleanup = run_git_checked(
            &main_repo,
            [
                "worktree",
                "remove",
                "--force",
                disposable
                    .to_str()
                    .context("restore preflight path is not valid UTF-8")?,
            ],
            "remove restore preflight worktree",
        );
        preflight.context("checkpoint failed disposable restore preflight")?;
        cleanup?;

        Ok(CheckpointRestorePlan {
            seed,
            label: manifest.label.clone(),
            target: target.to_path_buf(),
            original_head,
            recovery_ref: format!(
                "refs/aoe/recovery/{}/{}-{}",
                checkpoint_id,
                now_ms(),
                uuid::Uuid::new_v4().simple()
            ),
            changed_paths: repository.changed_path_names.clone(),
        })
    }

    /// Execute a previously prepared restore. A recovery ref is created before
    /// HEAD moves. On materialization failure the original commit is restored
    /// and only checkpoint-owned untracked paths are removed. The recovery ref
    /// remains available even after a successful rollback.
    pub fn restore(&self, plan: &CheckpointRestorePlan) -> Result<CheckpointRestoreResult> {
        let _lock = self.exclusive_lock()?;
        let manifest = self.inspect(&plan.seed.checkpoint_id)?;
        let repository = materializable_repository(&manifest)?;
        let expected_seed = seed_from_manifest(&manifest, repository)?;
        if expected_seed != plan.seed || repository.changed_path_names != plan.changed_paths {
            bail!("checkpoint manifest changed after restore preparation");
        }
        validate_clean_restore_target(&plan.target, repository, Some(&plan.original_head))?;
        let repo = crate::git::open_repo_at(&plan.target)?;
        let original_oid = git2::Oid::from_str(&plan.original_head)?;
        repo.reference(
            &plan.recovery_ref,
            original_oid,
            false,
            "aoe checkpoint restore recovery",
        )
        .with_context(|| format!("failed to create recovery ref {}", plan.recovery_ref))?;
        let restored_head = repository
            .head
            .as_deref()
            .context("checkpoint repository has no recorded HEAD")?;
        run_git_checked(
            &plan.target,
            ["reset", "--hard", restored_head],
            "move target to checkpoint HEAD",
        )?;

        if let Err(error) = self.materialize_new_worktree(&plan.seed, &plan.target) {
            let rollback = rollback_restore(&plan.target, &plan.original_head, &plan.changed_paths);
            return match rollback {
                Ok(()) => Err(error.context(format!(
                    "restore failed and was rolled back; recovery ref {} was preserved",
                    plan.recovery_ref
                ))),
                Err(rollback_error) => Err(anyhow!(
                    "restore failed: {error}; rollback also failed: {rollback_error}; recovery ref {} was preserved",
                    plan.recovery_ref
                )),
            };
        }
        Ok(CheckpointRestoreResult {
            checkpoint_id: manifest.id.clone(),
            target: plan.target.clone(),
            restored_head: restored_head.to_string(),
            recovery_ref: plan.recovery_ref.clone(),
            changed_paths: plan.changed_paths.len(),
        })
    }

    /// Recreate one checkpoint inside an isolated worktree that the ordinary
    /// session builder has just created at the recorded commit. This refuses
    /// dirty, mismatched, multi-repository, or partially captured targets. A
    /// caller must delete the new AoE-owned worktree if an error is returned;
    /// the source worktree is never touched.
    pub(crate) fn materialize_new_worktree(
        &self,
        seed: &CheckpointSeed,
        target: &Path,
    ) -> Result<()> {
        let manifest = self.inspect(&seed.checkpoint_id)?;
        let repository = materializable_repository(&manifest)?;
        let head = repository
            .head
            .as_deref()
            .context("checkpoint repository has no recorded HEAD")?;
        if head != seed.repository_head || repository.identity != seed.repository_identity {
            bail!("checkpoint manifest changed after fork preparation");
        }
        if !crate::git::GitWorktree::is_git_repo(target) {
            bail!("checkpoint target is not a git worktree");
        }
        let target_repo = crate::git::open_repo_at(target)?;
        let target_head = target_repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .map(|commit| commit.id().to_string())
            .context("checkpoint target has no commit at HEAD")?;
        if target_head != head {
            bail!("checkpoint target starts at {target_head}, expected recorded HEAD {head}");
        }
        if let Some(identity) = crate::git::get_remote_slug(target) {
            if identity != repository.identity {
                bail!(
                    "checkpoint target repository is {}, expected {}",
                    identity,
                    repository.identity
                );
            }
        }
        let before = changed_paths(&target_repo)?;
        if !before.tracked.is_empty() || !before.untracked.is_empty() {
            bail!("checkpoint target is not a clean, fresh worktree");
        }

        let patch_path = repository
            .patch
            .as_ref()
            .map(|artifact| self.verified_object(artifact, "text/x-diff", MAX_PATCH_BYTES as u64))
            .transpose()?;
        let archive_files = repository
            .untracked_archive
            .as_ref()
            .map(|artifact| {
                self.verified_object(artifact, "application/x-tar", MAX_UNTRACKED_ARCHIVE_BYTES)
                    .and_then(|path| fs::read(path).map_err(Into::into))
                    .and_then(|bytes| parse_archive(&bytes))
            })
            .transpose()?
            .unwrap_or_default();
        preflight_archive_targets(target, &archive_files, patch_path.is_some())?;

        if let Some(patch_path) = &patch_path {
            run_git_apply(target, patch_path, true)?;
            run_git_apply(target, patch_path, false)?;
        }
        preflight_archive_targets(target, &archive_files, false)?;
        extract_archive_files(target, archive_files)?;

        let after = changed_paths(&target_repo)?;
        let restored_paths = after
            .tracked
            .iter()
            .chain(after.untracked.iter())
            .collect::<BTreeSet<_>>()
            .len();
        if restored_paths != repository.changed_paths {
            bail!(
                "checkpoint restored {restored_paths} changed paths, expected {}",
                repository.changed_paths
            );
        }
        let head_after = target_repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .map(|commit| commit.id().to_string())
            .context("checkpoint target lost HEAD during materialization")?;
        if head_after != head {
            bail!("checkpoint target HEAD changed during materialization");
        }
        Ok(())
    }

    fn capture_repository(&self, request: &RepositoryCapture) -> Result<RepositoryManifest> {
        if !crate::git::GitWorktree::is_git_repo(&request.path) {
            bail!("not a git repository");
        }
        let repository = crate::git::open_repo_at(&request.path)
            .with_context(|| format!("failed to open {}", request.path.display()))?;
        let head_before = repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .map(|commit| commit.id().to_string())
            .context("repository has no commit at HEAD")?;
        let identity =
            crate::git::get_remote_slug(&request.path).unwrap_or_else(|| request.name.clone());
        let base_ref = request
            .base_ref
            .clone()
            .or_else(|| crate::git::diff::get_default_base_ref(&request.path).ok());
        let mut metadata_warnings = Vec::new();
        let base_commit = match base_ref.as_deref() {
            Some(base) => match repository
                .revparse_single(base)
                .and_then(|object| object.peel_to_commit())
            {
                Ok(commit) => Some(commit.id().to_string()),
                Err(error) => {
                    metadata_warnings.push(format!("failed to resolve base ref {base}: {error}"));
                    None
                }
            },
            None => {
                metadata_warnings.push("no base ref is available".to_string());
                None
            }
        };

        let before = changed_paths(&repository)?;
        let mut excluded_paths: Vec<String> = before
            .tracked
            .iter()
            .chain(before.untracked.iter())
            .filter(|path| is_sensitive_path(path) || !is_safe_relative_path(path))
            .cloned()
            .collect();
        excluded_paths.sort();
        excluded_paths.dedup();
        let safe_tracked: Vec<String> = before
            .tracked
            .iter()
            .filter(|path| !excluded_paths.contains(path))
            .cloned()
            .collect();
        let mut safe_untracked: Vec<String> = before
            .untracked
            .iter()
            .filter(|path| !excluded_paths.contains(path))
            .cloned()
            .collect();
        safe_untracked.retain(|path| {
            let supported = fs::symlink_metadata(request.path.join(path))
                .is_ok_and(|metadata| metadata.file_type().is_file());
            if !supported {
                excluded_paths.push(path.clone());
            }
            supported
        });
        excluded_paths.sort();
        excluded_paths.dedup();

        let stamp_before =
            worktree_stamp(&repository, &request.path, &safe_tracked, &safe_untracked);
        let patch_bytes = tracked_patch(&request.path, &head_before, &safe_tracked)?;
        let untracked_bytes = untracked_archive(&request.path, &safe_untracked)?;
        let after = changed_paths(&repository)?;
        let head_after = repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .map(|commit| commit.id().to_string())
            .context("repository lost HEAD during capture")?;
        let stamp_after =
            worktree_stamp(&repository, &request.path, &safe_tracked, &safe_untracked);
        if head_before != head_after || before != after || stamp_before != stamp_after {
            bail!("worktree changed during capture; retry when the current write finishes");
        }

        let patch = if patch_bytes.is_empty() {
            None
        } else {
            Some(self.store_object(&patch_bytes, "text/x-diff")?)
        };
        let untracked_archive = if untracked_bytes.is_empty() {
            None
        } else {
            Some(self.store_object(&untracked_bytes, "application/x-tar")?)
        };
        Ok(RepositoryManifest {
            name: request.name.clone(),
            identity,
            base_ref,
            base_commit,
            head: Some(head_before),
            changed_paths: before
                .tracked
                .iter()
                .chain(before.untracked.iter())
                .collect::<BTreeSet<_>>()
                .len(),
            changed_path_names: before
                .tracked
                .iter()
                .chain(before.untracked.iter())
                .filter(|path| !excluded_paths.contains(path))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            patch,
            untracked_archive,
            excluded_paths,
            error: (!metadata_warnings.is_empty()).then(|| metadata_warnings.join("; ")),
        })
    }

    fn store_object(&self, bytes: &[u8], media_type: &str) -> Result<ArtifactRef> {
        let sha256 = sha256_hex(bytes);
        let shard = &sha256[..2];
        let dir = self.objects_dir().join(shard);
        fs::create_dir_all(&dir)?;
        let path = dir.join(&sha256);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                let existing = fs::read(&path)?;
                if Sha256::digest(&existing) != Sha256::digest(bytes) {
                    bail!(
                        "checkpoint object {} does not match its content hash",
                        sha256
                    );
                }
            }
            Ok(_) => bail!("checkpoint object path is not a regular file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::session::atomic_write(&path, bytes)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(ArtifactRef {
            sha256,
            bytes: bytes.len() as u64,
            media_type: media_type.to_string(),
        })
    }

    fn verified_object(
        &self,
        artifact: &ArtifactRef,
        media_type: &str,
        max_bytes: u64,
    ) -> Result<PathBuf> {
        if artifact.media_type != media_type {
            bail!(
                "checkpoint artifact {} has media type {}, expected {media_type}",
                artifact.sha256,
                artifact.media_type
            );
        }
        validate_sha256(&artifact.sha256)?;
        if artifact.bytes > max_bytes {
            bail!(
                "checkpoint artifact {} exceeds the {} byte limit",
                artifact.sha256,
                max_bytes
            );
        }
        let path = self
            .objects_dir()
            .join(&artifact.sha256[..2])
            .join(&artifact.sha256);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("checkpoint artifact {} is missing", artifact.sha256))?;
        if !metadata.file_type().is_file() {
            bail!("checkpoint artifact path is not a regular file");
        }
        if metadata.len() != artifact.bytes {
            bail!(
                "checkpoint artifact {} size changed from {} to {} bytes",
                artifact.sha256,
                artifact.bytes,
                metadata.len()
            );
        }
        if sha256_file(&path)? != artifact.sha256 {
            bail!(
                "checkpoint artifact {} does not match its content hash",
                artifact.sha256
            );
        }
        Ok(path)
    }

    fn write_manifest(&self, manifest: &CheckpointManifest) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(manifest)?;
        crate::session::atomic_write(&self.manifest_path(&manifest.id), &bytes)
    }

    fn read_manifest_path(&self, path: &Path) -> Result<CheckpointManifest> {
        let metadata = fs::metadata(path)?;
        if metadata.len() > MAX_MANIFEST_BYTES {
            bail!("manifest exceeds {} bytes", MAX_MANIFEST_BYTES);
        }
        let bytes = fs::read(path)?;
        let manifest: CheckpointManifest = serde_json::from_slice(&bytes)?;
        validate_manifest(&manifest)?;
        if path
            .file_stem()
            .is_some_and(|stem| stem != manifest.id.as_str())
        {
            bail!("manifest id does not match its filename");
        }
        Ok(manifest)
    }

    fn manifests_dir(&self) -> PathBuf {
        self.root.join(MANIFESTS_SUBDIR)
    }

    fn objects_dir(&self) -> PathBuf {
        self.root.join(OBJECTS_SUBDIR)
    }

    fn exports_dir(&self) -> PathBuf {
        self.root.join(EXPORTS_SUBDIR)
    }

    fn manifest_path(&self, checkpoint_id: &str) -> PathBuf {
        self.manifests_dir().join(format!("{checkpoint_id}.json"))
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

fn validate_manifest(manifest: &CheckpointManifest) -> Result<()> {
    if manifest.schema_version != CHECKPOINT_SCHEMA_VERSION {
        bail!("unsupported checkpoint schema {}", manifest.schema_version);
    }
    validate_checkpoint_id(&manifest.id)?;
    if manifest.label.chars().count() > MAX_CHECKPOINT_LABEL_CHARS {
        bail!("checkpoint label exceeds the supported length");
    }
    for repository in &manifest.repositories {
        if repository.changed_path_names.len() > MAX_UNTRACKED_FILES {
            bail!("checkpoint changed-path list exceeds the supported limit");
        }
        for path in &repository.changed_path_names {
            if !is_safe_relative_path(path) {
                bail!("checkpoint changed-path list contains an unsafe path");
            }
        }
        for artifact in [&repository.patch, &repository.untracked_archive]
            .into_iter()
            .flatten()
        {
            validate_sha256(&artifact.sha256)?;
            if artifact.bytes > artifact_limit(&artifact.media_type)? {
                bail!("checkpoint artifact {} exceeds its limit", artifact.sha256);
            }
        }
    }
    Ok(())
}

fn manifest_artifacts(manifest: &CheckpointManifest) -> Vec<&ArtifactRef> {
    manifest
        .repositories
        .iter()
        .flat_map(|repository| {
            [
                repository.patch.as_ref(),
                repository.untracked_archive.as_ref(),
            ]
            .into_iter()
            .flatten()
        })
        .collect()
}

fn artifact_limit(media_type: &str) -> Result<u64> {
    match media_type {
        "text/x-diff" => Ok(MAX_PATCH_BYTES as u64),
        "application/x-tar" => Ok(MAX_UNTRACKED_ARCHIVE_BYTES),
        other => bail!("unsupported checkpoint artifact media type {other}"),
    }
}

fn append_bundle_bytes<W: Write>(
    archive: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes)?;
    Ok(())
}

fn append_bundle_file<W: Write>(
    archive: &mut tar::Builder<W>,
    path: &str,
    source: &Path,
) -> Result<()> {
    let bytes = fs::read(source)?;
    append_bundle_bytes(archive, path, &bytes)
}

fn read_bundle_entries(bytes: &[u8]) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut entries = std::collections::BTreeMap::new();
    let mut total = 0_u64;
    for entry in archive
        .entries()
        .context("checkpoint bundle is unreadable")?
    {
        if entries.len() >= MAX_BUNDLE_ENTRIES {
            bail!("checkpoint bundle has too many entries");
        }
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            bail!("checkpoint bundle contains a non-regular entry");
        }
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        let path = path
            .to_str()
            .context("checkpoint bundle path is not valid UTF-8")?
            .to_string();
        let declared = entry.header().size()?;
        total = total.saturating_add(declared);
        if total > MAX_BUNDLE_BYTES {
            bail!("checkpoint bundle expands beyond the 160 MiB limit");
        }
        let mut contents = Vec::with_capacity(declared.min(usize::MAX as u64) as usize);
        entry.read_to_end(&mut contents)?;
        if contents.len() as u64 != declared || entries.insert(path.clone(), contents).is_some() {
            bail!("checkpoint bundle contains an invalid or duplicate entry {path}");
        }
    }
    Ok(entries)
}

fn portable_filename_component(label: &str) -> String {
    let component: String = label
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() {
                Some(character.to_ascii_lowercase())
            } else if matches!(character, '-' | '_') || character.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .take(48)
        .collect();
    let component = component.trim_matches('-');
    if component.is_empty() {
        "checkpoint".to_string()
    } else {
        component.to_string()
    }
}

fn seed_from_manifest(
    manifest: &CheckpointManifest,
    repository: &RepositoryManifest,
) -> Result<CheckpointSeed> {
    let head = repository
        .head
        .as_deref()
        .context("checkpoint repository has no recorded HEAD")?;
    let mut warnings = manifest.incomplete_reasons.clone();
    if let Some(error) = &repository.error {
        warnings.push(error.clone());
    }
    warnings.sort();
    warnings.dedup();
    Ok(CheckpointSeed {
        checkpoint_id: manifest.id.clone(),
        repository_head: head.to_string(),
        repository_identity: repository.identity.clone(),
        agent: manifest.session.agent.clone(),
        model: manifest.session.model.clone(),
        effort: manifest.session.effort.clone(),
        permission_mode: manifest.session.permission_mode.clone(),
        view: manifest.session.view.clone(),
        warnings,
    })
}

fn validate_clean_restore_target(
    target: &Path,
    repository: &RepositoryManifest,
    expected_head: Option<&str>,
) -> Result<String> {
    if !crate::git::GitWorktree::is_git_repo(target) {
        bail!("checkpoint restore target is not a git worktree");
    }
    if let Some(identity) = crate::git::get_remote_slug(target) {
        if identity != repository.identity {
            bail!(
                "checkpoint restore target is {}, expected {}",
                identity,
                repository.identity
            );
        }
    }
    let repo = crate::git::open_repo_at(target)?;
    let head = repo
        .head()
        .and_then(|head| head.peel_to_commit())
        .map(|commit| commit.id().to_string())
        .context("checkpoint restore target has no commit at HEAD")?;
    if expected_head.is_some_and(|expected| expected != head) {
        bail!("checkpoint restore target HEAD changed after preview");
    }
    let paths = changed_paths(&repo)?;
    if !paths.tracked.is_empty() || !paths.untracked.is_empty() {
        bail!("checkpoint restore refuses a dirty target; commit, stash, or fork instead");
    }
    Ok(head)
}

fn run_git_checked<I, S>(repo: &Path, args: I, operation: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output =
        crate::git::command::run_git_with_timeout(repo, args, GIT_TIMEOUT)?.ok_or_else(|| {
            anyhow!(
                "{operation} timed out after {} seconds",
                GIT_TIMEOUT.as_secs()
            )
        })?;
    if !output.status.success() {
        bail!("{operation} failed: {}", compact_stderr(&output.stderr));
    }
    Ok(())
}

fn rollback_restore(target: &Path, original_head: &str, expected_paths: &[String]) -> Result<()> {
    run_git_checked(
        target,
        ["reset", "--hard", original_head],
        "restore original HEAD",
    )?;
    let repo = crate::git::open_repo_at(target)?;
    for relative in expected_paths {
        let status = match repo.status_file(Path::new(relative)) {
            Ok(status) => status,
            Err(_) => continue,
        };
        if !status.contains(git2::Status::WT_NEW) {
            continue;
        }
        let path = target.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => fs::remove_file(&path)?,
            Ok(_) => bail!(
                "rollback found non-regular checkpoint path {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    for relative in expected_paths.iter().rev() {
        let mut parent = target.join(relative).parent().map(Path::to_path_buf);
        while let Some(directory) = parent {
            if directory == target || !directory.starts_with(target) {
                break;
            }
            if fs::remove_dir(&directory).is_err() {
                break;
            }
            parent = directory.parent().map(Path::to_path_buf);
        }
    }
    Ok(())
}

fn materializable_repository(manifest: &CheckpointManifest) -> Result<&RepositoryManifest> {
    if matches!(manifest.state, ManifestState::Materializing) {
        bail!("checkpoint capture has not finished materializing");
    }
    let [repository] = manifest.repositories.as_slice() else {
        bail!("historical forks currently require exactly one repository");
    };
    if !repository.excluded_paths.is_empty() {
        bail!(
            "checkpoint excluded {} path(s), so it cannot recreate the recorded worktree exactly",
            repository.excluded_paths.len()
        );
    }
    let head = repository
        .head
        .as_deref()
        .context("checkpoint repository capture did not record HEAD")?;
    git2::Oid::from_str(head).context("checkpoint repository HEAD is invalid")?;
    if repository.changed_paths > 0
        && repository.patch.is_none()
        && repository.untracked_archive.is_none()
    {
        bail!("checkpoint has changed paths but no restorable artifacts");
    }
    Ok(repository)
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("checkpoint artifact has an invalid SHA-256 id");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn run_git_apply(target: &Path, patch: &Path, check_only: bool) -> Result<()> {
    let mut args = vec![
        OsString::from("apply"),
        OsString::from("--binary"),
        OsString::from("--whitespace=nowarn"),
    ];
    if check_only {
        args.push(OsString::from("--check"));
    }
    args.push(patch.as_os_str().to_owned());
    let output =
        crate::git::command::run_git_with_timeout(target, args, GIT_TIMEOUT)?.ok_or_else(|| {
            anyhow!(
                "git apply timed out after {} seconds",
                GIT_TIMEOUT.as_secs()
            )
        })?;
    if !output.status.success() {
        let operation = if check_only { "preflight" } else { "apply" };
        bail!(
            "checkpoint patch {operation} failed: {}",
            compact_stderr(&output.stderr)
        );
    }
    Ok(())
}

fn parse_archive(bytes: &[u8]) -> Result<Vec<ArchiveFile>> {
    let mut archive = tar::Archive::new(bytes);
    let mut files = Vec::new();
    let mut paths = BTreeSet::new();
    let mut total_bytes = 0_u64;
    for entry in archive
        .entries()
        .context("checkpoint archive is unreadable")?
    {
        let mut entry = entry.context("checkpoint archive entry is unreadable")?;
        if files.len() >= MAX_UNTRACKED_FILES {
            bail!(
                "checkpoint archive exceeds the {} file limit",
                MAX_UNTRACKED_FILES
            );
        }
        if !entry.header().entry_type().is_file() {
            bail!("checkpoint archive contains a non-regular entry");
        }
        let path = entry
            .path()
            .context("checkpoint archive path is invalid")?
            .into_owned();
        validate_archive_path(&path)?;
        if !paths.insert(path.clone()) {
            bail!("checkpoint archive repeats path {}", path.display());
        }
        let declared = entry
            .header()
            .size()
            .context("checkpoint archive entry size is invalid")?;
        total_bytes = total_bytes.saturating_add(declared);
        if total_bytes > MAX_UNTRACKED_BYTES {
            bail!("checkpoint archive content exceeds the 64 MiB limit");
        }
        let mode = entry
            .header()
            .mode()
            .context("checkpoint archive entry mode is invalid")?;
        let mut contents = Vec::with_capacity(declared.min(usize::MAX as u64) as usize);
        entry.read_to_end(&mut contents)?;
        if contents.len() as u64 != declared {
            bail!("checkpoint archive entry size does not match its header");
        }
        files.push(ArchiveFile {
            path,
            bytes: contents,
            mode,
        });
    }
    Ok(files)
}

fn validate_archive_path(path: &Path) -> Result<()> {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        bail!("checkpoint archive contains an unsafe path");
    };
    if first == ".git" {
        bail!("checkpoint archive cannot write repository metadata");
    }
    if !components.all(|component| matches!(component, Component::Normal(_))) {
        bail!("checkpoint archive contains an unsafe path");
    }
    Ok(())
}

fn preflight_archive_targets(
    target: &Path,
    files: &[ArchiveFile],
    patch_may_remove_existing: bool,
) -> Result<()> {
    for file in files {
        ensure_existing_parents_are_directories(target, &file.path)?;
        match fs::symlink_metadata(target.join(&file.path)) {
            Ok(_) if patch_may_remove_existing => {}
            Ok(_) => bail!(
                "checkpoint untracked path already exists: {}",
                file.path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn ensure_existing_parents_are_directories(target: &Path, path: &Path) -> Result<()> {
    let mut current = target.to_path_buf();
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    for component in parent.components() {
        let Component::Normal(part) = component else {
            bail!("checkpoint archive contains an unsafe parent path");
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => bail!(
                "checkpoint archive parent is not a directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn extract_archive_files(target: &Path, files: Vec<ArchiveFile>) -> Result<()> {
    for file in files {
        let destination = target.join(&file.path);
        let parent = destination
            .parent()
            .context("checkpoint archive entry has no parent")?;
        ensure_existing_parents_are_directories(target, &file.path)?;
        fs::create_dir_all(parent)?;
        ensure_existing_parents_are_directories(target, &file.path)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .with_context(|| format!("failed to create checkpoint path {}", file.path.display()))?;
        output.write_all(&file.bytes)?;
        output.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(file.mode & 0o777))?;
        }
    }
    Ok(())
}

fn empty_repository_manifest(request: &RepositoryCapture) -> RepositoryManifest {
    RepositoryManifest {
        name: request.name.clone(),
        identity: request.name.clone(),
        base_ref: request.base_ref.clone(),
        base_commit: None,
        head: None,
        changed_paths: 0,
        changed_path_names: Vec::new(),
        patch: None,
        untracked_archive: None,
        excluded_paths: Vec::new(),
        error: None,
    }
}

fn summary_from_manifest(
    manifest: &CheckpointManifest,
    loaded_from_disk: bool,
) -> CheckpointSummary {
    let state = match (&manifest.state, loaded_from_disk) {
        (ManifestState::Materializing, true) => CheckpointState::Incomplete,
        (ManifestState::Materializing, false) => CheckpointState::Materializing,
        (ManifestState::Complete, _) => CheckpointState::Complete,
        (ManifestState::Incomplete, _) => CheckpointState::Incomplete,
    };
    let detail = if matches!(manifest.state, ManifestState::Materializing) && loaded_from_disk {
        Some("capture was interrupted before materialization completed".to_string())
    } else {
        manifest.incomplete_reasons.first().cloned()
    };
    let repository = manifest.repositories.first();
    let mut snapshot_ref = repository
        .and_then(|repo| repo.head.as_deref())
        .map(short_hash)
        .unwrap_or_else(|| "metadata-only".to_string());
    if let Some(patch) = repository.and_then(|repo| repo.patch.as_ref()) {
        snapshot_ref.push('+');
        snapshot_ref.push_str(&short_hash(&patch.sha256));
    }
    CheckpointSummary {
        id: manifest.id.clone(),
        session_id: manifest.session.id.clone(),
        session_title: manifest.session.title.clone(),
        label: manifest.label.clone(),
        created_at_ms: manifest.created_at_ms,
        snapshot_ref,
        state,
        detail,
    }
}

fn normalize_label(label: &str) -> String {
    let normalized = label.split_whitespace().collect::<Vec<_>>().join(" ");
    let value = if normalized.is_empty() {
        "Manual checkpoint".to_string()
    } else {
        normalized
    };
    value.chars().take(MAX_CHECKPOINT_LABEL_CHARS).collect()
}

fn validate_checkpoint_id(checkpoint_id: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(checkpoint_id)
        .with_context(|| format!("invalid checkpoint id {checkpoint_id}"))?;
    if parsed.to_string() != checkpoint_id.to_ascii_lowercase() {
        bail!("checkpoint id is not canonical");
    }
    Ok(())
}

fn short_hash(value: &str) -> String {
    value.chars().take(12).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChangedPaths {
    tracked: Vec<String>,
    untracked: Vec<String>,
}

fn changed_paths(repository: &git2::Repository) -> Result<ChangedPaths> {
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .include_unmodified(false);
    let statuses = repository.statuses(Some(&mut options))?;
    let mut tracked = BTreeSet::new();
    let mut untracked = BTreeSet::new();
    for entry in statuses.iter() {
        let path = entry
            .path()
            .context("repository contains a changed path that is not valid UTF-8")?;
        let status = entry.status();
        if status.contains(git2::Status::WT_NEW) {
            untracked.insert(path.to_string());
        }
        if status != git2::Status::WT_NEW {
            tracked.insert(path.to_string());
            for delta in [entry.head_to_index(), entry.index_to_workdir()]
                .into_iter()
                .flatten()
            {
                for file in [delta.old_file(), delta.new_file()] {
                    if let Some(delta_path) = file.path() {
                        let delta_path = delta_path.to_str().context(
                            "repository contains a changed path that is not valid UTF-8",
                        )?;
                        if !delta_path.is_empty() {
                            tracked.insert(delta_path.to_string());
                        }
                    }
                }
            }
        }
    }
    Ok(ChangedPaths {
        tracked: tracked.into_iter().collect(),
        untracked: untracked.into_iter().collect(),
    })
}

fn tracked_patch(repo_path: &Path, head: &str, paths: &[String]) -> Result<Vec<u8>> {
    let mut patch = Vec::new();
    for chunk in paths.chunks(128) {
        let mut args = vec![
            OsString::from("diff"),
            OsString::from("--binary"),
            OsString::from("--full-index"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-renames"),
            OsString::from(head),
            OsString::from("--"),
        ];
        args.extend(chunk.iter().map(OsString::from));
        let output = crate::git::command::run_git_with_timeout(repo_path, args, GIT_TIMEOUT)?
            .ok_or_else(|| anyhow!("git diff timed out after {} seconds", GIT_TIMEOUT.as_secs()))?;
        if !output.status.success() {
            bail!("git diff failed: {}", compact_stderr(&output.stderr));
        }
        if patch.len().saturating_add(output.stdout.len()) > MAX_PATCH_BYTES {
            bail!("tracked patch exceeds the 64 MiB checkpoint limit");
        }
        patch.extend_from_slice(&output.stdout);
    }
    Ok(patch)
}

fn untracked_archive(repo_path: &Path, paths: &[String]) -> Result<Vec<u8>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if paths.len() > MAX_UNTRACKED_FILES {
        bail!(
            "untracked files exceed the {} file checkpoint limit",
            MAX_UNTRACKED_FILES
        );
    }
    let mut total = 0_u64;
    for path in paths {
        total = total.saturating_add(fs::symlink_metadata(repo_path.join(path))?.len());
    }
    if total > MAX_UNTRACKED_BYTES {
        bail!("untracked files exceed the 64 MiB checkpoint limit");
    }
    let mut bytes = Vec::new();
    {
        let mut archive = tar::Builder::new(&mut bytes);
        archive.mode(tar::HeaderMode::Deterministic);
        archive.follow_symlinks(false);
        for relative in paths {
            archive
                .append_path_with_name(repo_path.join(relative), relative)
                .with_context(|| format!("failed to archive untracked path {relative}"))?;
        }
        archive.finish()?;
    }
    if bytes.len() as u64 > MAX_UNTRACKED_ARCHIVE_BYTES {
        bail!("untracked archive exceeds the 80 MiB checkpoint limit");
    }
    Ok(bytes)
}

fn compact_stderr(stderr: &[u8]) -> String {
    let value = String::from_utf8_lossy(stderr);
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "unknown git error".to_string()
    } else {
        trimmed.chars().take(240).collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorktreeStamp {
    index: Option<FileStamp>,
    paths: Vec<(String, Option<FileStamp>)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    modified_ns: u128,
    kind: u8,
}

fn worktree_stamp(
    repository: &git2::Repository,
    repo_path: &Path,
    tracked: &[String],
    untracked: &[String],
) -> WorktreeStamp {
    let mut paths: Vec<(String, Option<FileStamp>)> = tracked
        .iter()
        .chain(untracked.iter())
        .map(|relative| (relative.clone(), file_stamp(&repo_path.join(relative))))
        .collect();
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    WorktreeStamp {
        index: file_stamp(&repository.path().join("index")),
        paths,
    }
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::symlink_metadata(path).ok()?;
    let kind = if metadata.file_type().is_file() {
        1
    } else if metadata.file_type().is_dir() {
        2
    } else if metadata.file_type().is_symlink() {
        3
    } else {
        4
    };
    Some(FileStamp {
        len: metadata.len(),
        modified_ns: metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos()),
        kind,
    })
}

fn is_safe_relative_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn is_sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    Path::new(&lower).components().any(|component| {
        let Component::Normal(part) = component else {
            return false;
        };
        let part = part.to_string_lossy();
        part == ".env"
            || part.starts_with(".env.")
            || matches!(
                part.as_ref(),
                ".aws"
                    | ".ssh"
                    | ".gnupg"
                    | ".netrc"
                    | ".npmrc"
                    | ".pypirc"
                    | "credentials"
                    | "credentials.json"
                    | "id_rsa"
                    | "id_ed25519"
                    | "plugin-data"
                    | "plugin_payloads"
            )
            || part.ends_with(".pem")
            || part.ends_with(".key")
            || part.ends_with(".p12")
            || part.ends_with(".pfx")
            || part.starts_with("secrets.")
            || part.starts_with("service-account") && part.ends_with(".json")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};

    fn committed_repo(root: &Path) -> Repository {
        let repository = Repository::init(root).unwrap();
        fs::write(root.join("tracked.txt"), "one\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        let tree_id = index.write_tree().unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = Signature::now("AoE Test", "aoe@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        repository
    }

    fn request(repo: &Path, label: &str) -> CaptureRequest {
        CaptureRequest {
            checkpoint_id: uuid::Uuid::new_v4().to_string(),
            label: normalize_label(label),
            created_at_ms: 10,
            session: SessionMetadata {
                id: "session-1".to_string(),
                title: "Carthage".to_string(),
                generation: 4,
                agent: "claude".to_string(),
                model: Some("model".to_string()),
                effort: None,
                view: "terminal".to_string(),
                permission_mode: "default".to_string(),
                conversation: None,
                sandbox: None,
            },
            repositories: vec![RepositoryCapture {
                name: "repo".to_string(),
                path: repo.to_path_buf(),
                base_ref: None,
            }],
            campaign_id: None,
            trial_id: None,
            verification: Vec::new(),
            reported_tokens: None,
        }
    }

    #[test]
    fn captures_tracked_and_untracked_content_as_hashed_objects() {
        let repo_dir = tempfile::tempdir().unwrap();
        let _repository = committed_repo(repo_dir.path());
        fs::write(repo_dir.path().join("tracked.txt"), "two\n").unwrap();
        fs::write(repo_dir.path().join("new.txt"), "new\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();

        let mut request = request(repo_dir.path(), " before\ntrial ");
        request.verification.push(VerificationMetadata {
            kind: "diff".to_string(),
            state: "passed".to_string(),
            updated_at_ms: 9,
            summary: "2 changed files".to_string(),
            source: "fleet_snapshot".to_string(),
        });
        let summary = store.capture(request).unwrap();
        let manifest = store.inspect(&summary.id).unwrap();

        assert_eq!(summary.state, CheckpointState::Complete);
        assert_eq!(manifest.label, "before trial");
        assert_eq!(manifest.repositories[0].changed_paths, 2);
        assert_eq!(
            manifest.repositories[0].changed_path_names,
            ["new.txt".to_string(), "tracked.txt".to_string()]
        );
        assert!(manifest.repositories[0].patch.is_some());
        assert!(manifest.repositories[0].untracked_archive.is_some());
        assert_eq!(manifest.verification[0].kind, "diff");
        assert!(summary.snapshot_ref.contains('+'));
    }

    #[test]
    fn materializes_a_verified_checkpoint_into_a_clean_clone() {
        let repo_dir = tempfile::tempdir().unwrap();
        let _repository = committed_repo(repo_dir.path());
        fs::write(repo_dir.path().join("tracked.txt"), "two\n").unwrap();
        fs::create_dir(repo_dir.path().join("nested")).unwrap();
        fs::write(repo_dir.path().join("nested/new.txt"), "new\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();
        let summary = store.capture(request(repo_dir.path(), "fork me")).unwrap();
        let plan = store
            .prepare_fork(&summary.id, "session-1", repo_dir.path())
            .unwrap();

        let target_parent = tempfile::tempdir().unwrap();
        let target = target_parent.path().join("fork");
        Repository::clone(repo_dir.path().to_str().unwrap(), &target).unwrap();
        assert!(
            target.join("tracked.txt").exists(),
            "clean clone must check out the recorded HEAD"
        );
        store.materialize_new_worktree(&plan.seed, &target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("tracked.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("nested/new.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(repo_dir.path().join("tracked.txt")).unwrap(),
            "two\n",
            "materialization must not modify the source"
        );
    }

    #[test]
    fn materializes_a_tracked_path_recreated_as_untracked() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repository = committed_repo(repo_dir.path());
        let mut index = repository.index().unwrap();
        index.remove_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        fs::write(repo_dir.path().join("tracked.txt"), "replacement\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();
        let summary = store
            .capture(request(repo_dir.path(), "recreated path"))
            .unwrap();
        let plan = store
            .prepare_fork(&summary.id, "session-1", repo_dir.path())
            .unwrap();
        let target_parent = tempfile::tempdir().unwrap();
        let target = target_parent.path().join("fork");
        Repository::clone(repo_dir.path().to_str().unwrap(), &target).unwrap();

        store.materialize_new_worktree(&plan.seed, &target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("tracked.txt")).unwrap(),
            "replacement\n"
        );
    }

    #[test]
    fn fork_preparation_detects_a_tampered_artifact() {
        let repo_dir = tempfile::tempdir().unwrap();
        let _repository = committed_repo(repo_dir.path());
        fs::write(repo_dir.path().join("tracked.txt"), "two\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();
        let summary = store.capture(request(repo_dir.path(), "tamper")).unwrap();
        let manifest = store.inspect(&summary.id).unwrap();
        let artifact = manifest.repositories[0].patch.as_ref().unwrap();
        let object_path = store
            .objects_dir()
            .join(&artifact.sha256[..2])
            .join(&artifact.sha256);
        fs::write(&object_path, vec![b'x'; artifact.bytes as usize]).unwrap();

        let error = store
            .prepare_fork(&summary.id, "session-1", repo_dir.path())
            .unwrap_err();

        assert!(error.to_string().contains("content hash"));
    }

    #[test]
    fn materialization_refuses_a_dirty_target_before_applying_artifacts() {
        let repo_dir = tempfile::tempdir().unwrap();
        let _repository = committed_repo(repo_dir.path());
        fs::write(repo_dir.path().join("tracked.txt"), "two\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();
        let summary = store
            .capture(request(repo_dir.path(), "dirty target"))
            .unwrap();
        let plan = store
            .prepare_fork(&summary.id, "session-1", repo_dir.path())
            .unwrap();
        let target_parent = tempfile::tempdir().unwrap();
        let target = target_parent.path().join("fork");
        Repository::clone(repo_dir.path().to_str().unwrap(), &target).unwrap();
        fs::write(target.join("local.txt"), "keep\n").unwrap();

        let error = store
            .materialize_new_worktree(&plan.seed, &target)
            .unwrap_err();

        assert!(error.to_string().contains("not a clean"));
        assert_eq!(
            fs::read_to_string(target.join("tracked.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("local.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn sensitive_paths_are_excluded_and_make_exact_restore_incomplete() {
        let repo_dir = tempfile::tempdir().unwrap();
        let _repository = committed_repo(repo_dir.path());
        fs::write(repo_dir.path().join(".env"), "TOKEN=secret\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();

        let summary = store.capture(request(repo_dir.path(), "safe")).unwrap();
        let manifest = store.inspect(&summary.id).unwrap();

        assert_eq!(summary.state, CheckpointState::Incomplete);
        assert_eq!(manifest.repositories[0].excluded_paths, [".env"]);
        assert!(manifest.incomplete_reasons[0].contains("excluded"));
        assert!(!serde_json::to_string(&manifest).unwrap().contains("TOKEN"));
        assert!(store
            .prepare_fork(&summary.id, "session-1", repo_dir.path())
            .unwrap_err()
            .to_string()
            .contains("cannot recreate"));
    }

    #[test]
    fn loading_keeps_valid_history_when_one_manifest_is_corrupt() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let _repository = committed_repo(repo_dir.path());
        store.capture(request(repo_dir.path(), "valid")).unwrap();
        fs::write(store.manifests_dir().join("broken.json"), "{").unwrap();

        let report = store.load().unwrap();

        assert_eq!(report.checkpoints.len(), 1);
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn structured_conversation_without_an_event_cutoff_is_explicitly_incomplete() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let _repository = committed_repo(repo_dir.path());
        let mut request = request(repo_dir.path(), "structured");
        request.session.view = "structured".to_string();
        request.session.conversation = Some(ConversationIdentity {
            kind: "acp".to_string(),
            id: "conversation-1".to_string(),
            event_sequence: None,
        });

        let summary = store.capture(request).unwrap();
        let manifest = store.inspect(&summary.id).unwrap();

        assert_eq!(summary.state, CheckpointState::Incomplete);
        assert!(manifest.incomplete_reasons[0].contains("event sequence"));
        assert!(manifest.repositories[0].head.is_some());
    }

    #[test]
    fn export_import_and_reference_counted_delete_preserve_shared_objects() {
        let repo_dir = tempfile::tempdir().unwrap();
        let _repository = committed_repo(repo_dir.path());
        fs::write(repo_dir.path().join("tracked.txt"), "two\n").unwrap();
        fs::write(repo_dir.path().join("new.txt"), "new\n").unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let source = CheckpointStore::at_root(source_dir.path().join("checkpoints")).unwrap();
        let first = source
            .capture(request(repo_dir.path(), "portable"))
            .unwrap();
        let second = source.capture(request(repo_dir.path(), "shared")).unwrap();
        let first_manifest = source.inspect(&first.id).unwrap();
        let export_dir = tempfile::tempdir().unwrap();
        let bundle = export_dir.path().join("portable.aoe-checkpoint.tar.gz");

        let exported = source.export_to(&first.id, &bundle).unwrap();
        assert_eq!(exported.artifacts, 2);
        assert!(exported.bytes > 0);
        let target_dir = tempfile::tempdir().unwrap();
        let target = CheckpointStore::at_root(target_dir.path().join("checkpoints")).unwrap();
        let imported = target.import_from(&bundle).unwrap();
        assert!(imported.imported);
        assert_eq!(target.inspect(&first.id).unwrap(), first_manifest);
        assert!(
            !target.import_from(&bundle).unwrap().imported,
            "idempotent import"
        );

        let deleted_first = source.delete(&first.id).unwrap();
        assert_eq!(deleted_first.artifacts_removed, 0);
        assert_eq!(deleted_first.artifacts_retained, 2);
        let deleted_second = source.delete(&second.id).unwrap();
        assert_eq!(deleted_second.artifacts_removed, 2);
        assert!(deleted_second.bytes_removed > 0);
    }

    #[test]
    fn restore_preflights_and_preserves_a_recovery_ref() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repository = committed_repo(repo_dir.path());
        fs::write(repo_dir.path().join("tracked.txt"), "checkpoint\n").unwrap();
        fs::write(repo_dir.path().join("new.txt"), "checkpoint new\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::at_root(store_dir.path().join("checkpoints")).unwrap();
        let checkpoint = store
            .capture(request(repo_dir.path(), "restore me"))
            .unwrap();

        run_git_checked(
            repo_dir.path(),
            ["reset", "--hard", "HEAD"],
            "clean fixture",
        )
        .unwrap();
        fs::remove_file(repo_dir.path().join("new.txt")).unwrap();
        fs::write(repo_dir.path().join("later.txt"), "later\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("later.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let parent = repository.head().unwrap().peel_to_commit().unwrap();
        let signature = Signature::now("Test", "test@example.com").unwrap();
        let later = repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "later",
                &tree,
                &[&parent],
            )
            .unwrap()
            .to_string();

        let plan = store
            .prepare_restore(&checkpoint.id, repo_dir.path())
            .unwrap();
        assert_eq!(plan.original_head, later);
        let restored = store.restore(&plan).unwrap();

        assert_eq!(
            fs::read_to_string(repo_dir.path().join("tracked.txt")).unwrap(),
            "checkpoint\n"
        );
        assert_eq!(
            fs::read_to_string(repo_dir.path().join("new.txt")).unwrap(),
            "checkpoint new\n"
        );
        assert_eq!(restored.changed_paths, 2);
        let recovery = repository
            .find_reference(&restored.recovery_ref)
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        assert_eq!(recovery, later);
    }
}
