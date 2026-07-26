use std::collections::BTreeMap;

use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, ProtocolDriverHandle, WaitingExecState,
    WaitingLlmState,
};
use lash_core::session_model::{
    ConversationRecord, Message, SessionHistoryRecord, SessionStreamEvent, make_error_event,
};
use lash_core::{
    CheckpointKind, DriverAction, DriverContextView, ExecResponse, LlmOutputPart, LlmResponse,
    ToolCallOutcome, ToolCallOutput, ToolCallRecord, ToolControl, ToolFailure, ToolFailureClass,
    ToolValue, TurnFinish, TurnOutcome, TurnStop, append_assistant_text_part,
    normalized_response_parts,
};
use lash_rlm_types::{
    RlmAssistantContent, RlmDiagnosticEvent, RlmProtocolEvent, RlmTermination, RlmTrajectoryEntry,
};
use serde_json::Value;

use crate::projection::rlm_protocol_event;
use crate::rlm_support::decode_rlm_termination_options;

use super::actions::{invalid_driver_state_actions, invalid_turn_options_actions};
use super::cell::{CellExtraction, extract_lashlang_cell};
use super::finish::{
    finish_required_reminder_message, finish_schema_mismatch_message,
    internal_assistant_prose_message, invalid_lashlang_cell_message, turn_limit_final_message,
    validate_finish_value,
};
use super::state::{RlmDriverState, decode_rlm_driver_state, rlm_driver_state};

pub struct RlmDriver;

