//! A turn's scope ends with its commit or its cancel (ADR 0132 §11). Owned
//! by L6b (FIG-5176).
//!
//! # Contracts
//!
//! - **The closure and the first batch ride the ending transaction.** L3's
//!   `turn.commit` and `turn.cancel` transactions call [`end_turn_scope`] on
//!   their own `tx`: it revokes the turn's waits and compacts each to its
//!   tombstone, marks the first batch of the turn's `Until` children for
//!   cancel through L6's `end_scope`, and uses the run's terminal to close
//!   the turn's scope, so no process registers under it once `tx` commits.
//!   The closure reads the children again inside `tx`, after it: when one
//!   is left unmarked, the same `tx` records the turn scope as ending in
//!   the session's row set.
//! - **The rest is cursor work on the session actor.** After either commit,
//!   and on every claim before anything else, the session activation runs
//!   [`continue_scope_ends`]: one `cascade.batch` transaction per further
//!   batch, the last of which clears the ending scope. Marking is
//!   idempotent (a child's first cancel request wins), so a crash between
//!   batches re-drives from the recorded scope until it is done.
//! - **A turn stop never hangs on a child (G1b).** [`await_turn_children`]
//!   waits for the children a turn's end marked through bounded
//!   `process_terminal` waits, all sharing one deadline `stop_grace` away.
//!   A parked or waiting child ends engine-free (L6), so its wait resolves;
//!   a child still running at the deadline is reported as possibly still
//!   running, and the stop completes anyway. It never waits on a fresh,
//!   unbound cancellation token.
//!
//! An operation's scope ends the same way, with the commit that settles its
//! plugin task's command ([`end_operation_scope`], FIG-5345).
//!
//! A parent's terminal does not mean its children have stopped: the
//! subtree's ends are not a durable fact. [`await_turn_children`] reads the
//! whole live subtree (`DurableReads::live_until_descendants`), below ended
//! processes too.

use std::time::Duration;

use lash_core_execution::runtime::actor::process::{self, CascadeProgress};
use lash_core_execution::runtime::actor::waits;
use lash_durable::domain::{ScopeKey, SessionCloseWrite};
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError};
use lash_execution_contracts::waits::WaitDeadline;

use crate::ActorContext;
use crate::CancelOrigin;
use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use lash_execution_contracts::process::ProcessOutcome;

/// Mark the next batch of `scope`'s `Until` children for cancel on `tx`, the
/// batch sized by the backend's `cascade_batch`. The one call into L6's
/// cascade, so its signature lives in one place. A child an earlier batch
/// marked is not read again, so every batch starts from the first unmarked
/// child: a crash between batches loses and repeats nothing.
///
/// # Errors
///
/// The children's read.
pub(super) async fn mark_until_children(
    cx: &ActorContext,
    tx: &mut ActorTx,
    scope: &ScopeKey,
) -> Result<CascadeProgress, DurableError> {
    let origin = match scope {
        ScopeKey::Turn(..) => CancelOrigin::TurnStopped,
        _ => CancelOrigin::ParentEnded,
    };
    process::end_scope(
        cx.durable_reads()?,
        tx,
        scope,
        None,
        cx.backend().config().settings().cascade_batch,
        origin,
        &scope.stored(),
    )
    .await
}

/// End `run`'s scope on `tx`, the transaction that ended the turn
/// (`turn.commit` or `turn.cancel`): settle its model calls, revoke the
/// turn's waits and compact them to tombstones, mark the first batch of its
/// `Until` children for cancel and close the scope. When a child is left
/// unmarked the scope is recorded as ending, for [`continue_scope_ends`].
///
/// A run named by a process id is that `SessionTurn` process's child turn
/// (FIG-5208): its end resolves the process's child-session wait, which
/// wakes the process to commit its terminal.
///
/// # Errors
///
/// The children's read; nothing is written.
pub async fn end_turn_scope(
    cx: &ActorContext,
    tx: &mut ActorTx,
    session: &SessionId,
    run: &TurnId,
) -> Result<(), DurableError> {
    let scope = ScopeKey::Turn(session.clone(), run.clone());
    // No call of an ended turn is sent again.
    super::phases::settle_model_calls(tx, session, run, None);
    mark_until_children(cx, tx, &scope).await?;
    waits::retire_scope(tx, &scope);
    if let Ok(process) = crate::ProcessId::parse(run.as_str()) {
        waits::resolve_child_session_waits(tx, &process)?;
    }
    process::close_scope(tx, &scope);
    Ok(())
}

