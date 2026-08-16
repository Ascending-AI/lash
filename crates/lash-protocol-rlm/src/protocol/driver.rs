use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, ProtocolDriverHandle, WaitingExecState,
    WaitingLlmState,
};
use lash_core::session_model::{
    ConversationRecord, Message, SessionHistoryRecord, SessionStreamEvent, make_error_envelope,
    make_error_event,
};
use lash_core::{
    CheckpointKind, DriverAction, DriverContextView, ExecResponse, LlmOutputPart, LlmResponse,
    LlmTerminalReason, ToolCallOutcome, ToolCallOutput, ToolCallRecord, ToolControl, ToolFailure,
    ToolFailureClass, ToolValue, facade_support::TurnFinish, facade_support::TurnOutcome,
    facade_support::TurnStop, facade_support::append_assistant_text_part,
    facade_support::normalized_response_parts,
};
use lash_rlm_types::{
    RlmDiagnosticEvent, RlmExecutedCall, RlmProtocolEvent, RlmTermination, RlmTrajectoryEntry,
};
use serde_json::Value;

#[cfg(feature = "testing")]
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};

use crate::dialect::{LashlangDialect, RlmDialect};
use crate::projection::rlm_protocol_event;
use crate::rlm_support::decode_rlm_termination_options;

use super::actions::{invalid_driver_state_actions, invalid_turn_options_actions};
use super::cell::{
    CellExtraction, CellExtractionError, extract_cell, project_visible_assistant_prose_with_tags,
};
use super::finish::{
    finish_required_reminder_message, finish_schema_mismatch_message,
    internal_assistant_prose_message, invalid_cell_message, no_progress_stop_message,
    output_limit_retry_message, turn_limit_final_message, validate_finish_value,
};
use super::state::{RlmDriverState, RlmReasoningPart, decode_rlm_driver_state, rlm_driver_state};

#[derive(Clone)]
pub struct RlmDriver {
    redaction_roots: Arc<[PathBuf]>,
    dialect: Arc<dyn RlmDialect>,
}

impl RlmDriver {
    pub fn new(redaction_roots: Arc<[PathBuf]>) -> Self {
        Self {
            redaction_roots,
            dialect: Arc::new(LashlangDialect::prompt_only(
                lash_lashlang_runtime::LashlangSurface::default(),
            )),
        }
    }

    /// A driver pinned to a registered dialect by language id.
    ///
    /// The plugin builds its driver from a live session, which owns the
    /// dialect; this is the seam for anything that needs a driver *without* a
    /// session — protocol-level assertions that must hold in a session which is
    /// not the default one. Asserting only the Lashlang direction cannot tell a
    /// dialect-generic implementation from one that happens to name Lashlang.
    ///
    /// Panics on an unregistered language id, which is a programming error:
    /// the registry is the authority and it is closed.
    pub fn for_language(redaction_roots: Arc<[PathBuf]>, language: &str) -> Self {
        let dialect: Arc<dyn RlmDialect> = match language {
            crate::dialect::typescript::LANGUAGE_ID => {
                Arc::new(crate::dialect::TypescriptDialect::prompt_only(
                    lash_lashlang_runtime::LashlangSurface::default(),
                ))
            }
            crate::dialect::lashlang::LANGUAGE_ID => Arc::new(LashlangDialect::prompt_only(
                lash_lashlang_runtime::LashlangSurface::default(),
            )),
            other => panic!("unregistered dialect `{other}`"),
        };
        Self::with_dialect(redaction_roots, dialect)
    }

    pub(crate) fn with_dialect(
        redaction_roots: Arc<[PathBuf]>,
        dialect: Arc<dyn RlmDialect>,
    ) -> Self {
        Self {
            redaction_roots,
            dialect,
        }
    }
}

impl Default for RlmDriver {
    fn default() -> Self {
        Self::new(Arc::from([]))
    }
}

