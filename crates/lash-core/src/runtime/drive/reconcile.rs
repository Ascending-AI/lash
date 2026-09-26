//! Reconciliation (ADR 0104 O2/O3/O4, FIG-3600 S7, HoS decisions 69–70): the
//! guaranteed owner of every piece of session work whose owner was lost.
//!
//! One tick, [`reconcile_once`], is engine-neutral and idempotent. It runs
//! six arms, each bounded by the tick's page and resuming from its own
//! cursor, so a tick never serializes the fleet and one arm's failure never
//! stops another:
//!
//! 1. **Parks (O3).** The engine's own view of stalled work becomes lash
//!    parks: [`SessionControlEngine::reconcile_parks`] reads what the engine
//!    stopped retrying (turns and processes) and records it through a
//!    [`ParkRecoveryWriter`]; a paused admission-only drive is resumed, never
//!    parked. One execution the pass cannot settle is reported and passed
//!    over, never failing the page.
//! 2. **Intents (O4).** Every open control intent (pending, or failed and
//!    retryable) has its engine half re-applied by [`apply_control_intent`].
//! 3. **Drives (O2).** Every live session with open ingress that no
//!    unresolved park holds is asked for a drive. Acceptance commits a row and then asks the engine to drive,
//!    fire-and-forget; a process that dies between the two, a lost send, a
//!    re-ask the engine deduplicated, or a batch that only became available
//!    later all leave a durable row nothing drives, and this arm asks again.
//! 4. **Scopes.** Terminal evidence is revisited for idempotent scope close
//!    after a crash between the commit and its notification.
//! 5. **Parent-end plans (FIG-3822).** A named slot:
//!    [`reconcile_parent_end_plans_slot`].
//! 6. **Drain hand-over (FIG-3799).** A named slot:
//!    [`drain_hand_over_slot`].
//!
//! The engine supplies only the schedule: each engine runs the tick on an
//! interval inside the driver's deployment and carries the
//! [`ReconcileCursor`] forward. A tick never runs inside a drive, and it
//! takes its tick id from the caller, which owns the clock and the journal.
//!
//! Every drive ask names its own request, derived from the tick and the row
//! it answers, never from the session alone: the engine dedupes a request id
//! across its runs, so an ask keyed by the session could be swallowed by a
//! drive already past its last admission, and the row would strand.
//!
//! Today's open ingress is the `pending_turn_inputs` and `queued_work` rows,
//! read per session because no store answers "sessions with open ingress"
//! across the catalog; the drive arm pages over the live catalog by session
//! id. S8's switch to the one session-ingress table replaces this scan with
//! that table's cross-session keyset read.

use std::num::NonZeroUsize;

use super::control::apply_control_intent;
use super::park::StoreParkRecovery;
use crate::engine::{
    DriveReconcileReport, DriveRequestId, EnginePage, ReconcileArm, ReconcileCursor,
    ReconcileFailure, ReconcileTick, ScopeCloseSink, SlotPass,
};
use crate::{
    Clock, ProcessRegistry, ProcessWorkSubstrate, SessionId, SessionStoreFactory,
    SessionWorkEngine, StoreError,
};

/// What one tick reaches: the catalog, the engine, the scope owner, and the
/// process side when the host runs processes.
#[derive(Clone, Copy)]
pub struct ReconcileParts<'a> {
    /// The deployment's session catalog.
    pub sessions: &'a dyn SessionStoreFactory,
    /// The engine that drives sessions; its [`control`](SessionWorkEngine::control)
    /// half reconciles parks and applies intents.
    pub work: &'a dyn SessionWorkEngine,
    /// Where a released root's scope is closed.
    pub scopes: &'a dyn ScopeCloseSink,
    /// The process registry and the engine's process port, when the host
    /// runs processes. The parent-end and drain slots read them.
    pub processes: Option<ReconcileProcesses<'a>>,
    /// The caller's clock: intent timestamps and batch availability.
    pub clock: &'a dyn Clock,
}

/// The process side of a tick: the registry the parent-end ledger lives in
/// and the port that delivers to a running process.
#[derive(Clone, Copy)]
pub struct ReconcileProcesses<'a> {
    pub registry: &'a dyn ProcessRegistry,
    pub port: &'a dyn ProcessWorkSubstrate,
}

/// The drive request a tick asks for when `session`'s oldest open row is
/// `row`: unique per tick and row.
#[must_use]
pub fn reconcile_drive_request(tick: &str, row: &str) -> DriveRequestId {
    DriveRequestId::new(format!("reconcile:{tick}:{row}"))
}

