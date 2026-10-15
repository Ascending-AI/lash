//! The turn's phase runner (ADR 0132 §4). Owned by V0 (FIG-5170), then L3
//! (FIG-5172).
//!
//! The runner polls the turn's machine through its [`TurnDrive`] and commits
//! at the catalog's labels: `model.start` admits a model call before its
//! first byte (ADR 0133 §6): a new call takes the next ordinal of the turn's
//! calls, composes its prompt into its request and is lowered to its request
//! template, every attachment its slots name is held under the turn's
//! execution, and its admission commits its pin, the checkpoint that
//! re-delivers it, the plugin namespaces the turn's run changed since its
//! last commit (the pending checkpoint-callback decisions among them; an
//! unchanged namespace is never written) and its admission record
//! (snapshot and template); a resend is the same call, prepares nothing and
//! sends the stored template with its slots filled afresh. `model.done` commits the
//! phase that re-delivers a round or a cell
//! before it starts, and `turn.commit` publishes the next revision of the
//! head the owner cached with the turn's terminal in one fenced
//! transaction, the store's compare-and-set against that head. A frame
//! switch's commit also mails the session its follow-on task, which the
//! session's next drain admits as its next turn. Everything
//! between commits is in memory and is
//! recomputed from committed state after a crash; nothing re-executes
//! orchestration to reach a recorded outcome. On a draining node the turn
//! stops where it would honour a cancel, before its next model call or cell
//! starts, and the next build resumes it from its rows.
//!
//! A turn cancel is honoured where [`turn_cancel`] says: any accepted request
//! before a model call starts, so an `AfterStep` request lets the step's
//! response, round or cell finish first; an `Immediate` one also before a
//! cell starts, while a call streams or a cell runs, and before
//! `turn.commit`. The owner commits the stop from the drive it holds; only
//! one honoured before `turn.commit`, whose machine is finished, leaves its
//! stop to the next pass, which restores it from its last checkpoint. The
//! turn's held terminal publishes only once the commit that ends it is
//! acknowledged ([`TurnDrive::committed`]).

use lash_durable::{CommitLabel, Release};

#[path = "output_boundary.rs"]
mod output_boundary;
use lash_durable::DomainWrite;
use lash_durable::DurableError;
use lash_durable::domain::{
    ModelCallId, PhaseAdvance, PromptCallKey, PromptWrite, RunSeq, SessionMailWrite, TurnWrite,
};
use output_boundary::OutputBoundary;

use super::head::HeadCache;
use super::phase_root::StagedPhase;
use super::session::{
    CellExit, CodeCell, ComposedCall, OpenTurn, ParkedTurnState, PhaseCheckpoint, PhaseExit,
    PreparedCall, TurnCancelRequest, TurnDone, TurnDrive, TurnError, TurnRow, TurnServices,
    UnfinishedPhase,
};
use super::session_mail::{agent_frame_switches, follow_on_mail};
use super::tool_round::{self, RoundExit};
use super::turn_scope::end_turn_scope;
use super::{model_call, turn_cancel, turn_trace};
use crate::plugin::prompt::{admission_record, load_admitted_call};
use crate::{
    ActorContext, Effect, HeldResponse, HostTurnProtocol, SessionStreamEvent, TurnMachine,
};
use lash_sansio::llm::types::AdmittedSend;
use lash_sansio::{CheckpointDelta, SessionId, TurnId};
use std::sync::Arc;

/// The phase checkpoint of `drive` as it stands: its machine's checkpoint
/// and the input its checkpoints delivered, staged as its PhaseRoot.
fn encode_checkpoint(drive: &mut dyn TurnDrive) -> Result<StagedPhase, TurnError> {
    let delta = drive.machine().checkpoint_delta();
    encode_phase(drive, delta)
}

/// `delta`, a checkpoint of `drive`'s machine, staged as its PhaseRoot: the
/// content the machine's checkpoint added, over the content an earlier
/// phase of the turn stored.
fn encode_phase(
    drive: &mut dyn TurnDrive,
    delta: CheckpointDelta<HostTurnProtocol>,
) -> Result<StagedPhase, TurnError> {
    let checkpoint = PhaseCheckpoint {
        saved: delta.saved,
        delivered: drive.delivered_inputs(),
        before_turn: drive.before_turn(),
        trace: drive.trace_scope(),
        config: drive.run_config(),
        cell: drive.cell_start(),
    };
    StagedPhase::of(&checkpoint, &delta.held).map_err(TurnError::Exec)
}

/// The plugin namespaces `drive`'s run changed that its rows do not record
/// yet, written into `tx` beside the phase (FIG-5301): what the phase's
/// commit records once it is acknowledged ([`TurnDrive::run_changes_committed`]).
pub(super) fn write_run_changes(
    drive: &dyn TurnDrive,
    tx: &mut lash_durable::ActorTx,
    session: &SessionId,
    run: &TurnId,
) -> Vec<lash_durable::domain::TurnNamespaceWrite> {
    let changes = drive.run_changes();
    if !changes.is_empty() {
        #[expect(
            clippy::expect_used,
            reason = "a namespace value always encodes, and an unbounded staging area places \
                      every value it holds"
        )]
        tx.write_namespaces(session, run, &changes)
            .expect("a run's namespace changes stage");
    }
    changes
}

