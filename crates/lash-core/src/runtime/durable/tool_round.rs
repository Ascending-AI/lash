//! A turn's tool round on the durable path (ADR 0132 §5). Owned by L4
//! (FIG-5174); the phase runner (L3) calls it from its tool arm and commits
//! the presentation it carries.
//!
//! - **Admission.** A round the rows do not hold yet is admitted in the
//!   `model.done` transaction: the turn's `Tools` phase with the checkpoint
//!   that re-delivers the round's calls, and `round::admit_round` with an
//!   `x_start` for every member. No body runs before that commit, so a model
//!   stream that never committed can never launch a tool.
//! - **Refusal.** A round past the session's tool-call limit is admitted
//!   with every member settled on that typed refusal in the same commit: no
//!   body runs, and a resume answers from those outcomes. No round is
//!   refused for a tool's configuration: a misconfigured tool cannot be
//!   registered, so the catalog never holds one.
//! - **Resume.** A round the rows hold is resumed from its fold: the
//!   re-delivered calls must be the ones it admitted, a started `Once`
//!   without an outcome is `Interrupted`, a started `Repeatable` reruns at
//!   its ordinal, and nothing it recorded runs again.
//! - **Trace admission.** Each call's trace scope is retained by the
//!   round's admission, whose commit selects its candidate. The admission
//!   owes each traced call's admission export until an owner records that
//!   it exported them (`round.traced`): the admitting owner does once its
//!   candidates are selected, and an owner that resumes a round without the
//!   record reconciles the exports first, as the admitting owner may have
//!   lost its life or the commit's acknowledgement before it selected them.
//!   A process's exporter dedupes an identity only in its own memory, so a
//!   resume of a round whose exports are recorded exports nothing: a node
//!   that takes the round over never exports a call's admission again
//!   (FIG-5382, FIG-5395, FIG-5452).
//! - **Cancel.** An `Immediate` cancel the turn accepted before the round's
//!   admission admits none of it, and one it accepts while the round runs
//!   ends its unfinished members `Cancelled`; the turn then ends as every
//!   phase does on a cancel ([`RoundExit::CancelRequested`]). An `AfterStep`
//!   request lets the round run to its end.
//! - **Suspension.** A round whose unsettled members are all parked on a
//!   wait or waiting out a retry's backoff runs nothing: once it stayed hot
//!   for `idle_evict` it suspends ([`RoundExit::Suspended`]), and the
//!   session releases as `waiting` until the earliest due, holding no claim
//!   slot. The wait's resolution, or the due time, re-claims it, and the
//!   restored turn resumes the round from its fold.
//! - **Presentation.** The machine is answered with each member's committed
//!   outcome, in declared order. The round's `present` record rides the
//!   turn's next commit: `round.present+model.start` when the machine calls
//!   the model next, `turn.commit` when it finishes.

use std::sync::Arc;

use lash_core_execution::runtime::actor::round::{
    self, AdmittedRound, RoundCalls, RoundDraft, RoundEnd, RoundError, RoundRunner, RoundTools,
};
use lash_core_store::effect_opener::EffectOpener;
use lash_durable::domain::{DomainWrite, OwnerKey, RunSeq, TurnWrite};
use lash_durable::{CommitLabel, DurableError, DurableInstant};
use tokio_util::sync::CancellationToken;

use super::session::{CellToolCalls, TurnDrive, TurnError, TurnRow, UnfinishedPhase};
use super::turn_cancel;
use crate::sansio::PendingToolCall;
use crate::{ActorContext, EffectId, Response};

/// How a tool round left the turn.
#[derive(Debug)]
pub(super) enum RoundExit {
    /// The machine is answered. The round's presentation commits with the
    /// turn's next commit.
    Answered(Option<DomainWrite>),
    /// The turn accepted a cancel: the round's unfinished members settled
    /// `Cancelled`, and the next pass finalizes the turn.
    CancelRequested,
    /// Nothing in the round runs and only parked waits or retry dues remain:
    /// the session releases as `waiting` until `due`.
    Suspended {
        /// The earliest retry due time or parked wait deadline.
        due: Option<DurableInstant>,
    },
}

/// Commit the record that `round`'s traced calls' admissions are exported,
/// discharging the export its admission owes (FIG-5452).
async fn record_trace_exported(cx: &ActorContext, round: &AdmittedRound) -> Result<(), TurnError> {
    let mut tx = cx.begin().await?;
    round::record_trace_exported(&mut tx, round);
    cx.commit(tx, CommitLabel::ROUND_TRACED).await?;
    Ok(())
}

/// The label of the turn's next model call: `round.present+model.start` when
/// it carries a round's presentation.
pub(super) fn model_start_label(carry: &Option<DomainWrite>) -> CommitLabel {
    if carry.is_some() {
        CommitLabel::ROUND_PRESENT_MODEL_START
    } else {
        CommitLabel::MODEL_START
    }
}

