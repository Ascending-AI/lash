//! Turn cancel on the durable path (ADR 0132 §3, §11). Owned by L3
//! (FIG-5172).
//!
//! A cancel request is the turn's cancel-request row plus a control wake,
//! committed by [`request_turn_cancel`](super::session::request_turn_cancel)
//! from outside the actor. The owner reads it on the turn's row:
//!
//! - before a model call starts, where it honours any accepted request: the
//!   boundary that closes a step, so an `AfterStep` request lets the step's
//!   response and the round or cell it asked for finish first, and its
//!   evidence names the protocol iteration that boundary closed;
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
//! A live model call settles cooperatively before the owner stops, preserving
//! its sealed attempts in the observed model-call record. Other in-memory
//! work is dropped when the stop wins. The owner then [`finalize`]s the turn
//! from the drive it holds, as `turn.commit` commits from it. Only a stop
//! honoured before `turn.commit`, whose machine has finished, leaves it to
//! the next activation pass, which finds the request on the row,
//! [`restore`]s the turn from its last checkpoint and finalizes it. A pass
//! that finds an `AfterStep` request on a turn in its round or cell (parked
//! on a wait, or cut by a crash) resumes it instead: the step finishes, and
//! the gate before the next model call honours the request (FIG-5377). Either
//! way the turn ends in one `turn.cancel` commit (ADR 0039): its `Cancelled`
//! terminal and its stop as the session head's next revision, which keeps
//! its input, the steps it completed and the steering input it delivered
//! (FIG-5320, FIG-5373). The commit also settles a tool round's unfinished
//! members `Cancelled`, records the tool calls a cell the turn stopped on
//! completed, and applies a `Drop` request's disposition to the input
//! addressed to the turn that it never delivered (FIG-5365); once a turn
//! accepted a cancel, its checkpoints deliver it no more input. A crash
//! anywhere in between leaves the request on the row, so the next owner
//! finalizes the turn from its rows, before it starts any new work.

use std::future::Future;

use lash_durable::domain::{DomainWrite, OwnerKey, RunSeq, TurnWrite};
use lash_durable::{CommitLabel, DurableError};

use super::head::HeadCache;
use super::session::{
    OpenTurn, TurnCancelRequest, TurnCommit, TurnDone, TurnDrive, TurnError, TurnRestore, TurnRow,
    TurnServices, UnfinishedPhase, cancel_evidence, cancelled_cause, undecodable_state,
};
use super::session_close::SESSION_CLOSE_MAIL;
use super::tool_round::AnsweredCell;
use crate::{ActorContext, Effect, TurnCancelMode};

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

/// A cancelled turn's drive where its stop commits: the live drive the
/// owner stopped, or the drive [`restore`]d from the turn's last checkpoint,
/// with what the turn's next commit owed.
pub(super) struct StoppedTurn {
    /// The turn's drive, its machine where the turn stopped.
    pub(super) drive: Box<dyn TurnDrive>,
    /// A settled round's presentation the turn has yet to commit.
    pub(super) present: Option<lash_core_execution::runtime::actor::round::PresentRecord>,
    /// The tool calls of a cell the turn has yet to record: one that
    /// answered, or the one it stopped on (FIG-5330).
    pub(super) cell: Option<AnsweredCell>,
    /// Whether the turn stopped between steps, before its next model call:
    /// the boundary an `AfterStep` request is honoured at, which its
    /// evidence names ([`honoured_evidence`]).
    pub(super) between_steps: bool,
}

/// `row`'s turn restored from its last checkpoint for the cancel it
/// accepted, with the tool calls the cell it stopped on completed. `None`
/// when its stop has no state to commit: a turn its cancel refused at its
/// start, before any phase checkpointed; one its committed state cannot
/// prepare; or one whose parked state this build does not decode, which its
/// cancel ends without reading, so the refusal never holds the session's one
/// way out (FIG-5601).
///
/// # Errors
///
/// [`TurnError`] when the turn or its stopped cell's records do not read.
pub(super) async fn restore(
    cx: &ActorContext,
    services: &dyn TurnServices,
    row: &TurnRow,
    heads: &mut HeadCache,
) -> Result<Option<StoppedTurn>, TurnError> {
    if row.phase == UnfinishedPhase::Admitted {
        return Ok(None);
    }
    let turn = match services.resume(cx, TurnRestore::new(cx, row, heads)).await {
        Ok(turn) => turn,
        Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => return Ok(None),
        Err(error) => match undecodable_state(&error) {
            Some(refusal) => {
                tracing::warn!(
                    session_id = %row.session,
                    run_id = %row.run,
                    refusal = %refusal.encode(),
                    "the cancelled turn ends without its state, which this build does not decode"
                );
                return Ok(None);
            }
            None => return Err(error),
        },
    };
    let OpenTurn {
        mut drive, pending, ..
    } = turn;
    let cell = stopped_cell(cx, drive.as_mut(), row, pending).await?;
    Ok(Some(StoppedTurn {
        drive,
        present: None,
        cell,
        // A turn restored in its model phase stands before that call: the
        // step before it closed.
        between_steps: row.phase.model().is_some(),
    }))
}

