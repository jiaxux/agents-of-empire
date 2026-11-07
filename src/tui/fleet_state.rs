//! Adapter between AoE session state and the UI-independent [`crate::fleet`]
//! indexes.
//!
//! The TUI owns one [`FleetOpsState`]. Session lifecycle and status changes
//! push into it (`sync_sessions`), a bounded worker pushes repository scans
//! into it (`apply_scan`), and the fleet dialog reads the immutable
//! [`FleetSnapshot`] it publishes. Rendering never recomputes an index and
//! never touches git.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::fleet::{
    Admission, AttentionInbox, AttentionItem, AttentionReason, BudgetGovernor, CampaignSummary,
    Checkpoint, CheckpointId, CheckpointState, CheckpointTimeline, ConflictRadar, Evidence,
    EvidenceKind, EvidenceState, FleetLimits, FleetUsage, ReadinessPolicy, ReadinessReport,
    ReadinessVerdict, SessionKey,
};
use crate::session::{Instance, Status};

/// How many checkpoints per session the in-memory timeline keeps.
const CHECKPOINT_RETENTION: usize = 32;

/// A session, reduced to the facts the fleet indexes consume. Built on the UI
/// thread from an [`Instance`]; nothing here requires git, tmux, or disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FleetSessionInput {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) status: Status,
    pub(crate) unread: bool,
    /// Repository this session's changes belong to. Sibling worktrees of one
    /// repo share an identity, which is what makes an overlap a conflict.
    /// `None` for a session that runs in a non-git directory.
    pub(crate) repo_identity: Option<String>,
    /// Directory to scan for changed paths (the worktree, not the main repo).
    pub(crate) scan_path: String,
    /// Base ref the session's diff is measured against, when the session or
    /// its worktree names one. The worker resolves the rest.
    pub(crate) base_branch: Option<String>,
    /// Epoch milliseconds the current state was entered, best effort. Older is
    /// ranked first within one attention priority.
    pub(crate) since_ms: u64,
}

impl FleetSessionInput {
    /// `None` for sessions the operator has already parked: archived, trashed,
    /// and snoozed rows use AoE's existing "not now" mechanisms, so promoting
    /// them into the attention queue would fight those features rather than
    /// deepen them.
    pub(crate) fn from_instance(instance: &Instance) -> Option<Self> {
        if instance.is_archived() || instance.is_trashed() || instance.is_snoozed() {
            return None;
        }
        let worktree = instance.worktree_info.as_ref();
        let repo_identity = worktree
            .map(|info| info.main_repo_path.clone())
            .or_else(|| Some(instance.project_path.clone()))
            .filter(|path| !path.is_empty());
        let since = instance
            .idle_entered_at
            .or(instance.last_accessed_at)
            .unwrap_or(instance.created_at);
        Some(Self {
            id: instance.id.clone(),
            title: instance.title.clone(),
            status: instance.status,
            unread: instance.unread,
            repo_identity,
            scan_path: instance.project_path.clone(),
            base_branch: instance
                .base_branch_override
                .clone()
                .or_else(|| worktree.and_then(|info| info.base_branch.clone())),
            since_ms: since.timestamp_millis().max(0) as u64,
        })
    }

    fn attention(&self) -> Option<(AttentionReason, String)> {
        match self.status {
            Status::Error => Some((AttentionReason::Failed, "session reported an error".into())),
            Status::Waiting => Some((
                AttentionReason::Approval,
                "agent is waiting on you".to_string(),
            )),
            _ if self.unread => Some((
                AttentionReason::Completed,
                "unread output since you last looked".to_string(),
            )),
            _ => None,
        }
    }

    /// Sessions holding a worker slot. Mirrors the states that actually own an
    /// agent subprocess, so the governor's usage matches
    /// `acp.max_concurrent_workers`.
    fn holds_worker(&self) -> bool {
        matches!(self.status, Status::Running | Status::Starting)
    }
}

/// What a session's repository scan is worth right now.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum ScanState {
    /// Nothing requested yet: the fleet view has never been opened.
    #[default]
    Dormant,
    Scanning {
        pending: usize,
    },
    /// Every requested scan came back.
    Ready {
        scanned: usize,
        skipped: usize,
    },
}

