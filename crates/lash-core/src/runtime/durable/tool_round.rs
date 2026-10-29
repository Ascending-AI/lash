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
//!   round's admission, its span derived from the turn's, so every owner
//!   that reads the admission back names the same span. A call's span
//!   closes once, on the owner whose acknowledged commit wrote its outcome,
//!   at the end that commit retained: an owner that resumes a round closes
//!   no member another owner settled (FIG-5382, P1).
//! - **Cancel.** An `Immediate` cancel the turn accepted before the round's
//!   admission admits none of it, and one it accepts while the round runs
//!   ends its unfinished members `Cancelled`. Each commit of members'
//!   answers reads the request in its own transaction, so a member that
//!   answers after it was accepted publishes nothing, its state included,
//!   whether or not its token fired first (FIG-5381); the turn then ends as every
//!   phase does on a cancel ([`RoundExit::CancelRequested`]). An `AfterStep`
//!   request lets the round run to its end.
//! - **Suspension.** A round whose unsettled members are all parked on a
//!   wait or waiting out a retry's backoff runs nothing: once it stayed hot
//!   for `idle_evict` it suspends ([`RoundExit::Suspended`]), and the
//!   session releases as `waiting` until the earliest due, holding no claim
//!   slot. The wait's resolution, or the due time, re-claims it, and the
//!   restored turn resumes the round from its fold.
//! - **Presentation.** Recorded outcomes answer the machine in declared order.
//!   Final plain answers may prepare the next provider call provisionally,
//!   when no callback or VM consumes them first: outcomes, presentation and
//!   model admission commit together as `round.outcome+present+model.start`.
//!   Their observations and emissions stay held until that commit succeeds.
//!   Otherwise `present` rides the next `round.present+model.start` or
//!   `turn.commit`, after the required recorded-outcome boundary.

use std::sync::Arc;

use lash_core_execution::runtime::actor::round::{
    self, AdmittedRound, DeferredOutcome, PreparedRoundEnd, RoundCalls, RoundDraft, RoundEnd,
    RoundError, RoundRunner, RoundTools,
};
use lash_core_store::effect_opener::EffectOpener;
use lash_durable::domain::{DomainWrite, OwnerKey, RunSeq, TurnWrite};
use lash_durable::{CommitLabel, DurableError, DurableInstant};
use tokio_util::sync::CancellationToken;

use super::session::{CellClose, CellToolCalls, TurnDrive, TurnError, TurnRow, UnfinishedPhase};
use super::turn_cancel;
use crate::ActorContext;
use crate::Effect;
use crate::Response;
use crate::sansio::PendingToolCall;
use lash_sansio::EffectId;

/// How a tool round left the turn.
#[derive(Debug)]
pub(super) enum RoundExit {
    /// The machine is answered. The round's presentation commits with the
    /// turn's next commit.
    Answered {
        present: Option<lash_core_execution::runtime::actor::round::PresentRecord>,
        outcome: Option<Box<DeferredOutcome>>,
    },
    /// The turn accepted a cancel: its stop settles the round's unfinished
    /// members `Cancelled` as it ends the turn.
    CancelRequested,
    /// Nothing in the round runs and only parked waits or retry dues remain:
    /// the session releases as `waiting` until `due`.
    Suspended {
        /// The earliest retry due time or parked wait deadline.
        due: Option<DurableInstant>,
    },
}

/// The label of the turn's next model call: `round.present+model.start` when
/// it carries a round's presentation.
pub(super) fn model_start_label(
    carry: &Option<lash_core_execution::runtime::actor::round::PresentRecord>,
) -> CommitLabel {
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
    pub(super) checkpoint: super::phase_root::StagedPhase,
    pub(super) reads: Vec<round::ModelReadPlan>,
    pub(super) tools: Arc<dyn RoundTools>,
    /// The protocol iteration the turn stands in.
    pub(super) iteration: u32,
    /// The cell whose answer the turn has yet to record, if any.
    pub(super) answered_cell: &'a mut Option<AnsweredCell>,
    /// The presentation of an earlier round of the same step, which the
    /// turn has yet to commit: a held control call's round follows its
    /// siblings' (FIG-5781).
    pub(super) present: &'a mut Option<lash_core_execution::runtime::actor::round::PresentRecord>,
    /// Whether the call that asked for the round is unsettled: the
    /// `model.done` that admits the round settles it, and clears this
    /// (FIG-5946).
    pub(super) unsettled: &'a mut bool,
    /// A callback-free receipt consumed by this admission. Kept until the
    /// commit acknowledges it, so a failed admission records it on exit.
    pub(super) receipt: &'a mut Option<super::phases::StagedReceipt>,
}

