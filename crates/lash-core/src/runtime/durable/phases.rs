//! The turn's phase runner (ADR 0132 §4). Owned by V0 (FIG-5170), then L3
//! (FIG-5172).
//!
//! The runner polls the turn's machine through its [`TurnDrive`] and commits
//! at the catalog's labels: `model.start` admits a model call before its
//! first byte (ADR 0133 §6): a new call takes the next ordinal of the turn's
//! calls, composes its prompt into its request and is lowered to its exact
//! provider body, and its admission commits its pin, the checkpoint that
//! re-delivers it, the plugin namespaces the turn's run changed since its
//! last commit (the pending checkpoint-callback decisions among them; an
//! unchanged namespace is never written) and its admission record
//! (snapshot and body); a
//! resend is the same call, prepares nothing and sends the stored body. `model.done` commits the
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
//! `turn.commit`. The turn's held terminal publishes only once `turn.commit`
//! is acknowledged ([`TurnDrive::committed`]).

use lash_durable::CommitLabel;
use lash_durable::DomainWrite;
use lash_durable::domain::{
    ModelCallId, PromptCallKey, RunSeq, SessionCommitWrite, SessionMailWrite, TurnWrite,
};

use super::head::HeadCache;
use super::session::{
    CellExit, CodeCell, ComposedCall, OpenTurn, PhaseCheckpoint, PhaseExit, PreparedCall, TurnDone,
    TurnDrive, TurnError, TurnRow, TurnServices, UnfinishedPhase,
};
use super::session_mail::follow_on_mail;
use super::tool_round::{self, RoundExit};
use super::turn_scope::end_turn_scope;
use super::{model_call, turn_cancel};
use crate::plugin::prompt::{admission_record, load_admitted_call};
use crate::{ActorContext, Effect, HostTurnProtocol, SessionStreamEvent, TurnMachine};
use lash_sansio::llm::types::ProviderRequestBody;
use lash_sansio::{SavedTurn, SessionId, TurnId};
use std::sync::Arc;

/// The phase checkpoint of `drive` as it stands: its machine's checkpoint
/// and the input and work its checkpoints delivered.
fn encode_checkpoint(drive: &mut dyn TurnDrive) -> Result<String, TurnError> {
    let saved = drive.machine().checkpoint();
    encode_phase(drive, saved)
}

fn encode_phase(
    drive: &mut dyn TurnDrive,
    saved: SavedTurn<HostTurnProtocol>,
) -> Result<String, TurnError> {
    let checkpoint = PhaseCheckpoint {
        saved,
        delivered: drive.delivered_inputs(),
        delivered_work: drive.delivered_work(),
        before_turn: drive.before_turn(),
    };
    serde_json::to_string(&checkpoint)
        .map_err(|error| TurnError::Exec(format!("the turn checkpoint does not encode: {error}")))
}

/// The plugin namespaces `drive`'s run changed that its rows do not record
/// yet, written into `tx` beside the phase (FIG-5301): what the phase's
/// commit records once it is acknowledged ([`TurnDrive::run_changes_committed`]).
pub(super) fn write_run_changes(
    drive: &dyn TurnDrive,
    tx: &mut lash_durable::ActorTx,
    session: &SessionId,
    run: &TurnId,
) -> Vec<lash_durable::domain::TurnNamespace> {
    let changes = drive.run_changes();
    if !changes.is_empty() {
        tx.write(DomainWrite::Turn(TurnWrite::Namespaces {
            session: session.clone(),
            run: run.clone(),
            namespaces: changes.clone(),
        }));
    }
    changes
}

