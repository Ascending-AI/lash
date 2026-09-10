use lash_sansio::TurnId;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, ProtocolDriverHandle, WaitingExecState,
    WaitingLlmState,
};
use lash_core::session_model::{
    ConversationRecord, Message, SessionHistoryRecord, SessionStreamEvent, make_error_event,
};
use lash_core::{
    CheckpointKind, DriverAction, DriverContextView, ExecResponse, LlmResponse, OmittedToolCalls,
    ToolCallOutcome, ToolCallOutput, ToolCallRecord, ToolControl, ToolFailure, ToolValue,
    facade_support::TurnFinish, facade_support::TurnOutcome, facade_support::TurnStop,
    facade_support::normalized_response_parts,
};
use lash_rlm_types::{
    RlmDiagnosticEvent, RlmExecutedCall, RlmProtocolEvent, RlmTermination, RlmTrajectoryEntry,
};
use serde_json::Value;

use crate::dialect::RlmDialect;
use crate::projection::rlm_protocol_event;
use crate::rlm_support::decode_rlm_termination_options;

use super::finish::{
    finish_required_reminder_message, finish_schema_mismatch_message,
    internal_assistant_prose_message_for_turn, no_progress_stop_message, turn_limit_final_message,
    validate_finish_value,
};
use super::stall::{
    LLM_EXTRACTION_PHASE, NO_PROGRESS_BUDGET_PHASE, reply_fingerprint, stalled_attempts,
};
use super::state::{RlmDriverState, RlmReasoningPart, decode_rlm_driver_state, rlm_driver_state};
use crate::protocol::actions::{invalid_driver_state_actions, invalid_turn_options_actions};

#[derive(Clone)]
pub struct NativeDriver {
    dialect: Arc<dyn RlmDialect>,
}

impl NativeDriver {
    pub(crate) fn with_dialect(dialect: Arc<dyn RlmDialect>) -> Self {
        Self { dialect }
    }
}