/// Run one reconcile tick from `cursor`, each paged arm reading at most
/// `page` items. `tick` names this tick (the engine's journaled tick id):
/// drive asks from one tick dedupe, asks from two do not.
///
/// Idempotent: every arm's effect is a store write or an engine call that a
/// second tick over the same state repeats as a no-op. Nothing here fails the
/// tick; each arm's failure is reported in [`ReconcileTick::failures`] and
/// retried by the next tick.
pub async fn reconcile_once(
    parts: &ReconcileParts<'_>,
    cursor: &ReconcileCursor,
    page: NonZeroUsize,
    tick: &str,
) -> ReconcileTick {
    let mut report = ReconcileTick::default();
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

    // 2. Intents: re-apply every open intent's engine half.
    report.next.intents = cursor.intents;
    match parts
        .sessions
        .list_open_control_intents(cursor.intents, page)
        .await
    {
        Ok(intents) => {
            let full = intents.len() >= page.get();
            let mut next = None;
            for intent in intents {
                let id = intent.id;
                match apply_control_intent(
                    parts.sessions,
                    control.as_ref(),
                    parts.work,
                    parts.scopes,
                    &intent,
                    parts.clock,
                )
                .await
                {
                    Ok(state) => report.intents.push((id, state)),
                    Err(error) => report.failures.push(ReconcileFailure {
                        arm: ReconcileArm::Intents,
                        error: format!("intent {id}: {error}"),
                    }),
                }
                next = Some(id);
            }
            report.next.intents = if full { next } else { None };
        }
        Err(error) => report.failures.push(ReconcileFailure {
            arm: ReconcileArm::Intents,
            error: error.to_string(),
        }),
    }

    // 3. Drives: ask again for every session with open ingress.
    let now_ms = parts.clock.timestamp_ms();
    match reconcile_session_drives(
        parts.sessions,
        parts.work,
        tick,
        cursor.drives.as_ref(),
        page,
        now_ms,
    )
    .await
    {
        Ok(drives) => {
            report.next.drives = drives.next.clone();
            report.drives = drives;
        }
        Err(error) => {
            report.next.drives = cursor.drives.clone();
            report.failures.push(ReconcileFailure {
                arm: ReconcileArm::Drives,
                error: error.to_string(),
            });
        }
    }

    // Revisit terminal evidence after a crash between commit and scope close.
    // The sink is idempotent, so no second completion ledger is needed here.
    if parts.scopes.owns_scopes() {
        match parts
            .sessions
            .list_terminal_roots(cursor.scopes.clone(), page)
            .await
        {
            Ok(terminals) => {
                let full = terminals.len() == page.get();
                let mut next = None;
                for terminal in terminals {
                    next = Some((terminal.session_id.clone(), terminal.root.clone()));
                    let controlling = match &terminal.cause {
                        crate::store::RootTerminalCause::OperatorCancelled { intent }
                        | crate::store::RootTerminalCause::Forked { intent, .. }
                        | crate::store::RootTerminalCause::SessionDeleted { intent } => {
                            Some(*intent)
                        }
                        _ => None,
                    };
                    // A verb's root closes once its engine half is done:
                    // acknowledged, or refused for good (the release that
                    // did not happen is surfaced on the intent; the root's
                    // end stands).
                    if let Some(intent) = controlling {
                        match parts.sessions.load_intent(intent).await {
                            Ok(Some(intent))
                                if matches!(
                                    intent.state,
                                    crate::store::ControlIntentState::Acknowledged { .. }
                                        | crate::store::ControlIntentState::Failed {
                                            retryable: false,
                                            ..
                                        }
                                ) => {}
                            Ok(_) => continue,
                            Err(error) => {
                                report.failures.push(ReconcileFailure {
                                    arm: ReconcileArm::Scopes,
                                    error: error.to_string(),
                                });
                                continue;
                            }
                        }
                    }
                    match parts.scopes.close_root_scope(&terminal).await {
                        Ok(()) => report.closed_scopes += 1,
                        Err(error) => report.failures.push(ReconcileFailure {
                            arm: ReconcileArm::Scopes,
                            error: error.to_string(),
                        }),
                    }
                }
                report.next.scopes = if full { next } else { None };
            }
            Err(error) => {
                report.next.scopes = cursor.scopes.clone();
                report.failures.push(ReconcileFailure {
                    arm: ReconcileArm::Scopes,
                    error: error.to_string(),
                });
            }
        }
    }

    // 4–5. The slots other slices fill.
    if let Some(processes) = parts.processes {
        match reconcile_parent_end_plans_slot(&processes, parts.sessions, page).await {
            Ok(pass) => report.parent_end_plans = pass,
            Err(error) => report.failures.push(ReconcileFailure {
                arm: ReconcileArm::ParentEndPlans,
                error: error.to_string(),
            }),
        }
        match drain_hand_over_slot(&processes, parts.sessions, page).await {
            Ok(pass) => report.drain_hand_over = pass,
            Err(error) => report.failures.push(ReconcileFailure {
                arm: ReconcileArm::DrainHandOver,
                error: error.to_string(),
            }),
        }
    }
    report
}