/// The bind of the steering input `drive`'s checkpoints delivered to its
/// run, written in every phase commit that records the delivery (ADR 0132
/// §4): the first binds the rows, and a later one finds them bound to the run
/// already. A row no longer open refuses the commit, which the turn then
/// recomputes without it.
pub(super) fn bind_delivered(
    drive: &dyn TurnDrive,
    session: &SessionId,
    run: &TurnId,
) -> Option<DomainWrite> {
    let inputs = drive
        .delivered_inputs()
        .iter()
        .flat_map(crate::AdmittedTurnInputs::input_ids)
        .collect::<Vec<_>>();
    (!inputs.is_empty()).then(|| {
        DomainWrite::SessionMail(SessionMailWrite::Admit {
            session: session.clone(),
            run: run.clone(),
            inputs,
            batches: Vec::new(),
        })
    })
}

/// `model.answer`: the receipt of the pinned call `drive` just answered,
/// the provider's outcome, committed beside the call's model phase under
/// the owner fence, before anything derives from it: the phase's call is
/// answered. The receipt is one immutable placed text the turn's row names:
/// the phase and its root stay as `model.start` committed them, for the
/// machine waits on the same call at the same checkpoint, so the commit
/// writes the receipt's own nodes and the row's reference, never a new
/// phase root (FIG-6253). The receipt states the callbacks its derivation
/// owes (S7). The drive then owes the receipt as it committed, which is
/// what every derivation reads, on this worker or after a restore: nothing
/// only the live outcome held reaches a hook. Its acknowledgement is
/// awaited: a failed or unacknowledged commit fails the attempt, whose next
/// pass reconciles from the rows (a committed receipt restores at
/// `model.answer` and is derived, never sent again; an uncommitted one
/// leaves the call unanswered at `model.start`), and never falls through to
/// a derivation. What the commit read of the turn's cancel comes back.
async fn record_answer(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    session: &SessionId,
    run: &TurnId,
) -> Result<lash_durable::ActorCommit, TurnError> {
    let answer = drive
        .owed_answer()
        .take()
        .ok_or_else(|| TurnError::Exec("the model call answered no receipt".to_owned()))?;
    let owed = answer.owed_callbacks();
    let mut staging = lash_core_store::checkpoint_graph::GraphStaging::new(
        lash_core_store::checkpoint_graph::EncodeBudget::new(
            lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
        ),
    );
    let (root, receipt) = lash_core_execution::runtime::actor::model_receipt::ModelReceipt {
        owed: owed.clone(),
        answer,
    }
    .stage(&mut staging)
    .map_err(TurnError::Exec)?;
    *drive.owed_answer() = Some(receipt.answer);
    let mut tx = cx.begin().await?;
    tx.stage_all(staging);
    tx.owe(root, owed);
    tx.write(DomainWrite::Turn(TurnWrite::Answer {
        session: session.clone(),
        run: run.clone(),
        answer: root,
    }));
    Ok(cx.commit(tx, CommitLabel::MODEL_ANSWER).await?)
}

fn iteration(machine: &TurnMachine) -> u32 {
    u32::try_from(machine.protocol_iteration()).unwrap_or(u32::MAX)
}

