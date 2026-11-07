use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EvidenceKind {
    Verification,
    Conflict,
    Tests,
    Format,
    Lint,
    Review,
    Diff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceState {
    Passed,
    Failed,
    Running,
    Missing,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub state: EvidenceState,
    pub updated_at_ms: u64,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadinessPolicy {
    required: BTreeSet<EvidenceKind>,
}

impl ReadinessPolicy {
    pub fn new(required: impl IntoIterator<Item = EvidenceKind>) -> Self {
        Self {
            required: required.into_iter().collect(),
        }
    }

    pub fn required(&self) -> impl Iterator<Item = EvidenceKind> + '_ {
        self.required.iter().copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessVerdict {
    Ready,
    Running,
    Blocked,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadinessReport {
    pub verdict: ReadinessVerdict,
    pub blockers: Vec<EvidenceKind>,
}

impl ReadinessReport {
    pub fn evaluate(
        policy: &ReadinessPolicy,
        evidence: impl IntoIterator<Item = Evidence>,
    ) -> Self {
        let evidence: BTreeMap<_, _> = evidence
            .into_iter()
            .map(|item| (item.kind, item.state))
            .collect();
        let mut failed = Vec::new();
        let mut running = Vec::new();
        let mut missing = Vec::new();

        for kind in policy.required() {
            match evidence
                .get(&kind)
                .copied()
                .unwrap_or(EvidenceState::Missing)
            {
                EvidenceState::Passed => {}
                EvidenceState::Failed => failed.push(kind),
                EvidenceState::Running => running.push(kind),
                EvidenceState::Missing | EvidenceState::Stale => missing.push(kind),
            }
        }

        if !failed.is_empty() {
            Self {
                verdict: ReadinessVerdict::Blocked,
                blockers: failed,
            }
        } else if !running.is_empty() {
            Self {
                verdict: ReadinessVerdict::Running,
                blockers: running,
            }
        } else if !missing.is_empty() {
            Self {
                verdict: ReadinessVerdict::Missing,
                blockers: missing,
            }
        } else {
            Self {
                verdict: ReadinessVerdict::Ready,
                blockers: Vec::new(),
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        self.verdict == ReadinessVerdict::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(kind: EvidenceKind, state: EvidenceState) -> Evidence {
        Evidence {
            kind,
            state,
            updated_at_ms: 0,
            summary: String::new(),
        }
    }

    #[test]
    fn uses_failure_running_missing_ready_precedence() {
        let policy = ReadinessPolicy::new([EvidenceKind::Tests, EvidenceKind::Lint]);
        let cases = [
            (
                vec![
                    evidence(EvidenceKind::Tests, EvidenceState::Failed),
                    evidence(EvidenceKind::Lint, EvidenceState::Running),
                ],
                ReadinessVerdict::Blocked,
            ),
            (
                vec![
                    evidence(EvidenceKind::Tests, EvidenceState::Passed),
                    evidence(EvidenceKind::Lint, EvidenceState::Running),
                ],
                ReadinessVerdict::Running,
            ),
            (
                vec![evidence(EvidenceKind::Tests, EvidenceState::Passed)],
                ReadinessVerdict::Missing,
            ),
            (
                vec![
                    evidence(EvidenceKind::Tests, EvidenceState::Passed),
                    evidence(EvidenceKind::Lint, EvidenceState::Passed),
                ],
                ReadinessVerdict::Ready,
            ),
        ];

        for (evidence, expected) in cases {
            assert_eq!(
                ReadinessReport::evaluate(&policy, evidence).verdict,
                expected
            );
        }
    }
}
