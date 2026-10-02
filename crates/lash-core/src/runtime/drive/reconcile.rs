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
//!    stopped retrying (turns, drives and processes) and records it through
//!    a [`ParkRecoveryWriter`]. Nothing the engine paused is resumed here:
//!    only a redrive verb resumes a park. One execution the pass cannot
//!    settle is reported and passed over, never failing the page.
//! 2. **Drain hand-over (FIG-3799).** Every live process of a generation an
//!    operator marked draining is woken to hand its open wait to a successor
//!    on the newest build, and a successor the newest build refused is sent
//!    back to a build of the generation (FIG-4750): [`drain_hand_over_slot`].
//!
//! A root the parks arm ended lost armed its scope close in that write, after
//! this tick's due passes claimed. The tick is that row's producer, so the
//! scope-close kind looks once more before the tick returns
//! ([`close_ended_roots`], FIG-4760): the root's children and registered
//! waits end with it, not a period later.
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

use super::lanes::RelayLanes;
use super::park::StoreParkRecovery;
use super::relay::{ObligationRelay, relay_kind};
use crate::engine::{
    EnginePage, ReconcileArm, ReconcileCursor, ReconcileFailure, ReconcileTick, ScopeCloseSink,
    SlotPass,
};
use crate::runtime::recovery_lease::RecoveryDuties;
use crate::store::ObligationKind;
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
    /// The deployment's lanes the relays' due passes run on, across ticks.
    pub lanes: &'a RelayLanes,
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
    let recovery = StoreParkRecovery::new(parts.sessions, parts.clock);
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
    let drain = async {
        match parts.processes {
            Some(processes) => Some((
                bounded_arm(
                    parts.clock,
                    deadline,
                    drain_hand_over_slot(
                        &processes,
                        DrainHandOverCursor {
                            wake: cursor.drain.as_ref(),
                            resend: cursor.resend.as_ref(),
                        },
                        page,
                    ),
                )
                .await,
                // A turn holds its build only across a wait on a process, so
                // a host that runs no processes has no parked turn to move.
                bounded_arm(
                    parts.clock,
                    deadline,
                    turn_hand_over_slot(
                        control.as_ref(),
                        processes.drain,
                        cursor.turns.as_ref(),
                        page,
                    ),
                )
                .await,
            )),
            None => None,
        }
    };
    let (parks, drain) = tokio::join!(parks, drain);
    match parks {
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

    if let Some((drain, turns)) = drain {
        match turns {
            Ok(hand_over) => {
                report.turn_hand_over = hand_over.pass;
                report.next.turns = hand_over.next;
            }
            Err(error) => {
                report.next.turns = cursor.turns.clone();
                report.failures.push(ReconcileFailure {
                    arm: ReconcileArm::DrainHandOver,
                    error: format!("turn hand-over: {error}"),
                });
            }
        }
        match drain {
            Ok(hand_over) => {
                report.drain_hand_over = hand_over.pass;
                report.next.drain = hand_over.next;
                report.next.resend = hand_over.next_resend;
            }
            Err(error) => {
                report.next.drain = cursor.drain.clone();
                report.next.resend = cursor.resend.clone();
                report.failures.push(ReconcileFailure {
                    arm: ReconcileArm::DrainHandOver,
                    error,
                });
            }
        }
    }
    close_ended_roots(parts, page, &mut report).await;
    report
}

/// Deliver the scope closes this tick's parks arm armed (FIG-4760): the
/// scope-close kind's due pass runs once more on its lane, under the same
/// wait as the tick's first.
///
/// A root the arm ended lost wrote its terminal after the tick's due passes
/// claimed, so without this its scope stays open for a whole period after
/// the root ended: its `Until`-scope children keep running and the waits
/// registered under it stay parked on a root that will never publish. A
/// pass still delivering when the wait runs out finishes on its lane, and a
/// lane an earlier pass still holds is reported busy; the next tick's due
/// pass owns what either leaves.
async fn close_ended_roots(
    parts: &ReconcileParts<'_>,
    page: NonZeroUsize,
    report: &mut ReconcileTick,
) {
    if !parts.duties.due_claims
        || report
            .parks
            .as_ref()
            .is_none_or(|parks| parks.ended_roots.is_empty())
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

/// What one pass of the turn hand-over slot did, and where the next resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnHandOverPass {
    /// Sessions asked (`handled`) and ones whose ask failed and waits for
    /// the next pass (`deferred`).
    pub pass: SlotPass,
    /// The last session this pass asked when it stopped at its page bound,
    /// with its generation; `None` when it read every draining generation to
    /// the end, so the next pass starts over.
    pub next: Option<(crate::engine::BuildGeneration, crate::SessionId)>,
}

