use std::collections::HashMap;

use super::SessionKey;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FleetLimits {
    pub max_active: usize,
    pub max_weight: u32,
    pub max_reserved_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseRequest {
    pub weight: u32,
    pub reserved_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FleetUsage {
    pub active: usize,
    pub weight: u32,
    pub reserved_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Granted,
    AlreadyActive,
    ActiveLimit { limit: usize },
    WeightLimit { requested: u32, available: u32 },
    TokenLimit { requested: u64, available: u64 },
}

/// Constant-time admission control for concurrent agent work.
#[derive(Debug)]
pub struct BudgetGovernor {
    limits: FleetLimits,
    usage: FleetUsage,
    leases: HashMap<SessionKey, LeaseRequest>,
}

impl BudgetGovernor {
    pub fn new(limits: FleetLimits) -> Self {
        Self {
            limits,
            usage: FleetUsage::default(),
            leases: HashMap::new(),
        }
    }

    pub fn acquire(&mut self, session: SessionKey, request: LeaseRequest) -> Admission {
        if self.leases.contains_key(&session) {
            return Admission::AlreadyActive;
        }
        if self.usage.active >= self.limits.max_active {
            return Admission::ActiveLimit {
                limit: self.limits.max_active,
            };
        }

        let available_weight = self.limits.max_weight.saturating_sub(self.usage.weight);
        if request.weight > available_weight {
            return Admission::WeightLimit {
                requested: request.weight,
                available: available_weight,
            };
        }

        let available_tokens = self
            .limits
            .max_reserved_tokens
            .saturating_sub(self.usage.reserved_tokens);
        if request.reserved_tokens > available_tokens {
            return Admission::TokenLimit {
                requested: request.reserved_tokens,
                available: available_tokens,
            };
        }

        self.usage.active += 1;
        self.usage.weight += request.weight;
        self.usage.reserved_tokens += request.reserved_tokens;
        self.leases.insert(session, request);
        Admission::Granted
    }

    pub fn release(&mut self, session: &SessionKey) -> bool {
        let Some(request) = self.leases.remove(session) else {
            return false;
        };
        self.usage.active -= 1;
        self.usage.weight -= request.weight;
        self.usage.reserved_tokens -= request.reserved_tokens;
        true
    }

    pub fn usage(&self) -> FleetUsage {
        self.usage
    }

    pub fn limits(&self) -> FleetLimits {
        self.limits
    }

    pub fn set_limits(&mut self, limits: FleetLimits) {
        self.limits = limits;
    }

    pub fn is_active(&self, session: &SessionKey) -> bool {
        self.leases.contains_key(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_each_limit_and_releases_capacity() {
        let mut governor = BudgetGovernor::new(FleetLimits {
            max_active: 2,
            max_weight: 4,
            max_reserved_tokens: 1_000,
        });
        let normal = LeaseRequest {
            weight: 2,
            reserved_tokens: 400,
        };

        assert_eq!(governor.acquire("one".into(), normal), Admission::Granted);
        assert_eq!(
            governor.acquire(
                "heavy".into(),
                LeaseRequest {
                    weight: 3,
                    reserved_tokens: 100,
                }
            ),
            Admission::WeightLimit {
                requested: 3,
                available: 2,
            }
        );
        assert_eq!(governor.acquire("two".into(), normal), Admission::Granted);
        assert!(matches!(
            governor.acquire("three".into(), normal),
            Admission::ActiveLimit { limit: 2 }
        ));
        assert!(governor.release(&SessionKey::from("one")));
        assert_eq!(governor.usage().active, 1);
    }
}