/// The tool calls the cell `row`'s turn stopped on completed, when the turn's
/// phase is a cell whose calls its rows do not record yet: what the commit
/// that ends the turn without the cell's answer records of it (FIG-5330).
async fn stopped_cell(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    row: &TurnRow,
    pending: Option<Effect>,
) -> Result<Option<AnsweredCell>, TurnError> {
    let UnfinishedPhase::Tools { run, .. } = &row.phase else {
        return Ok(None);
    };
    // The restored machine re-delivers the cell, which names its snapshot.
    let Some(Effect::ExecCode { id, .. }) = pending.or_else(|| drive.machine().poll_effect())
    else {
        return Ok(None);
    };
    if RunSeq(id.0) != *run {
        return Ok(None);
    }
    // A run the turn's facts hold at its phase is a tool round, whose
    // members are recorded already, or the cell's report.
    let owner = OwnerKey::Turn(row.session.clone(), row.run.clone());
    let facts = cx.durable_reads()?.run_facts(&owner).await?;
    if facts.rows.iter().any(|stored| stored.run == *run) {
        return Ok(None);
    }
    let calls = drive.stopped_cell_calls(cx, id).await?;
    Ok((!calls.is_empty()).then_some(AnsweredCell {
        id,
        calls,
        held: None,
    }))
}

/// End `row`'s turn for its accepted `request` in one `turn.cancel` commit
/// (ADR 0039): its `Cancelled` terminal, with the request's evidence as its
/// run's cause, and, when the turn `stopped` with state to commit, its stop
/// as the session head's next revision. That commit holds what the turn
/// reached through its last prompt-resume-safe point (its input, its
/// completed steps and the steering input it delivered, settled with their
/// applications); its unanswered tail is dropped. It also settles a tool
/// round's unfinished members `Cancelled`, records the tool calls of the
/// cell the turn answered or stopped on (FIG-5330), and, for a `Drop`
/// request, cancels the input addressed to the turn that it never delivered.
/// The terminal writer selects these inputs inside its transaction and records
/// their dispositions with the terminal, including `Defer`. It starts no work.
///
/// A stop whose head commit the session refuses for good, or whose commit
/// cannot be built, still ends the turn: its terminal commits without the
/// head, so nothing holds the cancel back.
///
/// # Errors
///
/// [`TurnError`]: ownership lost, the turn no longer open, or the session's
/// mailbox unreadable.
pub(super) async fn finalize(
    cx: &ActorContext,
    services: &dyn TurnServices,
    row: &TurnRow,
    request: &TurnCancelRequest,
    mut stopped: Option<StoppedTurn>,
    heads: &mut HeadCache,
) -> Result<(), TurnError> {
    let evidence = honoured_evidence(request, stopped.as_mut());
    let with_head = stopped.is_some();
    match end(cx, services, row, &evidence, stopped, heads).await {
        Err(TurnError::Runtime(refusal)) if with_head && refusal.is_terminal() => {
            tracing::warn!(
                session_id = %row.session,
                run_id = %row.run,
                error = %refusal,
                "the cancelled turn's stop was refused its head commit; the turn ends without it"
            );
            end(cx, services, row, &evidence, None, heads).await
        }
        ended => ended,
    }
}

/// The evidence of the cancel `request` the turn accepted, as its stop
/// records it: an `AfterStep` request honoured between steps names the
/// protocol iteration whose closing boundary honoured it, the one before the
/// iteration the turn stands in (ADR 0039, FIG-5364). One honoured before
/// any step closed, or where only an `Immediate` stop is, names none.
fn honoured_evidence(
    request: &TurnCancelRequest,
    stopped: Option<&mut StoppedTurn>,
) -> crate::runtime::TurnCancellationEvidence {
    let mut evidence = cancel_evidence(request);
    if request.mode == TurnCancelMode::AfterStep
        && let Some(stopped) = stopped.filter(|stopped| stopped.between_steps)
    {
        evidence.honoured_after_step = stopped.drive.machine().protocol_iteration().checked_sub(1);
    }
    evidence
}