const MAX_EXEC_TOOL_CALL_RECORDS: usize = 128;
const MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES: usize = 64 * 1024;

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for NativeDriver {
    fn project_visible_assistant_prose(&self, text: &str) -> String {
        text.to_string()
    }

    fn handles_output_limit_response(&self) -> bool {
        true
    }

    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        if let Err(err) = decode_rlm_termination_options(ctx.termination()) {
            return invalid_turn_options_actions(err);
        }
        let mut actions = Vec::new();
        let degraded_bindings = ctx
            .events()
            .iter()
            .filter_map(|record| {
                let SessionHistoryRecord::Protocol(event) = record else {
                    return None;
                };
                super::transport::repair_parts(event)
                    .err()
                    .map(super::transport::degraded_binding)
            })
            .collect::<Vec<_>>();
        if !degraded_bindings.is_empty() {
            actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                "projection_rehydration",
                serde_json::json!({"degraded_bindings": degraded_bindings}),
            )]));
        }
        actions.push(DriverAction::StartLlm {
            request: ctx.project_llm_request(false),
            driver_state: Some(rlm_driver_state(RlmDriverState::default())),
        });
        actions
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        mut waiting: WaitingLlmState<lash_core::HostTurnProtocol>,
        llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let mut actions = Vec::new();
        let parts = super::tool::assistant_parts(normalized_response_parts(&llm_response));
        let prose = llm_response.full_text();
        let reasoning = parts
            .iter()
            .filter(|part| part.kind == lash_core::PartKind::Reasoning)
            .map(|part| RlmReasoningPart {
                text: part.content.clone(),
                replay: part.reasoning_meta.clone(),
            })
            .collect::<Vec<_>>();
        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: prose.clone(),
            duration_ms: 0,
        }));
        let action = super::tool::normalize(&parts);
        if matches!(action, super::tool::NativeAction::ProseOnly) && prose.trim().is_empty() {
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
            Ok(value) => value,
            Err(error) => return invalid_turn_options_actions(error),
        };
        if llm_response.terminal_reason == lash_core::LlmTerminalReason::OutputLimit {
            let prose_only = matches!(action, super::tool::NativeAction::ProseOnly);
            let decision = if prose_only {
                "retry_output_limit_prose"
            } else {
                "retry_output_limit_call"
            };
            actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                LLM_EXTRACTION_PHASE,
                serde_json::json!({ "turn_id": ctx.turn_id(), "decision": decision, "dialect": self.dialect.language_id() }),
            )]));
            let cap = ctx
                .generation()
                .output_token_cap
                .map(|cap| format!(" (the request cap was {cap} tokens)"))
                .unwrap_or_default();
            let copy = format!(
                "Your answer was cut off by the output limit{cap} — retry with a shorter answer. Do less per program and continue in a later step."
            );
            let mut durable = Vec::new();
            let mut retry = Vec::new();
            if prose_only {
                retry.push(conversation_event(
                    internal_assistant_prose_message_for_turn(
                        ctx.turn_id(),
                        rlm_message_id(
                            ctx.turn_id(),
                            ctx.protocol_iteration(),
                            "truncated_assistant_response",
                        ),
                        prose,
                        &reasoning,
                    ),
                ));
                retry.push(conversation_event(Message {
                    id: rlm_message_id(
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        "output_limit_retry",
                    ),
                    role: lash_core::MessageRole::System,
                    parts: vec![lash_core::Part::text(
                        rlm_message_id(
                            ctx.turn_id(),
                            ctx.protocol_iteration(),
                            "output_limit_retry.p0",
                        ),
                        copy,
                        None,
                    )]
                    .into(),
                    origin: Some(lash_core::MessageOrigin::Plugin {
                        plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                        transient: false,
                    }),
                }));
            } else {
                durable.push(super::transport::repair_event(
                    ctx.turn_id(),
                    ctx.protocol_iteration(),
                    parts,
                    copy,
                ));
            }
            if let Err(error) = continue_or_stop_after_nonterminal(
                self.dialect.as_ref(),
                &ctx,
                &mut actions,
                durable,
                retry,
                AttemptProgress::Stalled,
            ) {
                return invalid_turn_options_actions(error);
            }
            return actions;
        }
        let decision = match &action {
            super::tool::NativeAction::Execute { .. } => {
                format!("execute_{}", self.dialect.language_id())
            }
            super::tool::NativeAction::Malformed { decision, .. } => decision.to_string(),
            super::tool::NativeAction::ProseOnly => {
                if matches!(termination, RlmTermination::Natural) {
                    "prose_only"
                } else {
                    "request_finish"
                }
                .to_string()
            }
        };
        actions.push(DriverAction::AppendEvents(vec![diagnostic_event(LLM_EXTRACTION_PHASE,
            serde_json::json!({ "turn_id": ctx.turn_id(), "decision": decision, "dialect": self.dialect.language_id(), "reply_fingerprint": reply_fingerprint(&serde_json::to_string(&parts).expect("parts serialize")) }))]));
        match action {
            super::tool::NativeAction::Malformed { repair_copy, .. } => {
                let events = vec![super::transport::repair_event(
                    ctx.turn_id(),
                    ctx.protocol_iteration(),
                    parts,
                    repair_copy,
                )];
                if let Err(error) = continue_or_stop_after_nonterminal(
                    self.dialect.as_ref(),
                    &ctx,
                    &mut actions,
                    events,
                    Vec::new(),
                    AttemptProgress::Stalled,
                ) {
                    return invalid_turn_options_actions(error);
                }
            }
            super::tool::NativeAction::ProseOnly => {
                if matches!(termination, RlmTermination::Natural) {
                    if !reasoning.is_empty() {
                        actions.push(DriverAction::AppendEvents(vec![conversation_event(
                            internal_assistant_prose_message_for_turn(
                                ctx.turn_id(),
                                rlm_message_id(
                                    ctx.turn_id(),
                                    ctx.protocol_iteration(),
                                    "assistant_response",
                                ),
                                prose.clone(),
                                &reasoning,
                            ),
                        )]));
                    }
                    actions.push(DriverAction::StartCheckpoint {
                        checkpoint: CheckpointKind::BeforeCompletion,
                        on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                            TurnFinish::AssistantMessage { text: prose },
                        )),
                    });
                } else {
                    let RlmTermination::FinishRequired { schema } = termination else {
                        unreachable!()
                    };
                    let events = vec![
                        conversation_event(internal_assistant_prose_message_for_turn(
                            ctx.turn_id(),
                            rlm_message_id(
                                ctx.turn_id(),
                                ctx.protocol_iteration(),
                                "assistant_response",
                            ),
                            prose,
                            &reasoning,
                        )),
                        conversation_event(finish_required_reminder_message(
                            self.dialect.as_ref(),
                            rlm_message_id(
                                ctx.turn_id(),
                                ctx.protocol_iteration(),
                                "finish_reminder",
                            ),
                            schema.is_some(),
                        )),
                    ];
                    if let Err(error) = continue_or_stop_after_nonterminal(
                        self.dialect.as_ref(),
                        &ctx,
                        &mut actions,
                        Vec::new(),
                        events,
                        AttemptProgress::Stalled,
                    ) {
                        return invalid_turn_options_actions(error);
                    }
                }
            }
            super::tool::NativeAction::Execute { code } => {
                let Some(raw_state) = waiting.take_driver_state() else {
                    return invalid_driver_state_actions("missing native driver state".to_string());
                };
                let mut state = match decode_rlm_driver_state(raw_state) {
                    Ok(state) => state,
                    Err(error) => return invalid_driver_state_actions(error),
                };
                state.code = code.clone();
                state.reasoning = reasoning;
                state.assistant_parts = parts;
                actions.push(DriverAction::Emit(SessionStreamEvent::Message {
                    text: code.clone(),
                    kind: self.dialect.code_stream_kind().to_string(),
                }));
                actions.push(DriverAction::StartExec {
                    language: self.dialect.language_id().to_string(),
                    code,
                    driver_state: rlm_driver_state(state),
                });
            }
        }
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

        // Cancellation evidence is recorded at the effect handoff, after the
        // executor returns and before this response is interpreted. It must
        // win even when cancellation raced with a normal success or error, or
        // that response could become feedback and re-enter the model.
        if let Some(evidence) = ctx.observed_cancellation() {
            return vec![DriverAction::FinishCancelled {
                evidence: evidence.clone(),
            }];
        }

        match result {
            Ok(response) => {
                let error = response.error;
                if !response.degraded_bindings.is_empty() {
                    actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                        "projection_rehydration",
                        serde_json::json!({
                            "degraded_bindings": response.degraded_bindings,
                        }),
                    )]));
                }
                let terminal_outcome = response
                    .calls
                    .iter()
                    .filter_map(|call| call.host_record.as_ref())
                    .find_map(terminal_outcome_from_tool_result);
                let (host_records, omitted) = bounded_exec_tool_call_records(&response.calls);
                actions.extend(
                    host_records
                        .into_iter()
                        .map(tool_call_event)
                        .map(DriverAction::Emit),
                );
                if let Some(summary) = omitted {
                    actions.push(DriverAction::Emit(SessionStreamEvent::ToolCallsOmitted {
                        summary,
                    }));
                }
                (state.calls, state.calls_omitted) = executed_call_ledger(&response.calls);
                state.images.extend(response.printed_images);
                for observation in response.observations {
                    if !observation.text.is_empty() {
                        state.output.push(observation.text);
                    }
                }
                if let Some(error) = error {
                    state.error = Some(error);
                }
                if let Some(finish_value) = response.terminal_finish {
                    state.terminal_finish = Some(finish_value);
                }
                if let Some(outcome) = terminal_outcome {
                    actions.push(DriverAction::AppendEvents(trajectory_events(
                        self.dialect.prompt_vocabulary(),
                        ctx.turn_id(),
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
                state.error = Some(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    error,
                ));
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
                    self.dialect.as_ref(),
                    &ctx,
                    &mut actions,
                    trajectory_events(
                        self.dialect.prompt_vocabulary(),
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        &state,
                        Some(error_text.clone()),
                        None,
                    ),
                    vec![conversation_event(finish_schema_mismatch_message(
                        self.dialect.as_ref(),
                        rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "schema_mismatch"),
                    ))],
                    AttemptProgress::Stalled,
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }

            actions.push(DriverAction::AppendEvents(trajectory_events(
                self.dialect.prompt_vocabulary(),
                ctx.turn_id(),
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
            self.dialect.as_ref(),
            &ctx,
            &mut actions,
            trajectory_events(
                self.dialect.prompt_vocabulary(),
                ctx.turn_id(),
                ctx.protocol_iteration(),
                &state,
                None,
                None,
            ),
            Vec::new(),
            if state.error.is_some() {
                AttemptProgress::Stalled
            } else {
                AttemptProgress::Executed
            },
        ) {
            return invalid_turn_options_actions(err);
        }
        actions
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptProgress {
    /// A cell executed and reported no error.
    Executed,
    /// No cell executed, or the one that did reported an error.
    Stalled,
}

fn continue_or_stop_after_nonterminal(
    dialect: &dyn RlmDialect,
    ctx: &DriverContextView<'_>,
    actions: &mut Vec<DriverAction>,
    durable_events: Vec<SessionHistoryRecord>,
    retry_events: Vec<SessionHistoryRecord>,
    progress: AttemptProgress,
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

    if progress == AttemptProgress::Stalled {
        let attempts = stalled_attempts(ctx, actions);
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

    let next_protocol_iteration = ctx.protocol_iteration() + 1;
    let reached_turn_limit = ctx
        .turn_budget()
        .max_turns()
        .is_some_and(|max_turns| next_protocol_iteration >= ctx.protocol_run_offset() + max_turns);
    if reached_turn_limit {
        // Final-turn-fresh doctrine: retry events, including the durable
        // reasoning record, are deliberately dropped at the turn limit.
        match decode_rlm_termination_options(ctx.termination())? {
            RlmTermination::FinishRequired { .. } => {
                actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                    TurnStop::MaxTurns,
                )));
                return Ok(());
            }
            RlmTermination::Natural => {
                if let Some(max_turns) = ctx.turn_budget().max_turns() {
                    actions.push(DriverAction::ScheduleTurnLimitFinal {
                        message: turn_limit_final_message(
                            dialect,
                            rlm_message_id(ctx.turn_id(), next_protocol_iteration, "turn_limit"),
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

fn bounded_exec_tool_call_records(
    calls: &[lash_core::ExecutedCall],
) -> (Vec<ToolCallRecord>, Option<OmittedToolCalls>) {
    // HostBridge supplies execution-index order, so concurrent dispatch keeps a
    // deterministic host-record order and first-128 retention boundary.
    let records = calls
        .iter()
        .filter_map(|call| call.host_record.as_ref())
        .collect::<Vec<_>>();
    let retained_count = records.len().min(MAX_EXEC_TOOL_CALL_RECORDS);
    let bounded = records[..retained_count]
        .iter()
        .map(|record| bounded_tool_call_record(record))
        .collect::<Vec<_>>();
    let omitted = &records[retained_count..];
    let summary = (!omitted.is_empty()).then(|| OmittedToolCalls {
        count: omitted.len(),
        failures: omitted
            .iter()
            .filter(|record| !record.output.is_success())
            .count(),
        attachments: omitted
            .iter()
            .flat_map(|record| tool_output_attachments(&record.output))
            .collect(),
    });
    (bounded, summary)
}

fn executed_call_ledger(records: &[lash_core::ExecutedCall]) -> (Vec<RlmExecutedCall>, usize) {
    let omitted = records.len().saturating_sub(MAX_EXEC_TOOL_CALL_RECORDS);
    let calls = records
        .iter()
        .skip(omitted)
        .map(|call| lash_core::ExecutedCallRecord {
            operation: call.operation.clone(),
            outcome: call.outcome,
        })
        .collect();
    (calls, omitted)
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
            frame_key,
            initial_nodes,
            task,
        } => ToolControl::SwitchAgentFrame {
            frame_key: frame_key.clone(),
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
        ToolValue::UntrustedJson(value) => ToolValue::untrusted_json(bounded_untrusted_json(value)),
        ToolValue::Null
        | ToolValue::Bool(_)
        | ToolValue::Number(_)
        | ToolValue::String(_)
        | ToolValue::Attachment(_) => value.clone(),
    }
}

fn bounded_untrusted_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) if value.len() > MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES => {
            serde_json::json!({ "omitted_bytes": value.len() })
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(bounded_untrusted_json).collect())
        }
        serde_json::Value::Object(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), bounded_untrusted_json(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn omitted_bytes_marker(omitted_bytes: usize) -> ToolValue {
    ToolValue::Object(BTreeMap::from([(
        "omitted_bytes".to_string(),
        ToolValue::untrusted_json(serde_json::json!(omitted_bytes)),
    )]))
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
    vocabulary: crate::dialect::DialectPromptVocabulary,
    turn_id: &TurnId,
    protocol_iteration: usize,
    state: &RlmDriverState,
    validation_error: Option<String>,
    final_output: Option<Value>,
) -> RlmTrajectoryEntry {
    let error = validation_error.or_else(|| {
        state
            .error
            .as_ref()
            .map(|failure| crate::feedback::render(failure, vocabulary.cell_noun))
    });
    RlmTrajectoryEntry {
        id: format!("lashlang_step_{turn_id}_{protocol_iteration}"),
        protocol_iteration,
        code: state.code.clone(),
        output: state.output.clone(),
        images: state.images.clone(),
        calls: state.calls.clone(),
        calls_omitted: state.calls_omitted,
        error,
        final_output,
    }
}

fn rlm_message_id(turn_id: &TurnId, protocol_iteration: usize, purpose: &str) -> String {
    format!("m_rlm_{turn_id}_{protocol_iteration}_{purpose}")
}

fn trajectory_events(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    turn_id: &TurnId,
    protocol_iteration: usize,
    state: &RlmDriverState,
    validation_error: Option<String>,
    final_output: Option<Value>,
) -> Vec<SessionHistoryRecord> {
    let entry = trajectory_entry(
        vocabulary,
        turn_id,
        protocol_iteration,
        state,
        validation_error,
        final_output,
    );
    vec![
        super::transport::execution_event(entry.id.clone(), state.assistant_parts.clone()),
        trajectory_event(entry),
    ]
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
