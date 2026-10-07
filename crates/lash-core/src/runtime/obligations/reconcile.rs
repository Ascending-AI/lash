//! Reconciliation (ADR 0104 O2/O4, ADR 0109 §1.4): the owner of every
//! obligation a store set still arms, until L10b (FIG-5191) retires the
//! relay.
//!
//! One tick, [`reconcile_once`], is idempotent. Every deployment starts the
//! obligation relays' due passes (ADR 0109 §1.4), claimed through every
//! registered kind's due index, not scanned out of their tables. Each kind's
//! pass runs on its own lane ([`RelayLanes`]) and the tick waits for them at
//! most its budget's `tick_wait`, so a slow delivery holds back no other
//! kind (ADR 0109 §1.8).
//!
//! Session work has no arm: its producers wake the session actor in their
//! own transactions (ADR 0132 §12), and a claim of the actor drains it.
//!
//! The deployment runs the tick on an interval and carries the
//! [`ReconcileCursor`] forward.

use std::num::NonZeroUsize;
use std::sync::Arc;

use super::lanes::RelayLanes;
use super::relay::ObligationRelay;
use crate::engine::{ReconcileArm, ReconcileCursor, ReconcileFailure, ReconcileTick};
use crate::runtime::recovery_lease::RecoveryDuties;

/// What one tick reaches: its relays and the lanes they run on.
#[derive(Clone, Copy)]
pub struct ReconcileParts<'a> {
    /// Which duties this deployment runs this tick (ADR 0109 §1.7): the
    /// due-obligation claims.
    pub duties: RecoveryDuties,
    /// Every obligation kind's relay whose due index this tick claims from.
    pub relays: &'a [Arc<dyn ObligationRelay>],
    /// The deployment's lanes the relays' due passes run on, across ticks.
    pub lanes: &'a RelayLanes,
}

/// Run one reconcile tick from `cursor`, each due pass claiming at most
/// `page` obligations.
///
/// Idempotent: every arm's effect is a store write or an engine call that a
/// second tick over the same state repeats as a no-op. Nothing here fails the
/// tick; each arm's failure is reported in [`ReconcileTick::failures`] and
/// retried by the next tick.
pub async fn reconcile_once(
    parts: &ReconcileParts<'_>,
    cursor: &ReconcileCursor,
    page: NonZeroUsize,
) -> ReconcileTick {
    let mut report = ReconcileTick {
        next: cursor.clone(),
        led: parts.duties.leader,
        ..ReconcileTick::default()
    };

    // Due obligations: every deployment where claims skip each other, the
    // leader alone where they do not (ADR 0109 §1.7).
    if parts.duties.due_claims {
        let lanes = parts.lanes.tick(parts.relays, page).await;
        for (kind, ended) in lanes.ended {
            match ended {
                Ok(pass) => report.obligations.push((kind, pass)),
                Err(error) => report.failures.push(ReconcileFailure {
                    arm: ReconcileArm::Obligations,
                    error: format!("{kind} obligations: {error}"),
                }),
            }
        }
        report.obligations_busy = lanes.busy;
    }
    report
}