const MAX_EXEC_TOOL_CALL_RECORDS: usize = 128;
const MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES: usize = 64 * 1024;
const EXEC_TOOL_CALL_OVERFLOW_NAME: &str = "lash.exec_tool_call_overflow";

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for RlmDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        if let Err(err) = decode_rlm_termination_options(ctx.termination()) {
            return invalid_turn_options_actions(err);
        }
        vec![DriverAction::StartLlm {
            request: ctx.project_llm_request(false),
            driver_state: Some(rlm_driver_state(RlmDriverState::default())),
        }]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        mut waiting: WaitingLlmState<lash_core::HostTurnProtocol>,
        llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let mut actions = vec![DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: llm_response.full_text.clone(),
            duration_ms: 0,
        })];

        let mut assistant_text = String::new();
        let mut reasoning_text = String::new();
        for part in normalized_response_parts(&llm_response) {
            match part {
                LlmOutputPart::Text { text, .. } => {
                    append_assistant_text_part(&mut assistant_text, &text);
                }
                LlmOutputPart::Reasoning { text, replay } => {
                    let reasoning = if text.trim().is_empty() {
                        replay
                            .as_ref()
                            .map(|meta| meta.summary.join("\n\n"))
                            .unwrap_or_default()
                    } else {
                        text
                    };
                    append_assistant_text_part(&mut reasoning_text, &reasoning);
                }
                LlmOutputPart::ToolCall { .. } => {}
            }
        }

        if assistant_text.trim().is_empty() && reasoning_text.trim().is_empty() {
            actions.push(DriverAction::Emit(make_error_event(
                "llm_provider",
                Some("empty_response"),
                "Model returned no assistant text.",
                None,
            )));
            actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                TurnStop::ProviderError,
            )));
            return actions;
        }

        let termination = match decode_rlm_termination_options(ctx.termination()) {
            Ok(termination) => termination,
            Err(err) => return invalid_turn_options_actions(err),
        };

        let extraction = match extract_lashlang_cell(&assistant_text) {
            Ok(extraction) => extraction,
            Err(err) => {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    "llm_extraction",
                    llm_extraction_payload(
                        "invalid_lashlang_cell",
                        &termination,
                        LlmExtractionCounts::prose_only(&assistant_text, &reasoning_text),
                    ),
                )]));
                if let Err(err) = continue_or_stop_after_nonterminal(
                    &ctx,
                    &mut actions,
                    Vec::new(),
                    vec![conversation_event(invalid_lashlang_cell_message(
                        rlm_message_id(ctx.protocol_iteration(), "invalid_cell"),
                        err.message(),
                    ))],
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }
        };
        let Some(cell) = extraction else {
            if matches!(termination, RlmTermination::Natural) {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    "llm_extraction",
                    llm_extraction_payload(
                        "finish_prose",
                        &termination,
                        LlmExtractionCounts::prose_only(&assistant_text, &reasoning_text),
                    ),
                )]));
                actions.push(DriverAction::StartCheckpoint {
                    checkpoint: CheckpointKind::BeforeCompletion,
                    on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                        TurnFinish::AssistantMessage {
                            text: assistant_text.clone(),
                        },
                    )),
                });
                return actions;
            }
            let RlmTermination::FinishRequired { ref schema } = termination else {
                unreachable!("Natural returned above");
            };
            actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                "llm_extraction",
                llm_extraction_payload(
                    "request_finish",
                    &termination,
                    LlmExtractionCounts::prose_only(&assistant_text, &reasoning_text),
                ),
            )]));
            let mut events = Vec::new();
            if !assistant_text.trim().is_empty() {
                events.push(conversation_event(internal_assistant_prose_message(
                    rlm_message_id(ctx.protocol_iteration(), "assistant_prose"),
                    assistant_text,
                )));
            }
            events.push(conversation_event(finish_required_reminder_message(
                rlm_message_id(ctx.protocol_iteration(), "finish_reminder"),
                schema.is_some(),
            )));
            if let Err(err) =
                continue_or_stop_after_nonterminal(&ctx, &mut actions, Vec::new(), events)
            {
                return invalid_turn_options_actions(err);
            }
            return actions;
        };

        actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
            "llm_extraction",
            llm_extraction_payload(
                "execute_lashlang",
                &termination,
                LlmExtractionCounts::cell(&assistant_text, &reasoning_text, &cell),
            ),
        )]));

        let Some(raw_state) = waiting.take_driver_state() else {
            return invalid_driver_state_actions("missing RLM driver state".to_string());
        };
        let mut state = match decode_rlm_driver_state(raw_state) {
            Ok(state) => state,
            Err(err) => return invalid_driver_state_actions(err),
        };
        state.executed_code = Some(cell.code.clone());
        state.reasoning = reasoning_text;
        state.prose = cell.prose.clone();

        // Emit the raw lashlang source as a `Message` with kind
        // `lashlang_code` so the CLI can reveal it in the full-expand
        // view (Alt+O) above the tool activities it produced.
        actions.push(DriverAction::Emit(SessionStreamEvent::Message {
            text: cell.code.clone(),
            kind: "lashlang_code".to_string(),
        }));
        actions.push(DriverAction::StartExec {
            language: "lashlang".to_string(),
            code: cell.code,
            driver_state: rlm_driver_state(state),
        });
        actions
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        ctx: DriverContextView<'_>,
        waiting: WaitingExecState<lash_core::HostTurnProtocol>,
        result: Result<ExecResponse, String>,
    ) -> Vec<DriverAction> {
        let mut state = match decode_rlm_driver_state(waiting.into_driver_state()) {
            Ok(state) => state,
            Err(err) => return invalid_driver_state_actions(err),
        };
        let mut actions = Vec::new();

        match result {
            Ok(response) => {
                let terminal_outcome = response
                    .tool_calls
                    .iter()
                    .find_map(terminal_outcome_from_tool_result);
                actions.extend(
                    bounded_exec_tool_call_records(&response.tool_calls)
                        .into_iter()
                        .map(tool_call_event)
                        .map(DriverAction::Emit),
                );
                state.images.extend(response.printed_images);
                for observation in response.observations {
                    if !observation.is_empty() {
                        state.output.push(observation);
                    }
                }
                if let Some(raw_error) = response.error {
                    state.exec_error = Some(raw_error);
                }
                if let Some(finish_value) = response.terminal_finish {
                    state.terminal_finish = Some(finish_value);
                }
                if let Some(outcome) = terminal_outcome {
                    actions.push(DriverAction::AppendEvents(trajectory_events(
                        ctx.protocol_iteration(),
                        &state,
                        None,
                        None,
                    )));
                    actions.push(DriverAction::StartCheckpoint {
                        checkpoint: CheckpointKind::BeforeCompletion,
                        on_empty: CheckpointResumeAction::Finish(outcome),
                    });
                    return actions;
                }
            }
            Err(error) => {
                state.exec_error = Some(error);
            }
        }

        if let Some(finish_value) = &state.terminal_finish {
            // Typed-RLM: validate against the declared schema. If it fails,
            // surface the error to the model and loop; otherwise fall
            // through to the shared terminate-with-value path below.
            let termination = match decode_rlm_termination_options(ctx.termination()) {
                Ok(termination) => termination,
                Err(err) => return invalid_turn_options_actions(err),
            };
            if let RlmTermination::FinishRequired {
                schema: Some(schema),
            } = termination
                && let Err(error_text) = validate_finish_value(finish_value, &schema)
            {
                if let Err(err) = continue_or_stop_after_nonterminal(
                    &ctx,
                    &mut actions,
                    trajectory_events(
                        ctx.protocol_iteration(),
                        &state,
                        Some(error_text.clone()),
                        None,
                    ),
                    vec![conversation_event(finish_schema_mismatch_message(
                        rlm_message_id(ctx.protocol_iteration(), "schema_mismatch"),
                        &error_text,
                    ))],
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }

            actions.push(DriverAction::AppendEvents(trajectory_events(
                ctx.protocol_iteration(),
                &state,
                None,
                Some(finish_value.clone()),
            )));
            actions.push(DriverAction::StartCheckpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                    TurnFinish::FinalValue {
                        value: finish_value.clone(),
                    },
                )),
            });
            return actions;
        }

        if let Err(err) = continue_or_stop_after_nonterminal(
            &ctx,
            &mut actions,
            trajectory_events(ctx.protocol_iteration(), &state, None, None),
            Vec::new(),
        ) {
            return invalid_turn_options_actions(err);
        }
        actions
    }
}

