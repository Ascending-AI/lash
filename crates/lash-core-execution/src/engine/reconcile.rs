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
use crate::SessionId;
use crate::store::{ControlIntentId, ControlIntentState};

/// Where the next tick resumes each paged arm. `None` starts an arm at its
/// beginning; an arm whose listing ran out wraps to `None`.
///
/// An engine carries it from tick to tick, so its shape is journaled.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileCursor {
    /// The engine's position in its own stalled-work listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parks: Option<EngineCursor>,
    /// The last open intent the previous tick applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intents: Option<ControlIntentId>,
    /// Last terminal root whose scope close was attempted.
    pub scopes: Option<(SessionId, crate::TurnId)>,
}

/// The arm of a tick a failure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileArm {
    /// The engine's stalled work into lash parks (O3).
    Parks,
    /// Open control intents re-applied (O4).
    Intents,
    /// The FIG-3822 parent-end plan slot.
    ParentEndPlans,
    /// The FIG-3799 drain hand-over slot.
    DrainHandOver,
    /// Terminal roots whose scope close may have been interrupted.
    Scopes,
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
    /// Each open intent this tick applied, with the state it reached.
    pub intents: Vec<(ControlIntentId, ControlIntentState)>,
    /// The FIG-3822 slot.
    pub parent_end_plans: SlotPass,
    /// The FIG-3799 slot.
    pub drain_hand_over: SlotPass,
    /// Every arm failure, in arm order.
    pub failures: Vec<ReconcileFailure>,
    /// Idempotent terminal-root close calls completed.
    pub closed_scopes: usize,
    /// Whether this tick ran the leader-only arms (ADR 0109 §1.7).
    pub led: bool,
    /// Each relay's due pass, in relay order; empty when this deployment
    /// may not claim due obligations this tick.
    pub obligations: Vec<(crate::store::ObligationKind, RelayPass)>,
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
