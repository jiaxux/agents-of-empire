use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};

use super::SessionKey;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionReason {
    Approval,
    Input,
    Failed,
    Capacity,
    Conflict,
    CampaignReady,
    Completed,
}

impl AttentionReason {
    pub(crate) fn priority(self) -> u8 {
        match self {
            Self::Failed => 5,
            Self::Approval => 4,
            Self::Capacity => 4,
            Self::Conflict => 3,
            Self::CampaignReady => 2,
            Self::Input => 2,
            Self::Completed => 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionItem {
    pub session: SessionKey,
    pub reason: AttentionReason,
    pub since_ms: u64,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RankKey {
    priority: Reverse<u8>,
    since_ms: u64,
    session: SessionKey,
}

impl From<&AttentionItem> for RankKey {
    fn from(item: &AttentionItem) -> Self {
        Self {
            priority: Reverse(item.reason.priority()),
            since_ms: item.since_ms,
            session: item.session.clone(),
        }
    }
}

/// Incremental priority index for sessions that need human attention.
#[derive(Debug, Default)]
pub struct AttentionInbox {
    items: HashMap<SessionKey, AttentionItem>,
    order: BTreeSet<RankKey>,
}

impl AttentionInbox {
    pub fn upsert(&mut self, item: AttentionItem) {
        if let Some(previous) = self.items.remove(&item.session) {
            self.order.remove(&RankKey::from(&previous));
        }
        self.order.insert(RankKey::from(&item));
        self.items.insert(item.session.clone(), item);
    }

    pub fn dismiss(&mut self, session: &SessionKey) -> Option<AttentionItem> {
        let item = self.items.remove(session)?;
        self.order.remove(&RankKey::from(&item));
        Some(item)
    }

    pub fn get(&self, session: &SessionKey) -> Option<&AttentionItem> {
        self.items.get(session)
    }

    pub fn top(&self, limit: usize) -> impl Iterator<Item = &AttentionItem> {
        self.order
            .iter()
            .take(limit)
            .filter_map(|key| self.items.get(&key.session))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prioritizes_and_replaces_attention_without_duplicates() {
        let mut inbox = AttentionInbox::default();
        inbox.upsert(AttentionItem {
            session: "waiting".into(),
            reason: AttentionReason::Input,
            since_ms: 10,
            summary: "needs a value".into(),
        });
        inbox.upsert(AttentionItem {
            session: "failed".into(),
            reason: AttentionReason::Failed,
            since_ms: 20,
            summary: "test failed".into(),
        });
        inbox.upsert(AttentionItem {
            session: "waiting".into(),
            reason: AttentionReason::Approval,
            since_ms: 30,
            summary: "approve command".into(),
        });

        let sessions: Vec<_> = inbox.top(10).map(|item| item.session.as_str()).collect();
        assert_eq!(sessions, ["failed", "waiting"]);
        assert_eq!(inbox.len(), 2);
        assert_eq!(
            inbox.dismiss(&SessionKey::from("failed")).unwrap().summary,
            "test failed"
        );
    }
}