/// End the operation scope `scope` on `tx`, the commit that settles its
/// plugin task's command (FIG-5345; ADR 0132 §11): mark the first batch of
/// its `Until` children for cancel and close it, so no process registers
/// under it once `tx` commits. When a child is left unmarked the scope is
/// recorded as ending, for [`continue_scope_ends`]. Its waits are the
/// session's, which its end leaves standing.
///
/// # Errors
///
/// The children's read; nothing is written.
pub(crate) async fn end_operation_scope(
    cx: &ActorContext,
    tx: &mut ActorTx,
    scope: &ScopeKey,
) -> Result<(), DurableError> {
    mark_until_children(cx, tx, scope).await?;
    process::close_scope(tx, scope);
    Ok(())
}

/// Mark the rest of every ending scope of `session`, one `cascade.batch`
/// transaction per batch, clearing each scope in the transaction that marks
/// its last batch. Runs after a turn's end commits and on every claim. The
/// scope closed with the turn, so no child joins it: the read before each
/// batch can only overcount what is left.
///
/// # Errors
///
/// The store's refusal: [`DurableError::OwnershipLost`] once the session is
/// someone else's.
pub async fn continue_scope_ends(
    cx: &ActorContext,
    session: &SessionId,
) -> Result<(), DurableError> {
    for scope in cx.backend().durable().ending_scopes(session).await? {
        loop {
            let mut tx = cx.begin().await?;
            let progress = mark_until_children(cx, &mut tx, &scope).await?;
            if progress == CascadeProgress::Done {
                tx.write(DomainWrite::SessionClose(SessionCloseWrite::ScopeEnded {
                    session: session.clone(),
                    scope: scope.clone(),
                }));
            }
            cx.commit(tx, CommitLabel::CASCADE_BATCH).await?;
            if progress == CascadeProgress::Done {
                break;
            }
        }
    }
    Ok(())
}

/// How a turn's stop left the children its end marked.
#[derive(Clone, Debug, Default)]
pub struct TurnChildrenStop {
    /// The children that ended within the grace, with their outcomes.
    pub ended: Vec<(ProcessId, ProcessOutcome)>,
    /// The children whose terminal did not arrive within the grace: they may
    /// still be running. Their cancel stands; the stop does not wait longer.
    pub may_still_be_running: Vec<ProcessId>,
}

/// Why a turn's stop could not wait for its children.
#[derive(Debug, thiserror::Error)]
pub enum TurnChildrenStopError {
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
}

/// Wait up to `stop_grace` for the first `limit` live processes of `run`'s
/// `Until` subtree to reach their terminals (G1b): the children the turn's
/// end marked for cancel, and their descendants, below ended ones too. Each
/// wait is a bounded `process_terminal` wait; all share one deadline, so the
/// stop takes at most `stop_grace` however many there are.
///
/// # Errors
///
/// [`TurnChildrenStopError`]: the store failed while the children were
/// awaited.
pub async fn await_turn_children(
    cx: &ActorContext,
    session: &SessionId,
    run: &TurnId,
    stop_grace: Duration,
    limit: usize,
) -> Result<TurnChildrenStop, TurnChildrenStopError> {
    let durable = cx.backend().durable();
    let scope = ScopeKey::Turn(session.clone(), run.clone());
    let live = durable.live_until_descendants(&scope, limit).await?;
    let deadline = WaitDeadline::after(stop_grace, durable.now().await?);
    if live.is_empty() {
        return Ok(TurnChildrenStop::default());
    }
    // These waits belong to one actor. Mint them in one transition and poll
    // one race at a time, so siblings cannot stale each other's drafts.
    let mut tx = cx.begin().await?;
    let wait_scope = waits::wait_scope(cx)?;
    let pending = live
        .into_iter()
        .map(|child| {
            let (wait, _) = waits::pin(
                &mut tx,
                lash_execution_contracts::waits::WaitSpec {
                    scope: wait_scope.clone(),
                    purpose: lash_durable::domain::WaitPurpose::ProcessTerminal {
                        process: child.clone(),
                        deadline: Some(deadline.at()),
                    },
                },
            );
            (child, wait)
        })
        .collect::<Vec<_>>();
    cx.commit(tx, CommitLabel::WAIT_MINT).await?;
    for (_, wait) in &pending {
        waits::resolve_ended_terminal(cx.backend(), wait).await?;
    }
    let mut stop = TurnChildrenStop::default();
    // Every wait keeps the same deadline. Poll them serially to preserve the
    // producer's descendant order and give timeout writes one owner as well.
    for (child, wait) in pending {
        match waits::race(cx, &[wait]).await? {
            lash_execution_contracts::waits::RaceWinner::Resolved { resolution, .. } => stop
                .ended
                .push((child, waits::process_outcome(resolution)?)),
            lash_execution_contracts::waits::RaceWinner::TimedOut(_)
            | lash_execution_contracts::waits::RaceWinner::Cancelled => {
                stop.may_still_be_running.push(child)
            }
        }
    }
    Ok(stop)
}