/// **FIG-4739 slot.** Ask the parked turn of every session a draining
/// generation holds in flight to hand over, at most `page` sessions, in
/// (generation, session id) order from `after`.
///
/// A turn on a draining build hands over at its next quiet point by itself;
/// one parked on a durable wait reaches no quiet point until the wait ends,
/// so the recovery leader wakes it
/// ([`SessionControlEngine::hand_over_turns`](crate::engine::SessionControlEngine::hand_over_turns)):
/// the turn ends at a segment boundary with the wait left open, and its Run
/// goes on in a new execution on the newest build, which restamps the root.
/// The session leaves the listing with that restamp, or when its root ends.
///
/// Unlike the process slot, the leader's own generation is not skipped: a
/// turn's successor is sent to the stable name, never back to a named
/// generation, and a draining build that holds the recovery lease must still
/// move its own turns. Idempotent: a wake finds only waits still parked. One
/// failed ask is logged and counted deferred, never failing the page.
pub async fn turn_hand_over_slot(
    control: &dyn crate::engine::SessionControlEngine,
    drain: &dyn crate::store::generation_drain::GenerationDrainStore,
    after: Option<&(crate::engine::BuildGeneration, crate::SessionId)>,
    page: NonZeroUsize,
) -> Result<TurnHandOverPass, StoreError> {
    let mut report = TurnHandOverPass::default();
    let mut remaining = page.get();
    for marked in drain.draining_generations().await? {
        let generation = marked.generation;
        let resume = match after {
            Some((cursor, _)) if generation < *cursor => continue,
            Some((cursor, session)) if generation == *cursor => Some(session),
            _ => None,
        };
        let Some(limit) = NonZeroUsize::new(remaining) else {
            return Ok(report);
        };
        let sessions = drain.sessions_in_flight(&generation, resume, limit).await?;
        for session in sessions {
            match control.hand_over_turns(&session, &generation).await {
                Ok(()) => report.pass.handled += 1,
                Err(error) => {
                    tracing::warn!(
                        session_id = session.as_str(),
                        generation = generation.as_str(),
                        error = %error.message,
                        "the drain's turn hand-over wake failed; a later pass wakes the session again"
                    );
                    report.pass.deferred += 1;
                }
            }
            remaining -= 1;
            report.next = Some((generation.clone(), session));
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

/// What one pass of the drain hand-over slot did, and where the next resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainHandOverPass {
    /// Wakes and re-sends delivered (`handled`) and ones that failed and
    /// wait for the next pass (`deferred`).
    pub pass: SlotPass,
    /// The last process this pass woke when it stopped at its page bound, with
    /// its generation; `None` when it read every draining generation to the
    /// end, so the next pass starts over.
    pub next: Option<(crate::engine::BuildGeneration, crate::ProcessId)>,
    /// The last parked process the re-send scan read when it stopped at its
    /// page bound; `None` when it read every park, so the next pass starts
    /// over.
    pub next_resend: Option<(u64, crate::ProcessId)>,
}

/// Where a pass of the drain hand-over slot resumes: the last process the
/// previous pass woke, and the last park its re-send scan read.
#[derive(Clone, Copy, Debug, Default)]
pub struct DrainHandOverCursor<'a> {
    pub wake: Option<&'a (crate::engine::BuildGeneration, crate::ProcessId)>,
    pub resend: Option<&'a (u64, crate::ProcessId)>,
}

/// **FIG-3799 slot.** Wake the live processes of every draining generation
/// but this deployment's own, at most `page` of them, in (generation,
/// process id) order from `after`: each live segment hands its open wait to
/// a successor on the newest build, which waits again. A process parked
/// because the newest build refused its successor is live too, and the same
/// call re-sends that successor to a build of its generation (FIG-4750),
/// where the process runs on until it ends.
///
/// The drain is a store fact — an operator marks a generation draining — and
/// this slot is the leader-only duty that moves its work (ADR 0109 §1.7),
/// run by the same tick as every other recovery. It is idempotent: a wake is
/// keyed by generation and segment, so a repeated wake of a segment is a
/// no-op, and one that lands while the segment is not waiting holds for its
/// next wait; a re-send is keyed by the segment, so a repeated one names
/// the first. A process leaves the listing once its successor's admission
/// restamps it with the newest generation, or once it ends. One failed wake
/// or re-send is logged and counted deferred, never failing the page; the
/// next pass that reaches it tries again.
///
/// The pass then re-sends every other refused successor
/// ([`resend_refused_successors`]): a refusal needs no drain mark to be
/// moved, and the leader's own generation's refusals are moved too.
pub async fn drain_hand_over_slot(
    processes: &ReconcileProcesses<'_>,
    after: DrainHandOverCursor<'_>,
    page: NonZeroUsize,
) -> Result<DrainHandOverPass, StoreError> {
    let draining = processes.drain.draining_generations().await?;
    let mut report = DrainHandOverPass::default();
    let mut remaining = page.get();
    // The generations whose refused successors the wake loop re-sends.
    let mut woken = Vec::new();
    let mut wakes_ended = true;
    for marked in draining {
        let generation = marked.generation;
        if &generation == processes.generation {
            continue;
        }
        woken.push(generation.clone());
        // Generations are visited in order; the cursor's generation resumes
        // after its process, an earlier one was finished last pass.
        let resume = match after.wake {
            Some((cursor, _)) if generation < *cursor => continue,
            Some((cursor, process)) if generation == *cursor => Some(process),
            _ => None,
        };
        let Some(limit) = NonZeroUsize::new(remaining) else {
            wakes_ended = false;
            continue;
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
            wakes_ended = false;
        }
    }
    if wakes_ended {
        // Every draining generation was read to its end: the next pass
        // starts over.
        report.next = None;
    }
    let resent =
        resend_refused_successors(processes, &woken, after.resend, remaining.max(1)).await?;
    report.pass.handled += resent.pass.handled;
    report.pass.deferred += resent.pass.deferred;
    report.next_resend = resent.next_resend;
    Ok(report)
}

/// Re-send the refused successors the wake loop does not reach (FIG-4750
/// residuals 2 and 4, FIG-4739): the processes parked `RetiredGeneration`
/// for a generation that is not marked draining, or that is this
/// deployment's own. At most `page` parks are read, in park order from
/// `after`.
///
/// A successor the newest build refused can only run on a build of the
/// generation that sent it, so nothing is gained by leaving it parked until
/// an operator marks that generation draining, and the build that holds the
/// recovery lease moves its own generation's refusals like any other's. The
/// engine decides which parks are refusals
/// ([`ProcessWorkSubstrate::resend_refused_successor`]): a park that names an
/// execution the engine still holds, or whose successor already started, is
/// left as it is. A generation in `woken` was served by the wake loop this
/// pass, which re-sends through the same engine call.
async fn resend_refused_successors(
    processes: &ReconcileProcesses<'_>,
    woken: &[crate::engine::BuildGeneration],
    after: Option<&(u64, crate::ProcessId)>,
    page: usize,
) -> Result<DrainHandOverPass, StoreError> {
    let mut report = DrainHandOverPass::default();
    let Some(limit) = NonZeroUsize::new(page) else {
        return Ok(report);
    };
    let parked = processes
        .registry
        .list_parked_processes(&crate::store::ProcessParkQuery {
            reasons: Some(std::collections::BTreeSet::from([
                crate::store::ParkReasonCode::RetiredGeneration,
            ])),
            parked_at_or_before_ms: None,
            after: after.cloned(),
            limit,
        })
        .await
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    let read = parked.len();
    for record in parked {
        let Some(park) = record.park() else {
            continue;
        };
        report.next_resend = Some((park.since_ms, record.id.clone()));
        let refused_for = park
            .build_generation
            .as_ref()
            .filter(|generation| park.engine.is_none() && !woken.contains(generation));
        let Some(generation) = refused_for else {
            continue;
        };
        match processes.port.resend_refused_successor(&record.id).await {
            Ok(true) => report.pass.handled += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    process_id = record.id.as_str(),
                    generation = generation.as_str(),
                    error = %error,
                    "the re-send of a refused successor failed; a later pass sends it again"
                );
                report.pass.deferred += 1;
            }
        }
    }
    if read < page {
        // Every park was read: the next pass starts over.
        report.next_resend = None;
    }
    Ok(report)
}