/// Run the turn's phases from `turn`, committing at each label, until it
/// commits over the head `heads` holds, suspends or loses ownership. Once
/// `turn.commit` moved the head, `heads` no longer holds it.
///
/// # Errors
///
/// [`TurnError`].
pub async fn run_phases(
    cx: &ActorContext,
    services: &dyn TurnServices,
    turn: OpenTurn,
    heads: &mut HeadCache,
) -> Result<PhaseExit, TurnError> {
    let OpenTurn {
        mut drive,
        mut pending,
        row,
    } = turn;
    let session = row.session.clone();
    let run = row.run.clone();
    let switches = agent_frame_switches(cx, &row).await?;
    let limit = services
        .execution_budgets(&session)
        .config()
        .agent_frame_switch_limit
        .get();
    if switches >= limit {
        tracing::debug!(session_id = %session, run_id = %run, switches, limit,
            "the agent frame switch chain reached its bound");
        pending = None;
        drive
            .machine()
            .finish_with_outcome(crate::TurnOutcome::Stopped(
                crate::TurnStop::AgentFrameSwitchLimit,
            ));
        drive
            .local(
                cx,
                Effect::Emit(lash_sansio::session_model::make_error_event(
                    crate::TurnFailureKind::Runtime,
                    Some(crate::TurnFailureCode::AgentFrameSwitchLimit.into()),
                    format!("logical turn reached the limit of {limit} agent frame switches"),
                    None,
                    lash_sansio::session_model::RuntimeOutputCuts::standard(),
                )),
            )
            .await?;
    }
    // The model call in flight as the rows left it, and the effect the
    // restored machine re-delivers it as: that call is resent as its next
    // attempt, under its recorded deadline, and never composed again.
    let mut model = row.phase.model().and_then(|pin| {
        drive
            .machine()
            .waiting_model_call()
            .map(|redelivered| (redelivered, pin.clone()))
    });
    // How many model calls the turn admitted: a new call takes the next
    // ordinal, a resend keeps its own.
    let mut calls = row.model_calls;
    let tool_calls_at_start = cx.tool_call_admissions();
    // Whether the last admitted call is unsettled, its request still held:
    // the call a model phase pins. The commit that leaves the model phase,
    // or admits the next call, settles it (FIG-5946).
    let mut unsettled = row.phase.model().is_some();
    let mut outcome = None;
    // A settled round's presentation, committed with the turn's next commit.
    let mut carry: Option<lash_core_execution::runtime::actor::round::PresentRecord> = None;
    // An answered cell's tool calls, recorded with the turn's next commit.
    let mut answered_cell: Option<tool_round::AnsweredCell> = None;
    let mut boundary = OutputBoundary::default();
    loop {
        let effect = match pending.take() {
            Some(effect) => effect,
            None => match drive.machine().poll_effect() {
                Some(effect) => effect,
                None => return Err(TurnError::Exec("the turn machine stalled".to_owned())),
            },
        };
        // An output other than the next provider call, or a terminal whose
        // finish drains activity, consumes recorded outcomes first.
        if boundary.held()
            && matches!(
                &effect,
                Effect::ExecCode { .. }
                    | Effect::ToolCalls { .. }
                    | Effect::ReportToolCalls { .. }
                    | Effect::Done { .. }
            )
            && !boundary.flush(cx, drive.as_mut(), None).await?
        {
            return Ok(PhaseExit::CancelRequested);
        }
        match effect {
            Effect::LlmCall { id, request } => {
                // A call restored at `model.answer` was answered: it derives
                // from its receipt, and is never sent again.
                if drive.owed_answer().is_some() {
                    if !matches!(model.take(), Some((redelivered, _)) if redelivered == id) {
                        return Err(TurnError::Exec(format!(
                            "the recorded completion does not answer model call {id:?}"
                        )));
                    }
                    drive.derive_answer(cx, id).await?;
                    continue;
                }
                // `model.start`'s open is the call's one read: the turn's
                // accepted cancel, a requested close and the store's clock
                // come with it.
                let mut tx = cx.begin().await?;
                // The turn is between steps: the stop an accepted request
                // asks for commits here, with the step the turn finished
                // (ADR 0039).
                if tx.turn_cancel().is_some() || turn_cancel::close_requested_in(&tx) {
                    if boundary.held() {
                        if turn_cancel::immediate_in(&tx) || turn_cancel::close_requested_in(&tx) {
                            return Ok(PhaseExit::CancelRequested);
                        }
                        drop(tx);
                        if !boundary.flush(cx, drive.as_mut(), None).await? {
                            return Ok(PhaseExit::CancelRequested);
                        }
                        tx = cx.begin().await?;
                    }
                    let request = tx.turn_cancel().cloned();
                    drop(tx);
                    let mut stopped = stopped(drive, &mut carry, &mut answered_cell);
                    stopped.between_steps = true;
                    return stop(cx, services, &row, request, stopped, heads).await;
                }
                if cx.draining() {
                    if boundary.held() {
                        drop(tx);
                        if !boundary
                            .flush(cx, drive.as_mut(), Some(Release::Ready))
                            .await?
                        {
                            return Ok(PhaseExit::CancelRequested);
                        }
                        return Ok(PhaseExit::Released);
                    }
                    return Ok(PhaseExit::Drained);
                }
                let current = iteration(drive.machine());
                // `model` is spent: only the call the restored machine
                // re-delivers is the pinned one; every other call is new.
                let (pinned, request, admitted, admission) = match model.take() {
                    Some((redelivered, pin)) if redelivered == id => {
                        // A resend sends the template its admission stored,
                        // read back byte for byte, and reads the response
                        // under the context stored with it; nothing prepares
                        // it again.
                        match admitted_send(cx, &session, &run, pin.call).await? {
                            Ok(admitted) => (Some(pin), request, admitted, None),
                            Err(unavailable) => {
                                drive.model_unsent(id, unavailable);
                                continue;
                            }
                        }
                    }
                    _ => {
                        // A new call is prepared over the turn's committed
                        // state: composed, decided and lowered to its
                        // request template, whose attachments are held
                        // under the turn's execution before it is admitted.
                        // The machine waits on the request that carries it,
                        // so the checkpoint names it.
                        let call = calls.saturating_add(1);
                        match drive.prepare_call(cx, id, call, request).await? {
                            PreparedCall::Admit(composed) => {
                                let ComposedCall {
                                    request,
                                    prompt,
                                    template,
                                } = *composed;
                                // The turn's execution holds them, as it holds
                                // the turn's own puts.
                                let turn_cx = cx.scoped(crate::AdmittedScope::turn(
                                    session.clone(),
                                    run.clone(),
                                ))?;
                                if let Err(refused) =
                                    crate::runtime::owned_call::hold_slots(&turn_cx, &template)
                                        .await
                                        .map_err(|error| {
                                            TurnError::Runtime(error.into_runtime_error())
                                        })?
                                {
                                    drive.model_unsent(id, refused);
                                    continue;
                                }
                                if !drive.machine().admit_request(id, Arc::clone(&request)) {
                                    return Err(TurnError::Exec(format!(
                                        "the turn machine does not wait on model call {id:?}"
                                    )));
                                }
                                let admitted =
                                    AdmittedSend::of_request(&request, Arc::new(template));
                                let record = admission_record(
                                    call_key(&session, &run, call),
                                    prompt.as_ref(),
                                    &admitted.template,
                                    &admitted.response,
                                    None,
                                )
                                .map_err(|error| {
                                    TurnError::Exec(format!(
                                        "the model call's admission does not encode: {error}"
                                    ))
                                })?;
                                (None, request, admitted, Some(record))
                            }
                            PreparedCall::Unsent(refused) => {
                                drive.model_unsent(id, refused);
                                continue;
                            }
                            PreparedCall::Ended => continue,
                        }
                    }
                };
                // The checkpoint `model.start` commits names the request by
                // content digest; its pin reuses that digest, beside where
                // the live replay stands before the call streams.
                let delta = drive.machine().checkpoint_delta();
                let start = model_call::start(
                    &services.execution_budgets(&session),
                    tx.opened_at(),
                    row.turn_deadline,
                    pinned,
                    calls.saturating_add(1),
                    model_call::request_ref(&delta.saved.checkpoint)?,
                    drive.live_stream_cursor(),
                )?;
                if let model_call::ModelStart::Send { pin, resent, .. } = &start {
                    // `model.start` admits the call: its identity and pin,
                    // the checkpoint that re-delivers its request, the
                    // plugin state the turn published, the pending
                    // checkpoint-callback decisions among it, and its
                    // admission record (its prompt snapshot and request
                    // template), in one transaction. A resend commits its next
                    // attempt and records nothing.
                    let label = if boundary.held() {
                        CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START
                    } else {
                        tool_round::model_start_label(&carry)
                    };
                    boundary.append_to(&mut tx);
                    if let Some(present) = carry.take() {
                        present.write(&mut tx);
                    }
                    if let Some(record) = admission {
                        tx.write(record);
                        // After the record, so a text both calls hold is
                        // never reclaimed between them.
                        if unsettled {
                            settle_model_calls(&mut tx, &session, &run, Some(pin.call));
                        }
                    }
                    let held = tool_round::consume_answered_cell(
                        cx,
                        drive.as_mut(),
                        &mut tx,
                        &row,
                        &mut answered_cell,
                        true,
                    )
                    .await?;
                    if let Some(bind) = bind_delivered(drive.as_ref(), &session, &run) {
                        tx.write(bind);
                    }
                    let written = write_run_changes(drive.as_ref(), &mut tx, &session, &run);
                    let (checkpoint, stored) =
                        encode_phase(drive.as_mut(), delta)?.staged_in(&mut tx);
                    tx.write(DomainWrite::Turn(TurnWrite::Advance {
                        session: session.clone(),
                        run: run.clone(),
                        phase: PhaseAdvance::Model {
                            pin: pin.clone(),
                            checkpoint,
                        },
                        iteration: current,
                    }));
                    let released = cx.draining();
                    let committed = if released {
                        cx.commit_draining(tx, label, Release::Ready).await?
                    } else {
                        cx.commit(tx, label).await?
                    };
                    boundary.committed(cx, drive.as_mut()).await?;
                    stored.committed(drive.as_mut());
                    drive.run_changes_committed(&written);
                    drive.phase_committed().await;
                    tool_round::release_cell_emissions(cx, drive.as_mut(), held, label).await?;
                    unsettled = true;
                    if !resent {
                        calls = pin.call;
                    }
                    if released {
                        return Ok(PhaseExit::Released);
                    }
                    // A cancel the turn accepted after the open is read
                    // inside the commit: the call is never sent.
                    if let Some(request) = committed.turn_cancel {
                        let stopped = stopped(drive, &mut carry, &mut answered_cell);
                        return stop(cx, services, &row, Some(request), stopped, heads).await;
                    }
                }
                if !model_call::send(cx, drive.as_mut(), id, request, &admitted, &start).await? {
                    drive.settle_cancelled_answer(cx, id).await?;
                    let request = cx.begin().await?.turn_cancel().cloned();
                    let stopped = stopped(drive, &mut carry, &mut answered_cell);
                    return stop(cx, services, &row, request, stopped, heads).await;
                }
                if matches!(start, model_call::ModelStart::Send { .. })
                    && drive.owed_answer().is_some()
                {
                    // The provider answered: its outcome commits as the
                    // call's receipt, acknowledged, before any stream hook
                    // is finished, any response hook runs, or anything is
                    // admitted, launched or published from it. Every call
                    // takes this path, with or without hooks.
                    let committed = record_answer(cx, drive.as_mut(), &session, &run).await?;
                    // An immediate cancel the turn accepted while the call
                    // answered wins over its derivation; the receipt stops
                    // with the turn.
                    if let Some(request) = committed
                        .turn_cancel
                        .filter(|request| request.mode == crate::TurnCancelMode::Immediate)
                    {
                        turn_cancel::settle_answer(
                            cx,
                            services,
                            turn_cancel::AnsweredPhase::Driven(drive.as_mut(), id),
                        )
                        .await?;
                        let stopped = stopped(drive, &mut carry, &mut answered_cell);
                        return stop(cx, services, &row, Some(request), stopped, heads).await;
                    }
                    drive.derive_answer(cx, id).await?;
                }
            }

            Effect::ExecCode { id, language, code } => {
                let mut tx = cx.begin().await?;
                if turn_cancel::immediate_in(&tx) {
                    let request = tx.turn_cancel().cloned();
                    drop(tx);
                    let stopped = stopped(drive, &mut carry, &mut answered_cell);
                    return stop(cx, services, &row, request, stopped, heads).await;
                }
                if cx.draining() {
                    return Ok(PhaseExit::Drained);
                }
                model = None;
                // `model.done`: the model's answer is the cell the checkpoint
                // re-delivers, committed before the cell starts or with its
                // first save. A restore re-delivers the cell, which resumes
                // from its own latest snapshot, never the model call; the
                // cell its row already names commits nothing again.
                let cell = RunSeq(id.0);
                let named = matches!(row.phase, UnfinishedPhase::Tools { run, .. } if run == cell);
                // The phase that holds the cell retains its first start: a
                // resumed cell keeps the one its phase read back.
                drive.begin_cell(id, &language);
                let mut held = None;
                if !named || carry.is_some() || answered_cell.is_some() {
                    // A turn that owes nothing of an earlier step holds the
                    // response for the cell's first save (FIG-6000).
                    let holds = carry.is_none() && answered_cell.is_none();
                    if let Some(present) = carry.take() {
                        present.write(&mut tx);
                    }
                    let emissions = tool_round::consume_answered_cell(
                        cx,
                        drive.as_mut(),
                        &mut tx,
                        &row,
                        &mut answered_cell,
                        true,
                    )
                    .await?;
                    if let Some(bind) = bind_delivered(drive.as_ref(), &session, &run) {
                        tx.write(bind);
                    }
                    let written = write_run_changes(drive.as_ref(), &mut tx, &session, &run);
                    let (checkpoint, stored) =
                        encode_checkpoint(drive.as_mut())?.staged_in(&mut tx);
                    tx.write(DomainWrite::Turn(TurnWrite::Advance {
                        session: session.clone(),
                        run: run.clone(),
                        phase: PhaseAdvance::Tools {
                            run: cell,
                            checkpoint,
                        },
                        iteration: iteration(drive.machine()),
                    }));
                    if unsettled {
                        settle_model_calls(&mut tx, &session, &run, None);
                    }
                    let response = HeldResponse::holding(
                        tx,
                        turn_cancel::immediate_in,
                        turn_cancel::stops_at_once,
                    );
                    let after = AfterModelDone { stored, written };
                    if holds && drive.hold_response(response.clone()) {
                        held = Some((response, after));
                    } else {
                        response
                            .commit_alone(cx)
                            .await
                            .map_err(TurnError::Durable)?;
                        unsettled = false;
                        after.committed(drive.as_mut()).await;
                        tool_round::release_cell_emissions(
                            cx,
                            drive.as_mut(),
                            emissions,
                            CommitLabel::MODEL_DONE,
                        )
                        .await?;
                    }
                } else {
                    drop(tx);
                }
                let ran = {
                    let cell = drive.exec_cell(cx, id, CodeCell { language, code });
                    let stopped = held.as_ref().map(|(response, _)| response.clone());
                    tokio::select! {
                        biased;
                        ran = turn_cancel::unless_cancelled(cx, cell) => ran?,
                        // The turn's cancel stopped the cell at its first
                        // save, which admitted nothing.
                        () = async {
                            match &stopped {
                                Some(response) => response.stopped().await,
                                None => std::future::pending().await,
                            }
                        } => None,
                    }
                };
                // Where the held response committed: with the cell's first
                // save, or on its own before the cell waited or ran past
                // its first slice. A cell that answered without saving
                // leaves it to the commit that consumes its answer.
                let unconsumed = match held {
                    Some((response, after)) if response.committed_as().is_some() => {
                        unsettled = false;
                        after.committed(drive.as_mut()).await;
                        None
                    }
                    Some((response, _)) => Some(response),
                    None => None,
                };
                match ran {
                    Some(ran) => {
                        // A worker failure before the cell's first save
                        // leaves `model.answer` as the durable phase. Its
                        // retry derives the paid receipt again, without
                        // sending the provider call (FIG-6238).
                        let exit = ran?;
                        if exit == CellExit::Suspended {
                            return Ok(PhaseExit::Suspended { due: None });
                        }
                        // Every answered cell is handed off, tools called
                        // or not: its working state is the turn's from the
                        // commit that consumes its answer.
                        let calls = drive.answered_cell_calls();
                        let held = (drive.answered_cell_end_held(id) || unconsumed.is_some())
                            .then(Vec::new);
                        answered_cell = Some(tool_round::AnsweredCell {
                            id,
                            calls,
                            held,
                            response: unconsumed,
                            close: drive.answered_cell_close(id),
                        });
                    }
                    None => {
                        drive.stop_cell();
                        // A response no commit held leaves the cell
                        // unanchored: it is lost with this execution, and
                        // nothing closes its span (P3).
                        if unconsumed.is_some() {
                            drive.forget_cell();
                        }
                        // The stop records the calls the cell completed:
                        // the cell never answers.
                        let calls = drive.stopped_cell_calls(cx, id).await?;
                        if !calls.is_empty() {
                            answered_cell = Some(tool_round::AnsweredCell {
                                id,
                                calls,
                                held: None,
                                response: None,
                                close: None,
                            });
                        }
                        let request = cx.begin().await?.turn_cancel().cloned();
                        let stopped = stopped(drive, &mut carry, &mut answered_cell);
                        return stop(cx, services, &row, request, stopped, heads).await;
                    }
                }
            }
            Effect::ToolCalls { id, calls, .. } => {
                model = None;
                let checkpoint = encode_checkpoint(drive.as_mut())?;
                let current = iteration(drive.machine());
                let done = tool_round::ModelDone {
                    checkpoint,
                    iteration: current,
                    answered_cell: &mut answered_cell,
                    present: &mut carry,
                    unsettled: &mut unsettled,
                };
                match tool_round::run(cx, drive.as_mut(), &row, id, calls, done).await? {
                    RoundExit::Answered { present, outcome } => {
                        carry = present;
                        boundary.set(outcome);
                    }
                    RoundExit::CancelRequested => {
                        let request = cx.begin().await?.turn_cancel().cloned();
                        let stopped = stopped(drive, &mut carry, &mut answered_cell);
                        return stop(cx, services, &row, request, stopped, heads).await;
                    }
                    RoundExit::Suspended { due } => return Ok(PhaseExit::Suspended { due }),
                }
            }
            Effect::ReportToolCalls { id, completed } => {
                if completed.is_empty() {
                    continue;
                }
                // A protocol refusal is a settled round too. Commit its
                // records with the checkpoint past this report before the
                // machine continues: resume neither loses the refusal nor
                // reports it twice, and no tool body is admitted to run.
                model = None;
                let mut tx = cx.begin().await?;
                if turn_cancel::immediate_in(&tx) {
                    let request = tx.turn_cancel().cloned();
                    drop(tx);
                    let stopped = stopped(drive, &mut carry, &mut answered_cell);
                    return stop(cx, services, &row, request, stopped, heads).await;
                }
                tool_round::record_refused(cx, &mut tx, &row, id, &completed)?;
                if let Some(present) = carry.take() {
                    present.write(&mut tx);
                }
                let held = tool_round::consume_answered_cell(
                    cx,
                    drive.as_mut(),
                    &mut tx,
                    &row,
                    &mut answered_cell,
                    true,
                )
                .await?;
                if let Some(bind) = bind_delivered(drive.as_ref(), &session, &run) {
                    tx.write(bind);
                }
                let written = write_run_changes(drive.as_ref(), &mut tx, &session, &run);
                let (checkpoint, stored) = encode_checkpoint(drive.as_mut())?.staged_in(&mut tx);
                tx.write(DomainWrite::Turn(TurnWrite::Advance {
                    session: session.clone(),
                    run: run.clone(),
                    phase: PhaseAdvance::Tools {
                        run: RunSeq(id.0),
                        checkpoint,
                    },
                    iteration: iteration(drive.machine()),
                }));
                if std::mem::take(&mut unsettled) {
                    settle_model_calls(&mut tx, &session, &run, None);
                }
                cx.commit(tx, CommitLabel::MODEL_DONE).await?;
                stored.committed(drive.as_mut());
                drive.run_changes_committed(&written);
                drive.phase_committed().await;
                tool_round::release_cell_emissions(
                    cx,
                    drive.as_mut(),
                    held,
                    CommitLabel::MODEL_DONE,
                )
                .await?;
                drive
                    .local(cx, Effect::ReportToolCalls { id, completed })
                    .await?;
            }
            Effect::Done {
                messages,
                event_delta,
                protocol_iteration,
            } => {
                // An `Immediate` request the turn accepted after its last
                // step still wins over the commit: the turn backtracks to its
                // last checkpoint, which the next pass restores to commit
                // its stop from (its machine here is finished).
                let mut tx = cx.begin().await?;
                if turn_cancel::immediate_in(&tx) {
                    return Ok(PhaseExit::CancelRequested);
                }
                // The last cell's calls commit with the turn; its finish
                // reports them beside the recorded ones, and drains the
                // turn's activity before the commit (FIG-5507): what the
                // turn held of the cell's emissions goes first.
                // The spans of the turn's cells its commit ends close once
                // it is acknowledged (§3.8): the last cell's, whose answer
                // it consumes, and one that never answered (a cell whose
                // failure ended the turn), which it discards.
                let discarded = drive.discarded_cell(cx).await?;
                let mut cell_closes = Vec::new();
                if let Some(cell) = &mut answered_cell {
                    drive.finishing_cell_calls(&cell.calls);
                    let mut held = cell.take_emissions();
                    cell_closes.extend(held.take_close());
                    tool_round::release_cell_emissions(
                        cx,
                        drive.as_mut(),
                        held,
                        CommitLabel::TURN_COMMIT,
                    )
                    .await?;
                }
                // The turn commits, and its run answers, the outcome the
                // model produced.
                let done = TurnDone {
                    messages,
                    event_delta,
                    protocol_iteration,
                    outcome: outcome.take(),
                    finish_value_schema: accepted_finish_value_schema(drive.machine()),
                };
                let cause = done.run_terminal_cause(&run)?;
                let kind = cause.kind();
                let traced = turn_trace::outcome(services.tracing(), &cause);
                // A frame switch mails its follow-on with its commit.
                let follow_on = match &done.outcome {
                    Some(outcome) => follow_on_mail(
                        &session,
                        outcome,
                        switches,
                        row.admission
                            .trace()
                            .and_then(lash_trace::DurableTraceScope::as_parent),
                    )?,
                    None => None,
                };
                let commit = drive
                    .finish(cx, done, heads.head(cx, &session).await?)
                    .await?;
                if let Some(present) = carry.take() {
                    present.write(&mut tx);
                }
                tool_round::consume_answered_cell(
                    cx,
                    drive.as_mut(),
                    &mut tx,
                    &row,
                    &mut answered_cell,
                    false,
                )
                .await?;
                // The commit settles what the turn's checkpoints delivered:
                // bound to the run first, as the settlement requires.
                if let Some(bind) = bind_delivered(drive.as_ref(), &session, &run) {
                    tx.write(bind);
                }
                super::head_commit::write_session_commit(
                    &mut tx,
                    &session,
                    commit.expected_head,
                    commit.commit,
                );
                tx.write(DomainWrite::Turn(TurnWrite::Terminal {
                    session: session.clone(),
                    run: run.clone(),
                    cause: Box::new(cause),
                    head_revision: Some(commit.expected_head.saturating_add(1)),
                }));
                if let Some(follow_on) = follow_on {
                    tx.write(DomainWrite::Prompt(PromptWrite::ResetPrefix {
                        session: session.clone(),
                    }));
                    tx.write(DomainWrite::SessionMail(follow_on));
                }
                // The turn's scope ends with its commit (L6b): its waits are
                // revoked and its first batch of `Until` children marked; the
                // next pass marks the rest.
                end_turn_scope(cx, &mut tx, &session, &run).await?;
                // A head commit whose content the session store refuses (a
                // node id the session already holds) is refused again on
                // every pass: it is the turn's runtime refusal, and a
                // terminal one ends the run (FIG-5352). One the store refuses
                // for the deployment or the state it holds is the pass's
                // failure: the session parks at the activation-loop budget
                // with the turn and its input kept (FIG-5398).
                let input =
                    turn_trace::input(cx, services.tracing(), &row, &drive.delivered_inputs())
                        .await;
                let released = cx.draining();
                let committed = if released {
                    let more = tx.domain().iter().any(|write| {
                        matches!(
                            write,
                            DomainWrite::Process(
                                lash_durable::domain::ProcessWrite::CascadeBatch {
                                    cursor: Some(_),
                                    ..
                                }
                            )
                        )
                    });
                    let release = if more { Release::Ready } else { Release::Idle };
                    cx.commit_draining(tx, CommitLabel::TURN_COMMIT, release)
                        .await
                } else {
                    cx.commit(tx, CommitLabel::TURN_COMMIT).await
                };
                committed.map_err(|error| {
                    let refusal = match &error {
                        DurableError::Domain(refusal) => refusal.session_commit_refusal(),
                        _ => None,
                    };
                    refusal.map_or(TurnError::Durable(error), TurnError::Runtime)
                })?;
                for close in cell_closes {
                    close.publish();
                }
                if let Some(discarded) = discarded {
                    turn_trace::members_ended(
                        services.tracing(),
                        &row,
                        &discarded.open,
                        turn_trace::MemberEnd::Discarded,
                    );
                    discarded.close.publish();
                }
                turn_trace::ended(
                    services.tracing(),
                    &row,
                    traced,
                    &drive.delivered_inputs(),
                    input,
                    (
                        calls,
                        Some(
                            row.tool_calls.saturating_add(
                                cx.tool_call_admissions()
                                    .saturating_sub(tool_calls_at_start),
                            ),
                        ),
                    ),
                );
                drive.committed().await;
                // The commit moved the head: the next turn loads it again.
                heads.evict();
                return Ok(if released {
                    PhaseExit::Released
                } else {
                    PhaseExit::Committed(kind)
                });
            }
            local => {
                if let Effect::Emit(SessionStreamEvent::TurnOutcome { outcome: ended }) = &local {
                    outcome = Some(ended.clone());
                }
                if boundary.held() && matches!(&local, Effect::Emit(_) | Effect::Log { .. }) {
                    boundary.hold(local);
                } else if let Some(local) = match &mut answered_cell {
                    Some(cell) => cell.hold(local),
                    None => Some(local),
                } {
                    drive.local(cx, local).await?;
                }
            }
        }
    }
}

