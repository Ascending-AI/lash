//! The attempt budget of an obligation delivery: a host lever (ADR 0014)
//! the obligation relays read.

/// How a deployment's recovery pass bounds its obligation deliveries (ADR
/// 0109 §1.8). Host levers (ADR 0014): lash implements the mechanics, the
/// host chooses the numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryPassBudget {
    /// The longest one obligation delivery attempt runs before it is
    /// abandoned and retried
    /// ([`RelayPolicy::attempt_budget_ms`](crate::runtime::obligations::relay::RelayPolicy::attempt_budget_ms)).
    /// Default 30 s. Keep it below the relay's 60 s claim TTL, so a claim
    /// never lapses under an attempt still running.
    pub attempt: std::time::Duration,
}

impl RecoveryPassBudget {
    /// Standard delivery preset: 30s per attempt. This historical value has
    /// no supporting workload measurement.
    pub fn standard() -> Self {
        Self {
            attempt: std::time::Duration::from_millis(
                crate::runtime::obligations::relay::RelayPolicy::DEFAULT_ATTEMPT_BUDGET_MS,
            ),
        }
    }
}

impl Default for RecoveryPassBudget {
    fn default() -> Self {
        Self::standard()
    }
}

impl RecoveryPassBudget {
    /// The attempt budget in milliseconds, as a relay policy carries it.
    #[must_use]
    pub fn attempt_ms(&self) -> u64 {
        u64::try_from(self.attempt.as_millis()).unwrap_or(u64::MAX)
    }
}