fn continue_or_stop_after_nonterminal(
    ctx: &DriverContextView<'_>,
    actions: &mut Vec<DriverAction>,
    durable_events: Vec<SessionHistoryRecord>,
    retry_events: Vec<SessionHistoryRecord>,
) -> Result<(), String> {
    if !durable_events.is_empty() {
        actions.push(DriverAction::AppendEvents(durable_events));
    }
    actions.push(DriverAction::AdvanceProtocolIteration);

    if ctx.should_force_exit_after_grace_turn() {
        actions.push(DriverAction::Finish(TurnOutcome::Stopped(
            TurnStop::MaxTurns,
        )));
        return Ok(());
    }

    let next_protocol_iteration = ctx.protocol_iteration() + 1;
    let reached_turn_limit = ctx
        .max_turns()
        .is_some_and(|max_turns| next_protocol_iteration >= ctx.protocol_run_offset() + max_turns);
    if reached_turn_limit {
        match decode_rlm_termination_options(ctx.termination())? {
            RlmTermination::FinishRequired { .. } => {
                actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                    TurnStop::MaxTurns,
                )));
                return Ok(());
            }
            RlmTermination::Natural => {
                if let Some(max_turns) = ctx.max_turns() {
                    actions.push(DriverAction::ScheduleTurnLimitFinal {
                        message: turn_limit_final_message(
                            rlm_message_id(next_protocol_iteration, "turn_limit"),
                            max_turns,
                        ),
                    });
                }
            }
        }
    } else if !retry_events.is_empty() {
        actions.push(DriverAction::AppendEvents(retry_events));
    }

    actions.push(DriverAction::StartCheckpoint {
        checkpoint: CheckpointKind::AfterWork,
        on_empty: CheckpointResumeAction::PrepareIteration,
    });
    Ok(())
}

fn terminal_outcome_from_tool_result(record: &ToolCallRecord) -> Option<TurnOutcome> {
    if !record.output.is_success() {
        return None;
    }
    lash_core::turn_outcome_from_tool_control(&record.tool, record.output.control.as_ref()?)
}

fn tool_call_event(record: ToolCallRecord) -> SessionStreamEvent {
    SessionStreamEvent::ToolCall {
        call_id: record.call_id,
        name: record.tool,
        args: record.args,
        output: record.output,
        duration_ms: record.duration_ms,
    }
}

fn bounded_exec_tool_call_records(records: &[ToolCallRecord]) -> Vec<ToolCallRecord> {
    let retained_count = records.len().min(MAX_EXEC_TOOL_CALL_RECORDS);
    let mut bounded = records[..retained_count]
        .iter()
        .map(bounded_tool_call_record)
        .collect::<Vec<_>>();
    let omitted = &records[retained_count..];
    if !omitted.is_empty() {
        bounded.push(exec_tool_call_overflow_record(omitted));
    }
    bounded
}

fn bounded_tool_call_record(record: &ToolCallRecord) -> ToolCallRecord {
    ToolCallRecord {
        call_id: record.call_id.clone(),
        tool: record.tool.clone(),
        args: record.args.clone(),
        output: bounded_tool_call_output(&record.output),
        duration_ms: record.duration_ms,
    }
}

