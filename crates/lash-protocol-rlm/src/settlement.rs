//! Settlement and continuation policy shared by both RLM channels.
//! Response admission and committed assistant history belong to each channel.

use lash_core::sansio::{CheckpointResumeAction, PendingWork};
use lash_core::session_model::{
    ConversationRecord, Message, MessageRole, Part, SessionHistoryRecord, SessionStreamEvent,
    shared_parts,
};
use lash_core::{
    CellOutcome, CellRecord, CheckpointKind, DriverAction, DriverContextView, ExecResponse,
    ToolCallOutcome, ToolCallRecord, facade_support::TurnOutcome, facade_support::TurnStop,
};
use lash_rlm_types::{RlmDiagnosticEvent, RlmDiagnosticPhase, RlmProtocolEvent};
use lash_sansio::TurnId;
use serde_json::Value;

use crate::dialect::SessionDialect;
use crate::projection::rlm_protocol_event;
use crate::protocol::actions::invalid_turn_options_actions;
use crate::protocol::stall::{NO_PROGRESS_BUDGET_PHASE, stalled_attempts_in_phase};

/// Adjudicate the executor's answer before the channel assembles its history.
/// The commit callback preserves the channel's assistant envelope and trajectory.
pub(crate) fn settle_execution(
    ctx: &DriverContextView<'_>,
    dialect: &SessionDialect,
    code: String,
    result: Result<ExecResponse, lash_core::ExecCodeFailure>,
    extraction_phase: RlmDiagnosticPhase,
    commit: impl FnOnce(CellRecord) -> Vec<SessionHistoryRecord>,
) -> Vec<DriverAction> {
    let mut actions = Vec::new();

    // Cancellation evidence is recorded at the effect handoff, after the
    // executor returns and before this response is interpreted. It must
    // win even when cancellation raced with a normal success or error, or
    // that response could become feedback and re-enter the model.
    if let Some(evidence) = ctx.observed_cancellation() {
        return vec![DriverAction::FinishCancelled {
            evidence: evidence.clone(),
        }];
    }

    // The cell's committed record: the executor's prints and result as
    // it reported them. Only an adjudication below changes the result.
    let mut record = CellRecord {
        id: crate::protocol::stall::cell_id(ctx.turn_id(), ctx.protocol_iteration()),
        protocol_iteration: ctx.protocol_iteration(),
        language: dialect.language_id().to_string(),
        code,
        ..CellRecord::default()
    };
    // The control the cell's control call settled as, with the call's
    // name: what the turn ends on when the cell ended on it.
    let mut settled_control = None;
    match result {
        Ok(response) => {
            if !response.degraded_bindings.is_empty() {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    lash_rlm_types::RlmDiagnosticPhase::ProjectionRehydration,
                    serde_json::json!({
                        "degraded_bindings": response.degraded_bindings,
                    }),
                    lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
                )]));
            }
            let terminal_outcome = response
                .tool_calls
                .iter()
                .find_map(terminal_outcome_from_tool_result);
            if let CellOutcome::Controlled { call_id, .. } = &response.result {
                settled_control = response
                    .tool_calls
                    .iter()
                    .find(|call| &call.call_id == call_id)
                    .and_then(|call| call.output.as_turn_control().cloned());
            }
            // Each call's result summary is projected here, from the
            // committed records, in the record's own commit (FIG-6293).
            record.calls = crate::call_feedback::with_result_summaries(
                response.calls,
                &response.tool_calls,
                dialect.presentation().call_feedback.max_result_bytes(),
            );
            actions.extend(
                response
                    .tool_calls
                    .into_iter()
                    .map(tool_call_event)
                    .map(DriverAction::Emit),
            );
            record.bindings =
                (*response.bindings).bounded(dialect.presentation().max_binding_change_names);
            record.images = response.printed_images;
            record.prints = response.prints;
            record.prints_retained = response.prints_retained;
            record.result = response.result;
            if let Some(outcome) = terminal_outcome {
                // A tool stopped the turn: the cell's own control never
                // took effect, so it is not the cell's result.
                if !record.result.is_failed() {
                    record.result = CellOutcome::Completed;
                }
                actions.push(DriverAction::AppendEvents(commit(record)));
                actions.push(DriverAction::Start(PendingWork::Checkpoint {
                    checkpoint: CheckpointKind::BeforeCompletion,
                    on_empty: CheckpointResumeAction::Finish(outcome),
                }));
                return actions;
            }
        }
        // The effect failed before the executor answered: a host
        // failure that keeps its closed reason.
        Err(failure) => record.result = CellOutcome::Failed(failure.into()),
    }

    if let (
        CellOutcome::Controlled {
            tool_name, call_id, ..
        },
        Some(control),
    ) = (&record.result, settled_control)
    {
        let candidate = lash_core::CompletionCandidate::new(
            ctx.protocol_iteration(),
            call_id.clone(),
            tool_name.clone(),
            control,
        );
        let superseded = crate::protocol::finish::completion_superseded_message(
            rlm_message_id(
                ctx.turn_id(),
                ctx.protocol_iteration(),
                "completion_superseded",
            ),
            &candidate,
        );
        actions.push(DriverAction::AppendEvents(commit(record)));
        actions.push(DriverAction::Start(PendingWork::Checkpoint {
            checkpoint: CheckpointKind::BeforeCompletion,
            on_empty: CheckpointResumeAction::Complete {
                candidate: Box::new(candidate),
                superseded: vec![superseded],
            },
        }));
        return actions;
    }

    let progress = if record.result.is_failed() {
        AttemptProgress::Stalled
    } else {
        AttemptProgress::Executed
    };
    if let Err(err) = continue_or_stop_after_nonterminal(
        ctx,
        &mut actions,
        commit(record),
        Vec::new(),
        progress,
        extraction_phase,
    ) {
        return invalid_turn_options_actions(err);
    }
    actions
}

