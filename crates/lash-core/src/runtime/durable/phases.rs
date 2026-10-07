//! The turn's phase runner (ADR 0132 §4). Owned by V0 (FIG-5170), then L3
//! (FIG-5172).
//!
//! The runner polls the turn's machine through its [`TurnDrive`] and commits
//! at the catalog's labels: `model.start` pins a model call before its first
//! byte, `model.done` commits the phase that re-delivers a round or a cell
//! before it starts, and `turn.commit` publishes the next revision of the
//! head the owner cached with the turn's terminal in one fenced
//! transaction, the store's compare-and-set against that head. Everything
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
use lash_durable::domain::{RunSeq, SessionCommitWrite, TurnWrite};

use super::head::HeadCache;
use super::session::{
    CodeCell, OpenTurn, PhaseExit, TurnDone, TurnDrive, TurnError, TurnServices, UnfinishedPhase,
};
use super::tool_round::{self, RoundExit};
use super::turn_scope::end_turn_scope;
use super::{model_call, turn_cancel};
use crate::{ActorContext, Effect, HostTurnProtocol, SessionStreamEvent, TurnMachine};
use lash_sansio::SavedTurn;

fn encode_checkpoint(machine: &TurnMachine) -> Result<String, TurnError> {
    encode_saved(&machine.checkpoint())
}

fn encode_saved(saved: &SavedTurn<HostTurnProtocol>) -> Result<String, TurnError> {
    serde_json::to_string(saved)
        .map_err(|error| TurnError::Exec(format!("the turn checkpoint does not encode: {error}")))
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
    // The model call in flight as the rows left it: a re-delivered call is
    // its next attempt, under its recorded deadline.
    let mut model = row.phase.model().map(|pin| (row.iteration, pin.clone()));
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
                if turn_cancel::requested(cx, &session).await?.is_some() {
                    return Ok(PhaseExit::CancelRequested);
                }
                if cx.draining() {
                    return Ok(PhaseExit::Drained);
                }
                let current = iteration(drive.machine());
                let pinned = match model.take() {
                    Some((pinned, pin)) if pinned == current => Some(pin),
                    _ => None,
                };
                // The checkpoint `model.start` commits names the request by
                // content digest; its pin reuses that digest.
                let saved = drive.machine().checkpoint();
                let start = model_call::start(
                    &services.execution_budgets(&session),
                    cx.durable_now().await?,
                    row.turn_deadline,
                    pinned,
                    model_call::request_ref(&saved.checkpoint)?,
                )?;
                if let model_call::ModelStart::Send { pin, .. } = &start {
                    let label = tool_round::model_start_label(&carry);
                    let mut tx = cx.begin().await?;
                    if let Some(present) = carry.take() {
                        tx.write(present);
                    }
                    tx.write(DomainWrite::Turn(TurnWrite::Advance {
                        session: session.clone(),
                        run: run.clone(),
                        phase: UnfinishedPhase::Model {
                            pin: pin.clone(),
                            checkpoint: encode_saved(&saved)?,
                        },
                        iteration: current,
                    }));
                    cx.commit(tx, label).await?;
                }
                // `model` is spent: only the first call after a restore
                // re-delivers the pinned one, and a later call of the same
                // iteration is a new call.
                let sent = turn_cancel::unless_cancelled(
                    cx,
                    &session,
                    model_call::send(cx, drive.as_mut(), id, request, &start),
                )
                .await?;
                match sent {
                    Some(sent) => sent?,
                    None => return Ok(PhaseExit::CancelRequested),
                }
            }
            Effect::ExecCode { id, language, code } => {
                if turn_cancel::immediate(cx, &session).await? {
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
                    let mut tx = cx.begin().await?;
                    if let Some(present) = carry.take() {
                        tx.write(present);
                    }
                    tx.write(DomainWrite::Turn(TurnWrite::Advance {
                        session: session.clone(),
                        run: run.clone(),
                        phase: UnfinishedPhase::Tools {
                            run: cell,
                            checkpoint: encode_checkpoint(drive.machine())?,
                        },
                        iteration: iteration(drive.machine()),
                    }));
                    cx.commit(tx, CommitLabel::MODEL_DONE).await?;
                }
                let cell = drive.exec_cell(cx, id, CodeCell { language, code });
                match turn_cancel::unless_cancelled(cx, &session, cell).await? {
                    Some(ran) => ran?,
                    None => {
                        drive.stop_cell();
                        return Ok(PhaseExit::CancelRequested);
                    }
                }
            }
            Effect::ToolCalls { id, calls, .. } => {
                model = None;
                let checkpoint = encode_checkpoint(drive.machine())?;
                let current = iteration(drive.machine());
                match tool_round::run(cx, drive.as_mut(), &row, id, calls, checkpoint, current)
                    .await?
                {
                    RoundExit::Answered(present) => carry = present,
                    RoundExit::CancelRequested => return Ok(PhaseExit::CancelRequested),
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
                if turn_cancel::immediate(cx, &session).await? {
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
                let commit = drive
                    .finish(cx, done, heads.head(cx, &session).await?)
                    .await?;
                let mut tx = cx.begin().await?;
                if let Some(present) = carry.take() {
                    tx.write(present);
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