async fn end(
    cx: &ActorContext,
    services: &dyn TurnServices,
    row: &TurnRow,
    evidence: &crate::runtime::TurnCancellationEvidence,
    stopped: Option<StoppedTurn>,
    heads: &mut HeadCache,
) -> Result<(), TurnError> {
    let cause = cancelled_cause(evidence.clone());
    let traced = super::turn_trace::outcome(services.tracing(), &cause);
    let mut tx = cx.begin().await?;
    // The round the turn stopped in is the one its row holds now: a live
    // stop's turn may have admitted it after `row` was read.
    let current = cx.durable_reads()?.turn(&row.session).await?;
    let current = current.as_ref().filter(|current| current.run == row.run);
    super::tool_round::cancel_open_round(cx, &mut tx, current.unwrap_or(row)).await?;
    let mut drive = None;
    let mut head_revision = None;
    if let Some(StoppedTurn {
        drive: mut stopped,
        present,
        mut cell,
        between_steps: _,
    }) = stopped
    {
        if let Some(cell) = &mut cell {
            stopped.finishing_cell_calls(&cell.calls);
            // The stop's commit drains the turn's activity first: what the
            // turn held of the cell's emissions goes before it.
            let held = cell.held.take().unwrap_or_default();
            super::tool_round::release_cell_emissions(cx, stopped.as_mut(), held).await?;
        }
        let commit =
            stop_commit(cx, stopped.as_mut(), evidence.clone(), heads, &row.session).await?;
        if let Some(present) = present {
            present.write(&mut tx);
        }
        super::tool_round::consume_answered_cell(
            cx,
            stopped.as_mut(),
            &mut tx,
            row,
            &mut cell,
            false,
        )
        .await?;
        // The commit settles what the turn's checkpoints delivered: bound
        // to the run first, as the settlement requires.
        if let Some(bind) = super::phases::bind_delivered(stopped.as_ref(), &row.session, &row.run)
        {
            tx.write(bind);
        }
        head_revision = Some(commit.expected_head.saturating_add(1));
        super::head_commit::write_session_commit(
            &mut tx,
            &row.session,
            commit.expected_head,
            commit.commit,
        );
        drive = Some(stopped);
    }
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: row.session.clone(),
        run: row.run.clone(),
        cause: Box::new(cause),
        head_revision,
    }));
    // The turn's scope ends with its cancel (L6b): its waits are revoked and
    // its first batch of `Until` children marked; the next pass marks the rest.
    super::turn_scope::end_turn_scope(cx, &mut tx, &row.session, &row.run).await?;
    // A turn cancelled before its first phase commit still owes its
    // admission's export: it is made before the commit that ends the turn
    // (FIG-5998).
    super::session::export_owed_admission(services.tracing(), current.unwrap_or(row));
    // A head commit the session store refuses for its content is the stop's
    // runtime refusal, as it is `turn.commit`'s (FIG-5352).
    cx.commit(tx, CommitLabel::TURN_CANCEL)
        .await
        .map_err(|error| {
            let refusal = match &error {
                DurableError::Domain(refusal) => refusal.session_commit_refusal(),
                _ => None,
            };
            refusal.map_or(TurnError::Durable(error), TurnError::Runtime)
        })?;
    super::turn_trace::ended(services.tracing(), row, traced);
    if let Some(mut drive) = drive {
        drive.committed().await;
        // The commit moved the head: the next turn loads it again.
        heads.evict();
    }
    Ok(())
}

/// The stop's head commit: `drive`'s machine finished for the cancel
/// `evidence` names where it stopped ([`TurnMachine::finish_cancelled`]),
/// then built over the session head `heads` holds. A machine the stop
/// already finished (a model call the cancel settled ends it) keeps what it
/// holds, and its outcome is the stop's. Only what reports is handled on
/// the way: the work the machine waited on is its abandoned tail.
///
/// [`TurnMachine::finish_cancelled`]: crate::TurnMachine::finish_cancelled
async fn stop_commit(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    evidence: crate::runtime::TurnCancellationEvidence,
    heads: &mut HeadCache,
    session: &crate::SessionId,
) -> Result<TurnCommit, TurnError> {
    let outcome = crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled {
        evidence: evidence.clone(),
    });
    if !drive.machine().is_done() {
        drive.machine().finish_cancelled(evidence);
    }
    loop {
        match drive.machine().poll_effect() {
            Some(Effect::Done {
                messages,
                event_delta,
                protocol_iteration,
            }) => {
                let done = TurnDone {
                    messages,
                    event_delta,
                    protocol_iteration,
                    outcome: Some(outcome),
                    finish_value_schema: None,
                };
                return drive.finish(cx, done, heads.head(cx, session).await?).await;
            }
            Some(Effect::Emit(crate::SessionStreamEvent::TurnOutcome { .. })) => {
                let ended = crate::SessionStreamEvent::TurnOutcome {
                    outcome: outcome.clone(),
                };
                drive.local(cx, Effect::Emit(ended)).await?;
            }
            Some(local @ (Effect::Emit(_) | Effect::Progress { .. } | Effect::Log { .. })) => {
                drive.local(cx, local).await?;
            }
            Some(_) => {}
            None => {
                return Err(TurnError::Exec(
                    "the cancelled turn's machine stalled before its stop".to_owned(),
                ));
            }
        }
    }
}