fn bounded_tool_call_output(output: &ToolCallOutput) -> ToolCallOutput {
    let outcome = match &output.outcome {
        ToolCallOutcome::Success(value) => ToolCallOutcome::Success(bounded_tool_value(value)),
        ToolCallOutcome::Failure(failure) => {
            ToolCallOutcome::Failure(bounded_tool_failure(failure))
        }
        ToolCallOutcome::Cancelled(cancellation) => {
            let mut bounded = cancellation.clone();
            bounded.raw = bounded.raw.as_ref().map(bounded_tool_value);
            ToolCallOutcome::Cancelled(bounded)
        }
    };
    let control = output.control.as_ref().map(|control| match control {
        ToolControl::SwitchAgentFrame {
            frame_id,
            initial_nodes,
            task,
        } => ToolControl::SwitchAgentFrame {
            frame_id: frame_id.clone(),
            initial_nodes: initial_nodes.clone(),
            task: task.clone(),
        },
        ToolControl::Finish { value } => ToolControl::Finish {
            value: bounded_tool_value(value),
        },
        ToolControl::Fail { failure } => ToolControl::Fail {
            failure: bounded_tool_failure(failure),
        },
    });
    ToolCallOutput { outcome, control }
}

fn bounded_tool_failure(failure: &ToolFailure) -> ToolFailure {
    let mut bounded = failure.clone();
    bounded.raw = bounded.raw.as_ref().map(bounded_tool_value);
    bounded
}

fn bounded_tool_value(value: &ToolValue) -> ToolValue {
    match value {
        ToolValue::String(value) if value.len() > MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES => {
            omitted_bytes_marker(value.len())
        }
        ToolValue::Array(values) => {
            ToolValue::Array(values.iter().map(bounded_tool_value).collect())
        }
        ToolValue::Object(entries) => ToolValue::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), bounded_tool_value(value)))
                .collect(),
        ),
        ToolValue::Null
        | ToolValue::Bool(_)
        | ToolValue::Number(_)
        | ToolValue::String(_)
        | ToolValue::Attachment(_) => value.clone(),
    }
}

fn omitted_bytes_marker(omitted_bytes: usize) -> ToolValue {
    ToolValue::Object(BTreeMap::from([(
        "omitted_bytes".to_string(),
        ToolValue::from(serde_json::json!(omitted_bytes)),
    )]))
}

fn exec_tool_call_overflow_record(omitted: &[ToolCallRecord]) -> ToolCallRecord {
    let omitted_failures = omitted
        .iter()
        .filter(|record| !record.output.is_success())
        .count();
    let attachments = omitted
        .iter()
        .flat_map(|record| tool_output_attachments(&record.output))
        .map(ToolValue::Attachment)
        .collect();
    let marker = ToolValue::Object(BTreeMap::from([
        ("attachments".to_string(), ToolValue::Array(attachments)),
        (
            "omitted_failures".to_string(),
            ToolValue::from(serde_json::json!(omitted_failures)),
        ),
        (
            "omitted_records".to_string(),
            ToolValue::from(serde_json::json!(omitted.len())),
        ),
    ]));
    let output = if omitted_failures == 0 {
        ToolCallOutput::success(marker)
    } else {
        let mut failure = ToolFailure::runtime(
            ToolFailureClass::ResourceLimit,
            "exec_tool_call_records_omitted",
            "exec tool-call records exceeded the accounting limit",
        );
        failure.raw = Some(marker);
        ToolCallOutput::failure(failure)
    };
    ToolCallRecord {
        call_id: None,
        tool: EXEC_TOOL_CALL_OVERFLOW_NAME.to_string(),
        args: serde_json::json!({}),
        output,
        duration_ms: 0,
    }
}

fn tool_output_attachments(output: &ToolCallOutput) -> Vec<lash_core::AttachmentSource> {
    let mut attachments = output.attachments();
    match output.control.as_ref() {
        Some(ToolControl::Finish { value }) => attachments.extend(value.attachments()),
        Some(ToolControl::Fail { failure }) => attachments.extend(
            failure
                .raw
                .as_ref()
                .map(ToolValue::attachments)
                .unwrap_or_default(),
        ),
        Some(ToolControl::SwitchAgentFrame { .. }) | None => {}
    }
    attachments
}

fn trajectory_entry(
    protocol_iteration: usize,
    state: &RlmDriverState,
    validation_error: Option<String>,
    final_output: Option<Value>,
) -> RlmTrajectoryEntry {
    RlmTrajectoryEntry {
        id: format!("lashlang_step_{protocol_iteration}"),
        protocol_iteration,
        code: state.executed_code.clone().unwrap_or_default(),
        output: state.output.clone(),
        images: state.images.clone(),
        error: validation_error.or_else(|| state.exec_error.clone()),
        final_output,
    }
}