const MAX_EXEC_TOOL_CALL_RECORDS: usize = 128;
/// Diagnostic phase emitted exactly once per provider attempt.
const LLM_EXTRACTION_PHASE: &str = "llm_extraction";
/// Diagnostic phase that records a turn stopped by its no-progress budget.
const NO_PROGRESS_BUDGET_PHASE: &str = "no_progress_budget";
const MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES: usize = 64 * 1024;
const EXEC_TOOL_CALL_OVERFLOW_NAME: &str = "lash.exec_tool_call_overflow";

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for RlmDriver {
    fn project_visible_assistant_prose(&self, text: &str) -> String {
        super::cell::project_visible_assistant_prose_with_tags(text, self.dialect.cell_tags())
    }

    fn handles_output_limit_response(&self) -> bool {
        true
    }

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
        let terminal_reason = llm_response.terminal_reason;
        let mut actions = Vec::new();

        let projected = match project_response(normalized_response_parts(&llm_response)) {
            Ok(projected) => projected,
            Err(tool_call) => {
                actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
                    protocol_iteration: ctx.protocol_iteration(),
                    content: llm_response.full_text,
                    duration_ms: 0,
                }));
                native_tool_call_failure_actions(&mut actions, ctx.protocol_iteration(), tool_call);
                return actions;
            }
        };
        let tags = self.dialect.cell_tags();
        let assistant_text = projected.assistant_text;
        let reasoning = projected.reasoning;
        let visible_prose = project_visible_assistant_prose_with_tags(&assistant_text, tags);
        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: visible_prose.clone(),
            duration_ms: 0,
        }));

        if assistant_text.trim().is_empty()
            && reasoning.iter().all(|part| part.text.trim().is_empty())
        {
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

        let extraction = match extract_cell(&assistant_text, tags) {
            Ok(extraction) => extraction,
            Err(err) => {
                let (decision, message) = match (err, terminal_reason) {
                    (CellExtractionError::UnclosedCell, LlmTerminalReason::OutputLimit) => (
                        "retry_output_limit_cell",
                        self.dialect.output_limit_cell_copy(
                            ctx.generation()
                                .output_token_cap
                                .map(std::num::NonZeroUsize::get),
                        ),
                    ),
                    (CellExtractionError::UnclosedCell, _) => {
                        ("retry_unclosed_cell", self.dialect.cell_error_message(err))
                    }
                };
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    LLM_EXTRACTION_PHASE,
                    llm_extraction_payload(
                        decision,
                        &termination,
                        LlmExtractionCounts::prose_only(&assistant_text, &reasoning),
                    ),
                )]));
                let mut retry_events = Vec::new();
                let retry_prose = visible_prose;
                if !retry_prose.trim().is_empty() || !reasoning.is_empty() {
                    retry_events.push(conversation_event(internal_assistant_prose_message(
                        rlm_message_id(
                            ctx.turn_id(),
                            ctx.protocol_iteration(),
                            "assistant_response",
                        ),
                        retry_prose,
                        &reasoning,
                    )));
                }
                retry_events.push(conversation_event(invalid_cell_message(
                    self.dialect.as_ref(),
                    rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "invalid_cell"),
                    &message,
                )));
                if let Err(err) = continue_or_stop_after_nonterminal(
                    self.dialect.as_ref(),
                    &ctx,
                    &mut actions,
                    Vec::new(),
                    retry_events,
                    AttemptProgress::Stalled,
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }
        };
        let Some(cell) = extraction else {
            // A cell of a registered-but-inactive dialect is recognized rather
            // than read as prose. Falling through here is what turned a
            // mis-dialected scripted reply into an unbounded re-prompt: the
            // model was asked to finish, answered with the same cell, and
            // nothing in the loop could ever name the mismatch.
            if let Some((_foreign_language, foreign_tags)) =
                super::cell::foreign_dialect_cell(&assistant_text, tags)
            {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    LLM_EXTRACTION_PHASE,
                    llm_extraction_payload(
                        "retry_foreign_dialect_cell",
                        &termination,
                        LlmExtractionCounts::prose_only(&assistant_text, &reasoning),
                    ),
                )]));
                let mut retry_events = Vec::new();
                if !visible_prose.trim().is_empty() || !reasoning.is_empty() {
                    retry_events.push(conversation_event(internal_assistant_prose_message(
                        rlm_message_id(
                            ctx.turn_id(),
                            ctx.protocol_iteration(),
                            "assistant_response",
                        ),
                        visible_prose,
                        &reasoning,
                    )));
                }
                retry_events.push(conversation_event(invalid_cell_message(
                    self.dialect.as_ref(),
                    rlm_message_id(
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        "foreign_dialect_cell",
                    ),
                    &self.dialect.foreign_cell_retry_copy(foreign_tags.open),
                )));
                if let Err(err) = continue_or_stop_after_nonterminal(
                    self.dialect.as_ref(),
                    &ctx,
                    &mut actions,
                    Vec::new(),
                    retry_events,
                    AttemptProgress::Stalled,
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }
            if terminal_reason == LlmTerminalReason::OutputLimit {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    LLM_EXTRACTION_PHASE,
                    llm_extraction_payload(
                        "retry_output_limit_prose",
                        &termination,
                        LlmExtractionCounts::prose_only(&assistant_text, &reasoning),
                    ),
                )]));
                let mut retry_events = Vec::new();
                if !assistant_text.trim().is_empty() || !reasoning.is_empty() {
                    retry_events.push(conversation_event(internal_assistant_prose_message(
                        rlm_message_id(
                            ctx.turn_id(),
                            ctx.protocol_iteration(),
                            "truncated_assistant_response",
                        ),
                        assistant_text,
                        &reasoning,
                    )));
                }
                retry_events.push(conversation_event(output_limit_retry_message(
                    self.dialect.prompt_vocabulary(),
                    rlm_message_id(
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        "output_limit_retry",
                    ),
                    ctx.generation()
                        .output_token_cap
                        .map(std::num::NonZeroUsize::get),
                )));
                if let Err(err) = continue_or_stop_after_nonterminal(
                    self.dialect.as_ref(),
                    &ctx,
                    &mut actions,
                    Vec::new(),
                    retry_events,
                    AttemptProgress::Stalled,
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }
            if matches!(termination, RlmTermination::Natural) {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    LLM_EXTRACTION_PHASE,
                    llm_extraction_payload(
                        "finish_prose",
                        &termination,
                        LlmExtractionCounts::prose_only(&assistant_text, &reasoning),
                    ),
                )]));
                if !reasoning.is_empty() {
                    actions.push(DriverAction::AppendEvents(vec![conversation_event(
                        internal_assistant_prose_message(
                            rlm_message_id(
                                ctx.turn_id(),
                                ctx.protocol_iteration(),
                                "assistant_response",
                            ),
                            assistant_text.clone(),
                            &reasoning,
                        ),
                    )]));
                }
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
                LLM_EXTRACTION_PHASE,
                llm_extraction_payload(
                    "request_finish",
                    &termination,
                    LlmExtractionCounts::prose_only(&assistant_text, &reasoning),
                ),
            )]));
            let mut events = Vec::new();
            if !assistant_text.trim().is_empty() {
                events.push(conversation_event(internal_assistant_prose_message(
                    rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "assistant_prose"),
                    assistant_text,
                    &reasoning,
                )));
            } else if !reasoning.is_empty() {
                events.push(conversation_event(internal_assistant_prose_message(
                    rlm_message_id(
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        "assistant_reasoning",
                    ),
                    String::new(),
                    &reasoning,
                )));
            }
            events.push(conversation_event(finish_required_reminder_message(
                self.dialect.as_ref(),
                rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "finish_reminder"),
                schema.is_some(),
            )));
            if let Err(err) = continue_or_stop_after_nonterminal(
                self.dialect.as_ref(),
                &ctx,
                &mut actions,
                Vec::new(),
                events,
                AttemptProgress::Stalled,
            ) {
                return invalid_turn_options_actions(err);
            }
            return actions;
        };

        actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
            LLM_EXTRACTION_PHASE,
            llm_extraction_payload(
                self.dialect.execution_diagnostic_name(),
                &termination,
                LlmExtractionCounts::cell(&assistant_text, &reasoning, &cell),
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
        state.reasoning = reasoning;
        state.prose = cell.prose.clone();

        // Emit the raw lashlang source as a `Message` with kind
        // `lashlang_code` so the CLI can reveal it in the full-expand
        // view (Alt+O) above the tool activities it produced.
        actions.push(DriverAction::Emit(SessionStreamEvent::Message {
            text: cell.code.clone(),
            kind: self.dialect.code_stream_kind().to_string(),
        }));
        actions.push(DriverAction::StartExec {
            language: self.dialect.language_id().to_string(),
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
                (state.calls, state.calls_omitted) = executed_call_ledger(&response.executed_calls);
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
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        &state,
                        &self.redaction_roots,
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
                    self.dialect.as_ref(),
                    &ctx,
                    &mut actions,
                    trajectory_events(
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        &state,
                        &self.redaction_roots,
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
                ctx.turn_id(),
                ctx.protocol_iteration(),
                &state,
                &self.redaction_roots,
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
                ctx.turn_id(),
                ctx.protocol_iteration(),
                &state,
                &self.redaction_roots,
                None,
                None,
            ),
            Vec::new(),
            if state.exec_error.is_some() {
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

struct ProjectedResponse {
    assistant_text: String,
    reasoning: Vec<RlmReasoningPart>,
}

#[derive(Debug)]
struct NativeToolCall {
    call_id: String,
    tool_name: String,
}

fn project_response(parts: Vec<LlmOutputPart>) -> Result<ProjectedResponse, NativeToolCall> {
    let mut assistant_text = String::new();
    let mut reasoning = Vec::new();
    for part in parts {
        match part {
            LlmOutputPart::Text { text, .. } => {
                append_assistant_text_part(&mut assistant_text, &text);
            }
            LlmOutputPart::Reasoning { text, replay } => {
                let text = if text.trim().is_empty() {
                    replay
                        .as_ref()
                        .map(|meta| meta.summary.join("\n\n"))
                        .unwrap_or_default()
                } else {
                    text
                };
                if !text.trim().is_empty() || replay.as_ref().is_some_and(|meta| !meta.is_empty()) {
                    reasoning.push(RlmReasoningPart { text, replay });
                }
            }
            LlmOutputPart::ToolCall {
                call_id, tool_name, ..
            } => {
                return Err(NativeToolCall { call_id, tool_name });
            }
        }
    }
    Ok(ProjectedResponse {
        assistant_text,
        reasoning,
    })
}

fn native_tool_call_failure_actions(
    actions: &mut Vec<DriverAction>,
    protocol_iteration: usize,
    tool_call: NativeToolCall,
) {
    let message = format!(
        "RLM protocol received native provider tool call `{}`; RLM tools must flow through Lashlang, so native provider tool calls are not allowed",
        tool_call.tool_name
    );
    let mut envelope = make_error_envelope(
        "rlm_protocol",
        Some("native_tool_call_not_allowed"),
        None,
        message.clone(),
        Some(format!(
            "tool_name={}, call_id={}, protocol_iteration={protocol_iteration}",
            tool_call.tool_name, tool_call.call_id
        )),
    );
    envelope.retryable = Some(false);
    actions.extend([
        DriverAction::AppendEvents(vec![diagnostic_event(
            "protocol_contract_violation",
            serde_json::json!({
                "code": "native_tool_call_not_allowed",
                "tool_name": tool_call.tool_name,
                "call_id": tool_call.call_id,
                "protocol_iteration": protocol_iteration,
                "constraint": "RLM tools must flow through Lashlang",
                "retryable": false,
            }),
        )]),
        DriverAction::Emit(SessionStreamEvent::Error {
            message,
            envelope: Some(envelope),
        }),
        DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
    ]);
}

/// Test support for exercising the production RLM response-to-history seam.
/// The bridge uses the same typed response projection, durable `Part`
/// representation, JSON round trip, and RLM history renderer as production.
#[cfg(feature = "testing")]
pub fn project_conformance_messages_through_rlm_history(
    messages: Vec<LlmMessage>,
) -> Result<Vec<LlmMessage>, String> {
    messages
        .into_iter()
        .map(|message| {
            if !matches!(message.role, LlmRole::Assistant) {
                return Ok(message);
            }
            let parts = message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text {
                        text,
                        response_meta,
                        ..
                    } => Some(LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: response_meta.clone(),
                    }),
                    LlmContentBlock::Reasoning { text, replay } => Some(LlmOutputPart::Reasoning {
                        text: text.clone(),
                        replay: replay.clone(),
                    }),
                    LlmContentBlock::ToolCall {
                        call_id,
                        tool_name,
                        input_json,
                        replay,
                    } => Some(LlmOutputPart::ToolCall {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        input_json: input_json.clone(),
                        replay: replay.clone(),
                    }),
                    LlmContentBlock::Attachment { .. } | LlmContentBlock::ToolResult { .. } => None,
                })
                .collect();
            let projected = project_response(parts).map_err(|tool_call| {
                format!(
                    "RLM conformance history fixture contains native tool call `{}` ({})",
                    tool_call.tool_name, tool_call.call_id
                )
            })?;
            let durable = internal_assistant_prose_message(
                "conformance.rlm.assistant".to_string(),
                projected.assistant_text,
                &projected.reasoning,
            );
            let encoded = serde_json::to_string(&durable).map_err(|err| {
                format!("RLM conformance history message did not serialize: {err}")
            })?;
            let durable: Message = serde_json::from_str(&encoded).map_err(|err| {
                format!("RLM conformance history message did not deserialize: {err}")
            })?;
            crate::driver::render_conformance_history_message(durable)
        })
        .collect()
}

/// Whether the attempt that is being continued past left a successful
/// execution committed to the turn.
///
/// This is the reset condition for the no-progress budget, and it is
/// deliberately narrower than "appended something": an attempt whose cell only
/// raised commits a trajectory entry carrying that error, and a model that
/// raises the same error forever has made exactly as much progress as one
/// whose cell never parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptProgress {
    /// A cell executed and reported no error.
    Executed,
    /// No cell executed, or the one that did reported an error.
    Stalled,
}