/// What followed `model.done`'s acknowledgement, kept for whichever commit
/// holds the response: its own, or the cell's first save (FIG-6000).
struct AfterModelDone {
    stored: super::phase_root::PhaseStored,
    written: Vec<lash_durable::domain::TurnNamespaceWrite>,
}

impl AfterModelDone {
    /// The response committed: the phase's content is stored, its run
    /// changes recorded and the working state it carried installed.
    async fn committed(self, drive: &mut dyn TurnDrive) {
        self.stored.committed(drive);
        drive.run_changes_committed(&self.written);
        drive.phase_committed().await;
    }
}

/// `drive` where the turn stopped, with what its next commit owed: within a
/// step, unless the caller says the turn stands between steps.
fn stopped(
    drive: Box<dyn TurnDrive>,
    carry: &mut Option<lash_core_execution::runtime::actor::round::PresentRecord>,
    answered_cell: &mut Option<tool_round::AnsweredCell>,
) -> turn_cancel::StoppedTurn {
    turn_cancel::StoppedTurn {
        drive,
        present: carry.take(),
        cell: answered_cell.take(),
        between_steps: false,
    }
}

/// The turn stopped where it honours a cancel: for the `request` it accepted,
/// its stop commits from `stopped`, the drive that holds it (ADR 0039). With
/// no request, a requested close stopped it, and the close ends the turn.
async fn stop(
    cx: &ActorContext,
    services: &dyn TurnServices,
    row: &TurnRow,
    request: Option<TurnCancelRequest>,
    stopped: turn_cancel::StoppedTurn,
    heads: &mut HeadCache,
) -> Result<PhaseExit, TurnError> {
    let Some(request) = request else {
        return Ok(PhaseExit::CancelRequested);
    };
    turn_cancel::finalize(cx, services, row, &request, Some(stopped), heads).await?;
    Ok(PhaseExit::Cancelled)
}

