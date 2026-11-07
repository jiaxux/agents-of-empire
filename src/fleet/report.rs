//! Shared, serializable Fleet Operations projection.
//!
//! TUI rendering reads its immutable in-memory snapshot directly. CLI and HTTP
//! callers request this one-shot projection, which feeds the same indexes and
//! durable stores. Repository inspection is synchronous here by design; HTTP
//! handlers must call it through `spawn_blocking`.

use serde::Serialize;

use crate::session::Instance;
use crate::tui::fleet_state::{
    CampaignLoadState, CheckpointLoadState, FleetOpsState, FleetSessionInput, FleetSnapshot,
    ScanState, TrialLoadState,
};

pub const FLEET_REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize)]
pub struct FleetReport {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub sessions: usize,
    pub attention: Vec<FleetAttentionReport>,
    pub conflicts: Vec<FleetConflictReport>,
    pub readiness: Vec<FleetReadinessReport>,
    pub budget: FleetBudgetReport,
    pub campaign: Option<FleetCampaignReport>,
    pub campaign_tasks: Vec<crate::campaigns::CampaignTaskSummary>,
    pub campaign_history: FleetHistoryReport,
    pub trials: Vec<crate::trials::TrialSummary>,
    pub trial_history: FleetHistoryReport,
    pub checkpoints: Vec<FleetCheckpointReport>,
    pub checkpoint_history: FleetHistoryReport,
    pub scan: FleetScanReport,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetAttentionReport {
    pub item_id: String,
    pub session_id: String,
    pub title: String,
    pub reason: String,
    pub summary: String,
    pub since_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetConflictReport {
    pub repository: String,
    pub path: String,
    pub session_ids: Vec<String>,
    pub sessions: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetReadinessReport {
    pub session_id: String,
    pub title: String,
    pub verdict: String,
    pub blockers: Vec<String>,
    pub evidence: Vec<FleetEvidenceReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetEvidenceReport {
    pub kind: String,
    pub state: String,
    pub observed_at_ms: u64,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetBudgetReport {
    pub active_workers: usize,
    pub max_active_workers: Option<usize>,
    pub active_weight: u32,
    pub reported_tokens: Option<u64>,
    pub rejected: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetCampaignReport {
    pub total: usize,
    pub ready: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub blocked: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetCheckpointReport {
    pub checkpoint_id: String,
    pub session_id: String,
    pub session_available: bool,
    pub title: String,
    pub label: String,
    pub snapshot_ref: String,
    pub created_at_ms: u64,
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetHistoryReport {
    pub state: String,
    pub loaded: usize,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FleetScanReport {
    pub state: String,
    pub scanned: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

impl FleetReport {
    /// Build a complete one-shot report from already-loaded session state.
    /// This function can inspect Git and durable storage and must never run on
    /// a UI/render thread or an async executor worker.
    pub fn build(instances: &[Instance], max_workers: u32) -> Self {
        let mut state = FleetOpsState::new(max_workers);
        state.sync_sessions(
            instances
                .iter()
                .filter_map(FleetSessionInput::from_instance)
                .collect(),
        );
        for request in state.pending_scans() {
            state.apply_scan(crate::tui::fleet_poller::scan_now(request));
        }
        match crate::checkpoints::CheckpointStore::open().and_then(|store| store.load()) {
            Ok(report) => state.apply_checkpoint_load(report),
            Err(error) => state.fail_checkpoint_load(error.to_string()),
        }
        match crate::trials::TrialStore::open().and_then(|store| store.load()) {
            Ok(report) => state.apply_trial_load(report),
            Err(error) => state.fail_trial_load(error.to_string()),
        }
        match crate::campaigns::CampaignStore::open().and_then(|store| store.load()) {
            Ok(report) => state.apply_campaign_load(report),
            Err(error) => state.fail_campaign_load(error.to_string()),
        }
        if let Ok(item_ids) = crate::fleet_acknowledgements::AttentionAcknowledgementStore::open()
            .and_then(|store| store.load())
        {
            state.apply_attention_acknowledgements(item_ids);
        }
        Self::from_snapshot(&state.snapshot())
    }

    fn from_snapshot(snapshot: &FleetSnapshot) -> Self {
        Self {
            schema_version: FLEET_REPORT_SCHEMA_VERSION,
            generated_at_ms: crate::trials::now_ms(),
            sessions: snapshot.sessions,
            attention: snapshot
                .attention
                .iter()
                .map(|row| FleetAttentionReport {
                    item_id: row.item_id.clone(),
                    session_id: row.session_id.clone(),
                    title: row.title.clone(),
                    reason: attention_reason(row.reason).to_string(),
                    summary: row.summary.clone(),
                    since_ms: row.since_ms,
                })
                .collect(),
            conflicts: snapshot
                .conflicts
                .iter()
                .map(|row| FleetConflictReport {
                    repository: row.repo.clone(),
                    path: row.path.clone(),
                    session_ids: row.session_ids.clone(),
                    sessions: row.sessions.clone(),
                })
                .collect(),
            readiness: snapshot
                .readiness
                .iter()
                .map(|row| FleetReadinessReport {
                    session_id: row.session_id.clone(),
                    title: row.title.clone(),
                    verdict: readiness_verdict(row.verdict).to_string(),
                    blockers: row
                        .blockers
                        .iter()
                        .map(|kind| evidence_kind(*kind).to_string())
                        .collect(),
                    evidence: row
                        .evidence
                        .iter()
                        .map(|evidence| FleetEvidenceReport {
                            kind: evidence_kind(evidence.kind).to_string(),
                            state: evidence_state(evidence.state).to_string(),
                            observed_at_ms: evidence.updated_at_ms,
                            summary: evidence.summary.clone(),
                        })
                        .collect(),
                })
                .collect(),
            budget: FleetBudgetReport {
                active_workers: snapshot.usage.active,
                max_active_workers: snapshot.limits.map(|limits| limits.max_active),
                active_weight: snapshot.usage.weight,
                reported_tokens: None,
                rejected: snapshot.rejected.clone(),
            },
            campaign: snapshot.campaign.map(|summary| FleetCampaignReport {
                total: summary.total,
                ready: summary.ready,
                running: summary.running,
                completed: summary.completed,
                failed: summary.failed,
                blocked: summary.blocked,
            }),
            campaign_tasks: snapshot.campaign_tasks.clone(),
            campaign_history: campaign_history(&snapshot.campaign_load),
            trials: snapshot.trials.clone(),
            trial_history: trial_history(&snapshot.trial_load),
            checkpoints: snapshot
                .checkpoints
                .iter()
                .map(|row| FleetCheckpointReport {
                    checkpoint_id: row.checkpoint_id.clone(),
                    session_id: row.session_id.clone(),
                    session_available: row.session_available,
                    title: row.title.clone(),
                    label: row.label.clone(),
                    snapshot_ref: row.snapshot_ref.clone(),
                    created_at_ms: row.created_at_ms,
                    state: checkpoint_state(row.state).to_string(),
                    detail: row.detail.clone(),
                })
                .collect(),
            checkpoint_history: checkpoint_history(&snapshot.checkpoint_load),
            scan: scan_report(&snapshot.scan, snapshot.scan_errors.clone()),
        }
    }
}

fn attention_reason(reason: super::AttentionReason) -> &'static str {
    match reason {
        super::AttentionReason::Approval => "approval",
        super::AttentionReason::Input => "input",
        super::AttentionReason::Failed => "failed",
        super::AttentionReason::Capacity => "capacity",
        super::AttentionReason::Conflict => "conflict",
        super::AttentionReason::CampaignReady => "campaign_ready",
        super::AttentionReason::Completed => "completed",
    }
}

fn readiness_verdict(verdict: super::ReadinessVerdict) -> &'static str {
    match verdict {
        super::ReadinessVerdict::Ready => "ready",
        super::ReadinessVerdict::Running => "running",
        super::ReadinessVerdict::Blocked => "blocked",
        super::ReadinessVerdict::Missing => "missing",
    }
}

fn evidence_kind(kind: super::EvidenceKind) -> &'static str {
    match kind {
        super::EvidenceKind::Verification => "verification",
        super::EvidenceKind::Conflict => "conflict",
        super::EvidenceKind::Tests => "tests",
        super::EvidenceKind::Format => "format",
        super::EvidenceKind::Lint => "lint",
        super::EvidenceKind::Review => "review",
        super::EvidenceKind::Diff => "diff",
    }
}

fn evidence_state(state: super::EvidenceState) -> &'static str {
    match state {
        super::EvidenceState::Passed => "passed",
        super::EvidenceState::Failed => "failed",
        super::EvidenceState::Running => "running",
        super::EvidenceState::Missing => "missing",
        super::EvidenceState::Stale => "stale",
    }
}

fn checkpoint_state(state: super::CheckpointState) -> &'static str {
    match state {
        super::CheckpointState::Materializing => "materializing",
        super::CheckpointState::Complete => "complete",
        super::CheckpointState::Incomplete => "incomplete",
    }
}

fn checkpoint_history(state: &CheckpointLoadState) -> FleetHistoryReport {
    match state {
        CheckpointLoadState::Dormant => history("dormant", 0, Vec::new()),
        CheckpointLoadState::Loading => history("loading", 0, Vec::new()),
        CheckpointLoadState::Ready { loaded, errors } => history("ready", *loaded, errors.clone()),
        CheckpointLoadState::Failed(error) => history("failed", 0, vec![error.clone()]),
    }
}

fn trial_history(state: &TrialLoadState) -> FleetHistoryReport {
    match state {
        TrialLoadState::Dormant => history("dormant", 0, Vec::new()),
        TrialLoadState::Loading => history("loading", 0, Vec::new()),
        TrialLoadState::Ready { loaded, errors } => history("ready", *loaded, errors.clone()),
        TrialLoadState::Failed(error) => history("failed", 0, vec![error.clone()]),
    }
}

fn campaign_history(state: &CampaignLoadState) -> FleetHistoryReport {
    match state {
        CampaignLoadState::Dormant => history("dormant", 0, Vec::new()),
        CampaignLoadState::Loading => history("loading", 0, Vec::new()),
        CampaignLoadState::Ready { loaded, errors } => history("ready", *loaded, errors.clone()),
        CampaignLoadState::Failed(error) => history("failed", 0, vec![error.clone()]),
    }
}

fn history(state: &str, loaded: usize, errors: Vec<String>) -> FleetHistoryReport {
    FleetHistoryReport {
        state: state.to_string(),
        loaded,
        errors,
    }
}

fn scan_report(state: &ScanState, errors: Vec<String>) -> FleetScanReport {
    match state {
        ScanState::Dormant => FleetScanReport {
            state: "dormant".to_string(),
            scanned: 0,
            skipped: 0,
            errors,
        },
        ScanState::Scanning { pending } => FleetScanReport {
            state: "scanning".to_string(),
            scanned: 0,
            skipped: *pending,
            errors,
        },
        ScanState::Ready { scanned, skipped } => FleetScanReport {
            state: "ready".to_string(),
            scanned: *scanned,
            skipped: *skipped,
            errors,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_report_has_a_stable_schema_and_honest_unknown_usage() {
        let report = FleetReport::from_snapshot(&FleetSnapshot::default());
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["schema_version"], FLEET_REPORT_SCHEMA_VERSION);
        assert_eq!(value["sessions"], 0);
        assert!(value["budget"]["reported_tokens"].is_null());
        assert!(value["attention"].as_array().unwrap().is_empty());
    }
}