fn rlm_message_id(protocol_iteration: usize, purpose: &str) -> String {
    format!("m_rlm_{protocol_iteration}_{purpose}")
}

fn trajectory_events(
    protocol_iteration: usize,
    state: &RlmDriverState,
    validation_error: Option<String>,
    final_output: Option<Value>,
) -> Vec<SessionHistoryRecord> {
    let mut events = Vec::new();
    if let Some(event) = assistant_content_event(protocol_iteration, &state.reasoning, &state.prose)
    {
        events.push(event);
    }
    events.push(trajectory_event(trajectory_entry(
        protocol_iteration,
        state,
        validation_error,
        final_output,
    )));
    events
}

fn assistant_content_event(
    protocol_iteration: usize,
    reasoning: &str,
    prose: &str,
) -> Option<SessionHistoryRecord> {
    let id = rlm_message_id(protocol_iteration, "assistant_content");
    let reasoning = reasoning.trim();
    let prose = prose.trim();
    (!reasoning.is_empty() || !prose.is_empty()).then(|| {
        SessionHistoryRecord::Protocol(rlm_protocol_event(RlmProtocolEvent::RlmAssistantContent(
            RlmAssistantContent {
                id,
                reasoning: reasoning.to_string(),
                prose: prose.to_string(),
            },
        )))
    })
}

fn conversation_event(message: Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(message))
}

fn trajectory_event(entry: RlmTrajectoryEntry) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(RlmProtocolEvent::RlmTrajectoryEntry(
        entry,
    )))
}

fn diagnostic_event(phase: &str, payload: Value) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(RlmProtocolEvent::RlmDiagnostic(
        RlmDiagnosticEvent {
            phase: phase.to_string(),
            payload,
        },
    )))
}

struct LlmExtractionCounts {
    full_text_chars: usize,
    prose_chars: usize,
    code_chars: usize,
    reasoning_chars: usize,
    lashlang_cell_count: usize,
}

impl LlmExtractionCounts {
    fn prose_only(assistant_text: &str, reasoning_text: &str) -> Self {
        Self {
            full_text_chars: assistant_text.chars().count(),
            prose_chars: assistant_text.chars().count(),
            code_chars: 0,
            reasoning_chars: reasoning_text.chars().count(),
            lashlang_cell_count: 0,
        }
    }

    fn cell(assistant_text: &str, reasoning_text: &str, cell: &CellExtraction) -> Self {
        Self {
            full_text_chars: assistant_text.chars().count(),
            prose_chars: cell.prose.chars().count(),
            code_chars: cell.code.chars().count(),
            reasoning_chars: reasoning_text.chars().count(),
            lashlang_cell_count: cell.lashlang_cell_count,
        }
    }
}

fn llm_extraction_payload(
    decision: &str,
    termination: &RlmTermination,
    counts: LlmExtractionCounts,
) -> Value {
    serde_json::json!({
        "decision": decision,
        "termination": termination_diagnostic_name(termination),
        "counts": {
            "full_text_chars": counts.full_text_chars,
            "prose_chars": counts.prose_chars,
            "code_chars": counts.code_chars,
            "reasoning_chars": counts.reasoning_chars,
            "lashlang_cell_count": counts.lashlang_cell_count,
        },
    })
}

