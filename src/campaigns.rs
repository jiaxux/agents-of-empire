//! Durable campaign and task metadata.
//!
//! Campaign records are compact coordination facts over ordinary sessions,
//! checkpoints, and Trials. Storage performs no dispatch and chooses no agent.
//! Blocking dependencies are validated on mutation, and the ready frontier is
//! persisted so every UI surface reads the same state after restart.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

pub const CAMPAIGN_SCHEMA_VERSION: u32 = 1;
pub const MAX_CAMPAIGN_LABEL_CHARS: usize = 80;
pub const MAX_CAMPAIGN_TASK_TITLE_CHARS: usize = 160;

const CAMPAIGNS_SUBDIR: &str = "campaigns";
const MANIFESTS_SUBDIR: &str = "manifests";
const STORE_LOCK_FILE: &str = ".lock";
const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MANIFESTS_LOADED: usize = 1024;
const MAX_TASKS_PER_CAMPAIGN: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignState {
    Active,
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignTaskState {
    Planned,
    Ready,
    Active,
    Waiting,
    Review,
    Done,
    Failed,
    Cancelled,
}

impl CampaignTaskState {
    fn satisfies_block(self) -> bool {
        self == Self::Done
    }

    fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Blocks,
    Informs,
    ConflictsWith,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CampaignDependency {
    pub task_id: String,
    pub kind: DependencyKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CampaignTaskLinks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CampaignTaskRecord {
    pub id: String,
    pub title: String,
    pub state: CampaignTaskState,
    pub dependencies: Vec<CampaignDependency>,
    pub links: CampaignTaskLinks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_key: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CampaignManifest {
    pub schema_version: u32,
    pub id: String,
    pub label: String,
    pub state: CampaignState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_key: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub tasks: Vec<CampaignTaskRecord>,
}

impl CampaignManifest {
    pub fn summary(&self) -> CampaignSummaryRecord {
        summarize(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CampaignTaskSummary {
    pub campaign_id: String,
    pub campaign_label: String,
    pub id: String,
    pub title: String,
    pub state: CampaignTaskState,
    pub ready: bool,
    pub blocked_by_failure: bool,
    pub dependency_ids: Vec<String>,
    pub links: CampaignTaskLinks,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CampaignSummaryRecord {
    pub id: String,
    pub label: String,
    pub state: CampaignState,
    pub total: usize,
    pub ready: usize,
    pub active: usize,
    pub review: usize,
    pub done: usize,
    pub failed: usize,
    pub blocked: usize,
    pub updated_at_ms: u64,
    pub tasks: Vec<CampaignTaskSummary>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CampaignLoadReport {
    pub campaigns: Vec<CampaignSummaryRecord>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CampaignCreateRequest {
    pub label: String,
    pub external_reference: Option<String>,
    pub producer_key: Option<String>,
}

impl CampaignCreateRequest {
    pub fn new(label: impl Into<String>) -> Result<Self> {
        let label = normalize_text(label.into(), MAX_CAMPAIGN_LABEL_CHARS, "campaign label")?;
        Ok(Self {
            label,
            external_reference: None,
            producer_key: None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CampaignTaskCreateRequest {
    pub title: String,
    pub dependencies: Vec<CampaignDependency>,
    pub links: CampaignTaskLinks,
    pub producer_key: Option<String>,
}

impl CampaignTaskCreateRequest {
    pub fn new(title: impl Into<String>) -> Result<Self> {
        Ok(Self {
            title: normalize_text(
                title.into(),
                MAX_CAMPAIGN_TASK_TITLE_CHARS,
                "campaign task title",
            )?,
            dependencies: Vec::new(),
            links: CampaignTaskLinks::default(),
            producer_key: None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct CampaignStore {
    root: PathBuf,
}

impl CampaignStore {
    pub fn open() -> Result<Self> {
        Self::at_root(crate::session::get_app_dir()?.join(CAMPAIGNS_SUBDIR))
    }

    pub fn at_root(root: PathBuf) -> Result<Self> {
        let store = Self { root };
        fs::create_dir_all(store.manifests_dir())?;
        Ok(store)
    }

    pub fn create(&self, request: CampaignCreateRequest) -> Result<CampaignManifest> {
        let _lock = self.exclusive_lock()?;
        if let Some(key) = request.producer_key.as_deref() {
            validate_producer_key(key)?;
            if let Some(existing) = self.find_campaign_by_producer_key(key)? {
                if existing.label == request.label
                    && existing.external_reference == request.external_reference
                {
                    return Ok(existing);
                }
                bail!("campaign producer key already exists with different content");
            }
        }
        let now = now_ms();
        let manifest = CampaignManifest {
            schema_version: CAMPAIGN_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            label: request.label,
            state: CampaignState::Active,
            external_reference: request.external_reference,
            producer_key: request.producer_key,
            created_at_ms: now,
            updated_at_ms: now,
            tasks: Vec::new(),
        };
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    /// Return the newest active campaign, creating a local default only when
    /// none exists. This is the TUI producer's one-key entry point.
    pub fn ensure_default(&self) -> Result<CampaignManifest> {
        if let Some(campaign) = self
            .load_manifests()?
            .into_iter()
            .filter(|campaign| campaign.state == CampaignState::Active)
            .max_by_key(|campaign| campaign.updated_at_ms)
        {
            return Ok(campaign);
        }
        let mut request = CampaignCreateRequest::new("Fleet campaign")?;
        request.producer_key = Some("aoe:tui:default".to_string());
        self.create(request)
    }

    pub fn inspect(&self, campaign_id: &str) -> Result<CampaignManifest> {
        validate_id(campaign_id, "campaign")?;
        self.read_manifest_path(&self.manifest_path(campaign_id))
    }

    pub fn add_task(
        &self,
        campaign_id: &str,
        request: CampaignTaskCreateRequest,
    ) -> Result<CampaignManifest> {
        let _lock = self.exclusive_lock()?;
        let mut manifest = self.inspect(campaign_id)?;
        if manifest.state != CampaignState::Active {
            bail!("campaign is not active");
        }
        if manifest.tasks.len() >= MAX_TASKS_PER_CAMPAIGN {
            bail!("campaign exceeds the {} task limit", MAX_TASKS_PER_CAMPAIGN);
        }
        if let Some(key) = request.producer_key.as_deref() {
            validate_producer_key(key)?;
            if let Some(existing) = manifest
                .tasks
                .iter()
                .find(|task| task.producer_key.as_deref() == Some(key))
            {
                if existing.title == request.title
                    && existing.dependencies == request.dependencies
                    && existing.links == request.links
                {
                    return Ok(manifest);
                }
                bail!("campaign task producer key already exists with different content");
            }
        }
        validate_dependencies(&manifest.tasks, &request.dependencies, None)?;
        let now = now_ms();
        manifest.tasks.push(CampaignTaskRecord {
            id: uuid::Uuid::new_v4().to_string(),
            title: request.title,
            state: CampaignTaskState::Planned,
            dependencies: request.dependencies,
            links: request.links,
            producer_key: request.producer_key,
            created_at_ms: now,
            updated_at_ms: now,
        });
        refresh_frontier(&mut manifest);
        manifest.updated_at_ms = now;
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn replace_dependencies(
        &self,
        campaign_id: &str,
        task_id: &str,
        dependencies: Vec<CampaignDependency>,
    ) -> Result<CampaignManifest> {
        let _lock = self.exclusive_lock()?;
        let mut manifest = self.inspect(campaign_id)?;
        validate_id(task_id, "task")?;
        validate_dependencies(&manifest.tasks, &dependencies, Some(task_id))?;
        let index = manifest
            .tasks
            .iter()
            .position(|task| task.id == task_id)
            .context("campaign task does not exist")?;
        let old = manifest.tasks[index].dependencies.clone();
        manifest.tasks[index].dependencies = dependencies;
        if let Some(cycle) = blocking_cycle(&manifest.tasks) {
            manifest.tasks[index].dependencies = old;
            bail!("blocking dependency cycle: {}", cycle.join(" -> "));
        }
        manifest.tasks[index].updated_at_ms = now_ms();
        refresh_frontier(&mut manifest);
        manifest.updated_at_ms = now_ms();
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn set_task_state(
        &self,
        campaign_id: &str,
        task_id: &str,
        state: CampaignTaskState,
    ) -> Result<CampaignManifest> {
        let _lock = self.exclusive_lock()?;
        let mut manifest = self.inspect(campaign_id)?;
        let task = manifest
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .context("campaign task does not exist")?;
        if task.state == state {
            return Ok(manifest);
        }
        if state == CampaignTaskState::Active
            && !matches!(
                task.state,
                CampaignTaskState::Ready | CampaignTaskState::Waiting
            )
        {
            bail!("only a ready or waiting campaign task can become active");
        }
        if task.state.terminal() {
            bail!("terminal campaign task state cannot be rewritten");
        }
        task.state = state;
        task.updated_at_ms = now_ms();
        refresh_frontier(&mut manifest);
        manifest.updated_at_ms = now_ms();
        manifest.state = derive_campaign_state(&manifest.tasks);
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn load(&self) -> Result<CampaignLoadReport> {
        let mut report = CampaignLoadReport::default();
        let mut paths = manifest_paths(&self.manifests_dir())?;
        if paths.len() > MAX_MANIFESTS_LOADED {
            report.errors.push(format!(
                "campaign history exceeds the {}-manifest load limit",
                MAX_MANIFESTS_LOADED
            ));
            paths.truncate(MAX_MANIFESTS_LOADED);
        }
        for (_, path) in paths {
            match self.read_manifest_path(&path) {
                Ok(manifest) => report.campaigns.push(summarize(&manifest)),
                Err(error) => report.errors.push(format!(
                    "{}: {error}",
                    path.file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_default()
                )),
            }
        }
        report
            .campaigns
            .sort_by_key(|campaign| std::cmp::Reverse(campaign.updated_at_ms));
        Ok(report)
    }

    fn load_manifests(&self) -> Result<Vec<CampaignManifest>> {
        manifest_paths(&self.manifests_dir())?
            .into_iter()
            .map(|(_, path)| self.read_manifest_path(&path))
            .collect()
    }

    fn find_campaign_by_producer_key(&self, key: &str) -> Result<Option<CampaignManifest>> {
        Ok(self
            .load_manifests()?
            .into_iter()
            .find(|campaign| campaign.producer_key.as_deref() == Some(key)))
    }

    fn write_manifest(&self, manifest: &CampaignManifest) -> Result<()> {
        validate_manifest(manifest)?;
        crate::session::atomic_write(
            &self.manifest_path(&manifest.id),
            &serde_json::to_vec_pretty(manifest)?,
        )
    }

    fn read_manifest_path(&self, path: &Path) -> Result<CampaignManifest> {
        if fs::metadata(path)?.len() > MAX_MANIFEST_BYTES {
            bail!("campaign manifest exceeds 2 MiB");
        }
        let manifest: CampaignManifest = serde_json::from_slice(&fs::read(path)?)?;
        validate_manifest(&manifest)?;
        if path
            .file_stem()
            .is_some_and(|stem| stem != manifest.id.as_str())
        {
            bail!("campaign manifest id does not match its filename");
        }
        Ok(manifest)
    }

    fn manifests_dir(&self) -> PathBuf {
        self.root.join(MANIFESTS_SUBDIR)
    }

    fn manifest_path(&self, campaign_id: &str) -> PathBuf {
        self.manifests_dir().join(format!("{campaign_id}.json"))
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

fn manifest_paths(directory: &Path) -> Result<Vec<(SystemTime, PathBuf)>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            paths.push((
                entry.metadata()?.modified().unwrap_or(UNIX_EPOCH),
                entry.path(),
            ));
        }
    }
    paths.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    Ok(paths)
}

fn validate_manifest(manifest: &CampaignManifest) -> Result<()> {
    if manifest.schema_version != CAMPAIGN_SCHEMA_VERSION {
        bail!("unsupported campaign schema {}", manifest.schema_version);
    }
    validate_id(&manifest.id, "campaign")?;
    normalize_text(
        manifest.label.clone(),
        MAX_CAMPAIGN_LABEL_CHARS,
        "campaign label",
    )?;
    if manifest.tasks.len() > MAX_TASKS_PER_CAMPAIGN {
        bail!("campaign exceeds the supported task limit");
    }
    let mut ids = BTreeSet::new();
    let mut producer_keys = BTreeSet::new();
    for task in &manifest.tasks {
        validate_id(&task.id, "task")?;
        normalize_text(
            task.title.clone(),
            MAX_CAMPAIGN_TASK_TITLE_CHARS,
            "campaign task title",
        )?;
        if !ids.insert(task.id.as_str()) {
            bail!("campaign task id is duplicated");
        }
        if let Some(key) = task.producer_key.as_deref() {
            validate_producer_key(key)?;
            if !producer_keys.insert(key) {
                bail!("campaign task producer key is duplicated");
            }
        }
    }
    for task in &manifest.tasks {
        validate_dependencies(&manifest.tasks, &task.dependencies, Some(&task.id))?;
    }
    if let Some(cycle) = blocking_cycle(&manifest.tasks) {
        bail!("blocking dependency cycle: {}", cycle.join(" -> "));
    }
    let mut expected = manifest.clone();
    refresh_frontier(&mut expected);
    if expected.tasks != manifest.tasks {
        bail!("campaign ready frontier is stale");
    }
    if derive_campaign_state(&manifest.tasks) != manifest.state {
        bail!("campaign state does not match its tasks");
    }
    Ok(())
}

fn validate_dependencies(
    tasks: &[CampaignTaskRecord],
    dependencies: &[CampaignDependency],
    target_task_id: Option<&str>,
) -> Result<()> {
    let ids: BTreeSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let mut seen = BTreeSet::new();
    for dependency in dependencies {
        validate_id(&dependency.task_id, "dependency task")?;
        if !ids.contains(dependency.task_id.as_str()) {
            bail!("campaign dependency does not exist: {}", dependency.task_id);
        }
        if target_task_id == Some(dependency.task_id.as_str()) {
            bail!("campaign task cannot depend on itself");
        }
        if !seen.insert((&dependency.task_id, dependency.kind as u8)) {
            bail!("campaign dependency is duplicated");
        }
    }
    Ok(())
}

fn refresh_frontier(manifest: &mut CampaignManifest) {
    let states: BTreeMap<String, CampaignTaskState> = manifest
        .tasks
        .iter()
        .map(|task| (task.id.clone(), task.state))
        .collect();
    for task in &mut manifest.tasks {
        if !matches!(
            task.state,
            CampaignTaskState::Planned | CampaignTaskState::Ready
        ) {
            continue;
        }
        let ready = task
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == DependencyKind::Blocks)
            .all(|dependency| {
                states
                    .get(&dependency.task_id)
                    .is_some_and(|state| state.satisfies_block())
            });
        task.state = if ready {
            CampaignTaskState::Ready
        } else {
            CampaignTaskState::Planned
        };
    }
    manifest.state = derive_campaign_state(&manifest.tasks);
}

fn derive_campaign_state(tasks: &[CampaignTaskRecord]) -> CampaignState {
    if !tasks.is_empty()
        && tasks
            .iter()
            .all(|task| task.state == CampaignTaskState::Done)
    {
        CampaignState::Completed
    } else if !tasks.is_empty()
        && tasks.iter().all(|task| {
            matches!(
                task.state,
                CampaignTaskState::Done | CampaignTaskState::Cancelled
            )
        })
    {
        CampaignState::Cancelled
    } else {
        CampaignState::Active
    }
}

fn blocking_cycle(tasks: &[CampaignTaskRecord]) -> Option<Vec<String>> {
    let edges: BTreeMap<&str, Vec<&str>> = tasks
        .iter()
        .map(|task| {
            (
                task.id.as_str(),
                task.dependencies
                    .iter()
                    .filter(|dependency| dependency.kind == DependencyKind::Blocks)
                    .map(|dependency| dependency.task_id.as_str())
                    .collect(),
            )
        })
        .collect();
    let mut permanent = BTreeSet::new();
    let mut stack = Vec::new();
    let mut active = BTreeSet::new();
    for id in edges.keys().copied() {
        if let Some(cycle) = visit_cycle(id, &edges, &mut permanent, &mut active, &mut stack) {
            return Some(cycle);
        }
    }
    None
}

fn visit_cycle<'a>(
    id: &'a str,
    edges: &BTreeMap<&'a str, Vec<&'a str>>,
    permanent: &mut BTreeSet<&'a str>,
    active: &mut BTreeSet<&'a str>,
    stack: &mut Vec<&'a str>,
) -> Option<Vec<String>> {
    if permanent.contains(id) {
        return None;
    }
    if active.contains(id) {
        let start = stack.iter().position(|entry| *entry == id).unwrap_or(0);
        let mut cycle: Vec<String> = stack[start..]
            .iter()
            .map(|entry| (*entry).to_string())
            .collect();
        cycle.push(id.to_string());
        return Some(cycle);
    }
    active.insert(id);
    stack.push(id);
    for dependency in edges.get(id).into_iter().flatten().copied() {
        if let Some(cycle) = visit_cycle(dependency, edges, permanent, active, stack) {
            return Some(cycle);
        }
    }
    stack.pop();
    active.remove(id);
    permanent.insert(id);
    None
}

fn summarize(manifest: &CampaignManifest) -> CampaignSummaryRecord {
    let states: BTreeMap<&str, CampaignTaskState> = manifest
        .tasks
        .iter()
        .map(|task| (task.id.as_str(), task.state))
        .collect();
    let tasks: Vec<CampaignTaskSummary> = manifest
        .tasks
        .iter()
        .map(|task| {
            let blocked_by_failure = task
                .dependencies
                .iter()
                .filter(|dependency| dependency.kind == DependencyKind::Blocks)
                .any(|dependency| {
                    states
                        .get(dependency.task_id.as_str())
                        .is_some_and(|state| {
                            matches!(
                                state,
                                CampaignTaskState::Failed | CampaignTaskState::Cancelled
                            )
                        })
                });
            CampaignTaskSummary {
                campaign_id: manifest.id.clone(),
                campaign_label: manifest.label.clone(),
                id: task.id.clone(),
                title: task.title.clone(),
                state: task.state,
                ready: task.state == CampaignTaskState::Ready,
                blocked_by_failure,
                dependency_ids: task
                    .dependencies
                    .iter()
                    .filter(|dependency| dependency.kind == DependencyKind::Blocks)
                    .map(|dependency| dependency.task_id.clone())
                    .collect(),
                links: task.links.clone(),
                updated_at_ms: task.updated_at_ms,
            }
        })
        .collect();
    CampaignSummaryRecord {
        id: manifest.id.clone(),
        label: manifest.label.clone(),
        state: manifest.state,
        total: tasks.len(),
        ready: tasks.iter().filter(|task| task.ready).count(),
        active: tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.state,
                    CampaignTaskState::Active | CampaignTaskState::Waiting
                )
            })
            .count(),
        review: tasks
            .iter()
            .filter(|task| task.state == CampaignTaskState::Review)
            .count(),
        done: tasks
            .iter()
            .filter(|task| task.state == CampaignTaskState::Done)
            .count(),
        failed: tasks
            .iter()
            .filter(|task| task.state == CampaignTaskState::Failed)
            .count(),
        blocked: tasks.iter().filter(|task| task.blocked_by_failure).count(),
        updated_at_ms: manifest.updated_at_ms,
        tasks,
    }
}

fn normalize_text(value: String, max: usize, kind: &str) -> Result<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        bail!("{kind} cannot be empty");
    }
    if value.chars().count() > max {
        bail!("{kind} exceeds {max} characters");
    }
    Ok(value)
}

fn validate_id(id: &str, kind: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(id).with_context(|| format!("invalid {kind} id {id}"))?;
    if parsed.to_string() != id.to_ascii_lowercase() {
        bail!("{kind} id is not canonical");
    }
    Ok(())
}

fn validate_producer_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 256 || key.chars().any(char::is_control) {
        bail!("campaign producer key is empty or invalid");
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_frontier_is_incremental_and_producer_requests_are_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let store = CampaignStore::at_root(root.path().join("campaigns")).unwrap();
        let mut create = CampaignCreateRequest::new("Release").unwrap();
        create.producer_key = Some("plugin:release-42".to_string());
        let campaign = store.create(create.clone()).unwrap();
        assert_eq!(store.create(create).unwrap().id, campaign.id);

        let mut setup = CampaignTaskCreateRequest::new("Set up").unwrap();
        setup.producer_key = Some("issue:1".to_string());
        let campaign = store.add_task(&campaign.id, setup.clone()).unwrap();
        let setup_id = campaign.tasks[0].id.clone();
        assert_eq!(campaign.tasks[0].state, CampaignTaskState::Ready);
        assert_eq!(
            store.add_task(&campaign.id, setup).unwrap().tasks.len(),
            1,
            "replayed producer request"
        );

        let mut build = CampaignTaskCreateRequest::new("Build").unwrap();
        build.dependencies.push(CampaignDependency {
            task_id: setup_id.clone(),
            kind: DependencyKind::Blocks,
        });
        let campaign = store.add_task(&campaign.id, build).unwrap();
        let build_id = campaign.tasks[1].id.clone();
        assert_eq!(campaign.tasks[1].state, CampaignTaskState::Planned);
        store
            .set_task_state(&campaign.id, &setup_id, CampaignTaskState::Active)
            .unwrap();
        let campaign = store
            .set_task_state(&campaign.id, &setup_id, CampaignTaskState::Done)
            .unwrap();
        assert_eq!(
            campaign
                .tasks
                .iter()
                .find(|task| task.id == build_id)
                .unwrap()
                .state,
            CampaignTaskState::Ready
        );
    }

    #[test]
    fn blocking_cycles_report_the_concrete_path_and_corrupt_files_are_isolated() {
        let root = tempfile::tempdir().unwrap();
        let store = CampaignStore::at_root(root.path().join("campaigns")).unwrap();
        let campaign = store
            .create(CampaignCreateRequest::new("Cycle").unwrap())
            .unwrap();
        let campaign = store
            .add_task(&campaign.id, CampaignTaskCreateRequest::new("A").unwrap())
            .unwrap();
        let a = campaign.tasks[0].id.clone();
        let mut b_request = CampaignTaskCreateRequest::new("B").unwrap();
        b_request.dependencies.push(CampaignDependency {
            task_id: a.clone(),
            kind: DependencyKind::Blocks,
        });
        let campaign = store.add_task(&campaign.id, b_request).unwrap();
        let b = campaign.tasks[1].id.clone();
        let error = store
            .replace_dependencies(
                &campaign.id,
                &a,
                vec![CampaignDependency {
                    task_id: b,
                    kind: DependencyKind::Blocks,
                }],
            )
            .unwrap_err();
        assert!(error.to_string().contains(" -> "));

        fs::write(store.manifests_dir().join("broken.json"), "{").unwrap();
        let report = store.load().unwrap();
        assert_eq!(report.campaigns.len(), 1);
        assert_eq!(report.errors.len(), 1);
    }
}
