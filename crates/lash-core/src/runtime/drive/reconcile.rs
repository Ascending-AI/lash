//! Reconciliation (ADR 0104 O2/O3/O4, FIG-3600 S7, HoS decisions 69–70): the
//! guaranteed owner of every piece of session work whose owner was lost.
//!
//! One tick, [`reconcile_once`], is engine-neutral and idempotent. Every
//! deployment first runs the obligation relays' due passes (ADR 0109 §1.4)
//! — among them control intents' engine halves, the scope closes a terminal
//! write armed, parent-end plans and the ingress its admission armed (ADR
//! 0109 §3) — claimed through every registered kind's due index, not scanned
//! out of their tables. Then the recovery leader runs the arms below, each
//! bounded by the tick's page and resuming from its own cursor, so a tick
//! never serializes the fleet and one arm's failure never stops another:
//!
//! 1. **Parks (O3).** The engine's own view of stalled work becomes lash
//!    parks: [`SessionControlEngine::reconcile_parks`] reads what the engine
//!    stopped retrying (turns, drives and processes) and records it through
//!    a [`ParkRecoveryWriter`]. Nothing the engine paused is resumed here:
//!    only a redrive verb resumes a park. One execution the pass cannot
//!    settle is reported and passed over, never failing the page.
//! 2. **Drain hand-over (FIG-3799).** Every live process of a generation an
//!    operator marked draining is woken to hand its open wait to a successor
//!    on the newest build: [`drain_hand_over_slot`].
//!
//! Undriven ingress has no arm: every admitted turn input and queued batch
//! carries an ingress obligation armed in its admission transaction, which
//! the producer delivers at once and the ingress relay retries through its
//! due index (ADR 0109 §3). No pass scans the session catalog for it.
//!
//! The engine supplies only the schedule: each engine runs the tick on an
//! interval inside the driver's deployment and carries the
//! [`ReconcileCursor`] forward. A tick never runs inside a drive.

use std::num::NonZeroUsize;
use std::sync::Arc;

use super::park::StoreParkRecovery;
use super::relay::{ObligationRelay, relay_due, relay_kind};
use crate::engine::{
    EnginePage, ReconcileArm, ReconcileCursor, ReconcileFailure, ReconcileTick, ScopeCloseSink,
    SlotPass,
};
use crate::runtime::recovery_lease::RecoveryDuties;
use crate::{
    Clock, DeploymentStore, ProcessRegistry, ProcessWorkSubstrate, SessionWorkEngine, StoreError,
};

/// What one tick reaches: the catalog, the engine, the scope owner, and the
/// process side when the host runs processes.
#[derive(Clone, Copy)]
pub struct ReconcileParts<'a> {
    /// The deployment's session catalog.
    pub sessions: &'a dyn DeploymentStore,
    /// The engine that drives sessions; its [`control`](SessionWorkEngine::control)
    /// half reconciles parks.
    pub work: &'a dyn SessionWorkEngine,
    /// Where a released root's scope is closed.
    pub scopes: &'a dyn ScopeCloseSink,
    /// The process registry and the engine's process port, when the host
    /// runs processes. The parent-end and drain slots read them.
    pub processes: Option<ReconcileProcesses<'a>>,
    /// The caller's clock: obligation due times and recovery slots.
    pub clock: &'a dyn Clock,
    /// Which duties this deployment runs this tick (ADR 0109 §1.7): the
    /// leader-only arms, and the due-obligation claims.
    pub duties: RecoveryDuties,
    /// Every obligation kind's relay whose due index this tick claims from.
    pub relays: &'a [Arc<dyn ObligationRelay>],
}