/// The bind of the steering input and queued turn work `drive`'s
/// checkpoints delivered to its run, written in every phase commit that
/// records the delivery (ADR 0132 §4): the first binds the rows, and a later
/// one finds them bound to the run already. A row no longer open refuses
/// the commit, which the turn then recomputes without it.
fn bind_delivered(drive: &dyn TurnDrive, session: &SessionId, run: &TurnId) -> Option<DomainWrite> {
    let inputs = drive
        .delivered_inputs()
        .iter()
        .flat_map(crate::AdmittedTurnInputs::input_ids)
        .collect::<Vec<_>>();
    let batches = drive
        .delivered_work()
        .iter()
        .flat_map(crate::AdmittedQueuedWork::batch_ids)
        .collect::<Vec<_>>();
    (!inputs.is_empty() || !batches.is_empty()).then(|| {
        DomainWrite::SessionMail(SessionMailWrite::Admit {
            session: session.clone(),
            run: run.clone(),
            inputs,
            batches,
        })
    })
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
    let mut outcome = None;
    // A settled round's presentation, committed with the turn's next commit.
    let mut carry: Option<DomainWrite> = None;
    loop {
        let effect = match pending.take() {
            Some(effect) => effect,
            None => match drive.machine().poll_effect() {
                Some(effect) => effect,
                None => return Err(TurnError::Exec("the turn machine stalled".to_owned())),
            },
        };
        match effect {
            Effect::LlmCall { id, request } => {
                // `model.start`'s open is the call's one read: the turn's
                // accepted cancel and the store's clock come with it.
                let mut tx = cx.begin().await?;
                if tx.turn_cancel().is_some() {
                    return Ok(PhaseExit::CancelRequested);
                }
                if cx.draining() {
                    return Ok(PhaseExit::Drained);
                }
                let current = iteration(drive.machine());
                // `model` is spent: only the call the restored machine
                // re-delivers is the pinned one; every other call is new.
                let (pinned, request, body, admission) = match model.take() {
                    Some((redelivered, pin)) if redelivered == id => {
                        // A resend sends the body its admission stored, read
                        // back byte for byte; nothing prepares it again.
                        let key = call_key(&session, &run, pin.call);
                        match admitted_body(cx, &key).await? {
                            Ok(body) => (Some(pin), request, body, None),
                            Err(unavailable) => {
                                settle_unsent(drive.as_mut(), id, unavailable);
                                continue;
                            }
                        }
                    }
                    _ => {
                        // A new call is prepared over the turn's committed
                        // state: composed, decided and lowered to its exact
                        // body. The machine waits on the request that
                        // carries it, so the checkpoint names it.
                        let call = calls.saturating_add(1);
                        match drive.prepare_call(cx, id, call, request).await? {
                            PreparedCall::Admit(composed) => {
                                let ComposedCall {
                                    request,
                                    prompt,
                                    body,
                                } = *composed;
                                if !drive.machine().admit_request(id, Arc::clone(&request)) {
                                    return Err(TurnError::Exec(format!(
                                        "the turn machine does not wait on model call {id:?}"
                                    )));
                                }
                                let record = admission_record(
                                    call_key(&session, &run, call),
                                    prompt.as_ref(),
                                    &body,
                                    None,
                                )
                                .map_err(|error| {
                                    TurnError::Exec(format!(
                                        "the model call's admission does not encode: {error}"
                                    ))
                                })?;
                                (None, request, body, Some(record))
                            }
                            PreparedCall::Unsent(refused) => {
                                settle_unsent(drive.as_mut(), id, refused);
                                continue;
                            }
                            PreparedCall::Ended => continue,
                        }
                    }
                };
                // The checkpoint `model.start` commits names the request by
                // content digest; its pin reuses that digest.
                let saved = drive.machine().checkpoint();
                let start = model_call::start(
                    &services.execution_budgets(&session),
                    tx.opened_at(),
                    row.turn_deadline,
                    pinned,
                    calls.saturating_add(1),
                    model_call::request_ref(&saved.checkpoint)?,
                )?;
                if let model_call::ModelStart::Send { pin, resent, .. } = &start {
                    // `model.start` admits the call: its identity and pin,
                    // the checkpoint that re-delivers its request, the
                    // plugin state the turn published, the pending
                    // checkpoint-callback decisions among it, and its
                    // admission record (its prompt snapshot and exact
                    // body), in one transaction. A resend commits its next
                    // attempt and records nothing.
                    let label = tool_round::model_start_label(&carry);
                    if let Some(present) = carry.take() {
                        tx.write(present);
                    }
                    if let Some(record) = admission {
                        tx.write(record);
                    }
                    if let Some(bind) = bind_delivered(drive.as_ref(), &session, &run) {
                        tx.write(bind);
                    }
                    let written = write_run_changes(drive.as_ref(), &mut tx, &session, &run);
                    tx.write(DomainWrite::Turn(TurnWrite::Advance {
                        session: session.clone(),
                        run: run.clone(),
                        phase: UnfinishedPhase::Model {
                            pin: pin.clone(),
                            checkpoint: encode_phase(drive.as_mut(), saved)?,
                        },
                        iteration: current,
                    }));
                    cx.commit(tx, label).await?;
                    drive.run_changes_committed(&written);
                    if !resent {
                        calls = pin.call;
                    }
                }
                let sent = turn_cancel::unless_cancelled(
                    cx,
                    &session,
                    model_call::send(cx, drive.as_mut(), id, request, &body, &start),
                )
                .await?;
                match sent {
                    Some(sent) => sent?,
                    None => return Ok(PhaseExit::CancelRequested),
                }
            }
            Effect::ExecCode { id, language, code } => {
                let mut tx = cx.begin().await?;
                if turn_cancel::immediate_in(&tx) {
                    return Ok(PhaseExit::CancelRequested);
                }
                if cx.draining() {
                    return Ok(PhaseExit::Drained);
                }
                model = None;
                // `model.done`: the model's answer is the cell the checkpoint
                // re-delivers, committed before the cell starts. A restore
                // re-delivers the cell, which resumes from its own latest
                // snapshot, never the model call; the cell its row already
                // names commits nothing again.
                let cell = RunSeq(id.0);
                let named = matches!(row.phase, UnfinishedPhase::Tools { run, .. } if run == cell);
                if !named || carry.is_some() {
                    if let Some(present) = carry.take() {
                        tx.write(present);
                    }
                    if let Some(bind) = bind_delivered(drive.as_ref(), &session, &run) {
                        tx.write(bind);
                    }
                    let written = write_run_changes(drive.as_ref(), &mut tx, &session, &run);
                    tx.write(DomainWrite::Turn(TurnWrite::Advance {
                        session: session.clone(),
                        run: run.clone(),
                        phase: UnfinishedPhase::Tools {
                            run: cell,
                            checkpoint: encode_checkpoint(drive.as_mut())?,
                        },
                        iteration: iteration(drive.machine()),
                    }));
                    cx.commit(tx, CommitLabel::MODEL_DONE).await?;
                    drive.run_changes_committed(&written);
                }
                let cell = drive.exec_cell(cx, id, CodeCell { language, code });
                match turn_cancel::unless_cancelled(cx, &session, cell).await? {
                    Some(ran) => {
                        if ran? == CellExit::Suspended {
                            return Ok(PhaseExit::Suspended { due: None });
                        }
                    }
                    None => {
                        drive.stop_cell();
                        return Ok(PhaseExit::CancelRequested);
                    }
                }
            }
            Effect::ToolCalls { id, calls, .. } => {
                model = None;
                let checkpoint = encode_checkpoint(drive.as_mut())?;
                let current = iteration(drive.machine());
                match tool_round::run(cx, drive.as_mut(), &row, id, calls, checkpoint, current)
                    .await?
                {
                    RoundExit::Answered(present) => carry = present,
                    RoundExit::CancelRequested => return Ok(PhaseExit::CancelRequested),
                    RoundExit::Suspended { due } => return Ok(PhaseExit::Suspended { due }),
                }
            }
            Effect::AwaitToolResults { .. } => {
                return Err(TurnError::Exec(
                    "a durable turn's checkpoint keeps its round's calls; no dispatch state settles them"
                        .to_owned(),
                ));
            }
            Effect::Done {
                messages,
                event_delta,
                protocol_iteration,
            } => {
                // An `Immediate` request the turn accepted after its last
                // step still wins over the commit: the turn backtracks.
                let mut tx = cx.begin().await?;
                if turn_cancel::immediate_in(&tx) {
                    return Ok(PhaseExit::CancelRequested);
                }
                let done = TurnDone {
                    messages,
                    event_delta,
                    protocol_iteration,
                    outcome: outcome.take(),
                };
                let cause = done.run_terminal_cause(&run)?;
                let kind = cause.kind();
                // A frame switch mails its follow-on with its commit.
                let follow_on = match &done.outcome {
                    Some(outcome) => follow_on_mail(&session, outcome)?,
                    None => None,
                };
                let commit = drive
                    .finish(cx, done, heads.head(cx, &session).await?)
                    .await?;
                if let Some(present) = carry.take() {
                    tx.write(present);
                }
                // The commit settles what the turn's checkpoints delivered:
                // bound to the run first, as the settlement requires.
                if let Some(bind) = bind_delivered(drive.as_ref(), &session, &run) {
                    tx.write(bind);
                }
                tx.write(DomainWrite::SessionCommit(SessionCommitWrite {
                    session: session.clone(),
                    expected_head: commit.expected_head,
                    commit_json: commit.commit_json,
                }));
                tx.write(DomainWrite::Turn(TurnWrite::Terminal {
                    session: session.clone(),
                    run: run.clone(),
                    cause: Box::new(cause),
                    head_revision: Some(commit.expected_head.saturating_add(1)),
                }));
                if let Some(follow_on) = follow_on {
                    tx.write(DomainWrite::SessionMail(follow_on));
                }
                // The turn's scope ends with its commit (L6b): its waits are
                // revoked and its first batch of `Until` children marked; the
                // next pass marks the rest.
                end_turn_scope(cx, &mut tx, &session, &run).await?;
                cx.commit(tx, CommitLabel::TURN_COMMIT).await?;
                drive.committed().await;
                // The commit moved the head: the next turn loads it again.
                heads.evict();
                return Ok(PhaseExit::Committed(kind));
            }
            local => {
                if let Effect::Emit(SessionStreamEvent::TurnOutcome { outcome: ended }) = &local {
                    outcome = Some(ended.clone());
                }
                drive.local(cx, local).await?;
            }
        }
    }
}

