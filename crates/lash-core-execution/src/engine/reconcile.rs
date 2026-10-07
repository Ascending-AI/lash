//! The reconcile tick's vocabulary (ADR 0104 O2/O4, ADR 0109 §1.4): what a
//! deployment's schedule carries from one tick to the next, and what a tick
//! reports.
//!
//! The tick itself is engine-neutral kernel code
//! (`lash_core::runtime::obligations::reconcile_once`): it runs every
//! obligation kind's due pass, until L10b (FIG-5191)
//! retires the obligation relay. A deployment runs it on an interval and
//! carries the [`ReconcileCursor`] forward.

use serde::{Deserialize, Serialize};

pub use crate::runtime::obligations::relay::RelayPass;

/// Where the next tick resumes. No arm is paged any more: the due passes
/// keep their own position in each kind's due index, so the cursor carries
/// nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileCursor {}

/// The arm of a tick a failure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileArm {
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

/// What one tick did, and where the next one resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileTick {
    /// The cursor the next tick starts from.
    pub next: ReconcileCursor,
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