/// The value schema the turn's accepted finish was validated under, as its
/// candidate kept it from the call's settlement.
fn accepted_finish_value_schema(machine: &crate::TurnMachine) -> Option<crate::JsonSchema> {
    machine
        .decided_completions()
        .iter()
        .find(|decided| decided.disposition == crate::CompletionDisposition::Accepted)
        .and_then(|decided| match &decided.candidate.control {
            crate::TurnControl::Finish { value_schema, .. } => value_schema.clone(),
            crate::TurnControl::SwitchAgentFrame { .. } => None,
        })
}

/// End `row`'s turn with `refusal`, the terminal error its preparation
/// (FIG-5246) or its finish (an after-turn callback's refusal) met: the
/// run's `Refused` terminal, which answers every input it took with the
/// refusal's code and cause, in one `turn.commit`. No turn commits, so the
/// session head does not move; an open round's members settle `Cancelled`
/// and the turn's scope ends, as a cancel's do.
///
/// # Errors
///
/// [`TurnError::Durable`]: ownership lost, or the turn no longer open.
pub(super) async fn refuse(
    cx: &ActorContext,
    tracing: Option<&crate::trace::TraceRuntime>,
    row: &TurnRow,
    refusal: crate::RuntimeError,
) -> Result<(), TurnError> {
    // Budget validation owns the routine decision at DEBUG; this boundary
    // reports the other refusals that unexpectedly ended a turn.
    if !matches!(
        refusal.code,
        crate::RuntimeErrorCode::StoreCommitNodeBudgetExceeded
            | crate::RuntimeErrorCode::StoreCommitByteBudgetExceeded
    ) {
        tracing::warn!(
            session_id = %row.session,
            run_id = %row.run,
            error = %refusal,
            "the turn was refused; the turn ends with the refusal"
        );
    }
    let current = cx.durable_reads()?.turn(&row.session).await?;
    let row = current
        .as_ref()
        .filter(|current| current.run == row.run)
        .unwrap_or(row);
    let mut tx = cx.begin().await?;
    let cancelled = tool_round::cancel_open_round(cx, &mut tx, row).await?;
    let cause = crate::store::RunTerminalCause::Refused {
        refusal: refusal.into(),
    };
    let traced = turn_trace::outcome(tracing, &cause);
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: row.session.clone(),
        run: row.run.clone(),
        cause: Box::new(cause),
        head_revision: None,
    }));
    end_turn_scope(cx, &mut tx, &row.session, &row.run).await?;
    let input = turn_trace::input(cx, tracing, row, &[]).await;
    cx.commit(tx, CommitLabel::TURN_COMMIT).await?;
    turn_trace::members_ended(tracing, row, &cancelled, turn_trace::MemberEnd::Cancelled);
    turn_trace::ended(
        tracing,
        row,
        traced,
        &[],
        input,
        (row.model_calls, Some(row.tool_calls)),
    );
    Ok(())
}

