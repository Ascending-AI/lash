//! Reconciliation (ADR 0104 O2/O3/O4, FIG-3600 S7, HoS decisions 69–70): the
//! guaranteed owner of every piece of session work whose owner was lost.
//!
//! One tick, [`reconcile_once`], is engine-neutral and idempotent. Every
//! deployment first starts the obligation relays' due passes (ADR 0109 §1.4)
//! — among them control intents' engine halves, the scope closes a terminal
//! write armed, parent-end plans and the ingress its admission armed (ADR
//! 0109 §3) — claimed through every registered kind's due index, not scanned
//! out of their tables. Each kind's pass runs on its own lane
//! ([`RelayLanes`]) and the tick waits for them at most its budget's
//! `tick_wait`, so a slow delivery holds back neither another kind nor the
//! arms below (ADR 0109 §1.8). Then the recovery leader runs the arms below,
//! each bounded by the tick's page and resuming from its own cursor, so a
//! tick never serializes the fleet and one arm's failure never stops
//! another:
//!
//! 1. **Parks (O3).** The engine's own view of stalled work becomes lash
//!    parks: [`SessionControlEngine::reconcile_parks`] reads what the engine
//!    stopped retrying (turns, shifts and processes) and records it through
//!    a [`ParkRecoveryWriter`]. Nothing the engine paused is resumed here:
//!    only a redrive verb resumes a park. One execution the pass cannot
//!    settle is reported and passed over, never failing the page.
//!
//! A run the parks arm ended lost armed its scope close in that write, after
//! this tick's due passes claimed. The tick is that row's producer, so the
//! scope-close kind looks once more before the tick returns
//! ([`close_ended_runs`], FIG-4760): the run's children and registered
//! waits end with it, not a period later.
//!
//! Unexecuted ingress has no arm: every admitted turn input and queued batch
//! carries an ingress obligation armed in its admission transaction, which
//! the producer delivers at once and the ingress relay retries through its
//! due index (ADR 0109 §3). No pass scans the session catalog for it.
//!
//! The engine supplies only the schedule: each engine runs the tick on an
//! interval inside the driver's deployment and carries the
//! [`ReconcileCursor`] forward. A tick never runs inside a shift.

use std::num::NonZeroUsize;
use std::sync::Arc;

use super::lanes::RelayLanes;
use super::park::StoreParkRecovery;
use super::relay::{ObligationRelay, relay_kind};
use crate::engine::{
    EnginePage, ReconcileArm, ReconcileCursor, ReconcileFailure, ReconcileTick, ScopeCloseSink,
};
use crate::runtime::recovery_lease::RecoveryDuties;
use crate::store::ObligationKind;
use crate::{Clock, DeploymentStore, SessionWorkEngine};

/// What one tick reaches: the catalog, the engine and the scope owner.
#[derive(Clone, Copy)]
pub struct ReconcileParts<'a> {
    /// The deployment's session catalog.
    pub sessions: &'a dyn DeploymentStore,
    /// The engine that works sessions; its [`control`](SessionWorkEngine::control)
    /// half reconciles parks.
    pub work: &'a dyn SessionWorkEngine,
    /// Where a released run's scope is closed.
    pub scopes: &'a dyn ScopeCloseSink,
    /// The caller's clock: obligation due times and recovery slots.
    pub clock: &'a dyn Clock,
    pub metrics: &'a lash_trace::telemetry::metrics::TelemetryMetrics,
    /// Which duties this deployment runs this tick (ADR 0109 §1.7): the
    /// leader-only arms, and the due-obligation claims.
    pub duties: RecoveryDuties,
    /// Every obligation kind's relay whose due index this tick claims from.
    pub relays: &'a [Arc<dyn ObligationRelay>],
    /// The deployment's lanes the relays' due passes run on, across ticks.
    pub lanes: &'a RelayLanes,
}

