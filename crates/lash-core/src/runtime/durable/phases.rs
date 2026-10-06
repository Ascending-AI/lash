//! The turn's phase runner (ADR 0132 §4). Owned by V0 (FIG-5170), then L3
//! (FIG-5172).
//!
//! The runner polls the turn's machine through its [`TurnDrive`] and commits
//! at the catalog's labels: `model.start` pins a model call before its first
//! byte, the cell's own commits carry the turn's rows, and `turn.commit`
//! publishes the session head's next revision with the turn's terminal in
//! one fenced transaction. Everything between commits is in memory and is
//! recomputed from committed state after a crash; nothing re-executes
//! orchestration to reach a recorded outcome.

use lash_durable::CommitLabel;
use lash_durable::DomainWrite;
use lash_durable::domain::{CellId, ExecKey, RunSeq, SessionCommitWrite, TurnWrite};

use super::session::{
    CodeCell, OpenTurn, PhaseExit, TurnDone, TurnDrive, TurnError, TurnPhase, TurnServices,
};
use super::turn_scope::end_turn_scope;
use super::{model_call, turn_cancel};
use crate::{ActorContext, Effect, SessionStreamEvent, TurnMachine};

fn encode_checkpoint(machine: &TurnMachine) -> Result<String, TurnError> {
    serde_json::to_string(&machine.checkpoint())
        .map_err(|error| TurnError::Exec(format!("the turn checkpoint does not encode: {error}")))
}

fn iteration(machine: &TurnMachine) -> u32 {
    u32::try_from(machine.protocol_iteration()).unwrap_or(u32::MAX)
}

/// Run the turn's phases from `turn`, committing at each label, until it
/// commits, suspends or loses ownership.
///
/// # Errors
///
/// [`TurnError`].
pub async fn run_phases(
    cx: &ActorContext,
    services: &dyn TurnServices,
    turn: OpenTurn,
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
    let mut model = match row.phase {
        TurnPhase::Model { .. } => row.model.clone().map(|pin| (row.iteration, pin)),
        _ => None,
    };
    let mut outcome = None;
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
                let current = iteration(drive.machine());
                let pinned = match model.take() {
                    Some((pinned, pin)) if pinned == current => Some(pin),
                    _ => None,
                };
                let start = model_call::start(
                    &services.execution_budgets(&session),
                    cx.durable_now().await?,
                    row.turn_deadline,
                    pinned,
                    &request,
                )?;
                if let model_call::ModelStart::Send { pin, .. } = &start {
                    let mut tx = cx.begin().await?;
                    tx.write(DomainWrite::Turn(TurnWrite::Advance {
                        session: session.clone(),
                        run: run.clone(),
                        phase: TurnPhase::Model {
                            attempt: pin.attempt,
                        },
                        iteration: current,
                        checkpoint_ref: Some(encode_checkpoint(drive.machine())?),
                        model: Some(pin.clone()),
                    }));
                    cx.commit(tx, CommitLabel::MODEL_START).await?;
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
                if turn_cancel::requested(cx, &session).await?.is_some() {
                    return Ok(PhaseExit::CancelRequested);
                }
                model = None;
                let exec = ExecKey::Cell(
                    session.clone(),
                    run.clone(),
                    CellId::new(format!("e{}", id.0)),
                );
                let with = vec![DomainWrite::Turn(TurnWrite::Advance {
                    session: session.clone(),
                    run: run.clone(),
                    phase: TurnPhase::Tools { run: RunSeq(id.0) },
                    iteration: iteration(drive.machine()),
                    checkpoint_ref: Some(encode_checkpoint(drive.machine())?),
                    model: None,
                })];
                drive
                    .exec_cell(cx, id, exec, CodeCell { language, code }, with)
                    .await?;
            }
            Effect::ToolCalls { .. } | Effect::AwaitToolResults { .. } => {
                return Err(TurnError::Exec(
                    "tool rounds are L4's (FIG-5174) on the durable path".to_owned(),
                ));
            }
            Effect::Done {
                messages,
                event_delta,
                protocol_iteration,
            } => {
                let commit = drive
                    .finish(
                        cx,
                        TurnDone {
                            messages,
                            event_delta,
                            protocol_iteration,
                            outcome: outcome.take(),
                        },
                    )
                    .await?;
                let terminal = commit.terminal;
                let mut tx = cx.begin().await?;
                tx.write(DomainWrite::SessionCommit(SessionCommitWrite {
                    session: session.clone(),
                    run: run.clone(),
                    expected_head: commit.expected_head,
                    commit_json: commit.commit_json,
                }));
                tx.write(DomainWrite::Turn(TurnWrite::Terminal {
                    session: session.clone(),
                    run: run.clone(),
                    terminal,
                    cause_json: commit.cause_json,
                    head_revision: Some(commit.expected_head.saturating_add(1)),
                }));
                // The turn's scope ends with its commit (L6b): its waits are
                // revoked and its first batch of `Until` children marked; the
                // next pass marks the rest.
                end_turn_scope(cx, &mut tx, &session, &run).await?;
                cx.commit(tx, CommitLabel::TURN_COMMIT).await?;
                return Ok(PhaseExit::Committed(terminal));
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