/// Settle `run`'s model calls on `tx` (FIG-5946): release the request of
/// each but `open`, the call `tx` admits. Their snapshots stay. It belongs
/// in every commit that leaves the turn's model phase, and in the one that
/// admits a call after an unsettled one: no settled call is sent again.
pub(super) fn settle_model_calls(
    tx: &mut lash_durable::ActorTx,
    session: &SessionId,
    run: &TurnId,
    open: Option<u32>,
) {
    tx.write(DomainWrite::Prompt(PromptWrite::Settle {
        session: session.clone(),
        run: run.clone(),
        open,
    }));
}

/// Model call `call` of `run`, as its admission is keyed.
fn call_key(session: &crate::SessionId, run: &crate::TurnId, call: u32) -> PromptCallKey {
    PromptCallKey {
        session: session.clone(),
        call: ModelCallId::Turn {
            run: run.clone(),
            ordinal: call,
        },
    }
}

/// The request template admitted call `call` of `run` sends, read back as
/// `model.start` stored it. The inner `Err` is the settlement of a call whose
/// template is gone or does not assemble: it is never sent, and nothing
/// rebuilds it (`AdmittedRequestUnavailable`).
///
/// # Errors
///
/// [`TurnError::Durable`] when the store cannot be read, and
/// [`TurnError::UndecodableState`] when the admission is in a format this
/// build does not decode: the call settles nothing, and its turn stays open.
async fn admitted_send(
    cx: &ActorContext,
    session: &SessionId,
    run: &TurnId,
    call: u32,
) -> Result<Result<AdmittedSend, crate::LlmCallError>, TurnError> {
    let key = &call_key(session, run, call);
    let unavailable = |message: String| crate::LlmCallError {
        message,
        retryable: false,
        kind: crate::ProviderFailureKind::Validation,
        raw: None,
        code: Some(crate::FailureCode::lash(
            crate::TurnFailureCode::AdmittedRequestUnavailable,
        )),
        terminal_reason: crate::LlmTerminalReason::ProviderError,
        request_body: None,
        partial_response: None,
    };
    match load_admitted_call(cx.durable_reads()?, key).await {
        Ok(Some(admitted)) => Ok(Ok(AdmittedSend {
            response: admitted.response(),
            template: admitted.template,
        })),
        Ok(None) => Ok(Err(unavailable(format!(
            "{} has no admitted request template to send",
            key.call
        )))),
        Err(crate::plugin::prompt::AdmittedCallLoadError::Store(error)) => Err(error.into()),
        Err(crate::plugin::prompt::AdmittedCallLoadError::Decode(error)) => {
            Err(TurnError::UndecodableState {
                state: ParkedTurnState::AdmittedCall { call },
                reason: error.to_string(),
            })
        }
        Err(error) => Ok(Err(unavailable(format!(
            "{}'s admitted request template cannot be sent: {error}",
            key.call
        )))),
    }
}
