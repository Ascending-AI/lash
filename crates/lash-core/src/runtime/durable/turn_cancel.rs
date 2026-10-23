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
//! [`restore`]s the turn from its last checkpoint and finalizes it; a paid
//! answer its model phase holds is settled as the live stop settles one. A
//! pass that finds an `AfterStep` request on a turn in its round or cell
//! (parked on a wait, or cut by a crash), or holding a paid answer, resumes
//! it instead: the step finishes, and the gate before the next model call
//! honours the request (FIG-5377, FIG-6263). Either
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
//!
//! A node that does not serve every claim requirement the session carries
//! claims it only for an `Immediate` request or its close, and
//! [`end_unserved`] ends the turn from its rows alone: no provider,
//! composition or turn drive is built or called (FIG-6072). An `AfterStep`
//! request waits for a node that can finish the step, unless the host
//! escalates it to `Immediate`.
//!
//! Every cancel that stops a turn at a paid answer, live, restored or from
//! the rows alone, ends its answered model phase through [`settle_cancelled_answer`],
//! which publishes the call's sealed record once (FIG-6274).

use std::future::Future;

pub(super) use lash_core_execution::runtime::turn_control::{close_requested_in, immediate_in};
use lash_durable::domain::{OwnerKey, RunSeq};
use lash_durable::runner::{Exit, Owned};
use lash_durable::{CommitLabel, DurableError, Release};

use super::head::SessionOwner;
use super::session::{
    ModelCall, OpenTurn, Pass, TurnCancelRequest, TurnCommit, TurnDone, TurnDrive, TurnError,
    TurnRestore, TurnRow, TurnServices, UnfinishedPhase, await_stop, cancel_evidence,
    cancelled_cause, resume_refusal,
};
use super::session_close::{Closing, close_pass};
use super::tool_round::AnsweredCell;
use super::turn_scope::continue_scope_ends;
use crate::ActorContext;
use crate::Effect;
use crate::TurnCancelMode;
use lash_vocabulary::identity::SessionId;

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

/// Whether `request`, a cancel a commit read under its fence, stops the turn
/// at once.
pub(super) fn stops_at_once(request: &lash_durable::domain::TurnCancelRequest) -> bool {
    request.mode == TurnCancelMode::Immediate
}

/// Whether the cancel `request` a turn the rows leave in `phase` accepted
/// ends it before anything resumes. An `Immediate` request does. An
/// `AfterStep` request lets the step the turn stands in finish, as the
/// uncut run would: a paid answer its receipt holds, which the turn
/// derives, or a round or cell, parked on a wait or cut by a crash. The gate
/// before its next model call honours the request (ADR 0039, FIG-5377). A
/// turn not yet started, or between steps with its model call in flight,
/// stops here.
pub(super) fn stops_before_resuming(request: &TurnCancelRequest, phase: &UnfinishedPhase) -> bool {
    if stops_at_once(request) {
        return true;
    }
    match phase {
        UnfinishedPhase::Admitted
        | UnfinishedPhase::Model {
            call: ModelCall::Requested,
            ..
        } => true,
        UnfinishedPhase::Model {
            call: ModelCall::Answered { .. },
            ..
        }
        | UnfinishedPhase::Tools { .. } => false,
    }
}

/// Watch the external stop frontier concurrently with owner work. A control
/// read may wait for a projection held by that work's in-flight commit, so
/// callers must keep polling both futures until one finishes.
pub(super) async fn wait_for_immediate(cx: &ActorContext) -> Result<(), TurnError> {
    loop {
        cx.wait_for_mail().await;
        if immediate(cx).await? {
            return Ok(());
        }
    }
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
    tokio::select! {
        biased;
        output = &mut work => Ok(Some(output)),
        stopped = wait_for_immediate(cx) => {
            stopped?;
            Ok(None)
        }
    }
}

/// End what a node that does not serve every claim requirement session
/// `owned` carries claimed it for ([`ClaimPurpose::CancelOnly`]), then let
/// it go (FIG-6072). The claim took it for its close or an `Immediate`
/// cancel of its open turn, and both end with registry writes alone: no
/// provider, composition or turn drive is built, and none is called. A
/// session claimed at its activation-loop budget with such a stop requested
/// ends it here too, before it parks (FIG-6096).
///
/// - A close runs as on any claim ([`close_pass`]).
/// - An `Immediate` cancel ends the turn from its rows ([`finalize`] with no
///   stopped drive), in one `turn.cancel` commit: its round's unfinished
///   members settle `Cancelled`, its terminal commits without a head
///   revision, its scope ends, and the session's claim requirements are
///   cleared. The calls a cell completed stay in their durable records. An
///   in-flight call's external work is never asked to stop, since its
///   provider is absent: the scope's end only ends the local wait.
///
/// Anything else is left for a node that serves the session, which the
/// actor goes back `ready` for: an `AfterStep` request waits for the step it
/// lets finish, unless the host escalates it. A session adopted in a format
/// set the node does not decode goes back unread.
///
/// [`ClaimPurpose::CancelOnly`]: lash_durable::ClaimPurpose::CancelOnly
pub(super) async fn end_unserved(
    owned: &Owned,
    cx: &ActorContext,
    services: &dyn TurnServices,
    session: &SessionId,
) -> Exit {
    match unserved_passes(owned, cx, services, session).await {
        Ok(()) | Err(TurnError::Durable(DurableError::OwnershipLost(_))) => Exit::Released,
        Err(_) => Exit::Abandoned,
    }
}