/// A conflict, resolved to display strings. Built once per snapshot, never in
/// a render pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConflictRow {
    pub(crate) repo: String,
    pub(crate) path: String,
    pub(crate) session_ids: Vec<String>,
    pub(crate) sessions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AttentionRow {
    pub(crate) item_id: String,
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) reason: AttentionReason,
    pub(crate) summary: String,
    pub(crate) since_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReadinessRow {
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) verdict: ReadinessVerdict,
    pub(crate) evidence: Vec<Evidence>,
    pub(crate) blockers: Vec<EvidenceKind>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CheckpointRow {
    pub(crate) checkpoint_id: String,
    pub(crate) session_id: String,
    pub(crate) session_available: bool,
    pub(crate) title: String,
    pub(crate) label: String,
    pub(crate) snapshot_ref: String,
    pub(crate) created_at_ms: u64,
    pub(crate) state: CheckpointState,
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum CheckpointLoadState {
    #[default]
    Dormant,
    Loading,
    Ready {
        loaded: usize,
        errors: Vec<String>,
    },
    Failed(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum TrialLoadState {
    #[default]
    Dormant,
    Loading,
    Ready {
        loaded: usize,
        errors: Vec<String>,
    },
    Failed(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum CampaignLoadState {
    #[default]
    Dormant,
    Loading,
    Ready {
        loaded: usize,
        errors: Vec<String>,
    },
    Failed(String),
}

/// Immutable projection the fleet dialog renders. Replaced wholesale when an
/// index changes; a render pass only reads it.
#[derive(Clone, Debug, Default)]
pub(crate) struct FleetSnapshot {
    pub(crate) version: u64,
    pub(crate) sessions: usize,
    pub(crate) attention: Vec<AttentionRow>,
    pub(crate) conflicts: Vec<ConflictRow>,
    pub(crate) readiness: Vec<ReadinessRow>,
    pub(crate) usage: FleetUsage,
    pub(crate) limits: Option<FleetLimits>,
    pub(crate) rejected: Vec<String>,
    pub(crate) campaign: Option<CampaignSummary>,
    pub(crate) campaign_tasks: Vec<crate::campaigns::CampaignTaskSummary>,
    pub(crate) campaign_load: CampaignLoadState,
    pub(crate) trials: Vec<crate::trials::TrialSummary>,
    pub(crate) trial_load: TrialLoadState,
    pub(crate) checkpoints: Vec<CheckpointRow>,
    pub(crate) checkpoint_load: CheckpointLoadState,
    pub(crate) scan: ScanState,
    pub(crate) scan_errors: Vec<String>,
}

/// One session's finished repository scan, produced off the UI thread.
#[derive(Clone, Debug)]
pub(crate) struct ScanOutcome {
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    pub(crate) result: Result<Vec<String>, String>,
}

/// What the worker needs to scan one session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScanRequest {
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    pub(crate) scan_path: String,
    pub(crate) base_branch: Option<String>,
}

#[derive(Clone, Debug)]
struct SessionEntry {
    input: FleetSessionInput,
    /// Bumped whenever the scan inputs change. A result stamped with an older
    /// generation is dropped instead of resurrecting a stale conflict.
    generation: u64,
    changed_paths: Option<usize>,
    scan_error: Option<String>,
}

pub(crate) struct FleetOpsState {
    inbox: AttentionInbox,
    governor: BudgetGovernor,
    /// One radar per repository identity, so the same filename in two
    /// unrelated repos never reads as a collision.
    radars: BTreeMap<String, ConflictRadar>,
    timeline: CheckpointTimeline,
    checkpoint_load: CheckpointLoadState,
    campaigns: BTreeMap<String, crate::campaigns::CampaignSummaryRecord>,
    campaign_load: CampaignLoadState,
    trials: BTreeMap<String, crate::trials::TrialSummary>,
    trial_evidence: HashMap<String, (String, Evidence)>,
    trial_load: TrialLoadState,
    acknowledged_attention: HashSet<String>,
    policy: ReadinessPolicy,
    sessions: BTreeMap<String, SessionEntry>,
    rejected: Vec<String>,
    rejected_sessions: HashSet<String>,
    in_flight: HashSet<String>,
    snapshot: Arc<FleetSnapshot>,
    version: u64,
    dirty: bool,
}

impl FleetOpsState {
    pub(crate) fn new(max_workers: u32) -> Self {
        Self {
            inbox: AttentionInbox::default(),
            governor: BudgetGovernor::new(worker_limits(max_workers)),
            radars: BTreeMap::new(),
            timeline: CheckpointTimeline::new(
                NonZeroUsize::new(CHECKPOINT_RETENTION).expect("non-zero retention"),
            ),
            checkpoint_load: CheckpointLoadState::Dormant,
            campaigns: BTreeMap::new(),
            campaign_load: CampaignLoadState::Dormant,
            trials: BTreeMap::new(),
            trial_evidence: HashMap::new(),
            trial_load: TrialLoadState::Dormant,
            acknowledged_attention: HashSet::new(),
            policy: ReadinessPolicy::new([
                EvidenceKind::Diff,
                EvidenceKind::Verification,
                EvidenceKind::Conflict,
            ]),
            sessions: BTreeMap::new(),
            rejected: Vec::new(),
            rejected_sessions: HashSet::new(),
            in_flight: HashSet::new(),
            snapshot: Arc::new(FleetSnapshot::default()),
            version: 0,
            dirty: true,
        }
    }

    pub(crate) fn set_max_workers(&mut self, max_workers: u32) {
        let limits = worker_limits(max_workers);
        if self.governor.limits() != limits {
            self.governor.set_limits(limits);
            self.dirty = true;
        }
    }

    /// Fold the current session list into every index. Cheap enough to call on
    /// each storage reload: it walks the sessions once and touches only the
    /// entries whose facts changed.
    pub(crate) fn sync_sessions(&mut self, inputs: Vec<FleetSessionInput>) {
        let live: HashSet<String> = inputs.iter().map(|input| input.id.clone()).collect();
        let gone_ids: Vec<String> = self
            .sessions
            .keys()
            .filter(|id| !live.contains(*id))
            .cloned()
            .collect();
        for id in &gone_ids {
            self.forget(id);
        }

        let previous_rejected = std::mem::take(&mut self.rejected);
        let previous_rejected_sessions = std::mem::take(&mut self.rejected_sessions);
        let mut changed = !gone_ids.is_empty();
        for input in inputs {
            let key = SessionKey::new(input.id.clone());
            match input.attention() {
                Some((reason, summary)) => self.inbox.upsert(AttentionItem {
                    session: key.clone(),
                    reason,
                    since_ms: input.since_ms,
                    summary,
                }),
                None => {
                    self.inbox.dismiss(&key);
                }
            }

            if input.holds_worker() {
                let admission = self.admit(&key);
                if !matches!(admission, Admission::Granted | Admission::AlreadyActive) {
                    self.rejected_sessions.insert(input.id.clone());
                    self.rejected.push(input.title.clone());
                }
            } else {
                self.governor.release(&key);
            }

            match self.sessions.get_mut(&input.id) {
                Some(entry) => {
                    if entry.input == input {
                        continue;
                    }
                    if entry.input.scan_path != input.scan_path
                        || entry.input.base_branch != input.base_branch
                        || entry.input.repo_identity != input.repo_identity
                        || completed_work_cycle(entry.input.status, input.status)
                    {
                        entry.generation += 1;
                        entry.changed_paths = None;
                        entry.scan_error = None;
                        self.in_flight.remove(&input.id);
                        remove_from_radars(&mut self.radars, &key);
                    }
                    entry.input = input;
                    changed = true;
                }
                None => {
                    self.sessions.insert(
                        input.id.clone(),
                        SessionEntry {
                            input,
                            generation: 1,
                            changed_paths: None,
                            scan_error: None,
                        },
                    );
                    changed = true;
                }
            }
        }
        if self.rejected != previous_rejected
            || self.rejected_sessions != previous_rejected_sessions
        {
            changed = true;
        }
        self.dirty |= changed;
    }

    fn admit(&mut self, key: &SessionKey) -> Admission {
        self.governor.acquire(
            key.clone(),
            crate::fleet::LeaseRequest {
                weight: 1,
                // Token reservations stay zero until a usage adapter reports
                // real numbers; an unknown must never trip a cap.
                reserved_tokens: 0,
            },
        )
    }

    /// Reserve one worker slot for an orchestration action before its session
    /// exists. The caller owns the stable reservation key and must release it
    /// on every success, failure, or cancellation path.
    pub(crate) fn reserve_operation(&mut self, key: String) -> Admission {
        let admission = self.admit(&SessionKey::new(key));
        if matches!(admission, Admission::Granted) {
            self.dirty = true;
        }
        admission
    }

    pub(crate) fn release_operation(&mut self, key: &str) -> bool {
        let released = self.governor.release(&SessionKey::new(key.to_string()));
        self.dirty |= released;
        released
    }

    fn forget(&mut self, id: &str) {
        let key = SessionKey::new(id.to_string());
        self.inbox.dismiss(&key);
        self.governor.release(&key);
        remove_from_radars(&mut self.radars, &key);
        self.sessions.remove(id);
        self.in_flight.remove(id);
    }

    /// Scan requests for every session whose changed paths are not known yet.
    /// Returns nothing once the fleet is fully scanned, so reopening the view
    /// on an unchanged fleet starts no git work.
    pub(crate) fn pending_scans(&mut self) -> Vec<ScanRequest> {
        let mut requests = Vec::new();
        for entry in self.sessions.values() {
            if entry.changed_paths.is_some()
                || entry.scan_error.is_some()
                || self.in_flight.contains(&entry.input.id)
            {
                continue;
            }
            if entry.input.repo_identity.is_none() || entry.input.scan_path.is_empty() {
                continue;
            }
            requests.push(ScanRequest {
                session_id: entry.input.id.clone(),
                generation: entry.generation,
                scan_path: entry.input.scan_path.clone(),
                base_branch: entry.input.base_branch.clone(),
            });
        }
        for request in &requests {
            self.in_flight.insert(request.session_id.clone());
        }
        if !requests.is_empty() {
            self.dirty = true;
        }
        requests
    }

    pub(crate) fn invalidate_scan_path(&mut self, scan_path: &std::path::Path) {
        let scan_path = scan_path.to_string_lossy();
        let ids: Vec<String> = self
            .sessions
            .values()
            .filter(|entry| entry.input.scan_path == scan_path)
            .map(|entry| entry.input.id.clone())
            .collect();
        for id in ids {
            if let Some(entry) = self.sessions.get_mut(&id) {
                entry.generation += 1;
                entry.changed_paths = None;
                entry.scan_error = None;
            }
            self.in_flight.remove(&id);
            remove_from_radars(&mut self.radars, &SessionKey::new(id));
            self.dirty = true;
        }
    }

    /// Drop every cached repository result. The operator's explicit refresh
    /// action is the only caller; the next normal fleet tick queues one scan
    /// per eligible session on the existing bounded worker.
    pub(crate) fn invalidate_all_scans(&mut self) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            if let Some(entry) = self.sessions.get_mut(&id) {
                entry.generation += 1;
                entry.changed_paths = None;
                entry.scan_error = None;
            }
            self.in_flight.remove(&id);
            remove_from_radars(&mut self.radars, &SessionKey::new(id));
        }
        self.dirty = true;
    }

    /// Apply a worker result. A result for a generation the session has since
    /// left is dropped.
    pub(crate) fn apply_scan(&mut self, outcome: ScanOutcome) {
        self.in_flight.remove(&outcome.session_id);
        let Some(entry) = self.sessions.get_mut(&outcome.session_id) else {
            return;
        };
        if entry.generation != outcome.generation {
            return;
        }
        let key = SessionKey::new(outcome.session_id.clone());
        match outcome.result {
            Ok(paths) => {
                entry.changed_paths = Some(paths.len());
                entry.scan_error = None;
                let repo = entry
                    .input
                    .repo_identity
                    .clone()
                    .unwrap_or_else(|| entry.input.scan_path.clone());
                remove_from_radars(&mut self.radars, &key);
                self.radars
                    .entry(repo)
                    .or_default()
                    .replace_paths(key, paths);
            }
            Err(error) => {
                entry.changed_paths = None;
                entry.scan_error = Some(error);
                remove_from_radars(&mut self.radars, &key);
            }
        }
        self.dirty = true;
    }

    /// Transition checkpoint history to loading once. A failed load may be
    /// retried after reopening the panel; ready or in-flight history is not
    /// re-read.
    pub(crate) fn begin_checkpoint_load(&mut self) -> bool {
        if matches!(
            self.checkpoint_load,
            CheckpointLoadState::Loading | CheckpointLoadState::Ready { .. }
        ) {
            return false;
        }
        self.checkpoint_load = CheckpointLoadState::Loading;
        self.dirty = true;
        true
    }

    pub(crate) fn apply_checkpoint_load(&mut self, report: crate::checkpoints::LoadReport) {
        let loaded = report.checkpoints.len();
        for summary in report.checkpoints {
            self.timeline.upsert(checkpoint_from_summary(summary));
        }
        self.checkpoint_load = CheckpointLoadState::Ready {
            loaded,
            errors: report.errors,
        };
        self.dirty = true;
    }

    pub(crate) fn fail_checkpoint_load(&mut self, error: String) {
        self.checkpoint_load = CheckpointLoadState::Failed(error);
        self.dirty = true;
    }

    /// Publish a materializing row before the background request is sent, so
    /// key dispatch gives immediate feedback without waiting on the worker.
    pub(crate) fn begin_checkpoint(&mut self, summary: crate::checkpoints::CheckpointSummary) {
        self.timeline.upsert(checkpoint_from_summary(summary));
        self.dirty = true;
    }

    pub(crate) fn apply_checkpoint(&mut self, summary: crate::checkpoints::CheckpointSummary) {
        self.timeline.upsert(checkpoint_from_summary(summary));
        self.dirty = true;
    }

    pub(crate) fn fail_checkpoint(&mut self, checkpoint_id: &str, error: String) {
        let id = CheckpointId::new(checkpoint_id.to_string());
        if let Some(checkpoint) = self.timeline.get_mut(&id) {
            checkpoint.state = CheckpointState::Incomplete;
            checkpoint.snapshot_ref = "metadata-only".to_string();
            checkpoint.detail = Some(error);
            self.dirty = true;
        }
    }

    pub(crate) fn remove_checkpoint(&mut self, checkpoint_id: &str) {
        if self
            .timeline
            .remove(&CheckpointId::new(checkpoint_id.to_string()))
        {
            self.dirty = true;
        }
    }

    pub(crate) fn begin_trial_load(&mut self) -> bool {
        if matches!(
            self.trial_load,
            TrialLoadState::Loading | TrialLoadState::Ready { .. }
        ) {
            return false;
        }
        self.trial_load = TrialLoadState::Loading;
        self.dirty = true;
        true
    }

    pub(crate) fn apply_trial_load(&mut self, report: crate::trials::TrialLoadReport) {
        let loaded = report.trials.len();
        self.trial_evidence.clear();
        for trial in &report.trials {
            self.update_trial_evidence(trial);
        }
        for trial in report.trials {
            self.trials.insert(trial.id.clone(), trial);
        }
        self.trial_load = TrialLoadState::Ready {
            loaded,
            errors: report.errors,
        };
        self.dirty = true;
    }

    pub(crate) fn apply_trial(&mut self, trial: crate::trials::TrialSummary) {
        self.update_trial_evidence(&trial);
        self.trials.insert(trial.id.clone(), trial);
        self.dirty = true;
    }

    fn update_trial_evidence(&mut self, trial: &crate::trials::TrialSummary) {
        self.trial_evidence
            .retain(|_, (trial_id, _)| trial_id != &trial.id);
        for contender in &trial.contenders {
            let Some(session_id) = contender.session_id.clone() else {
                continue;
            };
            let (state, observed_at_ms, summary) = match &contender.verification {
                Some(result) => {
                    let state = match result.state {
                        crate::trials::VerificationState::Passed => EvidenceState::Passed,
                        crate::trials::VerificationState::Failed
                        | crate::trials::VerificationState::TimedOut
                        | crate::trials::VerificationState::Error => EvidenceState::Failed,
                    };
                    (state, result.observed_at_ms, result.summary.clone())
                }
                None if matches!(
                    contender.state,
                    crate::trials::ContenderState::Pending
                        | crate::trials::ContenderState::Creating
                        | crate::trials::ContenderState::Starting
                        | crate::trials::ContenderState::Running
                        | crate::trials::ContenderState::Completed
                ) && trial.verification_recipe.is_some() =>
                {
                    (
                        EvidenceState::Running,
                        trial.updated_at_ms,
                        "Trial verification has not finished".to_string(),
                    )
                }
                None if contender.state == crate::trials::ContenderState::Failed => (
                    EvidenceState::Failed,
                    trial.updated_at_ms,
                    contender
                        .error
                        .clone()
                        .unwrap_or_else(|| "Trial contender did not complete".to_string()),
                ),
                None if contender.state == crate::trials::ContenderState::Cancelled => (
                    EvidenceState::Missing,
                    trial.updated_at_ms,
                    "Trial contender was cancelled".to_string(),
                ),
                None => (
                    EvidenceState::Missing,
                    trial.updated_at_ms,
                    "Trial has no verification recipe".to_string(),
                ),
            };
            self.trial_evidence.insert(
                session_id,
                (
                    trial.id.clone(),
                    Evidence {
                        kind: EvidenceKind::Verification,
                        state,
                        updated_at_ms: observed_at_ms,
                        summary,
                    },
                ),
            );
        }
    }

    pub(crate) fn fail_trial_load(&mut self, error: String) {
        self.trial_load = TrialLoadState::Failed(error);
        self.dirty = true;
    }

    pub(crate) fn apply_attention_acknowledgements(&mut self, item_ids: HashSet<String>) {
        if self.acknowledged_attention != item_ids {
            self.acknowledged_attention = item_ids;
            self.dirty = true;
        }
    }

    pub(crate) fn acknowledge_attention(&mut self, item_id: String) -> bool {
        let inserted = self.acknowledged_attention.insert(item_id);
        self.dirty |= inserted;
        inserted
    }

    pub(crate) fn unacknowledge_attention(&mut self, item_id: &str) -> bool {
        let removed = self.acknowledged_attention.remove(item_id);
        self.dirty |= removed;
        removed
    }

    pub(crate) fn begin_campaign_load(&mut self) -> bool {
        if matches!(
            self.campaign_load,
            CampaignLoadState::Loading | CampaignLoadState::Ready { .. }
        ) {
            return false;
        }
        self.campaign_load = CampaignLoadState::Loading;
        self.dirty = true;
        true
    }

    pub(crate) fn apply_campaign_load(&mut self, report: crate::campaigns::CampaignLoadReport) {
        let loaded = report.campaigns.len();
        self.campaigns = report
            .campaigns
            .into_iter()
            .map(|campaign| (campaign.id.clone(), campaign))
            .collect();
        self.campaign_load = CampaignLoadState::Ready {
            loaded,
            errors: report.errors,
        };
        self.dirty = true;
    }

    pub(crate) fn apply_campaign(&mut self, campaign: crate::campaigns::CampaignSummaryRecord) {
        self.campaigns.insert(campaign.id.clone(), campaign);
        self.campaign_load = CampaignLoadState::Ready {
            loaded: self.campaigns.len(),
            errors: Vec::new(),
        };
        self.dirty = true;
    }

    pub(crate) fn fail_campaign_load(&mut self, error: String) {
        self.campaign_load = CampaignLoadState::Failed(error);
        self.dirty = true;
    }

    /// The projection the dialog renders. Rebuilt only when an index changed.
    pub(crate) fn snapshot(&mut self) -> Arc<FleetSnapshot> {
        if self.dirty {
            self.rebuild();
        }
        Arc::clone(&self.snapshot)
    }

    fn rebuild(&mut self) {
        self.version += 1;
        let titles: HashMap<&str, &str> = self
            .sessions
            .values()
            .map(|entry| (entry.input.id.as_str(), entry.input.title.as_str()))
            .collect();
        let title_of = |id: &str| {
            titles
                .get(id)
                .map(|title| (*title).to_string())
                .unwrap_or_else(|| id.to_string())
        };

        let mut attention_by_session: HashMap<String, AttentionRow> = self
            .inbox
            .top(usize::MAX)
            .map(|item| AttentionRow {
                item_id: attention_item_id(
                    "session",
                    item.session.as_str(),
                    item.since_ms,
                    attention_reason_key(item.reason),
                ),
                session_id: item.session.as_str().to_string(),
                title: title_of(item.session.as_str()),
                reason: item.reason,
                summary: item.summary.clone(),
                since_ms: item.since_ms,
            })
            .map(|row| (row.session_id.clone(), row))
            .collect();

        for (session_id, (_, evidence)) in &self.trial_evidence {
            if evidence.state != EvidenceState::Failed || !titles.contains_key(session_id.as_str())
            {
                continue;
            }
            upsert_attention(
                &mut attention_by_session,
                AttentionRow {
                    item_id: attention_item_id(
                        "verification",
                        session_id,
                        evidence.updated_at_ms,
                        &evidence.summary,
                    ),
                    session_id: session_id.clone(),
                    title: title_of(session_id),
                    reason: AttentionReason::Failed,
                    summary: format!("verification failed: {}", evidence.summary),
                    since_ms: evidence.updated_at_ms,
                },
            );
        }

        for session_id in &self.rejected_sessions {
            let Some(entry) = self.sessions.get(session_id) else {
                continue;
            };
            upsert_attention(
                &mut attention_by_session,
                AttentionRow {
                    item_id: attention_item_id(
                        "capacity",
                        session_id,
                        entry.input.since_ms,
                        "worker_limit",
                    ),
                    session_id: session_id.clone(),
                    title: entry.input.title.clone(),
                    reason: AttentionReason::Capacity,
                    summary: "worker capacity rejected this active session".to_string(),
                    since_ms: entry.input.since_ms,
                },
            );
        }

        for task in self
            .campaigns
            .values()
            .flat_map(|campaign| campaign.tasks.iter())
            .filter(|task| task.ready)
        {
            let Some(session_id) = task.links.session_id.as_deref() else {
                continue;
            };
            let Some(entry) = self.sessions.get(session_id) else {
                continue;
            };
            upsert_attention(
                &mut attention_by_session,
                AttentionRow {
                    item_id: attention_item_id(
                        "campaign_ready",
                        session_id,
                        task.updated_at_ms,
                        &task.id,
                    ),
                    session_id: session_id.to_string(),
                    title: entry.input.title.clone(),
                    reason: AttentionReason::CampaignReady,
                    summary: format!("campaign task is ready: {}", task.title),
                    since_ms: task.updated_at_ms,
                },
            );
        }

        let mut conflicts = Vec::new();
        let mut conflict_paths: HashMap<String, Vec<String>> = HashMap::new();
        for (repo, radar) in &self.radars {
            for conflict in radar.conflicts() {
                let path = conflict.path.to_string_lossy().into_owned();
                let session_ids: Vec<_> = conflict
                    .sessions
                    .iter()
                    .map(|session| session.as_str().to_string())
                    .collect();
                for session_id in &session_ids {
                    conflict_paths
                        .entry(session_id.clone())
                        .or_default()
                        .push(path.clone());
                }
                conflicts.push(ConflictRow {
                    repo: repo_label(repo),
                    path,
                    session_ids,
                    sessions: conflict
                        .sessions
                        .iter()
                        .map(|session| title_of(session.as_str()))
                        .collect(),
                });
            }
        }

        for (session_id, paths) in &conflict_paths {
            let Some(entry) = self.sessions.get(session_id) else {
                continue;
            };
            let summary = if paths.len() == 1 {
                format!("changed path overlaps another session: {}", paths[0])
            } else {
                format!("{} changed paths overlap other sessions", paths.len())
            };
            upsert_attention(
                &mut attention_by_session,
                AttentionRow {
                    item_id: attention_item_id(
                        "conflict",
                        session_id,
                        entry.generation,
                        &paths.join("\n"),
                    ),
                    session_id: session_id.clone(),
                    title: entry.input.title.clone(),
                    reason: AttentionReason::Conflict,
                    summary,
                    since_ms: entry.input.since_ms,
                },
            );
        }

        let mut attention: Vec<_> = attention_by_session.into_values().collect();
        attention.sort_by(|left, right| {
            right
                .reason
                .priority()
                .cmp(&left.reason.priority())
                .then_with(|| left.since_ms.cmp(&right.since_ms))
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        attention.retain(|row| !self.acknowledged_attention.contains(&row.item_id));

        let readiness = self
            .sessions
            .values()
            .map(|entry| {
                let evidence = self.evidence_for(
                    entry,
                    conflict_paths.get(&entry.input.id).map(Vec::as_slice),
                );
                let report = ReadinessReport::evaluate(&self.policy, evidence.clone());
                ReadinessRow {
                    session_id: entry.input.id.clone(),
                    title: entry.input.title.clone(),
                    verdict: report.verdict,
                    blockers: report.blockers,
                    evidence,
                }
            })
            .collect();

        let mut checkpoints: Vec<_> = self
            .timeline
            .all()
            .map(|checkpoint| CheckpointRow {
                checkpoint_id: checkpoint.id.as_str().to_string(),
                session_id: checkpoint.session.as_str().to_string(),
                session_available: titles.contains_key(checkpoint.session.as_str()),
                title: titles.get(checkpoint.session.as_str()).map_or_else(
                    || checkpoint.session_title.clone(),
                    |title| (*title).to_string(),
                ),
                label: checkpoint.label.clone(),
                snapshot_ref: checkpoint.snapshot_ref.clone(),
                created_at_ms: checkpoint.created_at_ms,
                state: checkpoint.state,
                detail: checkpoint.detail.clone(),
            })
            .collect();
        checkpoints.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| left.checkpoint_id.cmp(&right.checkpoint_id))
        });

        let scan_errors = self
            .sessions
            .values()
            .filter_map(|entry| {
                entry
                    .scan_error
                    .as_ref()
                    .map(|error| format!("{}: {error}", entry.input.title))
            })
            .collect();

        let scanned = self
            .sessions
            .values()
            .filter(|entry| entry.changed_paths.is_some())
            .count();
        let skipped = self
            .sessions
            .values()
            .filter(|entry| entry.scan_error.is_some())
            .count();
        let scan = if !self.in_flight.is_empty() {
            ScanState::Scanning {
                pending: self.in_flight.len(),
            }
        } else if scanned == 0 && skipped == 0 {
            ScanState::Dormant
        } else {
            ScanState::Ready { scanned, skipped }
        };

        let campaign = (!self.campaigns.is_empty()).then(|| {
            self.campaigns
                .values()
                .fold(CampaignSummary::default(), |mut total, current| {
                    total.total += current.total;
                    total.ready += current.ready;
                    total.running += current.active + current.review;
                    total.completed += current.done;
                    total.failed += current.failed;
                    total.blocked += current.blocked;
                    total
                })
        });
        let mut campaign_tasks: Vec<_> = self
            .campaigns
            .values()
            .flat_map(|campaign| campaign.tasks.iter().cloned())
            .collect();
        campaign_tasks.sort_by(|left, right| {
            right
                .ready
                .cmp(&left.ready)
                .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        });

        self.snapshot = Arc::new(FleetSnapshot {
            version: self.version,
            sessions: self.sessions.len(),
            attention,
            conflicts,
            readiness,
            usage: self.governor.usage(),
            limits: Some(self.governor.limits()),
            rejected: self.rejected.clone(),
            campaign,
            campaign_tasks,
            campaign_load: self.campaign_load.clone(),
            trials: {
                let mut trials: Vec<_> = self.trials.values().cloned().collect();
                trials.sort_by_key(|trial| std::cmp::Reverse(trial.updated_at_ms));
                trials
            },
            trial_load: self.trial_load.clone(),
            checkpoints,
            checkpoint_load: self.checkpoint_load.clone(),
            scan,
            scan_errors,
        });
        self.dirty = false;
    }

    fn evidence_for(
        &self,
        entry: &SessionEntry,
        conflict_paths: Option<&[String]>,
    ) -> Vec<Evidence> {
        let diff = match (entry.changed_paths, &entry.scan_error) {
            (Some(0), _) => Evidence {
                kind: EvidenceKind::Diff,
                state: EvidenceState::Passed,
                updated_at_ms: entry.input.since_ms,
                summary: "no changed files".to_string(),
            },
            (Some(count), _) => Evidence {
                kind: EvidenceKind::Diff,
                state: EvidenceState::Passed,
                updated_at_ms: entry.input.since_ms,
                summary: format!("{count} changed file(s)"),
            },
            (None, Some(error)) => Evidence {
                kind: EvidenceKind::Diff,
                state: EvidenceState::Failed,
                updated_at_ms: entry.input.since_ms,
                summary: error.clone(),
            },
            (None, None) => Evidence {
                kind: EvidenceKind::Diff,
                state: if self.in_flight.contains(&entry.input.id) {
                    EvidenceState::Running
                } else {
                    EvidenceState::Missing
                },
                updated_at_ms: entry.input.since_ms,
                summary: "not scanned yet".to_string(),
            },
        };
        let conflict = match conflict_paths {
            Some(paths) if !paths.is_empty() => Evidence {
                kind: EvidenceKind::Conflict,
                state: EvidenceState::Failed,
                updated_at_ms: entry.input.since_ms,
                summary: format!("{} overlapping changed path(s)", paths.len()),
            },
            _ if entry.changed_paths.is_some() => Evidence {
                kind: EvidenceKind::Conflict,
                state: EvidenceState::Passed,
                updated_at_ms: entry.input.since_ms,
                summary: "no overlapping changed paths".to_string(),
            },
            _ if self.in_flight.contains(&entry.input.id) => Evidence {
                kind: EvidenceKind::Conflict,
                state: EvidenceState::Running,
                updated_at_ms: entry.input.since_ms,
                summary: "repository scan is running".to_string(),
            },
            _ => Evidence {
                kind: EvidenceKind::Conflict,
                state: EvidenceState::Missing,
                updated_at_ms: entry.input.since_ms,
                summary: "conflict status is unknown".to_string(),
            },
        };
        // The remaining kinds have no producer wired yet. They are listed as
        // missing rather than omitted so the panel cannot read as "all clear".
        let mut evidence = vec![diff, conflict];
        evidence.push(
            self.trial_evidence
                .get(&entry.input.id)
                .map(|(_, evidence)| evidence.clone())
                .unwrap_or_else(|| Evidence {
                    kind: EvidenceKind::Verification,
                    state: EvidenceState::Missing,
                    updated_at_ms: 0,
                    summary: "no verification recipe result".to_string(),
                }),
        );
        for kind in [
            EvidenceKind::Tests,
            EvidenceKind::Format,
            EvidenceKind::Lint,
            EvidenceKind::Review,
        ] {
            evidence.push(Evidence {
                kind,
                state: EvidenceState::Missing,
                updated_at_ms: 0,
                summary: "no producer connected".to_string(),
            });
        }
        evidence
    }
}

fn checkpoint_from_summary(summary: crate::checkpoints::CheckpointSummary) -> Checkpoint {
    Checkpoint {
        id: CheckpointId::new(summary.id),
        session: SessionKey::new(summary.session_id),
        session_title: summary.session_title,
        created_at_ms: summary.created_at_ms,
        label: summary.label,
        snapshot_ref: summary.snapshot_ref,
        state: summary.state,
        detail: summary.detail,
    }
}

fn completed_work_cycle(previous: Status, current: Status) -> bool {
    matches!(
        previous,
        Status::Running | Status::Starting | Status::Creating
    ) && !matches!(
        current,
        Status::Running | Status::Starting | Status::Creating
    )
}

fn attention_reason_key(reason: AttentionReason) -> &'static str {
    match reason {
        AttentionReason::Approval => "approval",
        AttentionReason::Input => "input",
        AttentionReason::Failed => "failed",
        AttentionReason::Capacity => "capacity",
        AttentionReason::Conflict => "conflict",
        AttentionReason::CampaignReady => "campaign_ready",
        AttentionReason::Completed => "completed",
    }
}

