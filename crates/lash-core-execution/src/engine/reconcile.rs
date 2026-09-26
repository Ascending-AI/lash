//! The reconcile tick's vocabulary (ADR 0104 O2/O3/O4, FIG-3600 S7, HoS
//! decisions 69–70): what an engine's schedule carries from one tick to the
//! next, and what a tick reports.
//!
//! The tick itself is engine-neutral kernel code (`lash_core::drive::
//! reconcile_once`); an engine only schedules it, through
//! [`SessionDriver::reconcile`](crate::SessionDriver::reconcile). On Restate
//! a `LashReconcile` object re-sends itself with a delay and runs one tick
//! per send, carrying the [`ReconcileCursor`] in its state.

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
    /// The last live session the previous tick's drive arm read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drives: Option<SessionId>,
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
    /// Sessions with open ingress re-asked for a drive (O2).
    Drives,
    /// The FIG-3822 parent-end plan slot.
    ParentEndPlans,
    /// The FIG-3799 drain hand-over slot.
    DrainHandOver,
    /// Terminal roots whose scope close may have been interrupted.
    Scopes,
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

/// What one pass of the drive arm did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriveReconcileReport {
    /// Live sessions the pass read.
    pub scanned: usize,
    /// Sessions with open ingress, each asked for a drive, in catalog order.
    pub scheduled: Vec<SessionId>,
    /// Sessions the pass could not read, with why. The next pass that
    /// reaches them asks again.
    pub unreadable: Vec<(SessionId, String)>,
    /// The last session this pass read when it stopped at its page bound;
    /// `None` when it read to the end of the catalog.
    pub next: Option<SessionId>,
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
    /// The drive arm.
    pub drives: DriveReconcileReport,
    /// The FIG-3822 slot.
    pub parent_end_plans: SlotPass,
    /// The FIG-3799 slot.
    pub drain_hand_over: SlotPass,
    /// Every arm failure, in arm order.
    pub failures: Vec<ReconcileFailure>,
    /// Idempotent terminal-root close calls completed.
    pub closed_scopes: usize,
}