async fn unserved_passes(
    owned: &Owned,
    cx: &ActorContext,
    services: &dyn TurnServices,
    session: &SessionId,
) -> Result<(), TurnError> {
    let decoded = owned
        .store()
        .actor(owned.actor())
        .await?
        .is_some_and(|actor| {
            cx.backend()
                .formats()
                .session_decodes()
                .contains(&actor.formats)
        });
    if !decoded {
        let mut tx = cx.begin().await?;
        tx.give_up(Release::Idle);
        cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
        return Ok(());
    }
    let mut heads = SessionOwner::default();
    loop {
        if owned.draining() {
            cx.drain_release().await?;
            return Ok(());
        }
        continue_scope_ends(cx, session).await?;
        let mut tx = match close_pass(cx, session, services.tracing()).await? {
            Closing::Ran(Pass::Again) => continue,
            Closing::Ran(_) => return Ok(()),
            Closing::Not(tx) => *tx,
        };
        if let Some(row) = cx.durable_reads()?.turn(session).await?
            && let Some(request) = row.cancel.clone()
            && stops_at_once(&request)
        {
            drop(tx);
            finalize(cx, services, &row, &request, None, &mut heads).await?;
            await_stop(cx, services, session, &row.run).await?;
            continue;
        }
        tx.give_up(Release::Ready);
        cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
        return Ok(());
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
    heads: &mut SessionOwner,
) -> Result<Option<StoppedTurn>, TurnError> {
    if row.phase == UnfinishedPhase::Admitted {
        return Ok(None);
    }
    let turn = match services.resume(cx, TurnRestore::new(cx, row, heads)).await {
        Ok(turn) => turn,
        Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => return Ok(None),
        Err(error) => match resume_refusal(&error) {
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
    let (cell, between_steps) = match &row.phase {
        // Never here: an admitted turn has no state to stop with.
        UnfinishedPhase::Admitted => (None, false),
        // A turn restored with its model call in flight stands before that
        // call: the step before it closed.
        UnfinishedPhase::Model {
            call: ModelCall::Requested,
            ..
        } => (None, true),
        // A paid answer the cancel stops is settled as the live stop
        // settles one: its call recorded and its stream finished, never
        // derived.
        UnfinishedPhase::Model {
            call: ModelCall::Answered { .. },
            ..
        } => {
            settle_restored_answer(cx, drive.as_mut(), pending).await?;
            (None, false)
        }
        UnfinishedPhase::Tools { .. } => {
            (stopped_cell(cx, drive.as_mut(), row, pending).await?, false)
        }
    };
    Ok(Some(StoppedTurn {
        drive,
        present: None,
        cell,
        between_steps,
    }))
}

/// Settle the paid answer a cancel restored with its turn: the restored
/// machine re-delivers the model call the receipt answers.
async fn settle_restored_answer(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    pending: Option<Effect>,
) -> Result<(), TurnError> {
    let Some(Effect::LlmCall { id, .. }) = pending.or_else(|| drive.machine().poll_effect()) else {
        return Err(TurnError::Exec(
            "the restored receipt answers no model call the turn waits on".to_owned(),
        ));
    };
    drive.settle_cancelled_answer(cx, id).await
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
        response: None,
        close: None,
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
    heads: &mut SessionOwner,
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
    heads: &mut SessionOwner,
) -> Result<(), TurnError> {
    let cause = cancelled_cause(evidence.clone());
    let traced = super::turn_trace::outcome(services.tracing(), &cause);
    let mut tx = cx.begin().await?;
    // The round the turn stopped in is the one its row holds now: a live
    // stop's turn may have admitted it after `row` was read.
    let current = match cx.session_projection().await? {
        Some(projection) => projection.turn,
        None => cx.durable_reads()?.turn(&row.session).await?,
    };
    let current = current.as_ref().filter(|current| current.run == row.run);
    let cancelled =
        super::tool_round::cancel_open_round(cx, &mut tx, current.unwrap_or(row)).await?;
    let mut drive = None;
    let mut head_revision = None;
    // The spans of the turn's cells this commit ends: an answered cell's,
    // which it consumes, and one it stops before the cell answered, which
    // it discards. Each closes once the commit is acknowledged (§3.8).
    let mut cell_closes = Vec::new();
    let mut discarded = None;
    if let Some(StoppedTurn {
        drive: mut stopped,
        present,
        mut cell,
        between_steps: _,
    }) = stopped
    {
        discarded = stopped.discarded_cell(cx).await?;
        if let Some(cell) = &mut cell {
            stopped.finishing_cell_calls(&cell.calls);
            // The stop's commit drains the turn's activity first: what the
            // turn held of the cell's emissions goes before it.
            let mut held = cell.take_emissions();
            cell_closes.extend(held.take_close());
            super::tool_round::release_cell_emissions(
                cx,
                stopped.as_mut(),
                held,
                CommitLabel::TURN_CANCEL,
            )
            .await?;
        }
        let commit =
            stop_commit(cx, stopped.as_mut(), evidence.clone(), heads, &row.session).await?;
        stopped.stage_model_calls(&mut tx)?;
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
        tx.session_postimage(commit.postimage);
        head_revision = Some(commit.expected_head.saturating_add(1));
        super::head_commit::write_session_commit(
            &mut tx,
            &row.session,
            commit.expected_head,
            commit.commit,
        );
        drive = Some(stopped);
    }
    super::turn_terminal::write_terminal(cx, &mut tx, &row.session, &row.run, cause, head_revision)
        .await?;
    // The turn's scope ends with its cancel (L6b): its waits are revoked and
    // its first batch of `Until` children marked; the next pass marks the rest.
    super::turn_scope::end_turn_scope(cx, &mut tx, &row.session, &row.run).await?;
    // Nothing the ended turn called is owed any more: the commit's claim
    // requirements are what the session's head alone still owes, the
    // observers of a config change it has not delivered (FIG-6072,
    // FIG-6164).
    // A head commit the session store refuses for its content is the stop's
    // runtime refusal, as it is `turn.commit`'s (FIG-5352).
    let delivered = drive
        .as_ref()
        .map(|drive| drive.delivered_inputs())
        .unwrap_or_default();
    let input = super::turn_trace::input(cx, services.tracing(), row, &delivered).await;
    let executor = drive.as_ref().and_then(|drive| drive.code_executor());
    // Publication outlives the projection if this terminal releases the
    // claim. Capture its base before committing and use the receipt's head
    // revision when this stop also publishes a new head.
    let publication_revision = match cx.session_projection().await? {
        Some(projection) => crate::runtime::observation::observation_revision(&projection.head),
        None => crate::SessionRevision(
            cx.backend()
                .session_store_factory()
                .load_session_head_meta(&row.session)
                .await
                .map_err(|error| TurnError::Runtime(error.runtime_error()))?
                .map_or(0, |head| head.head_revision),
        ),
    };
    let receipt = super::session_publication::commit(
        cx,
        executor.as_ref(),
        &row.session,
        tx,
        CommitLabel::TURN_CANCEL,
        None,
    )
    .await
    .map_err(|error| {
        let refusal = match &error {
            DurableError::Domain(refusal) => refusal.session_commit_refusal(),
            _ => None,
        };
        refusal.map_or(TurnError::Durable(error), TurnError::Runtime)
    })?;
    let publication_revision = receipt
        .session_heads
        .iter()
        .find(|head| head.session == row.session)
        .map_or(publication_revision, |head| {
            crate::SessionRevision(head.receipt.head_revision)
        });
    heads.acknowledged(cx, &row.session).await?;
    for record in cx
        .durable_reads()?
        .model_calls(
            &row.session,
            &lash_durable::domain::ModelCallId::turn_owner(&row.run),
        )
        .await?
    {
        services
            .publish_model_call(cx, row, &record.call_id, publication_revision)
            .await?;
    }
    super::turn_trace::members_ended(
        services.tracing(),
        row,
        &cancelled,
        super::turn_trace::MemberEnd::Cancelled,
    );
    for close in cell_closes {
        close.publish();
    }
    if let Some(discarded) = discarded {
        super::turn_trace::members_ended(
            services.tracing(),
            row,
            &discarded.open,
            super::turn_trace::MemberEnd::Discarded,
        );
        discarded.close.publish();
    }
    super::turn_trace::ended(
        services.tracing(),
        row,
        traced,
        &delivered,
        (input, Vec::new()),
        (
            current.unwrap_or(row).model_calls,
            Some(current.unwrap_or(row).tool_calls),
        ),
    );
    if let Some(mut drive) = drive {
        drive.committed(cx, &receipt).await?;
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
    heads: &mut SessionOwner,
    session: &lash_vocabulary::identity::SessionId,
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