/// End `row`'s turn with `refusal`, the terminal error its preparation
/// met (FIG-5246): the run's `Refused` terminal, which answers every input it
/// took with the refusal's code and cause, in one `turn.commit`. Nothing ran,
/// so the session head does not move; an open round's members settle
/// `Cancelled` and the turn's scope ends, as a cancel's do.
///
/// # Errors
///
/// [`TurnError::Durable`]: ownership lost, or the turn no longer open.
pub(super) async fn refuse(
    cx: &ActorContext,
    row: &TurnRow,
    refusal: crate::RuntimeError,
) -> Result<(), TurnError> {
    tracing::warn!(
        session = %row.session,
        run = %row.run,
        error = %refusal,
        "the turn's preparation was refused; the turn ends with the refusal"
    );
    let mut tx = cx.begin().await?;
    tool_round::cancel_open_round(cx, &mut tx, row).await?;
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: row.session.clone(),
        run: row.run.clone(),
        cause: Box::new(crate::store::RunTerminalCause::Refused {
            code: refusal.code,
            message: refusal.message,
            refusal_cause: refusal.cause,
        }),
        head_revision: None,
    }));
    end_turn_scope(cx, &mut tx, &row.session, &row.run).await?;
    cx.commit(tx, CommitLabel::TURN_COMMIT).await?;
    Ok(())
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