/// Consecutive provider attempts, ending at the turn's latest record, that
/// committed no error-free execution.
///
/// Derived from the turn's own durable records rather than carried in driver
/// state: driver state is rebuilt per protocol iteration, and a counter that
/// survives park, replay, and continuation has to be readable from what the
/// turn committed. Every attempt appends exactly one `llm_extraction`
/// diagnostic, so counting those since the last error-free trajectory entry
/// counts attempts.
///
/// The pending actions are counted too. Whether this attempt's own diagnostic
/// is already committed depends on which handler is deciding — the extraction
/// paths decide in the same action batch that appends it, the execution path
/// decides an effect later — and a count that reads only committed history
/// would be one short on exactly one of them.
fn stalled_attempts(ctx: &DriverContextView<'_>, actions: &[DriverAction]) -> usize {
    fn count(records: &[SessionHistoryRecord], attempts: &mut usize) {
        for record in records {
            let SessionHistoryRecord::Protocol(event) = record else {
                continue;
            };
            match crate::projection::decode_rlm_protocol_event(event) {
                Some(RlmProtocolEvent::RlmTrajectoryEntry(entry)) if entry.error.is_none() => {
                    *attempts = 0;
                }
                Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                    if diagnostic.phase == LLM_EXTRACTION_PHASE =>
                {
                    *attempts += 1;
                }
                _ => {}
            }
        }
    }

    let mut attempts = 0;
    count(ctx.events(), &mut attempts);
    for action in actions {
        if let DriverAction::AppendEvents(records) = action {
            count(records, &mut attempts);
        }
    }
    attempts
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

fn executed_call_ledger(
    records: &[lash_core::ExecutedCallRecord],
) -> (Vec<RlmExecutedCall>, usize) {
    let omitted = records.len().saturating_sub(MAX_EXEC_TOOL_CALL_RECORDS);
    let calls = records.iter().skip(omitted).cloned().collect();
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
    turn_id: &str,
    protocol_iteration: usize,
    state: &RlmDriverState,
    redaction_roots: &[PathBuf],
    validation_error: Option<String>,
    final_output: Option<Value>,
) -> RlmTrajectoryEntry {
    let error = validation_error.or_else(|| state.exec_error.clone());
    RlmTrajectoryEntry {
        id: format!("lashlang_step_{turn_id}_{protocol_iteration}"),
        protocol_iteration,
        code: state.executed_code.clone().unwrap_or_default(),
        output: state.output.clone(),
        images: state.images.clone(),
        calls: state.calls.clone(),
        calls_omitted: state.calls_omitted,
        error: error
            .as_deref()
            .map(|error| crate::public_error::public_error_message(error, redaction_roots)),
        final_output,
    }
}

fn rlm_message_id(turn_id: &str, protocol_iteration: usize, purpose: &str) -> String {
    format!("m_rlm_{turn_id}_{protocol_iteration}_{purpose}")
}

fn trajectory_events(
    turn_id: &str,
    protocol_iteration: usize,
    state: &RlmDriverState,
    redaction_roots: &[PathBuf],
    validation_error: Option<String>,
    final_output: Option<Value>,
) -> Vec<SessionHistoryRecord> {
    let mut events = Vec::new();
    if let Some(event) =
        assistant_content_event(turn_id, protocol_iteration, &state.reasoning, &state.prose)
    {
        events.push(event);
    }
    events.push(trajectory_event(trajectory_entry(
        turn_id,
        protocol_iteration,
        state,
        redaction_roots,
        validation_error,
        final_output,
    )));
    events
}

fn assistant_content_event(
    turn_id: &str,
    protocol_iteration: usize,
    reasoning: &[RlmReasoningPart],
    prose: &str,
) -> Option<SessionHistoryRecord> {
    let id = rlm_message_id(turn_id, protocol_iteration, "assistant_content");
    let prose = prose.trim();
    (!reasoning.is_empty() || !prose.is_empty()).then(|| {
        conversation_event(internal_assistant_prose_message(
            id,
            prose.to_string(),
            reasoning,
        ))
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
    fn prose_only(assistant_text: &str, reasoning: &[RlmReasoningPart]) -> Self {
        Self {
            full_text_chars: assistant_text.chars().count(),
            prose_chars: assistant_text.chars().count(),
            code_chars: 0,
            reasoning_chars: reasoning_diagnostic_chars(reasoning),
            lashlang_cell_count: 0,
        }
    }

    fn cell(assistant_text: &str, reasoning: &[RlmReasoningPart], cell: &CellExtraction) -> Self {
        Self {
            full_text_chars: assistant_text.chars().count(),
            prose_chars: cell.prose.chars().count(),
            code_chars: cell.code.chars().count(),
            reasoning_chars: reasoning_diagnostic_chars(reasoning),
            lashlang_cell_count: cell.lashlang_cell_count,
        }
    }
}

fn reasoning_diagnostic_chars(reasoning: &[RlmReasoningPart]) -> usize {
    reasoning
        .iter()
        .map(|part| {
            part.text.chars().count().max(usize::from(
                part.replay
                    .as_ref()
                    .is_some_and(|replay| !replay.is_empty()),
            ))
        })
        .sum()
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
        AttachmentId, AttachmentSource, AttachmentTypeMetadata, MediaType, ToolCancellation,
        facade_support::AttachmentMeta,
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

    #[test]
    fn protocol_message_ids_include_turn_identity() {
        let first = rlm_message_id("turn-1", 0, "assistant_content");
        let replay = rlm_message_id("turn-1", 0, "assistant_content");
        let next_turn = rlm_message_id("turn-2", 0, "assistant_content");

        assert_eq!(first, replay);
        assert_ne!(first, next_turn);
    }

    #[test]
    fn response_projection_preserves_reasoning_replay_byte_faithfully() {
        let replay = lash_core::llm::types::ProviderReasoningReplay {
            item_id: Some("reasoning-item".to_string()),
            encrypted_content: Some(" \nopaque-replay-e\u{301}\n ".to_string()),
            signature: Some("signed".to_string()),
            redacted: true,
            summary: vec!["summary".to_string()],
            ..Default::default()
        };

        let projected = project_response(vec![LlmOutputPart::Reasoning {
            text: "trajectory summary".to_string(),
            replay: Some(replay.clone()),
        }])
        .expect("reasoning is valid in RLM");

        assert_eq!(projected.reasoning.len(), 1);
        assert_eq!(projected.reasoning[0].text, "trajectory summary");
        assert_eq!(projected.reasoning[0].replay, Some(replay));
    }

    #[test]
    fn extraction_counts_record_opaque_reasoning_replay_presence() {
        let reasoning = [RlmReasoningPart {
            text: String::new(),
            replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                item_id: Some("opaque".to_string()),
                encrypted_content: Some("encrypted-only".to_string()),
                signature: None,
                redacted: false,
                summary: Vec::new(),
                ..Default::default()
            }),
        }];

        assert_eq!(reasoning_diagnostic_chars(&reasoning), 1);
        assert_eq!(
            LlmExtractionCounts::prose_only("", &reasoning).reasoning_chars,
            1
        );
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
    fn executed_call_ledger_elides_arguments_and_keeps_the_diagnostic_tail() {
        let records = (0..MAX_EXEC_TOOL_CALL_RECORDS + 3)
            .map(|index| lash_core::ExecutedCallRecord {
                operation: format!("module.call_{index}"),
                outcome: if index == MAX_EXEC_TOOL_CALL_RECORDS + 2 {
                    lash_core::ExecutedCallOutcome::Err
                } else {
                    lash_core::ExecutedCallOutcome::Ok
                },
            })
            .collect::<Vec<_>>();

        let (calls, omitted) = executed_call_ledger(&records);

        assert_eq!(calls.len(), MAX_EXEC_TOOL_CALL_RECORDS);
        assert_eq!(omitted, 3);
        assert_eq!(calls[0].operation, "module.call_3");
        assert_eq!(
            calls.last().expect("retained tail").operation,
            format!("module.call_{}", MAX_EXEC_TOOL_CALL_RECORDS + 2)
        );
        assert_eq!(calls[0].outcome, lash_rlm_types::RlmExecutedCallOutcome::Ok);
        assert_eq!(
            calls.last().expect("retained tail").outcome,
            lash_rlm_types::RlmExecutedCallOutcome::Err
        );
    }

    #[test]
    fn trajectory_capture_redacts_model_visible_error_before_journaling() {
        let workspace = std::path::PathBuf::from("/workspace");
        let private_path = workspace.join("private").join("secret.txt");
        let state = RlmDriverState {
            exec_error: Some(format!("read failed at {}", private_path.display())),
            ..RlmDriverState::default()
        };

        let entry = trajectory_entry(
            "turn",
            0,
            &state,
            std::slice::from_ref(&workspace),
            None,
            None,
        );
        let error = entry.error.expect("captured public error");

        assert!(!error.contains(&workspace.display().to_string()));
        assert_eq!(error, "read failed at private/secret.txt");
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