fn exec(error: impl std::fmt::Display) -> TurnError {
    TurnError::Exec(error.to_string())
}

fn round_error(error: RoundError) -> TurnError {
    match error {
        RoundError::Durable(error) => TurnError::Durable(error),
        RoundError::Stopped => TurnError::Exec("the activation stopped during a round".to_owned()),
        other => exec(other),
    }
}

/// What a round's `model.done` commits beside its admission.
pub(super) struct ModelDone<'a> {
    /// The turn's checkpoint, re-delivering the round.
    pub(super) checkpoint: String,
    /// The protocol iteration the turn stands in.
    pub(super) iteration: u32,
    /// The cell whose answer the turn has yet to record, if any.
    pub(super) answered_cell: &'a mut Option<AnsweredCell>,
    /// The presentation of an earlier round of the same step, which the
    /// turn has yet to commit: a held control call's round follows its
    /// siblings' (FIG-5781).
    pub(super) present: &'a mut Option<DomainWrite>,
}

/// Run the tool round of effect `id` over `calls` to its members' outcomes,
/// admitting it in `model.done` unless the rows already hold it, and answer
/// the machine. `done` is what `model.done` commits beside the admission.
///
/// # Errors
///
/// [`TurnError`]: ownership lost, or the round's rows or calls refused.
pub(super) async fn run(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    row: &TurnRow,
    id: EffectId,
    calls: Vec<PendingToolCall>,
    done: ModelDone<'_>,
) -> Result<RoundExit, TurnError> {
    let ModelDone {
        checkpoint,
        iteration,
        answered_cell,
        present,
    } = done;
    let session = row.session.clone();
    let owner = OwnerKey::Turn(session.clone(), row.run.clone());
    let opener = EffectOpener::turn(session.clone(), row.run.clone());
    let run = RunSeq(id.0);
    let tools: Arc<dyn RoundTools> = drive.tools()?;
    let policies = tools.policies();
    if calls.is_empty() {
        drive.machine().handle_response(Response::ToolResults {
            id,
            results: Vec::new(),
        });
        return Ok(RoundExit::Answered(None));
    }
    let reads = cx.durable_reads()?;
    let rows: Vec<_> = reads
        .run_records(&owner)
        .await?
        .into_iter()
        .filter(|stored| stored.run == run)
        .collect();
    let waits = round::PinnedWaits::read(reads, &rows).await?;
    let folded = round::fold(&rows, &policies, &waits).map_err(exec)?;
    let bodies = Arc::new(RoundCalls::new(Arc::clone(&tools), &calls));
    let runner = match folded.round(run) {
        Some(view) => {
            let drafts: Vec<_> = view.members().iter().map(|member| member.draft()).collect();
            round::require_admitted(&opener, run, &drafts, &calls).map_err(exec)?;
            let owed = view.owed_trace_exports();
            if !owed.is_empty() {
                for scope in owed {
                    tools.export_admitted(scope);
                }
                let admitted = folded
                    .admitted_round(run)
                    .ok_or_else(|| exec(DurableError::Store(missing_round(run))))?;
                record_trace_exported(cx, &admitted).await?;
            }
            RoundRunner::resumed(cx, owner.clone(), run, policies, bodies)
        }
        None => {
            // The admission's open is its one read: the turn's accepted
            // cancel and the store's clock come with it. An `AfterStep`
            // request lets the step's round run; the turn honours it
            // before its next model call.
            let mut tx = cx.begin().await?;
            if turn_cancel::immediate_in(&tx) {
                return Ok(RoundExit::CancelRequested);
            }
            let now_ms = u64::try_from(tx.opened_at().0).unwrap_or(0);
            // Each call's trace admission is proposed with the round's: the
            // admission record retains its scope, which every later owner
            // reads back, and the candidate is selected once that record
            // commits (FIG-5382).
            let mut candidates = Vec::new();
            let members = calls
                .iter()
                .map(|call| {
                    let draft = round::call_draft(&opener, call, tools.pin(call, now_ms))?;
                    let proposal = tools.propose_trace(call);
                    let trace = proposal.map(|proposal| {
                        candidates.push(proposal.candidate);
                        proposal.scope
                    });
                    Ok::<_, round::RoundCallsRefusal>(draft.with_trace(trace))
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(exec)?;
            let refused = tools.refusal(&calls);
            if let Some(present) = present.take() {
                tx.write(present);
            }
            record_answered_cell(cx, &mut tx, row, answered_cell)?;
            let written = super::phases::write_run_changes(&*drive, &mut tx, &session, &row.run);
            tx.write(DomainWrite::Turn(TurnWrite::Advance {
                session: session.clone(),
                run: row.run.clone(),
                phase: UnfinishedPhase::Tools { run, checkpoint },
                iteration,
            }));
            let admitted: AdmittedRound = round::admit_round(
                &mut tx,
                &lash_core_execution::runtime::actor::waits::execution_wait_scope(cx, &owner)?,
                RoundDraft {
                    owner: owner.clone(),
                    run,
                    members,
                },
            )
            .map_err(exec)?;
            if let Some(refused) = &refused {
                for (member, answer) in admitted.members().iter().zip(refused) {
                    let output = round::completed_material(&opener, answer).map_err(exec)?;
                    round::settle(
                        &mut tx,
                        member,
                        round::SettledOutput::Completed(output),
                        Vec::new(),
                    )
                    .map_err(exec)?;
                }
            }
            // A commit whose acknowledgement was lost may have landed: its
            // candidates are deferred, and the owner that resumes the round
            // reconciles their admissions if so.
            let exports = candidates
                .iter()
                .any(|candidate| candidate.anchor().context().is_some());
            let committed = cx.commit(tx, CommitLabel::MODEL_DONE).await;
            for candidate in candidates {
                super::session::settle_trace_admission(candidate, &committed);
            }
            committed?;
            drive.run_changes_committed(&written);
            // The candidates are selected: their exports are discharged
            // before any body runs, so an owner that takes the round over
            // exports none again.
            if exports {
                record_trace_exported(cx, &admitted).await?;
            }
            if refused.is_some() {
                RoundRunner::resumed(cx, owner.clone(), run, policies, bodies)
            } else {
                RoundRunner::admitted(cx, &admitted, policies, bodies)
                    .ok_or_else(|| exec("an admitted round has no members"))?
            }
        }
    };

    // Run the round, watching the turn's row for a cancel whenever mail
    // may have arrived: an accepted `Immediate` request cancels the
    // unfinished members, which then settle.
    let cancel = CancellationToken::new();
    let running = runner.cancelled_by(cancel.clone()).run();
    tokio::pin!(running);
    let end = loop {
        tokio::select! {
            biased;
            end = &mut running => break end.map_err(round_error)?,
            () = cx.wait_for_mail(), if !cancel.is_cancelled() => {
                if turn_cancel::immediate(cx).await? {
                    cancel.cancel();
                }
            }
        }
    };
    if cancel.is_cancelled() {
        return Ok(RoundExit::CancelRequested);
    }
    let end = match end {
        RoundEnd::Settled(end) => end,
        RoundEnd::Suspended { due } => return Ok(RoundExit::Suspended { due }),
    };

    let view = end.round();
    let results = calls
        .iter()
        .zip(view.members())
        .map(|(call, member)| {
            let output = member
                .outcome()
                .ok_or_else(|| exec(format!("call {} ended without an outcome", call.call_id)))?;
            Ok(tools.completed(call, output))
        })
        .collect::<Result<Vec<_>, TurnError>>()?;
    let admitted = end
        .fold()
        .admitted_round(run)
        .ok_or_else(|| exec(DurableError::Store(missing_round(run))))?;
    let carry = round::presentation(&admitted, end.fold()).1;
    drive
        .machine()
        .handle_response(Response::ToolResults { id, results });
    Ok(RoundExit::Answered(carry))
}

/// Record calls the protocol already refused, in the transaction that
/// checkpoints past their report. The ordinary settled-round fold retains
/// their requests and typed answers; no catalog pin, wait or body runs.
pub(super) fn record_refused(
    cx: &ActorContext,
    tx: &mut lash_durable::ActorTx,
    row: &TurnRow,
    id: EffectId,
    completed: &[round::CompletedCall],
) -> Result<(), TurnError> {
    let opener = EffectOpener::turn(row.session.clone(), row.run.clone());
    let members = completed
        .iter()
        .map(|answer| {
            let call = PendingToolCall {
                call_id: answer.call_id.clone(),
                provider_call_id: answer.provider_call_id.clone(),
                tool_name: answer.tool_name.clone(),
                args: answer.args.clone(),
                replay: answer.replay.clone(),
            };
            round::call_draft(
                &opener,
                &call,
                round::MemberPin {
                    tool: crate::ToolId::new(answer.tool_name.clone()),
                    policy: crate::ExecutionPolicy::Once,
                    limit: crate::ExecutionLimit {
                        expires_at: u64::try_from(tx.opened_at().0).unwrap_or(0),
                        max_slice: std::time::Duration::ZERO,
                    },
                    park: None,
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(exec)?;
    let admitted = round::admit_round(
        tx,
        &lash_core_execution::runtime::actor::waits::wait_scope(cx)?,
        RoundDraft {
            owner: OwnerKey::Turn(row.session.clone(), row.run.clone()),
            run: RunSeq(id.0),
            members,
        },
    )
    .map_err(exec)?;
    for (member, answer) in admitted.members().iter().zip(completed) {
        let output = round::completed_material(&opener, answer).map_err(exec)?;
        round::settle(
            tx,
            member,
            round::SettledOutput::Completed(output),
            Vec::new(),
        )
        .map_err(exec)?;
    }
    Ok(())
}

/// The tool calls of a cell the turn has yet to record: one that answered
/// the machine, which the turn's next commit records, or one the turn
/// stopped on, which the commit that ends the turn records.
#[derive(Debug)]
pub(super) struct AnsweredCell {
    /// The cell's effect.
    pub(super) id: EffectId,
    /// Its tool calls, as the code executor's bound keeps them.
    pub(super) calls: CellToolCalls,
}

/// The tool a cell round's last member is recorded under when the code
/// executor's bound left calls out: the member's answer is their
/// [`OmittedToolCalls`](crate::OmittedToolCalls) accounting, not a call.
const OMITTED_CELL_CALLS: &str = "cell-omitted:";

/// The accounting a cell round's member holds of the calls its bound left
/// out, when `tool` and `answer` are such a member's.
pub(super) fn omitted_cell_calls(
    tool: &crate::ToolId,
    answer: &round::CompletedCall,
) -> Option<crate::OmittedToolCalls> {
    if tool.as_str() != OMITTED_CELL_CALLS {
        return None;
    }
    let crate::ToolCallOutcome::Success(value) = &answer.output.outcome else {
        return None;
    };
    serde_json::from_value(value.to_json_value()).ok()
}

/// Record the tool calls of the cell `answered` names as a settled round of
/// the turn at the cell's effect, in the transaction that checkpoints past
/// the cell or ends the turn on it (FIG-5330). The cell's own records of
/// them are pruned as it advances, and its answer and its snapshot are their
/// last copies: recorded here, the turn's settled-round fold reports them
/// beside its other rounds, in call order. A crash before this commit
/// re-delivers the cell, which answers with them again from its snapshot.
///
/// The round is bounded by the code executor
/// ([`bound_tool_call_records`](crate::plugin::CodeExecutorPlugin::bound_tool_call_records)):
/// it holds one member for each record the bound keeps, with the output the
/// bound cut, and one more for the accounting of the calls it left out.
pub(super) fn record_answered_cell(
    cx: &ActorContext,
    tx: &mut lash_durable::ActorTx,
    row: &TurnRow,
    answered: &mut Option<AnsweredCell>,
) -> Result<(), TurnError> {
    let Some(AnsweredCell { id, calls }) = answered.take() else {
        return Ok(());
    };
    let CellToolCalls { calls, omitted } = calls;
    let mut completed = calls
        .into_iter()
        .map(|record| round::CompletedCall {
            model_return: crate::ModelToolReturn::from_output(record.tool.clone(), &record.output),
            call_id: record.call_id,
            provider_call_id: record.provider_call_id,
            tool_name: record.tool,
            args: record.args,
            output: record.output,
            intent_outcomes: Vec::new(),
            replay: None,
        })
        .collect::<Vec<_>>();
    if let Some(omitted) = omitted {
        let root = crate::ToolCallRoot::turn(row.run.as_str()).map_err(exec)?;
        let output = crate::ToolCallOutput::success(serde_json::to_value(&omitted).map_err(exec)?);
        completed.push(round::CompletedCall {
            model_return: crate::ModelToolReturn::from_output(
                OMITTED_CELL_CALLS.to_owned(),
                &output,
            ),
            call_id: crate::ToolCallId::derive(
                "",
                root,
                &[crate::ToolCallPosition::EffectOrdinal(id.0)],
            ),
            provider_call_id: None,
            tool_name: OMITTED_CELL_CALLS.to_owned(),
            args: serde_json::Value::Null,
            output,
            intent_outcomes: Vec::new(),
            replay: None,
        });
    }
    if completed.is_empty() {
        return Ok(());
    }
    record_refused(cx, tx, row, id, &completed)
}

fn missing_round(run: RunSeq) -> lash_durable::StoreFailure {
    lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: format!("the settled round {run:?} is not in its fold"),
    }
}

/// Settle the open members of `row`'s round `Cancelled` on `tx`, when the
/// turn is in its `Tools` phase: what a turn cancel leaves of a round whose
/// owner stopped, or crashed, before its members settled.
///
/// # Errors
///
/// [`TurnError`] when the round's rows cannot be read or do not fold.
pub(super) async fn cancel_open_round(
    cx: &ActorContext,
    tx: &mut lash_durable::ActorTx,
    row: &TurnRow,
) -> Result<(), TurnError> {
    let UnfinishedPhase::Tools { run, .. } = row.phase else {
        return Ok(());
    };
    let owner = OwnerKey::Turn(row.session.clone(), row.run.clone());
    round::settle_cancelled(cx, tx, &owner, run)
        .await
        .map_err(round_error)
}
