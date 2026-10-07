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
//! Honouring it stops the turn's in-memory work and leaves the rest to the
//! next activation pass, which finds the request on the row and
//! [`finalize`]s the turn: its `Cancelled` terminal in one `turn.cancel`
//! commit, which also settles a tool round's unfinished members `Cancelled`.
//! A crash anywhere in between leaves the request on the row, so the next
//! owner finalizes the turn the same way, before it starts any new work.

use std::future::Future;

use lash_durable::CommitLabel;
use lash_durable::domain::{DomainWrite, TurnWrite};

use super::session::{
    TurnCancelRequest, TurnError, TurnRow, TurnTerminal, cancel_evidence, cancelled_cause,
};
use crate::{ActorContext, SessionId, TurnCancelMode};

/// The cancel request the session's unfinished turn accepted, as of now.
///
/// # Errors
///
/// [`TurnError::Durable`] when the row cannot be read.
pub(super) async fn requested(
    cx: &ActorContext,
    session: &SessionId,
) -> Result<Option<TurnCancelRequest>, TurnError> {
    Ok(cx
        .durable_reads()?
        .turn(session)
        .await?
        .and_then(|row| row.cancel))
}

/// Whether the session's unfinished turn accepted an `Immediate` cancel
/// request, as of now.
///
/// # Errors
///
/// [`TurnError::Durable`] when the row cannot be read.
pub(super) async fn immediate(cx: &ActorContext, session: &SessionId) -> Result<bool, TurnError> {
    Ok(requested(cx, session)
        .await?
        .is_some_and(|request| request.mode == TurnCancelMode::Immediate))
}

/// Run `work` until it finishes or the turn accepts an `Immediate` cancel
/// request: `None` when the request won, and `work` was dropped.
///
/// # Errors
///
/// [`TurnError::Durable`] when the turn row cannot be read.
pub(super) async fn unless_cancelled<F: Future>(
    cx: &ActorContext,
    session: &SessionId,
    work: F,
) -> Result<Option<F::Output>, TurnError> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            output = &mut work => return Ok(Some(output)),
            () = cx.wait_for_mail() => {
                if immediate(cx, session).await? {
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
) -> Result<TurnTerminal, TurnError> {
    let cause = cancelled_cause(cancel_evidence(request))?;
    let mut tx = cx.begin().await?;
    super::tool_round::cancel_open_round(cx, &mut tx, row).await?;
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: row.session.clone(),
        run: row.run.clone(),
        terminal: TurnTerminal::Cancelled,
        cause_json: Some(cause),
        head_revision: None,
    }));
    // The turn's scope ends with its cancel (L6b): its waits are revoked and
    // its first batch of `Until` children marked; the next pass marks the rest.
    super::turn_scope::end_turn_scope(cx, &mut tx, &row.session, &row.run).await?;
    cx.commit(tx, CommitLabel::TURN_CANCEL).await?;
    Ok(TurnTerminal::Cancelled)
}
