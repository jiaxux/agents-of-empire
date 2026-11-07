use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;

use super::SessionKey;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CheckpointId(String);

impl CheckpointId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointState {
    Materializing,
    Complete,
    Incomplete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub session: SessionKey,
    pub session_title: String,
    pub created_at_ms: u64,
    pub label: String,
    /// Opaque reference understood by the checkpoint backend.
    pub snapshot_ref: String,
    pub state: CheckpointState,
    pub detail: Option<String>,
}

/// Bounded in-memory projection of durable checkpoints.
#[derive(Debug)]
pub struct CheckpointTimeline {
    retention: NonZeroUsize,
    by_session: HashMap<SessionKey, VecDeque<Checkpoint>>,
}

impl CheckpointTimeline {
    pub fn new(retention: NonZeroUsize) -> Self {
        Self {
            retention,
            by_session: HashMap::new(),
        }
    }

    /// Insert or replace one durable checkpoint. Replaying a store load is
    /// idempotent, and each session retains only its newest configured rows.
    pub fn upsert(&mut self, checkpoint: Checkpoint) {
        let checkpoints = self
            .by_session
            .entry(checkpoint.session.clone())
            .or_default();
        if let Some(index) = checkpoints
            .iter()
            .position(|existing| existing.id == checkpoint.id)
        {
            checkpoints.remove(index);
        }
        checkpoints.push_back(checkpoint);
        checkpoints
            .make_contiguous()
            .sort_by_key(|checkpoint| checkpoint.created_at_ms);
        while checkpoints.len() > self.retention.get() {
            checkpoints.pop_front();
        }
    }

    pub fn latest(&self, session: &SessionKey) -> Option<&Checkpoint> {
        self.by_session.get(session)?.back()
    }

    pub fn get(&self, session: &SessionKey, id: &CheckpointId) -> Option<&Checkpoint> {
        self.by_session
            .get(session)?
            .iter()
            .find(|checkpoint| &checkpoint.id == id)
    }

    pub fn get_mut(&mut self, id: &CheckpointId) -> Option<&mut Checkpoint> {
        self.by_session
            .values_mut()
            .flat_map(|checkpoints| checkpoints.iter_mut())
            .find(|checkpoint| &checkpoint.id == id)
    }

    pub fn remove(&mut self, id: &CheckpointId) -> bool {
        let mut removed = false;
        self.by_session.retain(|_, checkpoints| {
            let before = checkpoints.len();
            checkpoints.retain(|checkpoint| &checkpoint.id != id);
            removed |= checkpoints.len() != before;
            !checkpoints.is_empty()
        });
        removed
    }

    pub fn for_session(&self, session: &SessionKey) -> impl Iterator<Item = &Checkpoint> {
        self.by_session
            .get(session)
            .into_iter()
            .flat_map(|checkpoints| checkpoints.iter().rev())
    }

    pub fn all(&self) -> impl Iterator<Item = &Checkpoint> {
        self.by_session
            .values()
            .flat_map(|checkpoints| checkpoints.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(session: &SessionKey, id: &str, at: u64, label: &str) -> Checkpoint {
        Checkpoint {
            id: CheckpointId::new(id),
            session: session.clone(),
            session_title: "Agent".to_string(),
            created_at_ms: at,
            label: label.to_string(),
            snapshot_ref: id.to_string(),
            state: CheckpointState::Complete,
            detail: None,
        }
    }

    #[test]
    fn retains_a_bounded_newest_first_timeline_and_replays_idempotently() {
        let mut timeline = CheckpointTimeline::new(NonZeroUsize::new(2).unwrap());
        let session = SessionKey::from("agent");
        timeline.upsert(checkpoint(&session, "first", 10, "first"));
        timeline.upsert(checkpoint(&session, "second", 20, "second"));
        timeline.upsert(checkpoint(&session, "third", 30, "third"));
        timeline.upsert(checkpoint(&session, "third", 30, "third updated"));
        timeline.upsert(checkpoint(&session, "older", 5, "older"));

        let labels: Vec<_> = timeline
            .for_session(&session)
            .map(|checkpoint| checkpoint.label.as_str())
            .collect();
        assert_eq!(labels, ["third updated", "second"]);
        assert!(timeline
            .get(&session, &CheckpointId::new("first"))
            .is_none());
        assert_eq!(timeline.latest(&session).unwrap().id.as_str(), "third");
    }
}
