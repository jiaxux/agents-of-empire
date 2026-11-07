use std::fmt;

use super::SessionKey;

const MAX_SCORE: u16 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrialWeights {
    pub correctness: u16,
    pub quality: u16,
    pub focus: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrialCandidate {
    pub session: SessionKey,
    pub correctness: u16,
    pub quality: u16,
    pub focus: u16,
    pub elapsed_ms: u64,
    pub tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrialResult {
    pub session: SessionKey,
    /// Weighted score in thousandths, from 0 through 1,000.
    pub score: u16,
    pub elapsed_ms: u64,
    pub tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrialError {
    EmptyWeights,
    ScoreOutOfRange {
        session: SessionKey,
        dimension: &'static str,
        score: u16,
    },
}

impl fmt::Display for TrialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyWeights => formatter.write_str("trial weights must include one dimension"),
            Self::ScoreOutOfRange {
                session,
                dimension,
                score,
            } => write!(
                formatter,
                "trial score {score} for {session} {dimension} exceeds {MAX_SCORE}"
            ),
        }
    }
}

impl std::error::Error for TrialError {}

pub fn rank_trials(
    candidates: &[TrialCandidate],
    weights: TrialWeights,
) -> Result<Vec<TrialResult>, TrialError> {
    let total_weight =
        u64::from(weights.correctness) + u64::from(weights.quality) + u64::from(weights.focus);
    if total_weight == 0 {
        return Err(TrialError::EmptyWeights);
    }

    let mut results = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        for (dimension, score) in [
            ("correctness", candidate.correctness),
            ("quality", candidate.quality),
            ("focus", candidate.focus),
        ] {
            if score > MAX_SCORE {
                return Err(TrialError::ScoreOutOfRange {
                    session: candidate.session.clone(),
                    dimension,
                    score,
                });
            }
        }

        let weighted = u64::from(candidate.correctness) * u64::from(weights.correctness)
            + u64::from(candidate.quality) * u64::from(weights.quality)
            + u64::from(candidate.focus) * u64::from(weights.focus);
        results.push(TrialResult {
            session: candidate.session.clone(),
            score: (weighted / total_weight) as u16,
            elapsed_ms: candidate.elapsed_ms,
            tokens: candidate.tokens,
        });
    }

    results.sort_unstable_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.elapsed_ms.cmp(&right.elapsed_ms))
            .then_with(|| left.tokens.cmp(&right.tokens))
            .then_with(|| left.session.cmp(&right.session))
    });
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_quality_then_latency_then_cost() {
        let candidates = [
            TrialCandidate {
                session: "slow".into(),
                correctness: 900,
                quality: 800,
                focus: 700,
                elapsed_ms: 200,
                tokens: 100,
            },
            TrialCandidate {
                session: "fast".into(),
                correctness: 900,
                quality: 800,
                focus: 700,
                elapsed_ms: 100,
                tokens: 200,
            },
            TrialCandidate {
                session: "best".into(),
                correctness: 1_000,
                quality: 900,
                focus: 800,
                elapsed_ms: 300,
                tokens: 300,
            },
        ];
        let ranked = rank_trials(
            &candidates,
            TrialWeights {
                correctness: 3,
                quality: 2,
                focus: 1,
            },
        )
        .unwrap();

        let sessions: Vec<_> = ranked
            .iter()
            .map(|result| result.session.as_str())
            .collect();
        assert_eq!(sessions, ["best", "fast", "slow"]);
        assert!(ranked[0].score > ranked[1].score);
    }
}