/// Run one reconcile tick from `cursor`, each paged arm reading at most
/// `page` items.
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
    if !parts.duties.leader {
        // Every arm below is a leader-only repair pass; a follower keeps
        // its cursors for the tick it leads.
        return report;
    }
    let control = parts.work.control();

    // Each leader arm runs independently. The engine spends one page budget
    // on each of its recovery catalogs; the outer guard leaves another
    // budget for it to return the report, and bounds an unresponsive engine.
    let budget = parts.lanes.tick_wait();
    let deadline = parts.clock.now() + budget.saturating_mul(2);
    let recovery =
        StoreParkRecovery::new(parts.sessions, parts.clock).with_metrics(parts.metrics.clone());
    let parks = bounded_arm(
        parts.clock,
        deadline,
        control.reconcile_parks(
            &recovery,
            EnginePage {
                after: cursor.parks.clone(),
                limit: page,
                budget,
            },
        ),
    );
    match parks.await {
        Ok(parks) => {
            report.next.parks = parks.next.clone();
            report
                .failures
                .extend(
                    parks
                        .failed
                        .iter()
                        .map(|(execution, error)| ReconcileFailure {
                            arm: ReconcileArm::Parks,
                            error: format!("execution {}: {error}", execution.as_str()),
                        }),
                );
            report.parks = Some(parks);
        }
        Err(error) => {
            report.next.parks = cursor.parks.clone();
            report.failures.push(ReconcileFailure {
                arm: ReconcileArm::Parks,
                error,
            });
        }
    }

    close_ended_runs(parts, page, &mut report).await;
    report
}

/// Deliver the scope closes this tick's parks arm armed (FIG-4760): the
/// scope-close kind's due pass runs once more on its lane, under the same
/// wait as the tick's first.
///
/// A run the arm ended lost wrote its terminal after the tick's due passes
/// claimed, so without this its scope stays open for a whole period after
/// the run ended: its `Until`-scope children keep running and the waits
/// registered under it stay parked on a run that will never publish. A
/// pass still delivering when the wait runs out finishes on its lane, and a
/// lane an earlier pass still holds is reported busy; the next tick's due
/// pass owns what either leaves.
async fn close_ended_runs(
    parts: &ReconcileParts<'_>,
    page: NonZeroUsize,
    report: &mut ReconcileTick,
) {
    if !parts.duties.due_claims
        || report
            .parks
            .as_ref()
            .is_none_or(|parks| parks.ended_runs.is_empty())
    {
        return;
    }
    let Some(relay) = parts
        .relays
        .iter()
        .find(|relay| relay_kind(relay.as_ref()) == ObligationKind::ScopeClose)
    else {
        return;
    };
    let lanes = parts.lanes.tick(std::slice::from_ref(relay), page).await;
    for (kind, ended) in lanes.ended {
        match ended {
            Ok(pass) => match report
                .obligations
                .iter_mut()
                .find(|(seen, _)| *seen == kind)
            {
                Some((_, total)) => {
                    total.claimed += pass.claimed;
                    total.delivered += pass.delivered;
                    total.requested += pass.requested;
                    total.retried += pass.retried;
                    total.stalled += pass.stalled;
                    total.claim_lost += pass.claim_lost;
                }
                None => report.obligations.push((kind, pass)),
            },
            Err(error) => report.failures.push(ReconcileFailure {
                arm: ReconcileArm::Obligations,
                error: format!("{kind} obligations: {error}"),
            }),
        }
    }
    for kind in lanes.busy {
        if !report.obligations_busy.contains(&kind) {
            report.obligations_busy.push(kind);
        }
    }
}

async fn bounded_arm<T, E: std::fmt::Display>(
    clock: &dyn Clock,
    deadline: std::time::Instant,
    arm: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    tokio::select! {
        result = arm => result.map_err(|error| error.to_string()),
        () = clock.sleep_until(deadline) => Err("recovery arm time budget exhausted".into()),
    }
}