fn attention_item_id(kind: &str, session_id: &str, revision: u64, detail: &str) -> String {
    let mut digest = Sha256::new();
    let revision = revision.to_string();
    for part in [kind, session_id, revision.as_str(), detail] {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part.as_bytes());
    }
    let digest = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{digest}")
}

fn upsert_attention(rows: &mut HashMap<String, AttentionRow>, candidate: AttentionRow) {
    match rows.entry(candidate.session_id.clone()) {
        std::collections::hash_map::Entry::Occupied(mut entry)
            if candidate.reason.priority() > entry.get().reason.priority() =>
        {
            entry.insert(candidate);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(candidate);
        }
        _ => {}
    }
}

fn worker_limits(max_workers: u32) -> FleetLimits {
    let max_active = max_workers.max(1) as usize;
    FleetLimits {
        max_active,
        max_weight: max_workers.max(1),
        max_reserved_tokens: u64::MAX,
    }
}

fn remove_from_radars(radars: &mut BTreeMap<String, ConflictRadar>, key: &SessionKey) {
    for radar in radars.values_mut() {
        radar.remove_session(key);
    }
}

fn repo_label(repo: &str) -> String {
    std::path::Path::new(repo)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(id: &str, status: Status, repo: &str) -> FleetSessionInput {
        FleetSessionInput {
            id: id.to_string(),
            title: format!("{id}-title"),
            status,
            unread: false,
            repo_identity: Some(repo.to_string()),
            scan_path: format!("{repo}/{id}"),
            base_branch: Some("main".to_string()),
            since_ms: 1_000,
        }
    }

    fn ok_scan(state: &mut FleetOpsState, id: &str, generation: u64, paths: &[&str]) {
        state.apply_scan(ScanOutcome {
            session_id: id.to_string(),
            generation,
            result: Ok(paths.iter().map(|path| path.to_string()).collect()),
        });
    }

    #[test]
    fn attention_and_capacity_track_session_state() {
        let mut state = FleetOpsState::new(1);
        let cases = [
            (Status::Error, Some(AttentionReason::Failed)),
            (Status::Waiting, Some(AttentionReason::Approval)),
            (Status::Idle, None),
            (Status::Running, None),
        ];
        for (status, expected) in cases {
            state.sync_sessions(vec![input("one", status, "/repo")]);
            let snapshot = state.snapshot();
            assert_eq!(
                snapshot.attention.first().map(|row| row.reason),
                expected,
                "{status:?}"
            );
            assert_eq!(
                snapshot.usage.active,
                usize::from(status == Status::Running),
                "{status:?} lease"
            );
        }

        // An unread idle session is attention-worthy even though its status is not.
        let mut unread = input("one", Status::Idle, "/repo");
        unread.unread = true;
        state.sync_sessions(vec![unread]);
        assert_eq!(
            state.snapshot().attention.first().map(|row| row.reason),
            Some(AttentionReason::Completed)
        );

        // A session that disappears takes its queue entry and its lease with it.
        state.sync_sessions(Vec::new());
        let snapshot = state.snapshot();
        assert!(snapshot.attention.is_empty());
        assert_eq!(snapshot.usage.active, 0);
    }

    #[test]
    fn attention_acknowledgement_hides_the_same_fact_until_its_revision_changes() {
        let mut state = FleetOpsState::new(4);
        let mut unread = input("one", Status::Idle, "/repo");
        unread.unread = true;
        state.sync_sessions(vec![unread.clone()]);
        let item_id = state.snapshot().attention[0].item_id.clone();
        assert!(state.acknowledge_attention(item_id.clone()));
        assert!(state.snapshot().attention.is_empty());

        let mut reloaded = FleetOpsState::new(4);
        reloaded.apply_attention_acknowledgements(HashSet::from([item_id]));
        reloaded.sync_sessions(vec![unread.clone()]);
        assert!(reloaded.snapshot().attention.is_empty());

        unread.since_ms += 1;
        reloaded.sync_sessions(vec![unread]);
        assert_eq!(reloaded.snapshot().attention.len(), 1, "new occurrence");
    }

    #[test]
    fn operation_reservations_hold_capacity_until_a_real_session_replaces_them() {
        let mut state = FleetOpsState::new(1);
        assert_eq!(
            state.reserve_operation("trial:one".to_string()),
            Admission::Granted
        );
        assert!(matches!(
            state.reserve_operation("trial:two".to_string()),
            Admission::ActiveLimit { limit: 1 }
        ));

        state.sync_sessions(vec![input("one", Status::Starting, "/repo")]);
        assert_eq!(
            state.snapshot().usage.active,
            1,
            "reservation owns the slot"
        );
        assert_eq!(
            state.snapshot().attention[0].reason,
            AttentionReason::Capacity
        );
        assert!(state.release_operation("trial:one"));
        state.sync_sessions(vec![input("one", Status::Starting, "/repo")]);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.usage.active, 1, "real session takes the slot");
        assert!(snapshot.rejected.is_empty());
    }

    #[test]
    fn repeated_identical_syncs_publish_no_new_snapshot() {
        let mut state = FleetOpsState::new(4);
        let sessions = vec![input("one", Status::Running, "/repo")];
        state.sync_sessions(sessions.clone());
        let first = state.snapshot().version;
        state.sync_sessions(sessions.clone());
        assert_eq!(state.snapshot().version, first, "unchanged fleet");
        state.sync_sessions(vec![input("one", Status::Waiting, "/repo")]);
        assert_ne!(state.snapshot().version, first, "changed status");
    }

    #[test]
    fn overlapping_paths_collide_only_within_one_repository() {
        let mut state = FleetOpsState::new(4);
        state.sync_sessions(vec![
            input("alpha", Status::Running, "/repo-a"),
            input("beta", Status::Running, "/repo-a"),
            input("gamma", Status::Running, "/repo-b"),
        ]);
        let requests = state.pending_scans();
        assert_eq!(requests.len(), 3);
        for request in requests {
            ok_scan(
                &mut state,
                &request.session_id,
                request.generation,
                &["src/lib.rs"],
            );
        }

        let conflicts = state.snapshot().conflicts.clone();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].path, "src/lib.rs");
        assert_eq!(
            conflicts[0].sessions,
            ["alpha-title".to_string(), "beta-title".to_string()]
        );
        assert_eq!(state.snapshot().attention.len(), 2);
        assert!(state
            .snapshot()
            .attention
            .iter()
            .all(|row| row.reason == AttentionReason::Conflict));
        let readiness = state.snapshot().readiness.clone();
        assert!(readiness
            .iter()
            .filter(|row| row.session_id != "gamma")
            .all(|row| row.verdict == ReadinessVerdict::Blocked
                && row.blockers == [EvidenceKind::Conflict]));
        assert_eq!(
            readiness
                .iter()
                .find(|row| row.session_id == "gamma")
                .map(|row| row.verdict),
            Some(ReadinessVerdict::Missing)
        );

        // A scan stamped with a superseded generation cannot restore a conflict.
        state.sync_sessions(vec![
            input("alpha", Status::Running, "/repo-a"),
            input("gamma", Status::Running, "/repo-b"),
        ]);
        ok_scan(&mut state, "alpha", 1, &["src/lib.rs", "src/other.rs"]);
        assert!(state.snapshot().conflicts.is_empty());
    }

    #[test]
    fn a_scanned_fleet_requests_no_further_work() {
        let mut state = FleetOpsState::new(4);
        state.sync_sessions(vec![input("one", Status::Idle, "/repo")]);
        let requests = state.pending_scans();
        assert_eq!(requests.len(), 1);
        // In flight: not re-requested.
        assert!(state.pending_scans().is_empty());
        ok_scan(&mut state, "one", requests[0].generation, &["a.rs"]);
        assert!(state.pending_scans().is_empty(), "already scanned");

        // Changing the scan inputs invalidates the result and asks again.
        let mut moved = input("one", Status::Idle, "/repo");
        moved.base_branch = Some("develop".to_string());
        state.sync_sessions(vec![moved]);
        assert_eq!(state.pending_scans().len(), 1);
    }

    #[test]
    fn completed_work_cycle_and_manual_refresh_each_request_one_new_scan() {
        let mut state = FleetOpsState::new(4);
        state.sync_sessions(vec![input("one", Status::Running, "/repo")]);
        let first = state.pending_scans();
        ok_scan(&mut state, "one", first[0].generation, &["a.rs"]);
        assert!(state.pending_scans().is_empty());

        state.sync_sessions(vec![input("one", Status::Idle, "/repo")]);
        let after_completion = state.pending_scans();
        assert_eq!(after_completion.len(), 1);
        assert!(after_completion[0].generation > first[0].generation);
        ok_scan(
            &mut state,
            "one",
            after_completion[0].generation,
            &["a.rs", "b.rs"],
        );
        state.sync_sessions(vec![input("one", Status::Idle, "/repo")]);
        assert!(state.pending_scans().is_empty(), "stable idle state");

        state.invalidate_all_scans();
        assert_eq!(state.pending_scans().len(), 1, "explicit refresh");
        assert!(state.pending_scans().is_empty(), "refresh coalesces");
    }

    #[test]
    fn failed_trial_verification_enters_attention() {
        let mut state = FleetOpsState::new(4);
        state.sync_sessions(vec![input("one", Status::Idle, "/repo")]);
        let mut request = crate::trials::TrialCreateRequest::new(
            uuid::Uuid::new_v4().to_string(),
            "try it",
            ["claude".to_string(), "codex".to_string()],
            "default",
        )
        .unwrap()
        .with_verification_recipe(crate::trials::VerificationRecipe::new("true", 30).unwrap());
        request.manifest.contenders[0].session_id = Some("one".to_string());
        request.manifest.contenders[0].state = crate::trials::ContenderState::Completed;
        request.manifest.contenders[0].verification = Some(crate::trials::VerificationResult {
            state: crate::trials::VerificationState::Failed,
            exit_code: Some(1),
            duration_ms: 10,
            observed_at_ms: 20,
            summary: "tests failed".to_string(),
        });

        state.apply_trial(request.manifest.summary());

        let attention = state.snapshot().attention.clone();
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].reason, AttentionReason::Failed);
        assert!(attention[0].summary.contains("tests failed"));
    }

    #[test]
    fn linked_ready_campaign_task_enters_attention() {
        let mut state = FleetOpsState::new(4);
        state.sync_sessions(vec![input("one", Status::Idle, "/repo")]);
        state.apply_campaign(crate::campaigns::CampaignSummaryRecord {
            id: "campaign".to_string(),
            label: "Release".to_string(),
            state: crate::campaigns::CampaignState::Active,
            total: 1,
            ready: 1,
            active: 0,
            review: 0,
            done: 0,
            failed: 0,
            blocked: 0,
            updated_at_ms: 10,
            tasks: vec![crate::campaigns::CampaignTaskSummary {
                campaign_id: "campaign".to_string(),
                campaign_label: "Release".to_string(),
                id: "task".to_string(),
                title: "Publish".to_string(),
                state: crate::campaigns::CampaignTaskState::Ready,
                ready: true,
                blocked_by_failure: false,
                dependency_ids: Vec::new(),
                links: crate::campaigns::CampaignTaskLinks {
                    session_id: Some("one".to_string()),
                    ..crate::campaigns::CampaignTaskLinks::default()
                },
                updated_at_ms: 10,
            }],
        });

        let attention = state.snapshot().attention.clone();
        assert_eq!(attention[0].reason, AttentionReason::CampaignReady);
        assert!(attention[0].summary.contains("Publish"));
    }

    /// Manual distribution report for the Fleet hot path. Keep this ignored
    /// in normal CI because elapsed-time checks are hardware dependent. Run
    /// the ignored test from a release profile when changing the session fold
    /// or snapshot path.
    #[test]
    #[ignore = "manual release-mode performance report"]
    fn fleet_state_performance_report() {
        use std::hint::black_box;
        use std::time::Instant;

        fn percentile(samples: &mut [u128], percent: usize) -> u128 {
            samples.sort_unstable();
            let index = (samples.len() - 1) * percent / 100;
            samples[index]
        }

        fn report(samples: &mut [u128]) -> String {
            format!("{}/{} ns", percentile(samples, 50), percentile(samples, 95))
        }

        eprintln!("sessions | unchanged p50/p95 | rebuild p50/p95 | snapshot p50/p95");
        for session_count in [1_usize, 20, 100, 500] {
            let sessions: Vec<_> = (0..session_count)
                .map(|index| input(&format!("session-{index}"), Status::Idle, "/repo"))
                .collect();
            let iterations = match session_count {
                1 | 20 => 1_000,
                100 => 500,
                _ => 200,
            };

            let mut state = FleetOpsState::new((session_count + 1) as u32);
            state.sync_sessions(sessions.clone());
            let mut unchanged = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let started = Instant::now();
                state.sync_sessions(black_box(sessions.clone()));
                black_box(state.snapshot());
                unchanged.push(started.elapsed().as_nanos());
            }

            let mut rebuild = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let started = Instant::now();
                let mut rebuilt = FleetOpsState::new((session_count + 1) as u32);
                rebuilt.sync_sessions(black_box(sessions.clone()));
                black_box(rebuilt.snapshot());
                rebuild.push(started.elapsed().as_nanos());
            }

            let mut snapshot = Vec::with_capacity(iterations * 10);
            for _ in 0..iterations * 10 {
                let started = Instant::now();
                black_box(state.snapshot());
                snapshot.push(started.elapsed().as_nanos());
            }

            eprintln!(
                "{session_count} | {} | {} | {}",
                report(&mut unchanged),
                report(&mut rebuild),
                report(&mut snapshot)
            );
        }
    }

    #[test]
    fn a_failed_scan_is_reported_rather_than_read_as_clean() {
        let mut state = FleetOpsState::new(4);
        state.sync_sessions(vec![input("one", Status::Idle, "/repo")]);
        let generation = state.pending_scans()[0].generation;
        state.apply_scan(ScanOutcome {
            session_id: "one".to_string(),
            generation,
            result: Err("not a git repository".to_string()),
        });
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.scan_errors,
            ["one-title: not a git repository".to_string()]
        );
        let readiness = &snapshot.readiness[0];
        assert_eq!(readiness.verdict, ReadinessVerdict::Blocked);
        assert_eq!(readiness.blockers, [EvidenceKind::Diff]);
    }

    #[test]
    fn checkpoint_updates_are_idempotent_and_outlive_the_active_session_row() {
        let mut state = FleetOpsState::new(4);
        state.sync_sessions(vec![input("one", Status::Idle, "/repo")]);
        assert!(state.begin_checkpoint_load());
        assert!(
            !state.begin_checkpoint_load(),
            "one load while a request is in flight"
        );
        state.fail_checkpoint_load("temporary failure".to_string());
        assert!(state.begin_checkpoint_load(), "failed loads are retryable");

        let pending = crate::checkpoints::CheckpointSummary {
            id: "checkpoint-1".to_string(),
            session_id: "one".to_string(),
            session_title: "one-title".to_string(),
            label: "before trial".to_string(),
            created_at_ms: 10,
            snapshot_ref: "capturing".to_string(),
            state: CheckpointState::Materializing,
            detail: None,
        };
        state.begin_checkpoint(pending.clone());
        state.begin_checkpoint(pending);
        assert_eq!(state.snapshot().checkpoints.len(), 1);

        state.apply_checkpoint(crate::checkpoints::CheckpointSummary {
            id: "checkpoint-1".to_string(),
            session_id: "one".to_string(),
            session_title: "one-title".to_string(),
            label: "before trial".to_string(),
            created_at_ms: 10,
            snapshot_ref: "123456789abc".to_string(),
            state: CheckpointState::Complete,
            detail: None,
        });
        state.sync_sessions(Vec::new());
        let snapshot = state.snapshot();
        assert_eq!(snapshot.checkpoints.len(), 1);
        assert_eq!(snapshot.checkpoints[0].state, CheckpointState::Complete);
        assert_eq!(snapshot.checkpoints[0].title, "one-title");
        assert!(!snapshot.checkpoints[0].session_available);
    }
}