fn termination_diagnostic_name(termination: &RlmTermination) -> &'static str {
    match termination {
        RlmTermination::FinishRequired { .. } => "finish_required",
        RlmTermination::Natural => "natural",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{
        AttachmentId, AttachmentMeta, AttachmentSource, AttachmentTypeMetadata, MediaType,
        ToolCancellation,
    };

    fn image_ref(id: &str) -> AttachmentSource {
        AttachmentSource::stored(
            AttachmentMeta::new(
                AttachmentId::new(id),
                MediaType::parse("image/png").unwrap(),
                3,
                Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
                Some("tiny".to_string()),
            )
            .as_ref(),
        )
    }

    fn record(index: usize, output: ToolCallOutput) -> ToolCallRecord {
        ToolCallRecord {
            call_id: Some(format!("call-{index}")),
            tool: "test_tool".to_string(),
            args: serde_json::json!({ "index": index }),
            output,
            duration_ms: index as u64,
        }
    }

    #[test]
    fn bounded_output_replaces_oversized_scalars_without_losing_structure_or_attachments() {
        let attachment = image_ref("nested-attachment");
        let oversized = "x".repeat(MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES + 17);
        let output = ToolCallOutput::success(ToolValue::Object(BTreeMap::from([
            (
                "nested".to_string(),
                ToolValue::Array(vec![
                    ToolValue::String("kept".to_string()),
                    ToolValue::String(oversized.clone()),
                    ToolValue::Attachment(attachment.clone()),
                ]),
            ),
            ("sibling".to_string(), ToolValue::Bool(true)),
        ])));

        let bounded = bounded_tool_call_record(&record(0, output));
        assert_eq!(bounded.args, serde_json::json!({ "index": 0 }));
        let attachment_json = ToolValue::Attachment(attachment.clone()).to_json_value();
        assert_eq!(
            bounded.output.value_for_projection(),
            serde_json::json!({
                "nested": [
                    "kept",
                    { "omitted_bytes": oversized.len() },
                    attachment_json,
                ],
                "sibling": true,
            })
        );
        assert_eq!(bounded.output.attachments(), vec![attachment]);
    }

    #[test]
    fn bounded_output_recurses_through_failure_and_cancellation_raw_values() {
        let failure_attachment = image_ref("failure-attachment");
        let cancellation_attachment = image_ref("cancellation-attachment");
        let oversized = "x".repeat(MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES + 1);
        let mut failure = ToolFailure::tool(
            ToolFailureClass::Execution,
            "failed",
            "failure recovered by Lashlang",
        );
        failure.raw = Some(ToolValue::Array(vec![
            ToolValue::String(oversized.clone()),
            ToolValue::Attachment(failure_attachment.clone()),
        ]));
        let cancellation = ToolCancellation {
            message: "cancelled".to_string(),
            source: lash_core::ToolFailureSource::Cancellation,
            raw: Some(ToolValue::Array(vec![
                ToolValue::String(oversized.clone()),
                ToolValue::Attachment(cancellation_attachment.clone()),
            ])),
        };

        let bounded = bounded_exec_tool_call_records(&[
            record(0, ToolCallOutput::failure(failure)),
            record(1, ToolCallOutput::cancelled(cancellation)),
        ]);

        assert_eq!(bounded[0].output.attachments(), vec![failure_attachment]);
        assert_eq!(
            bounded[1].output.attachments(),
            vec![cancellation_attachment]
        );
        for record in bounded {
            assert!(
                record
                    .output
                    .value_for_projection()
                    .to_string()
                    .contains("omitted_bytes")
            );
        }
    }

    #[test]
    fn overflow_marker_preserves_counts_failure_status_and_omitted_attachments() {
        let attachment = image_ref("overflow-attachment");
        let mut records = (0..MAX_EXEC_TOOL_CALL_RECORDS + 3)
            .map(|index| record(index, ToolCallOutput::success(serde_json::json!(index))))
            .collect::<Vec<_>>();
        records[MAX_EXEC_TOOL_CALL_RECORDS + 1].output =
            ToolCallOutput::failure(ToolFailure::tool(
                ToolFailureClass::Execution,
                "recovered_failure",
                "failure recovered by Lashlang",
            ));
        records[MAX_EXEC_TOOL_CALL_RECORDS + 2].output =
            ToolCallOutput::success(ToolValue::Attachment(attachment.clone()));

        let bounded = bounded_exec_tool_call_records(&records);

        assert_eq!(bounded.len(), MAX_EXEC_TOOL_CALL_RECORDS + 1);
        let marker = bounded.last().expect("overflow marker");
        assert_eq!(marker.tool, EXEC_TOOL_CALL_OVERFLOW_NAME);
        assert!(!marker.output.is_success());
        assert_eq!(marker.output.attachments(), vec![attachment]);
        let payload = marker.output.value_for_projection();
        assert_eq!(
            payload.pointer("/raw/omitted_records"),
            Some(&serde_json::json!(3))
        );
        assert_eq!(
            payload.pointer("/raw/omitted_failures"),
            Some(&serde_json::json!(1))
        );
    }

    #[test]
    fn all_success_overflow_marker_does_not_create_a_failure() {
        let records = (0..MAX_EXEC_TOOL_CALL_RECORDS + 1)
            .map(|index| record(index, ToolCallOutput::success(serde_json::json!(index))))
            .collect::<Vec<_>>();

        let marker = bounded_exec_tool_call_records(&records)
            .pop()
            .expect("overflow marker");

        assert!(marker.output.is_success());
        assert_eq!(marker.output.value_for_projection()["omitted_failures"], 0);
    }
}