/// This is the reset condition for the no-progress budget, and it is
/// deliberately narrower than "appended something": an attempt whose cell only
/// raised commits a trajectory entry carrying that error, and a model that
/// raises the same error forever has made exactly as much progress as one
/// whose cell never parsed.
///
/// Two consequences of that law are intended, not oversights. A loop that
/// raises while genuinely converging is bounded the same as one that is not —
/// the bound is set far above ordinary repair traffic precisely so that
/// convergence has room, and a repair that needs more attempts than the bound
/// is indistinguishable from a stall from outside the model. And an attempt
/// whose cell executed but only printed observations resets nothing here; that
/// is real work, and bounding *it* is the turn budget's job, not this one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptProgress {
    /// A cell executed and reported no error.
    Executed,
    /// No cell executed, or the one that did reported an error.
    Stalled,
}

pub(crate) fn continue_or_stop_after_nonterminal(
    ctx: &DriverContextView<'_>,
    actions: &mut Vec<DriverAction>,
    durable_events: Vec<SessionHistoryRecord>,
    retry_events: Vec<SessionHistoryRecord>,
    progress: AttemptProgress,
    extraction_phase: RlmDiagnosticPhase,
) -> Result<(), String> {
    if !durable_events.is_empty() {
        actions.push(DriverAction::AppendEvents(durable_events));
    }
    actions.push(DriverAction::AdvanceProtocolIteration);

    let next_protocol_iteration = ctx.protocol_iteration() + 1;
    let reached_turn_limit = ctx
        .turn_budget()
        .max_turns()
        .is_some_and(|max_turns| next_protocol_iteration >= ctx.protocol_run_offset() + max_turns);
    if reached_turn_limit {
        // Final-turn-fresh doctrine: retry events, including no-progress
        // feedback, are deliberately dropped at the turn limit.
        actions.push(DriverAction::Finish(TurnOutcome::Stopped(
            TurnStop::MaxTurns,
        )));
        return Ok(());
    }

    if progress == AttemptProgress::Stalled {
        let attempts = stalled_attempts_in_phase(ctx, actions, extraction_phase)
            .map_err(|error| error.to_string())?;
        let budget = ctx.no_progress_budget();
        if budget.is_exhausted_by(attempts) {
            actions.push(DriverAction::AppendEvents(vec![
                diagnostic_event(
                    NO_PROGRESS_BUDGET_PHASE,
                    serde_json::json!({
                        "turn_id": ctx.turn_id(),
                        "decision": "stop_no_progress",
                        "consecutive_attempts": attempts,
                        "max_attempts": budget.max_attempts(),
                    }),
                    lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
                ),
                conversation_event(no_progress_stop_message(
                    rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "no_progress"),
                    attempts,
                )),
            ]));
            actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                TurnStop::MaxTurns,
            )));
            return Ok(());
        }
    }

    if !retry_events.is_empty() {
        actions.push(DriverAction::AppendEvents(retry_events));
    }

    actions.push(DriverAction::Start(PendingWork::Checkpoint {
        checkpoint: CheckpointKind::AfterWork,
        on_empty: CheckpointResumeAction::PrepareIteration,
    }));
    Ok(())
}

fn terminal_outcome_from_tool_result(record: &ToolCallRecord) -> Option<TurnOutcome> {
    if let ToolCallOutcome::Cancelled(_) = &record.output.outcome {
        // A cancelled call is an uncatchable host terminal, not a value the
        // model can react to: the run it was dispatched for is over, so the
        // turn ends cancelled with evidence lash mints for itself.
        return Some(TurnOutcome::Stopped(TurnStop::Cancelled {
            evidence: lash_core::facade_support::TurnCancellationEvidence::internal(format!(
                "tool-call-cancelled:{}",
                record.tool
            )),
        }));
    }
    if !record.output.is_success() {
        return None;
    }
    lash_core::turn_stop_from_tool_control(&record.tool, record.output.control.as_ref()?)
}

fn tool_call_event(record: ToolCallRecord) -> SessionStreamEvent {
    SessionStreamEvent::ToolCall {
        call_id: record.call_id,
        provider_call_id: record.provider_call_id,
        name: record.tool,
        args: record.args,
        output: record.output,
    }
}

fn rlm_message_id(turn_id: &TurnId, protocol_iteration: usize, purpose: &str) -> String {
    format!("m_rlm_{turn_id}_{protocol_iteration}_{purpose}")
}

fn conversation_event(message: Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(message))
}

fn diagnostic_event(
    phase: lash_rlm_types::RlmDiagnosticPhase,
    payload: Value,
    schema_version: u32,
) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmDiagnostic(RlmDiagnosticEvent { phase, payload }),
        schema_version,
    ))
}

/// The transcript record left behind when a turn exhausts its no-progress
/// budget.
///
/// It is a system message rather than dialect copy because it addresses no
/// language construct: the turn is over, and nothing will read it as an
/// instruction to repair.
fn no_progress_stop_message(id: String, attempts: usize) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            format!(
                "Stopped after {attempts} consecutive model responses that executed nothing. \
                 The turn's no-progress budget is exhausted; no further model calls were made."
            ),
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
        reply_marker: None,
    }
}
