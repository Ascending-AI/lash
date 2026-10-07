//! The recovery leader lease's cadence and the attempt budget of an
//! obligation delivery: host levers (ADR 0014) the remaining obligation
//! relays and the recovery leader read.

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
    /// The longest a recovery tick waits on its kinds' due passes before its
    /// leader-only arms run. A pass still delivering then finishes on its
    /// kind's lane, and the next tick reports it. The leader arms run
    /// concurrently under a guard of twice this duration. Default 1 s.
    pub tick_wait: std::time::Duration,
}

impl Default for RecoveryPassBudget {
    fn default() -> Self {
        Self {
            attempt: std::time::Duration::from_millis(
                crate::runtime::obligations::relay::RelayPolicy::DEFAULT_ATTEMPT_BUDGET_MS,
            ),
            tick_wait: std::time::Duration::from_secs(1),
        }
    }
}

impl RecoveryPassBudget {
    /// The attempt budget in milliseconds, as a relay policy carries it.
    #[must_use]
    pub fn attempt_ms(&self) -> u64 {
        u64::try_from(self.attempt.as_millis()).unwrap_or(u64::MAX)
    }
}

/// The recovery leader lease's cadence (ADR 0109 §1.6). Host levers
/// (ADR 0014).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryLeaseTimings {
    /// How long a renew keeps the lease.
    pub ttl: std::time::Duration,
    /// How often a leader renews.
    pub renew_every: std::time::Duration,
    /// How long one acquire or renew may take before it counts as failed.
    pub renew_timeout: std::time::Duration,
    /// How far before the TTL a leader stops trusting its lease.
    pub trust_margin: std::time::Duration,
    /// How often a follower tries to acquire.
    pub follower_retry: std::time::Duration,
    /// The most random delay added to a follower's retry.
    pub follower_jitter: std::time::Duration,
    /// How long a holder leads before a higher rank may preempt it.
    pub min_tenure: std::time::Duration,
}

impl Default for RecoveryLeaseTimings {
    fn default() -> Self {
        Self {
            ttl: std::time::Duration::from_secs(15),
            renew_every: std::time::Duration::from_secs(5),
            renew_timeout: std::time::Duration::from_millis(2_500),
            trust_margin: std::time::Duration::from_secs(2),
            follower_retry: std::time::Duration::from_secs(5),
            follower_jitter: std::time::Duration::from_millis(500),
            min_tenure: std::time::Duration::from_secs(30),
        }
    }
}

/// How this deployment competes for the recovery leader lease (ADR 0109
/// §1.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryLeaseConfig {
    /// This build's rank: a higher rank preempts a lower-ranked leader once
    /// that leader has held the lease for
    /// [`min_tenure`](RecoveryLeaseTimings::min_tenure). A rolling deploy
    /// gives each new build a higher rank than the last, so the newest
    /// build leads recovery. Defaults to 0.
    pub generation_rank: i64,
    /// The lease's cadence.
    pub timings: RecoveryLeaseTimings,
}
