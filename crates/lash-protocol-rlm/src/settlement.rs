//! Settlement and continuation policy for RLM execution.
//! Response admission and committed assistant history belong to the native driver.

use lash_core::session_model::{
    ConversationRecord, Message, MessageRole, Part, SessionHistoryRecord, SessionStreamEvent,
    shared_parts,
};
use lash_core::{
    CellOutcome, CellRecord, DriverAction, DriverContextView, ExecResponse, ToolCallRecord,
};
use lash_rlm_types::{RlmDiagnosticEvent, RlmProtocolEvent};
use lash_sansio::sansio::{ContinueDecision, IterationProgress};
use lash_vocabulary::identity::TurnId;
use serde_json::Value;

use crate::dialect::SessionDialect;
use crate::projection::rlm_protocol_event;
use crate::protocol::stall::NO_PROGRESS_BUDGET_PHASE;

/// Adjudicate the executor's answer before the driver assembles its history.
/// The commit callback preserves the driver's assistant envelope and trajectory.
pub(crate) fn settle_execution(
    ctx: &DriverContextView<'_>,
    dialect: &SessionDialect,
    code: String,
    result: Result<ExecResponse, lash_core::ExecCodeFailure>,
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
            let terminal_end = response.tool_calls.iter().find_map(|call| {
                if call.output.as_turn_control().is_some() {
                    return None;
                }
                let end = lash_core::step_end_from_tool_output(
                    ctx.protocol_iteration(),
                    &call.call_id,
                    &call.tool,
                    &call.output,
                );
                matches!(end, lash_protocol_types::StepEnd::Stop(_)).then_some(end)
            });
            if let CellOutcome::Controlled { call_id, .. } = &response.result {
                settled_control = response
                    .tool_calls
                    .iter()
                    .find(|call| &call.call_id == call_id)
                    .map(|call| {
                        lash_core::step_end_from_tool_output(
                            ctx.protocol_iteration(),
                            &call.call_id,
                            &call.tool,
                            &call.output,
                        )
                    });
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
            record.consumption = response.consumption;
            record.read_facts = std::sync::Arc::unwrap_or_clone(response.read_facts);
            if let Some(end) = terminal_end {
                // A tool stopped the turn: the cell's own control never
                // took effect, so it is not the cell's result.
                if !record.result.is_failed() {
                    record.result = CellOutcome::Completed;
                }
                actions.push(DriverAction::AppendEvents(commit(record)));
                actions.push(DriverAction::EndStep {
                    end,
                    superseded: Vec::new(),
                });
                return actions;
            }
        }
        // The effect failed before the executor answered: a host
        // failure that keeps its closed reason.
        Err(failure) => record.result = CellOutcome::Failed(failure.into()),
    }

    if let CellOutcome::Failed(failure) = &record.result
        && let Some(panic) = &failure.read_panic
    {
        let stop = lash_protocol_types::TurnStop::ReadPanicked {
            panic: panic.as_ref().clone(),
        };
        actions.push(DriverAction::AppendEvents(commit(record)));
        actions.push(DriverAction::EndStep {
            end: lash_protocol_types::StepEnd::Stop(stop),
            superseded: Vec::new(),
        });
        return actions;
    }

    if let Some(lash_protocol_types::StepEnd::Completion(candidate)) = settled_control {
        let superseded = crate::protocol::finish::completion_superseded_message(
            rlm_message_id(
                ctx.turn_id(),
                ctx.protocol_iteration(),
                "completion_superseded",
            ),
            &candidate,
        );
        actions.push(DriverAction::AppendEvents(commit(record)));
        actions.push(DriverAction::EndStep {
            end: lash_protocol_types::StepEnd::Completion(candidate),
            superseded: vec![superseded],
        });
        return actions;
    }

    let progress = if record.result.is_failed() {
        IterationProgress::Unproductive
    } else {
        IterationProgress::Productive
    };
    continue_or_stop_after_nonterminal(ctx, &mut actions, commit(record), Vec::new, progress);
    actions
}

pub(crate) fn continue_or_stop_after_nonterminal(
    ctx: &DriverContextView<'_>,
    actions: &mut Vec<DriverAction>,
    durable_events: Vec<SessionHistoryRecord>,
    retry_events: impl FnOnce() -> Vec<SessionHistoryRecord>,
    progress: IterationProgress,
) {
    if !durable_events.is_empty() {
        actions.push(DriverAction::AppendEvents(durable_events));
    }
    actions.push(ctx.finish_iteration(progress, |decision| match decision {
        ContinueDecision::AfterWork => retry_events(),
        ContinueDecision::NoProgressLimit { consecutive } => vec![
            diagnostic_event(NO_PROGRESS_BUDGET_PHASE, serde_json::json!({
                "turn_id": ctx.turn_id(), "decision": "stop_no_progress",
                "consecutive_attempts": consecutive, "max_attempts": ctx.no_progress_budget().max_attempts(),
            }), lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION)),
            conversation_event(no_progress_stop_message(
                rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "no_progress"), consecutive,
            )),
        ],
        ContinueDecision::TurnLimit => Vec::new(),
    }));
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
