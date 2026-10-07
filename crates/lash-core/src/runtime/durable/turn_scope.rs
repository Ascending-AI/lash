//! A turn's scope ends with its commit or its cancel (ADR 0132 §11). Owned
//! by L6b (FIG-5176).
//!
//! # Contracts
//!
//! - **The first batch rides the ending transaction.** L3's `turn.commit`
//!   and `turn.cancel` transactions call [`end_turn_scope`] on their own
//!   `tx`: it revokes the turn's waits and marks the first batch of the
//!   turn's `Until` children for cancel through L6's `end_scope`. When
//!   children remain, the same `tx` records the turn scope as ending in the
//!   session's row set ([`SessionCloseWrite::ScopeEnding`]).
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
//! A parent's terminal does not mean its children have stopped: the
//! subtree's ends are not a durable fact (`DurableReads::live_until_descendants`).

use std::time::Duration;

use lash_core_execution::runtime::actor::process::{self, CascadeProgress};
use lash_core_execution::runtime::actor::waits::{self, ProcessWaitOutcome, WaitDeadline};
use lash_durable::domain::{ScopeKey, SessionCloseWrite};
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError};

use crate::{ActorContext, CancelOrigin, ProcessId, ProcessOutcome, SessionId, TurnId};

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
/// (`turn.commit` or `turn.cancel`): revoke the turn's waits and mark the
/// first batch of its `Until` children for cancel. When children remain the
/// scope is recorded as ending, for [`continue_scope_ends`].
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
) -> Result<CascadeProgress, DurableError> {
    let scope = ScopeKey::Turn(session.clone(), run.clone());
    let progress = mark_until_children(cx, tx, &scope).await?;
    waits::revoke_scope(tx, &scope);
    if let Ok(process) = crate::ProcessId::parse(run.as_str()) {
        waits::resolve_child_session_waits(tx, &process)?;
    }
    if progress != CascadeProgress::Done {
        tx.write(DomainWrite::SessionClose(SessionCloseWrite::ScopeEnding {
            session: session.clone(),
            scope,
        }));
    }
    Ok(progress)
}

/// Mark the rest of every ending scope of `session`, one `cascade.batch`
/// transaction per batch, clearing each scope in the transaction that marks
/// its last batch. Runs after a turn's end commits and on every claim.
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
    /// The grace does not make a wait deadline.
    #[error(transparent)]
    Deadline(#[from] waits::WaitDeadlineRefusal),
}

/// Wait up to `stop_grace` for the first `limit` live `Until` children of
/// `run`, which the turn's end marked for cancel, to reach their terminals
/// (G1b). Each wait is a bounded `process_terminal` wait; all share one
/// deadline, so the stop takes at most `stop_grace` however many children
/// there are.
///
/// # Errors
///
/// [`TurnChildrenStopError`]: the grace makes no deadline, or the store
/// failed while the children were awaited.
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
    let deadline = WaitDeadline::resolve(
        Some(stop_grace),
        stop_grace,
        stop_grace,
        durable.now().await?,
    )?;
    let outcomes = futures_util::future::join_all(
        live.iter()
            .map(|child| waits::await_process(cx, child, deadline)),
    )
    .await;
    let mut stop = TurnChildrenStop::default();
    for (child, outcome) in live.iter().zip(outcomes) {
        match outcome? {
            ProcessWaitOutcome::Resolved(outcome) => stop.ended.push((child.clone(), outcome)),
            ProcessWaitOutcome::TimedOut | ProcessWaitOutcome::Cancelled => {
                stop.may_still_be_running.push(child.clone());
            }
        }
    }
    Ok(stop)
}
