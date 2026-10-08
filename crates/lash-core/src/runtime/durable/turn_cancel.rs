//! Turn cancel on the durable path (ADR 0132 §3, §11). Owned by L3
//! (FIG-5172).
//!
//! A cancel request is the turn's cancel-request row plus a control wake,
//! committed by [`request_turn_cancel`](super::session::request_turn_cancel)
//! from outside the actor. The owner reads it on the turn's row:
//!
//! - before a model call starts, where it honours any accepted request: the
//!   boundary that closes a step, so an `AfterStep` request lets the step's
//!   response and the round or cell it asked for finish first;
//! - before a round is admitted or a cell starts, and before the turn's
//!   `turn.commit`, where it honours an `Immediate` request only;
//! - while a model call streams, a round runs or a cell runs, whenever mail
//!   may have arrived (the wake hint, or the claim poll), where it honours an
//!   `Immediate` request only.
//!
//! A requested session close (its close mail, not yet drained) is honoured
//! at each of these points as an `Immediate` request is: the close cancels
//! the open turn as its first step, so nothing the turn's in-memory work
//! would still finish survives it, and a model call, round or cell that
//! never answers cannot hold the close back (FIG-3871).
//!
//! Honouring it stops the turn's in-memory work and leaves the rest to the
//! next activation pass, which finds the request on the row and
//! [`finalize`]s the turn: its `Cancelled` terminal in one `turn.cancel`
//! commit, which also settles a tool round's unfinished members `Cancelled`.
//! A crash anywhere in between leaves the request on the row, so the next
//! owner finalizes the turn the same way, before it starts any new work.

use std::future::Future;

use lash_durable::CommitLabel;
use lash_durable::domain::{DomainWrite, TurnWrite};

use super::session::{TurnCancelRequest, TurnError, TurnRow, cancel_evidence, cancelled_cause};
use super::session_close::SESSION_CLOSE_MAIL;
use crate::{ActorContext, TurnCancelMode};

/// Whether the session's unfinished turn must stop now, as of a fenced
/// read: it accepted an `Immediate` cancel request, or the session's close
/// was requested ([`immediate_in`]).
///
/// # Errors
///
/// [`TurnError::Durable`] when the actor cannot be read, ownership lost
/// among them.
pub(super) async fn immediate(cx: &ActorContext) -> Result<bool, TurnError> {
    Ok(immediate_in(&cx.begin().await?))
}

/// Whether `tx`'s open read says the turn stops at once: its cancel request
/// is `Immediate`, or the session holds an undrained close request. The
/// boundary check a phase commit's own open answers, with no read of its
/// own.
pub(super) fn immediate_in(tx: &lash_durable::ActorTx) -> bool {
    tx.turn_cancel()
        .is_some_and(|request| request.mode == TurnCancelMode::Immediate)
        || close_requested_in(tx)
}

/// Whether the session holds a close request its actor has not drained, as
/// of `tx`'s open read.
pub(super) fn close_requested_in(tx: &lash_durable::ActorTx) -> bool {
    tx.mail()
        .iter()
        .any(|mail| mail.kind.as_str() == SESSION_CLOSE_MAIL)
}

/// Run `work` until it finishes, or the turn accepts an `Immediate` cancel
/// request or the session's close is requested: `None` when the stop won,
/// and `work` was dropped.
///
/// # Errors
///
/// [`TurnError::Durable`] when the actor cannot be read.
pub(super) async fn unless_cancelled<F: Future>(
    cx: &ActorContext,
    work: F,
) -> Result<Option<F::Output>, TurnError> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            output = &mut work => return Ok(Some(output)),
            () = cx.wait_for_mail() => {
                if immediate(cx).await? {
                    return Ok(None);
                }
            }
        }
    }
}

/// End `row`'s turn for its accepted `request`: the `Cancelled` terminal,
/// with the request's evidence as its run's cause, in one `turn.cancel` commit. It
/// starts no work; the session head does not move.
///
/// # Errors
///
/// [`TurnError::Durable`]: ownership lost, or the turn no longer open.
pub(super) async fn finalize(
    cx: &ActorContext,
    row: &TurnRow,
    request: &TurnCancelRequest,
) -> Result<(), TurnError> {
    let mut tx = cx.begin().await?;
    super::tool_round::cancel_open_round(cx, &mut tx, row).await?;
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: row.session.clone(),
        run: row.run.clone(),
        cause: Box::new(cancelled_cause(cancel_evidence(request))),
        head_revision: None,
    }));
    // The turn's scope ends with its cancel (L6b): its waits are revoked and
    // its first batch of `Until` children marked; the next pass marks the rest.
    super::turn_scope::end_turn_scope(cx, &mut tx, &row.session, &row.run).await?;
    cx.commit(tx, CommitLabel::TURN_CANCEL).await?;
    Ok(())
}