/// The process side of a tick: the registry the parent-end ledger lives in,
/// the port that delivers to a running process, and what the drain slot
/// reads — the drain marks and the build generation this deployment runs.
#[derive(Clone, Copy)]
pub struct ReconcileProcesses<'a> {
    pub registry: &'a dyn ProcessRegistry,
    pub port: &'a dyn ProcessWorkSubstrate,
    /// The drain marks and each generation's live processes (FIG-3799).
    pub drain: &'a dyn crate::store::generation_drain::GenerationDrainStore,
    /// The build generation this deployment runs: never drained by its own
    /// hand-over, since a successor on the newest build could land right
    /// back on it.
    pub generation: &'a crate::engine::BuildGeneration,
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
        for relay in parts.relays {
            let kind = relay_kind(relay.as_ref());
            match relay_due(relay.as_ref(), parts.clock, page).await {
                Ok(pass) => report.obligations.push((kind, pass)),
                Err(error) => report.failures.push(ReconcileFailure {
                    arm: ReconcileArm::Obligations,
                    error: format!("{kind} obligations: {error}"),
                }),
            }
        }
    }
    if !parts.duties.leader {
        // Every arm below is a leader-only repair pass; a follower keeps
        // its cursors for the tick it leads.
        return report;
    }
    let control = parts.work.control();

    // 1. Parks: the engine's stalled work becomes lash parks.
    let recovery = StoreParkRecovery::new(parts.sessions, parts.clock);
    match control
        .reconcile_parks(
            &recovery,
            EnginePage {
                after: cursor.parks.clone(),
                limit: page,
            },
        )
        .await
    {
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
                error: error.to_string(),
            });
        }
    }

    // 2. The slot other slices fill.
    if let Some(processes) = parts.processes {
        match drain_hand_over_slot(&processes, cursor.drain.as_ref(), page).await {
            Ok(hand_over) => {
                report.drain_hand_over = hand_over.pass;
                report.next.drain = hand_over.next;
            }
            Err(error) => {
                report.next.drain = cursor.drain.clone();
                report.failures.push(ReconcileFailure {
                    arm: ReconcileArm::DrainHandOver,
                    error: error.to_string(),
                });
            }
        }
    }
    report
}

/// What one pass of the drain hand-over slot did, and where the next resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainHandOverPass {
    /// Wakes delivered (`handled`) and wakes that failed and wait for the
    /// next pass (`deferred`).
    pub pass: SlotPass,
    /// The last process this pass woke when it stopped at its page bound, with
    /// its generation; `None` when it read every draining generation to the
    /// end, so the next pass starts over.
    pub next: Option<(crate::engine::BuildGeneration, crate::ProcessId)>,
}

/// **FIG-3799 slot.** Wake the live processes of every draining generation
/// but this deployment's own, at most `page` of them, in (generation,
/// process id) order from `after`: each live segment hands its open wait to
/// a successor on the newest build, which waits again.
///
/// The drain is a store fact — an operator marks a generation draining — and
/// this slot is the leader-only duty that moves its work (ADR 0109 §1.7),
/// run by the same tick as every other recovery. It is idempotent: a wake is
/// keyed by generation and segment, so a repeated wake of a segment is a
/// no-op, and one that lands while the segment is not waiting holds for its
/// next wait. A process leaves the listing once its successor's admission
/// restamps it with the newest generation, or once it ends. One failed wake
/// is logged and counted deferred, never failing the page; the next pass
/// that reaches it wakes it again.
pub async fn drain_hand_over_slot(
    processes: &ReconcileProcesses<'_>,
    after: Option<&(crate::engine::BuildGeneration, crate::ProcessId)>,
    page: NonZeroUsize,
) -> Result<DrainHandOverPass, StoreError> {
    let draining = processes.drain.draining_generations().await?;
    let mut report = DrainHandOverPass::default();
    let mut remaining = page.get();
    for marked in draining {
        let generation = marked.generation;
        if &generation == processes.generation {
            continue;
        }
        // Generations are visited in order; the cursor's generation resumes
        // after its process, an earlier one was finished last pass.
        let resume = match after {
            Some((cursor, _)) if generation < *cursor => continue,
            Some((cursor, process)) if generation == *cursor => Some(process),
            _ => None,
        };
        let Some(limit) = NonZeroUsize::new(remaining) else {
            break;
        };
        let live = processes
            .drain
            .live_processes(&generation, resume, limit)
            .await?;
        for process_id in live {
            match processes
                .port
                .deliver_hand_over(&process_id, &generation)
                .await
            {
                Ok(()) => report.pass.handled += 1,
                Err(error) => {
                    tracing::warn!(
                        process_id = process_id.as_str(),
                        generation = generation.as_str(),
                        error = %error,
                        "the drain's hand-over wake failed; a later pass wakes the process again"
                    );
                    report.pass.deferred += 1;
                }
            }
            remaining -= 1;
            report.next = Some((generation.clone(), process_id));
        }
        if remaining == 0 {
            return Ok(report);
        }
    }
    // Every draining generation was read to its end: the next pass starts
    // over.
    report.next = None;
    Ok(report)
}
