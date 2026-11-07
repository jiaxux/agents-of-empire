//! Fast, UI-independent primitives for operating several agent sessions.
//!
//! The fleet layer performs no I/O and starts no background work. Callers feed
//! it state changes, then render the resulting snapshots. This keeps expensive
//! repository inspection and terminal interaction out of the TUI render loop.

mod attention;
mod budget;
mod campaign;
mod checkpoint;
mod conflict;
mod readiness;
mod report;
mod trials;

pub use attention::{AttentionInbox, AttentionItem, AttentionReason};
pub use budget::{Admission, BudgetGovernor, FleetLimits, FleetUsage, LeaseRequest};
pub use campaign::{
    CampaignError, CampaignGraph, CampaignSummary, CampaignTask, TaskKey, TaskStatus,
};
pub use checkpoint::{Checkpoint, CheckpointId, CheckpointState, CheckpointTimeline};
pub use conflict::{ConflictRadar, FileConflict};
pub use readiness::{
    Evidence, EvidenceKind, EvidenceState, ReadinessPolicy, ReadinessReport, ReadinessVerdict,
};
pub use report::{
    FleetAttentionReport, FleetBudgetReport, FleetCampaignReport, FleetCheckpointReport,
    FleetConflictReport, FleetEvidenceReport, FleetHistoryReport, FleetReadinessReport,
    FleetReport, FleetScanReport, FLEET_REPORT_SCHEMA_VERSION,
};
pub use trials::{rank_trials, TrialCandidate, TrialError, TrialResult, TrialWeights};

use std::fmt;

/// Stable adapter key used by fleet indexes without coupling them to storage.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl From<&str> for SessionKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SessionKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
