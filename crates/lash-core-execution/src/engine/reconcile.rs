//! The reconcile tick's vocabulary (ADR 0104 O2/O3/O4, FIG-3600 S7, HoS
//! decisions 69–70): what an engine's schedule carries from one tick to the
//! next, and what a tick reports.
//!
//! The tick itself is engine-neutral kernel code (`lash_core::drive::
//! reconcile_once`); an engine only schedules it, through
//! [`SessionDriver::reconcile`](crate::SessionDriver::reconcile). Each
//! engine runs the tick on an interval inside the driver's deployment and
//! carries the [`ReconcileCursor`] forward.

use serde::{Deserialize, Serialize};

use super::control::{EngineCursor, ParkReconcileReport};

/// Where the next tick resumes each paged arm. `None` starts an arm at its
/// beginning; an arm whose listing ran out wraps to `None`.
///
/// An engine carries it from tick to tick, so its shape is journaled.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileCursor {
    /// The engine's position in its own stalled-work listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parks: Option<EngineCursor>,
    /// The last live process of a draining generation the previous tick's
    /// hand-over slot woke (FIG-3799), with that generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<(super::BuildGeneration, crate::ProcessId)>,
}

/// The arm of a tick a failure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileArm {
    /// The engine's stalled work into lash parks (O3).
    Parks,
    /// The FIG-3799 drain hand-over slot.
    DrainHandOver,
    /// Due obligations claimed through a kind's due index (ADR 0109 §1.4).
    Obligations,
}

/// One arm's failure in a tick. A failed listing keeps its cursor; an
/// individual failed item is revisited when the bounded scan wraps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileFailure {
    pub arm: ReconcileArm,
    pub error: String,
}

/// What a slot's pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotPass {
    /// Items the pass settled.
    pub handled: usize,
    /// Items the pass left for a later tick.
    pub deferred: usize,
}

/// What one due-obligation pass of one kind did (ADR 0109 §1.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayPass {
    /// Obligations the pass claimed.
    pub claimed: usize,
    /// Claims the engine accepted.
    pub delivered: usize,
    /// Claims whose delivery asked the engine and left the claim for the
    /// kind's consumer to settle in its own transaction (ADR 0109 §3,
    /// ingress: the drive's admission); a claim nobody settles lapses and
    /// the relay asks again.
    pub requested: usize,
    /// Claims handed back for a later attempt.
    pub retried: usize,
    /// Claims that stalled: refused, undecodable, or at the attempt ceiling.
    pub stalled: usize,
    /// Claims another relay, or the delivery itself, settled first.
    pub claim_lost: usize,
}

/// What one tick did, and where the next one resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileTick {
    /// The cursor the next tick starts from.
    pub next: ReconcileCursor,
    /// The engine's park reconcile, when it answered.
    pub parks: Option<ParkReconcileReport>,
    /// The FIG-3799 slot.
    pub drain_hand_over: SlotPass,
    /// Every arm failure, in arm order.
    pub failures: Vec<ReconcileFailure>,
    /// Whether this tick ran the leader-only arms (ADR 0109 §1.7).
    pub led: bool,
    /// Each kind's due pass that finished by the time this tick's leader
    /// arms ran, in relay order: one this tick started, or one an earlier
    /// tick started that ran on past it (ADR 0109 §1.8). Empty when this
    /// deployment may not claim due obligations this tick.
    pub obligations: Vec<(crate::store::ObligationKind, RelayPass)>,
    /// Kinds whose due pass an earlier tick started was still delivering, so
    /// this tick started none for them.
    pub obligations_busy: Vec<crate::store::ObligationKind>,
}

/// How a deployment's recovery pass bounds its obligation deliveries (ADR
/// 0109 §1.8). Host levers (ADR 0014): lash implements the mechanics, the
/// host chooses the numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryPassBudget {
    /// The longest one obligation delivery attempt runs before it is
    /// abandoned and retried
    /// ([`RelayPolicy::attempt_budget_ms`](crate::runtime::drive::relay::RelayPolicy::attempt_budget_ms)).
    /// Default 30 s. Keep it below the relay's 60 s claim TTL, so a claim
    /// never lapses under an attempt still running.
    pub attempt: std::time::Duration,
    /// The longest a recovery tick waits on its kinds' due passes before its
    /// leader-only arms run. A pass still delivering then finishes on its
    /// kind's lane, and the next tick reports it. Default 1 s.
    pub tick_wait: std::time::Duration,
}

impl Default for RecoveryPassBudget {
    fn default() -> Self {
        Self {
            attempt: std::time::Duration::from_millis(
                crate::runtime::drive::relay::RelayPolicy::DEFAULT_ATTEMPT_BUDGET_MS,
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