/// The exact body admitted call `key` sends, read back as `model.start`
/// stored it. The inner `Err` is the settlement of a call whose body is
/// gone or does not assemble: it is never sent, and nothing rebuilds it
/// (`AdmittedRequestUnavailable`).
///
/// # Errors
///
/// [`TurnError::Durable`] when the store cannot be read.
async fn admitted_body(
    cx: &ActorContext,
    key: &PromptCallKey,
) -> Result<Result<ProviderRequestBody, crate::LlmCallError>, TurnError> {
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
        Ok(Some(admitted)) => Ok(Ok(admitted.body)),
        Ok(None) => Ok(Err(unavailable(format!(
            "{} has no admitted body to send",
            key.call
        )))),
        Err(crate::plugin::prompt::AdmittedCallLoadError::Store(error)) => Err(error.into()),
        Err(error) => Ok(Err(unavailable(format!(
            "{}'s admitted body cannot be sent: {error}",
            key.call
        )))),
    }
}

/// Hand `drive`'s machine the settlement of a model call it never sent.
pub(super) fn settle_unsent(
    drive: &mut dyn TurnDrive,
    id: crate::EffectId,
    error: crate::LlmCallError,
) {
    drive
        .machine()
        .handle_response(crate::Response::LlmComplete {
            id,
            result: Err(error),
            text_streamed: false,
        });
}