/// Bind a fresh model round once. Restored rounds use their saved plan.
pub(super) fn bind_reads(
    drive: &mut dyn TurnDrive,
    tools: &dyn RoundTools,
    row: &TurnRow,
    id: EffectId,
    calls: &mut [PendingToolCall],
) -> Result<Vec<round::ModelReadPlan>, TurnError> {
    if matches!(row.phase, UnfinishedPhase::Tools { run, .. } if run == RunSeq(id.0)) {
        return Ok(Vec::new());
    }
    let caller = crate::ToolReadCaller::ModelRound {
        round_key: serde_json::to_string(&(&row.session, &row.run, id.0)).map_err(exec)?,
    };
    let mut ids = std::collections::BTreeMap::new();
    let mut plans = Vec::new();
    for (slot, call) in calls.iter_mut().enumerate() {
        if let Some(mut plan) = tools.plan_read(call, caller.clone(), plans.len() as u64 + 1) {
            plan.slot = slot as u64;
            ids.insert(call.call_id.clone(), plan.call_id.clone());
            call.call_id = plan.call_id.clone();
            plans.push(plan);
        }
    }
    drive.machine().bind_tool_call_ids(&ids);
    Ok(plans)
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
        reads: planned_reads,
        tools,
        iteration,
        answered_cell,
        present,
        unsettled,
        receipt,
    } = done;
    let session = row.session.clone();
    let owner = OwnerKey::Turn(session.clone(), row.run.clone());
    let opener = EffectOpener::turn(session.clone(), row.run.clone());
    let run = RunSeq(id.0);
    let policies = tools.policies();
    if calls.is_empty() {
        drive.machine().handle_response(Response::ToolResults {
            id,
            results: Vec::new(),
        });
        return Ok(RoundExit::Answered {
            present: None,
            outcome: None,
        });
    }
    let reads = cx.durable_reads()?;
    // Only a restore at this round can have admitted it. New rounds get
    // their facts from the admission's acknowledged postimage below.
    let rows = if matches!(row.phase, UnfinishedPhase::Tools { run: held, .. } if held == run) {
        reads.run_facts_of(&owner, run).await?
    } else {
        lash_durable::domain::RunFacts::default()
    };
    let saved_rows = rows.clone();
    let waits = round::PinnedWaits::read(reads, &rows).await?;
    let folded = round::fold(&rows, &policies, &waits).map_err(exec)?;
    // Each member's attempt reads the turn's accepted stop before its answer
    // escapes, so an answer decided after the request was accepted is the
    // stop's, whether or not this owner fired its token yet (FIG-5381).
    let stop: round::AcceptedStop = {
        let cx = cx.clone();
        Arc::new(move || {
            let cx = cx.clone();
            Box::pin(async move { turn_cancel::immediate(&cx).await.unwrap_or(false) })
        })
    };
    let mut planned_reads = folded
        .round(run)
        .map_or(planned_reads, |view| view.reads().to_vec());
    let all_calls = calls;
    let recorded_calls = (!planned_reads.is_empty()).then(|| {
        all_calls
            .iter()
            .filter(|call| {
                !planned_reads
                    .iter()
                    .any(|plan| plan.call_id == call.call_id)
            })
            .cloned()
            .collect::<Vec<_>>()
    });
    let calls = recorded_calls.as_deref().unwrap_or(&all_calls);
    for plan in &planned_reads {
        let call = all_calls
            .get(plan.slot as usize)
            .filter(|call| call.call_id == plan.call_id)
            .ok_or_else(|| exec("a saved read plan names another model slot"))?;
        if round::request_material(&opener, call).map_err(exec)? != plan.request {
            return Err(exec("a saved model read request changed"));
        }
    }
    let bodies = Arc::new(RoundCalls::new(Arc::clone(&tools), calls).stopped_by(stop));
    let runner = match folded.round(run) {
        Some(view) => {
            let drafts: Vec<_> = view.members().iter().map(|member| member.draft()).collect();
            round::require_admitted(&opener, run, &drafts, calls).map_err(exec)?;
            // A member that will run a body again runs it only on a node
            // that serves its recorded entry's tool; one with an outcome or a
            // park replays without its provider.
            let runnable = view
                .members()
                .iter()
                .filter(|member| {
                    member.outcome().is_none()
                        && !matches!(member.state(), round::MemberState::Waiting { .. })
                })
                .filter_map(|member| calls.iter().find(|call| &call.call_id == member.call()))
                .collect::<Vec<_>>();
            if let Some(drift) = tools.drift(&runnable) {
                return Err(TurnError::Runtime(drift));
            }
            RoundRunner::resumed(cx, owner.clone(), run, policies, bodies)
                .with_committed_facts(rows, waits)
        }
        None => {
            // A call whose recorded entry's tool is missing on this node
            // parks the turn before anything of the round is admitted (S5).
            if let Some(drift) = tools.drift(&calls.iter().collect::<Vec<_>>()) {
                return Err(TurnError::Runtime(drift));
            }
            // The admission's open is its one read: the turn's accepted
            // cancel and the store's clock come with it. An `AfterStep`
            // request lets the step's round run; the turn honours it
            // before its next model call.
            let mut tx = cx.begin().await?;
            if turn_cancel::immediate_in(&tx) {
                return Ok(RoundExit::CancelRequested);
            }
            let now_ms = u64::try_from(tx.opened_at().0).unwrap_or(0);
            // Each call's trace scope is admitted with the round's: the
            // admission record retains it, and every later owner reads it
            // back and names the call's span from it (FIG-5382).
            let members = calls
                .iter()
                .map(|call| {
                    let draft = round::call_draft(&opener, call, tools.pin(call, now_ms))?;
                    Ok::<_, round::RoundCallsRefusal>(draft.with_trace(tools.trace_scope(call)))
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(exec)?;
            let refused = tools.refusal(&all_calls);
            if let Some(refused) = &refused {
                for plan in &mut planned_reads {
                    if let Some(answer) =
                        refused.iter().find(|answer| answer.call_id == plan.call_id)
                        && let crate::ToolCallOutcome::Failure(failure) = &answer.output.outcome
                    {
                        plan.refusal = Some(failure.clone());
                    }
                }
            }
            if let Some(present) = present.take() {
                present.write(&mut tx);
            }
            let held = consume_answered_cell(cx, drive, &mut tx, row, answered_cell, true).await?;
            let written =
                super::phases::write_run_changes(&mut *drive, &mut tx, &session, &row.run);
            let (checkpoint, stored) = checkpoint.staged_in(&mut tx);
            tx.write(DomainWrite::Turn(TurnWrite::Advance {
                session: session.clone(),
                run: row.run.clone(),
                phase: lash_durable::domain::PhaseAdvance::Tools { run, checkpoint },
                iteration,
            }));
            if *unsettled {
                super::phases::settle_model_calls(&mut tx, &session, &row.run, None);
            }
            let admitted: AdmittedRound = round::admit_model_round(
                &mut tx,
                &lash_core_execution::runtime::actor::waits::execution_wait_scope(cx, &owner)?,
                RoundDraft {
                    owner: owner.clone(),
                    run,
                    members,
                },
                planned_reads.clone(),
            )
            .map_err(exec)?;
            if let Some(refused) = &refused {
                for member in admitted.members() {
                    let answer = refused
                        .iter()
                        .find(|answer| &answer.call_id == member.call())
                        .ok_or_else(|| exec("a refusal lost a recorded slot"))?;
                    let output =
                        round::completed_material(&opener, answer.clone()).map_err(exec)?;
                    round::settle(
                        &mut tx,
                        member,
                        lash_core_store::tool_run::SettledOutput::Completed(output),
                        Vec::new(),
                        None,
                    )
                    .map_err(exec)?;
                }
            }
            let facts =
                lash_durable::domain::RunFacts::appended(tx.domain(), tx.graph(), tx.epoch())
                    .of_run(run);
            super::session_publication::commit(
                cx,
                drive.code_executor().as_ref(),
                &session,
                tx,
                CommitLabel::MODEL_DONE,
                None,
            )
            .await?;
            // The checkpoint consumed the response and its sealed call
            // record. No raw receipt root survives the model phase.
            receipt.take();
            *unsettled = false;
            let waits = round::PinnedWaits::read_admitted(reads, &admitted).await?;
            stored.committed(&mut *drive);
            drive.run_changes_committed(&written);
            drive.phase_committed(cx).await?;
            release_cell_emissions(cx, drive, held, CommitLabel::MODEL_DONE).await?;
            if refused.is_some() {
                // The admission's commit wrote the refused members'
                // outcomes: their spans close on this owner.
                RoundRunner::resumed(cx, owner.clone(), run, policies, bodies)
                    .settled_here(admitted.members())
                    .with_committed_facts(facts, waits)
            } else if admitted.members().is_empty() {
                RoundRunner::resumed(cx, owner.clone(), run, policies, bodies)
                    .with_committed_facts(facts, waits)
            } else {
                RoundRunner::admitted(cx, &admitted, policies, bodies)
                    .ok_or_else(|| exec("an admitted round has no members"))?
                    .with_committed_facts(facts, waits)
            }
        }
    };

    // Run the round, watching the turn's row for a cancel whenever mail
    // may have arrived: an accepted `Immediate` request cancels the
    // unfinished members, which then settle.
    // The transaction that commits a member's answer reads the turn's
    // accepted stop itself: an answer that lands after the request was
    // accepted, before this owner saw it, publishes nothing (FIG-5381).
    // A final batch of plain answers rides the turn's next output, whatever
    // plugins the session installs: everything that prepares that output
    // reads the turn's live state, never the round's records (FIG-6162).
    let cancel = CancellationToken::new();
    let running = runner
        .cancelled_by(cancel.clone())
        .fenced_by(Arc::new(turn_cancel::immediate_in))
        .run_or_prepare();
    tokio::pin!(running);
    let end = tokio::select! {
        biased;
        end = &mut running => end.map_err(round_error)?,
        stopped = turn_cancel::wait_for_immediate(cx) => {
            stopped?;
            cancel.cancel();
            running.await.map_err(round_error)?
        }
    };
    if cancel.is_cancelled() {
        return Ok(RoundExit::CancelRequested);
    }
    let (end, outcome) = match end {
        PreparedRoundEnd::Recorded(RoundEnd::Settled(end)) => (end, None),
        PreparedRoundEnd::Recorded(RoundEnd::Suspended { due }) => {
            return Ok(RoundExit::Suspended { due });
        }
        PreparedRoundEnd::Prepared { round, outcome } => (round, Some(outcome)),
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
    let (results, carry) = if planned_reads.is_empty() {
        (results, carry)
    } else {
        let Some(ReadPresentation { results, present }) = Box::pin(
            ModelReads {
                tools: tools.as_ref(),
                all_calls: &all_calls,
                planned_reads: &planned_reads,
                view,
                saved_rows: &saved_rows,
                cancel: &cancel,
            }
            .present(cx, results, carry),
        )
        .await?
        else {
            return Ok(RoundExit::CancelRequested);
        };
        (results, present)
    };
    drive
        .machine()
        .handle_response(Response::ToolResults { id, results });
    Ok(RoundExit::Answered {
        present: carry,
        outcome,
    })
}

struct ReadPresentation {
    results: Vec<round::CompletedCall>,
    present: Option<round::PresentRecord>,
}

struct ModelReads<'a> {
    tools: &'a dyn RoundTools,
    all_calls: &'a [PendingToolCall],
    planned_reads: &'a [round::ModelReadPlan],
    view: &'a round::RoundView,
    saved_rows: &'a lash_durable::domain::RunFacts,
    cancel: &'a CancellationToken,
}

impl ModelReads<'_> {
    async fn present(
        self,
        cx: &ActorContext,
        results: Vec<round::CompletedCall>,
        present: Option<round::PresentRecord>,
    ) -> Result<Option<ReadPresentation>, TurnError> {
        let Self {
            tools,
            all_calls,
            planned_reads,
            view,
            saved_rows,
            cancel,
        } = self;
        let mut results: std::collections::BTreeMap<_, _> = results
            .into_iter()
            .map(|result| (result.call_id.clone(), result))
            .collect();
        let mut read_nodes = Vec::new();
        let mut staging = lash_core_store::checkpoint_graph::GraphStaging::new(
            lash_core_store::checkpoint_graph::EncodeBudget::new(
                lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
            ),
        );
        for (index, plan) in planned_reads.iter().enumerate() {
            let call = &all_calls[plan.slot as usize];
            let saved = if view.presented().is_some() {
                let node = *view
                    .read_facts()
                    .get(index)
                    .ok_or_else(|| exec("a saved model round lost a read fact"))?;
                read_nodes.push(node);
                round::graph::load_model_read(
                    &mut lash_core_store::checkpoint_graph::GraphDecoder::new(
                        saved_rows,
                        saved_rows.read_limits,
                    ),
                    node,
                )
                .map_err(exec)?
            } else {
                let reading = tools.read(call, plan, cancel);
                tokio::pin!(reading);
                let answer = tokio::select! {
                    biased;
                    stopped = turn_cancel::wait_for_immediate(cx) => {
                        stopped?;
                        cancel.cancel();
                        return Ok(None);
                    }
                    () = cx.cancel().cancelled() => return Err(exec("the activation stopped during a model read")),
                    answer = &mut reading => answer.map_err(exec)?,
                };
                let presentation = tools
                    .present_read(call, plan, &answer.fact)
                    .await
                    .map_err(exec)?;
                let saved = round::ModelReadResult {
                    fact: answer.fact,
                    presentation,
                };
                let node = round::graph::stage_model_read(&mut staging, &saved).map_err(exec)?;
                read_nodes.push(node);
                saved
            };
            results.insert(call.call_id.clone(), saved.completed(call));
        }
        let results = all_calls
            .iter()
            .map(|call| {
                results
                    .remove(&call.call_id)
                    .ok_or_else(|| exec("a model round lost a result slot"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let present = present.map(|carry| carry.with_reads(read_nodes).with_staging(staging));
        Ok(Some(ReadPresentation { results, present }))
    }
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
                    admitted_at: tx.opened_at(),
                    tool: lash_vocabulary::tool_contract::ToolId::new(answer.tool_name.clone()),
                    policy: lash_kernel_value::tool_contract::ExecutionPolicy::Once,
                    limit: crate::ExecutionLimit {
                        expires_at: u64::try_from(tx.opened_at().0).unwrap_or(0),
                        max_slice: std::time::Duration::ZERO,
                    },
                    park: None,
                    declaration: None,
                    binding: None,
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
        let output = round::completed_material(&opener, answer.clone()).map_err(exec)?;
        round::settle(
            tx,
            member,
            lash_core_store::tool_run::SettledOutput::Completed(output),
            Vec::new(),
            None,
        )
        .map_err(exec)?;
    }
    Ok(())
}

/// A cell the turn has yet to record: one that answered the machine, whose
/// tool calls and working state the turn's next commit records, or one the
/// turn stopped on, whose tool calls the commit that ends the turn records.
#[derive(Debug)]
pub(super) struct AnsweredCell {
    /// The cell's effect.
    pub(super) id: EffectId,
    /// Its tool calls, as the code executor's bound keeps them.
    pub(super) calls: CellToolCalls,
    /// What the turn emitted from the answer of a cell whose end is held
    /// in memory (FIG-5999), kept for the commit that consumes the answer:
    /// a successor that loses that commit runs the cell to its end again.
    /// `None` for a cell whose end is recorded.
    pub(super) held: Option<Vec<Effect>>,
    /// The model response that asked for the cell, which no save of the
    /// cell carried (FIG-6000): the commit that consumes the cell's answer
    /// commits it, and what the cell published waits for that commit. A
    /// successor that loses it sends the model call again.
    pub(super) response: Option<crate::HeldResponse>,
    /// The close of the span of a cell that answered: published once the
    /// commit that consumes the answer is acknowledged (P1). `None` for a
    /// cell the turn stopped on, which the terminal discards, and for one
    /// nothing observes.
    pub(super) close: Option<CellClose>,
}

/// What the turn held of an answered cell's publications for the commit
/// that consumes its answer: its own, kept while its model response was
/// held (FIG-6000), then the emissions the turn derived from its answer.
#[derive(Default)]
pub(super) struct CellEmissions {
    response: Option<crate::HeldResponse>,
    effects: Vec<Effect>,
    close: Option<CellClose>,
}

impl CellEmissions {
    /// The close of the cell's span, taken, for a commit that ends the turn:
    /// the turn's activity is published before that commit, and the cell's
    /// close only after its acknowledgement (P1).
    pub(super) fn take_close(&mut self) -> Option<CellClose> {
        self.close.take()
    }
}

impl AnsweredCell {
    /// What the turn held of the cell's publications, taken.
    pub(super) fn take_emissions(&mut self) -> CellEmissions {
        CellEmissions {
            response: self.response.take(),
            effects: self.held.take().unwrap_or_default(),
            close: self.close.take(),
        }
    }

    /// Keeps `effect` for the commit that consumes the cell's answer, when
    /// the cell's end is held and `effect` is an emission; else hands it
    /// back.
    pub(super) fn hold(&mut self, effect: Effect) -> Option<Effect> {
        match &mut self.held {
            Some(held) if matches!(&effect, Effect::Emit(_) | Effect::Log { .. }) => {
                held.push(effect);
                None
            }
            _ => Some(effect),
        }
    }
}

/// Publish what the turn held of an answered cell's publications
/// ([`CellEmissions`]), for the commit labelled `label` that consumes the
/// answer: after it, or before a commit that ends the turn, whose finish
/// drains the turn's activity first. The cell's span closes last; a commit
/// that ends the turn takes its close first ([`CellEmissions::take_close`])
/// and publishes it after its acknowledgement.
pub(super) async fn release_cell_emissions(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    held: CellEmissions,
    label: CommitLabel,
) -> Result<(), TurnError> {
    if let Some(response) = held.response {
        response.committed(label);
    }
    for effect in held.effects {
        drive.local(cx, effect).await?;
    }
    if let Some(close) = held.close {
        close.publish();
    }
    Ok(())
}

/// Retain references to the cell's original ledger with the transaction
/// consuming its answer (FIG-6171). A crash before it re-delivers the cell;
/// no body is copied into the turn and no presentation limit applies.
pub(super) fn report_answered_cell(
    tx: &mut lash_durable::ActorTx,
    row: &TurnRow,
    answered: &mut Option<AnsweredCell>,
) -> Result<(), TurnError> {
    let Some(AnsweredCell { id, calls, .. }) = answered.take() else {
        return Ok(());
    };
    let CellToolCalls {
        mut ledger,
        read_facts,
        ..
    } = calls;
    if !read_facts.is_empty() {
        let mut staging = lash_core_store::checkpoint_graph::GraphStaging::new(
            lash_core_store::checkpoint_graph::EncodeBudget::new(
                lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
            ),
        );
        ledger.reads = read_facts
            .iter()
            .map(|fact| round::graph::stage_read_fact(&mut staging, fact))
            .collect::<Result<_, _>>()
            .map_err(exec)?;
        tx.stage_all(staging);
    }
    if ledger.entries.is_empty() && ledger.reads.is_empty() {
        return Ok(());
    }
    round::report_cell(
        tx,
        &OwnerKey::Turn(row.session.clone(), row.run.clone()),
        RunSeq(id.0),
        &ledger,
    )
    .map_err(exec)
}

/// Record the cell `answered` names, in the transaction that consumes its
/// answer: its report ([`report_answered_cell`]), and its handoff
/// ([`TurnDrive::consume_cell`](super::session::TurnDrive::consume_cell)):
/// with `working`, the session's state after it as the turn's working
/// state, and the retirement of its ended root. The phase's commit is then
/// answered with
/// [`TurnDrive::phase_committed`](super::session::TurnDrive::phase_committed),
/// and then with the emissions this answers, which the turn held for it
/// ([`release_cell_emissions`]).
pub(super) async fn consume_answered_cell(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    tx: &mut lash_durable::ActorTx,
    row: &TurnRow,
    answered: &mut Option<AnsweredCell>,
    working: bool,
) -> Result<CellEmissions, TurnError> {
    let Some(cell) = answered.as_mut() else {
        return Ok(CellEmissions::default());
    };
    let id = cell.id;
    let held = cell.take_emissions();
    if let Some(writes) = held.response.as_ref().and_then(crate::HeldResponse::writes) {
        // A cell that answered before saving still owes its model response
        // and execution registration. Keep that draft before the writes that
        // consume it, retaining the fresh transaction's mailbox/cancel read.
        let consuming = tx.take_domain();
        tx.append(writes)?;
        for write in consuming {
            tx.write(write);
        }
    }
    report_answered_cell(tx, row, answered)?;
    drive.consume_cell(cx, tx, id, working).await?;
    Ok(held)
}

fn missing_round(run: RunSeq) -> lash_durable::StoreFailure {
    lash_durable::StoreFailure {
        code: None,
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: format!("the settled round {run:?} is not in its fold"),
    }
}

/// Settle the open members of `row`'s round `Cancelled` on `tx`, when the
/// turn is in its `Tools` phase: what a turn cancel leaves of a round whose
/// owner stopped, or crashed, before its members settled. Returns the
/// members it settled, whose spans the terminal's owner closes once `tx`'s
/// commit is acknowledged ([`super::turn_trace::members_ended`]).
///
/// # Errors
///
/// [`TurnError`] when the round's rows cannot be read or do not fold.
pub(super) async fn cancel_open_round(
    cx: &ActorContext,
    tx: &mut lash_durable::ActorTx,
    row: &TurnRow,
) -> Result<Vec<(round::ExecutionDraft, lash_trace::TraceToolName)>, TurnError> {
    let UnfinishedPhase::Tools { run, .. } = row.phase else {
        return Ok(Vec::new());
    };
    let owner = OwnerKey::Turn(row.session.clone(), row.run.clone());
    let members = round::settle_cancelled(cx, tx, &owner, run)
        .await
        .map_err(round_error)?;
    Ok(named_members(cx, row, members).await)
}

/// The open members of `row`'s round, when the turn is in its `Tools`
/// phase: what a terminal that settles nothing, the session close's
/// cancel, discards.
///
/// # Errors
///
/// [`TurnError`] when the round's rows cannot be read or do not fold.
pub(super) async fn open_round_members(
    cx: &ActorContext,
    row: &TurnRow,
) -> Result<Vec<(round::ExecutionDraft, lash_trace::TraceToolName)>, TurnError> {
    let UnfinishedPhase::Tools { run, .. } = row.phase else {
        return Ok(Vec::new());
    };
    let owner = OwnerKey::Turn(row.session.clone(), row.run.clone());
    let members = round::unsettled(cx, &owner, run)
        .await
        .map_err(round_error)?;
    Ok(named_members(cx, row, members).await)
}

/// Trace labels come from the run's committed definitions, never from a
/// ToolId presented as a name. An undecodable or absent phase still permits
/// cancellation: tracing uses the typed identity when its name cannot be read.
async fn named_members(
    cx: &ActorContext,
    row: &TurnRow,
    members: Vec<round::ExecutionDraft>,
) -> Vec<(round::ExecutionDraft, lash_trace::TraceToolName)> {
    if !members.iter().any(|member| member.trace().is_some()) {
        return Vec::new();
    }
    let projection = cx.session_projection().await.ok().flatten();
    let current = projection.as_ref().is_some_and(|projection| {
        projection.turn.as_ref().is_some_and(|turn| {
            turn.run == row.run && turn.phase.checkpoint() == row.phase.checkpoint()
        })
    });
    let retained = projection
        .as_ref()
        .filter(|_| current)
        .and_then(|projection| projection.phase.clone())
        .filter(|phase| Some(phase.root) == row.phase.checkpoint());
    let read_class = match (&projection, current) {
        (Some(_), true) => lash_core_ids::operational_metrics::sql::OwnerReadClass::Warm,
        (Some(_), false) => lash_core_ids::operational_metrics::sql::OwnerReadClass::Historical,
        (None, _) => lash_core_ids::operational_metrics::sql::OwnerReadClass::Recovery,
    };
    let surface = match retained {
        Some(phase) => phase.surface,
        None => lash_core_ids::operational_metrics::sql::classify_owner_reads(read_class, async {
            match cx.durable_reads() {
                Ok(_reads) => {
                    match super::phase_root::read_plugin_phase(
                        cx.backend().durable().as_ref(),
                        &row.session,
                        &row.run,
                    )
                    .await
                    {
                        Ok(phase) => phase
                            .and_then(|phase| phase.surface)
                            .map(std::sync::Arc::new),
                        Err(error) => {
                            tracing::warn!(%error, "terminal tool trace names could not be read");
                            None
                        }
                    }
                }
                Err(_) => None,
            }
        })
        .await,
    };
    members
        .into_iter()
        .filter(|member| member.trace().is_some())
        .map(|member| {
            let name = surface
                .as_ref()
                .and_then(|surface| surface.entry_for(member.tool()))
                .map(|entry| lash_trace::TraceToolName::Name(entry.definition.manifest.tool()))
                .unwrap_or_else(|| lash_trace::TraceToolName::Identity {
                    tool_id: member.tool().clone(),
                });
            (member, name)
        })
        .collect()
}