/// **FIG-3822 slot.** Apply every recorded but unapplied parent-end plan,
/// at most `page` of them, and re-derive a plan whose parent's end is
/// durable but whose record is missing.
///
/// A plan is left unapplied when the invocation that recorded it was killed
/// or paused, or when a process ended off-workflow. This slot is that
/// plan's only recovery owner: there is no second background actor. It runs
/// once per tick, only on a host with processes, and must be idempotent and
/// bounded by `page`; one failed plan never aborts the page.
///
/// FIG-3822 fills the body with
/// `lash_core_execution::reconcile_parent_end_plans(processes.registry,
/// processes.port, sessions, page)`, mapping its report onto [`SlotPass`].
/// Until then it applies nothing.
pub async fn reconcile_parent_end_plans_slot(
    processes: &ReconcileProcesses<'_>,
    sessions: &dyn SessionStoreFactory,
    page: NonZeroUsize,
) -> Result<SlotPass, crate::PluginError> {
    let _ = (processes, sessions, page);
    Ok(SlotPass::default())
}

/// **FIG-3799 slot.** Wake and hand over the waiting processes of a draining
/// generation, at most `page` of them: wake, hand over to the next segment
/// on the latest build, re-wait.
///
/// The drain is a store fact (`lash drain --generation G` marks G draining);
/// this slot is the step that moves its waiting work, run by the same tick
/// as every other recovery, so a drain needs no second background actor. It
/// must be idempotent and bounded by `page`.
///
/// FIG-3799 fills the body. Until then it hands over nothing.
pub async fn drain_hand_over_slot(
    processes: &ReconcileProcesses<'_>,
    sessions: &dyn SessionStoreFactory,
    page: NonZeroUsize,
) -> Result<SlotPass, crate::PluginError> {
    let _ = (processes, sessions, page);
    Ok(SlotPass::default())
}

/// The drive arm: ask `engine` to drive every live session with open
/// ingress, reading at most `page` sessions after `after` in session-id
/// order. The catalog lists no session an unresolved park holds: it admits
/// nothing until a verb resolves the park, and that verb asks for the drive
/// itself, so an ask every tick would only pile up drives that admit
/// nothing.
///
/// A session whose store cannot be read is reported, not fatal: one broken
/// session never stops the others' recovery. Only a catalog that cannot be
/// listed fails the pass. A queued batch whose `available_at_ms` is still
/// ahead of `now_ms` does not count as open: a later tick asks for it once
/// it is due. A session the engine still holds live work for is left alone:
/// its ask was not lost, its owner is simply still running, and a sibling
/// drive would fence it.
pub async fn reconcile_session_drives(
    sessions: &dyn SessionStoreFactory,
    engine: &dyn SessionWorkEngine,
    tick: &str,
    after: Option<&SessionId>,
    page: NonZeroUsize,
    now_ms: u64,
) -> Result<DriveReconcileReport, StoreError> {
    let mut live = sessions
        .list_reconcilable_sessions(after, page.saturating_add(1))
        .await?;
    let mut report = DriveReconcileReport::default();
    if live.len() > page.get() {
        live.truncate(page.get());
        report.next = live.last().cloned();
    }
    for session in live {
        report.scanned += 1;
        match oldest_open_row(sessions, &session, now_ms).await {
            Ok(Some(row)) => {
                // Checked after the row read: a row's writer registered
                // with the engine before it wrote, so a live owner is
                // visible here and keeps its session — a suspended
                // invocation's resumption re-decides the session's open
                // ingress itself, and a sibling drive would only fence it.
                if engine.session_work_in_flight(&session).await {
                    continue;
                }
                engine.schedule_drive(&session, reconcile_drive_request(tick, &row));
                report.scheduled.push(session);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    session_id = session.as_str(),
                    error = %error,
                    "reconcile could not read the session's open ingress"
                );
                report.unreadable.push((session, error.to_string()));
            }
        }
    }
    Ok(report)
}

/// The id of one of `session`'s open ingress rows (its oldest turn input,
/// else its oldest queued batch that is due at `now_ms`), if it has any.
async fn oldest_open_row(
    sessions: &dyn SessionStoreFactory,
    session: &SessionId,
    now_ms: u64,
) -> Result<Option<String>, StoreError> {
    let Some(store) = sessions.open_existing_store_by_id(session).await? else {
        return Ok(None);
    };
    // The row only names the ask; the drive admits whatever is open.
    let inputs = store.list_pending_turn_inputs(session).await?;
    if let Some(read) = inputs.iter().min_by_key(|read| read.input.enqueue_seq) {
        return Ok(Some(format!("input:{}", read.input.input_id)));
    }
    let batches = store.list_pending_queued_work(session).await?;
    Ok(batches
        .iter()
        .filter(|batch| batch.available_at_ms <= now_ms)
        .min_by_key(|batch| batch.enqueue_seq)
        .map(|batch| format!("batch:{}", batch.batch_id)))
}
