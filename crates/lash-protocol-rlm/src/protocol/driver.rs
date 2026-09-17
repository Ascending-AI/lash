use lash_sansio::TurnId;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, ProtocolDriverHandle, WaitingExecState,
    WaitingLlmState,
};
use lash_core::session_model::{
    ConversationRecord, Message, SessionHistoryRecord, SessionStreamEvent, TurnFailureCode,
    TurnFailureKind, make_error_event,
};
use lash_core::{
    CheckpointKind, DriverAction, DriverContextView, ExecResponse, LlmOutputPart, LlmResponse,
    LlmTerminalReason, OmittedToolCalls, ToolCallOutcome, ToolCallOutput, ToolCallRecord,
    ToolControl, ToolFailure, ToolValue, facade_support::TurnFinish, facade_support::TurnOutcome,
    facade_support::TurnStop, facade_support::append_assistant_text_part,
    facade_support::normalized_response_parts,
};
use lash_rlm_types::{
    CellOutcome, RlmDiagnosticEvent, RlmExecutedCall, RlmProtocolEvent, RlmTermination,
    RlmTrajectoryEntry,
};
use serde_json::Value;

#[cfg(feature = "testing")]
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};

use crate::dialect::TypescriptDialect;
use crate::projection::rlm_protocol_event;
use crate::rlm_support::decode_rlm_termination_options;

use super::actions::{invalid_driver_state_actions, invalid_turn_options_actions};
use super::cell::{
    CellExtraction, CellExtractionError, extract_cell, malformed_cell_fence,
    project_visible_assistant_prose_with_tags,
};
#[cfg(feature = "testing")]
use super::finish::internal_assistant_prose_message;
use super::finish::{
    finish_required_reminder_message, finish_schema_mismatch_message,
    internal_assistant_prose_message_for_turn, invalid_cell_message, no_progress_stop_message,
    output_limit_retry_message, validate_finish_value,
};
use super::stall::{
    ExtractionCounts, ExtractionDiagnostic, LLM_EXTRACTION_PHASE, NO_PROGRESS_BUDGET_PHASE,
    reply_fingerprint, stalled_attempts,
};
use super::state::{RlmDriverState, RlmReasoningPart, decode_rlm_driver_state, rlm_driver_state};

#[derive(Clone)]
pub struct RlmDriver {
    dialect: Arc<TypescriptDialect>,
}

impl RlmDriver {
    /// A driver on TypeScript, because it is the only language a session can be
    /// served (ADR 0096).
    ///
    /// This used to answer with the retired surface. With the dialect selector
    /// gone, a default that still named lashlang would be the compatibility
    /// reader this cutover exists to remove.
    pub fn new() -> Self {
        Self {
            dialect: Arc::new(crate::dialect::TypescriptDialect::prompt_only(
                lash_lashlang_runtime::LashlangSurface::default(),
            )),
        }
    }

    pub(crate) fn with_dialect(dialect: Arc<TypescriptDialect>) -> Self {
        Self { dialect }
    }

    /// The tail every stall-retry branch shares: the extraction diagnostic,
    /// the reply's durable assistant message when the projection carries
    /// content, the branch's retry message, and the nonterminal
    /// continue-or-stop. `assistant_message` names one projection for both
    /// guard and content, so a reply with nothing visible can never write a
    /// zero-part assistant message (S17-A2).
    fn stall_retry_epilogue(
        &self,
        ctx: &DriverContextView<'_>,
        actions: &mut Vec<DriverAction>,
        retry: StallRetry<'_>,
    ) -> Result<(), String> {
        actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
            LLM_EXTRACTION_PHASE,
            llm_extraction_payload(
                ctx.turn_id(),
                retry.fingerprint,
                retry.decision,
                retry.termination,
                prose_only_counts(self.dialect.language_id(), retry.raw_text, retry.reasoning),
            ),
        )]));
        let mut retry_events = Vec::new();
        if let Some((prose, purpose)) = retry.assistant_message
            && (!prose.trim().is_empty() || !retry.reasoning.is_empty())
        {
            retry_events.push(conversation_event(
                internal_assistant_prose_message_for_turn(
                    ctx.turn_id(),
                    rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), purpose),
                    prose.to_string(),
                    retry.reasoning,
                ),
            ));
        }
        retry_events.push(conversation_event(retry.retry));
        continue_or_stop_after_nonterminal(
            ctx,
            actions,
            Vec::new(),
            retry_events,
            AttemptProgress::Stalled,
        )
    }
}

impl Default for RlmDriver {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_EXEC_TOOL_CALL_RECORDS: usize = 128;
const MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES: usize = 64 * 1024;

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

        let projected = match project_response(llm_response.parts.clone()) {
            Ok(projected) => projected,
            Err(tool_call) => {
                let full_text = llm_response.full_text();
                actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
                    protocol_iteration: ctx.protocol_iteration(),
                    content: full_text.clone(),
                    duration_ms: 0,
                }));
                // A provider tool call on a request that declared no tools is
                // malformed provider output, not a protocol crime: the model
                // was never shown a tool surface, so the stray call gets the
                // extraction-failure repair the loop already runs for a reply
                // with no usable cell — one repair round, and the ordinary
                // stall bound decides the turn if the model repeats it
                // (FIG-2777).
                let termination = match decode_rlm_termination_options(ctx.termination()) {
                    Ok(termination) => termination,
                    Err(err) => return invalid_turn_options_actions(err),
                };
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    LLM_EXTRACTION_PHASE,
                    llm_extraction_payload(
                        ctx.turn_id(),
                        &reply_fingerprint(&full_text),
                        "retry_native_tool_call",
                        &termination,
                        prose_only_counts(self.dialect.language_id(), &full_text, &[]),
                    ),
                )]));
                let retry_events = vec![conversation_event(invalid_cell_message(
                    self.dialect.as_ref(),
                    rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "native_tool_call"),
                    &self.dialect.native_tool_call_copy(&tool_call.tool_name),
                ))];
                if let Err(err) = continue_or_stop_after_nonterminal(
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
        let visible_assistant_text =
            match project_response(normalized_response_parts(&llm_response)) {
                Ok(projected) => projected.assistant_text,
                Err(_) => {
                    unreachable!("raw RLM response projection already rejected native tool calls")
                }
            };
        let tags = self.dialect.cell_tags();
        let assistant_text = projected.assistant_text;
        let reasoning = projected.reasoning;
        let fingerprint = reply_fingerprint(&assistant_text);
        let extraction = extract_cell(&assistant_text, tags);
        let visible_prose = project_visible_assistant_prose_with_tags(
            if matches!(&extraction, Ok(Some(_))) {
                &assistant_text
            } else {
                &visible_assistant_text
            },
            tags,
        );
        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: visible_prose.clone(),
            duration_ms: 0,
        }));

        if assistant_text.trim().is_empty()
            && reasoning.iter().all(|part| part.text.trim().is_empty())
        {
            actions.push(DriverAction::Emit(make_error_event(
                TurnFailureKind::LlmProvider,
                Some(TurnFailureCode::EmptyResponse),
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

        let extraction = match extraction {
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
                if let Err(err) = self.stall_retry_epilogue(
                    &ctx,
                    &mut actions,
                    StallRetry {
                        decision,
                        fingerprint: &fingerprint,
                        termination: &termination,
                        raw_text: &assistant_text,
                        reasoning: &reasoning,
                        assistant_message: Some((&visible_prose, "assistant_response")),
                        retry: invalid_cell_message(
                            self.dialect.as_ref(),
                            rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "invalid_cell"),
                            &message,
                        ),
                    },
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }
        };
        let Some(cell) = extraction else {
            if terminal_reason == LlmTerminalReason::OutputLimit {
                if let Err(err) = self.stall_retry_epilogue(
                    &ctx,
                    &mut actions,
                    StallRetry {
                        decision: "retry_output_limit_prose",
                        fingerprint: &fingerprint,
                        termination: &termination,
                        raw_text: &assistant_text,
                        reasoning: &reasoning,
                        assistant_message: Some((
                            &visible_assistant_text,
                            "truncated_assistant_response",
                        )),
                        retry: output_limit_retry_message(
                            self.dialect.prompt_vocabulary(),
                            rlm_message_id(
                                ctx.turn_id(),
                                ctx.protocol_iteration(),
                                "output_limit_retry",
                            ),
                            ctx.generation()
                                .output_token_cap
                                .map(std::num::NonZeroUsize::get),
                        ),
                    },
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }
            // A reply that opened a line with the active dialect's tag and still
            // produced no cell put a fence somewhere the grammar refuses. Read
            // as prose it is silence: the model is asked to finish, answers with
            // the same reply, and nothing in the loop ever names what was wrong
            // with it (FIG-1475).
            //
            // Only where a cell is *required*. On a `Natural` turn prose is an
            // answer, and prose about cells — "`<typescript>` and `</typescript>`
            // are the tags you asked about" — opens a line with the tag while
            // being exactly what the user wanted. Correcting a fence there would
            // bury the answer under a lecture and spend the turn's attempts on a
            // reply that had nothing wrong with it.
            if !matches!(termination, RlmTermination::Natural)
                && malformed_cell_fence(&assistant_text, tags)
            {
                if let Err(err) = self.stall_retry_epilogue(
                    &ctx,
                    &mut actions,
                    StallRetry {
                        decision: "retry_malformed_cell_fence",
                        fingerprint: &fingerprint,
                        termination: &termination,
                        raw_text: &assistant_text,
                        reasoning: &reasoning,
                        assistant_message: Some((&visible_prose, "assistant_response")),
                        retry: invalid_cell_message(
                            self.dialect.as_ref(),
                            rlm_message_id(
                                ctx.turn_id(),
                                ctx.protocol_iteration(),
                                "malformed_cell_fence",
                            ),
                            &self.dialect.malformed_cell_fence_retry_copy(),
                        ),
                    },
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }
            if matches!(termination, RlmTermination::Natural) {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    LLM_EXTRACTION_PHASE,
                    llm_extraction_payload(
                        ctx.turn_id(),
                        &fingerprint,
                        "finish_prose",
                        &termination,
                        prose_only_counts(self.dialect.language_id(), &assistant_text, &reasoning),
                    ),
                )]));
                if !reasoning.is_empty() {
                    actions.push(DriverAction::AppendEvents(vec![conversation_event(
                        internal_assistant_prose_message_for_turn(
                            ctx.turn_id(),
                            rlm_message_id(
                                ctx.turn_id(),
                                ctx.protocol_iteration(),
                                "assistant_response",
                            ),
                            visible_assistant_text.clone(),
                            &reasoning,
                        ),
                    )]));
                }
                actions.push(DriverAction::StartCheckpoint {
                    checkpoint: CheckpointKind::BeforeCompletion,
                    on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                        TurnFinish::AssistantMessage {
                            text: visible_assistant_text.clone(),
                        },
                    )),
                });
                return actions;
            }
            let RlmTermination::FinishRequired { ref schema } = termination else {
                unreachable!("Natural returned above");
            };
            let assistant_message = if !visible_assistant_text.trim().is_empty() {
                Some((visible_assistant_text.as_str(), "assistant_prose"))
            } else if !reasoning.is_empty() {
                Some(("", "assistant_reasoning"))
            } else {
                None
            };
            if let Err(err) = self.stall_retry_epilogue(
                &ctx,
                &mut actions,
                StallRetry {
                    decision: "request_finish",
                    fingerprint: &fingerprint,
                    termination: &termination,
                    raw_text: &assistant_text,
                    reasoning: &reasoning,
                    assistant_message,
                    retry: finish_required_reminder_message(
                        self.dialect.as_ref(),
                        rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "finish_reminder"),
                        schema.is_some(),
                    ),
                },
            ) {
                return invalid_turn_options_actions(err);
            }
            return actions;
        };

        actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
            LLM_EXTRACTION_PHASE,
            llm_extraction_payload(
                ctx.turn_id(),
                &fingerprint,
                self.dialect.execution_diagnostic_name(),
                &termination,
                cell_counts(
                    self.dialect.language_id(),
                    &assistant_text,
                    &reasoning,
                    &cell,
                ),
            ),
        )]));

        let Some(raw_state) = waiting.take_driver_state() else {
            return invalid_driver_state_actions("missing RLM driver state".to_string());
        };
        let mut state = match decode_rlm_driver_state(raw_state) {
            Ok(state) => state,
            Err(err) => return invalid_driver_state_actions(err),
        };
        state.code = cell.code.clone();
        state.reasoning = reasoning;
        state.prose = cell.prose.clone();

        // Emit the raw cell source as a `Message` with kind
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
                // Fold the executor's `error` / `terminal_finish` pair into the
                // one outcome it describes; a pair carrying both resolves to
                // the failure rather than discarding it.
                let outcome = CellOutcome::from_parts(response.error, response.terminal_finish);
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
                match outcome {
                    CellOutcome::Running => {}
                    outcome => *state.outcome = outcome,
                }
                if let Some(outcome) = terminal_outcome {
                    actions.push(DriverAction::AppendEvents(trajectory_events(
                        self.dialect.prompt_vocabulary(),
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        &state,
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
                *state.outcome = CellOutcome::Failed(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    error,
                ));
            }
        }

        if let Some(finish_value) = state.outcome.terminal_value() {
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
                        self.dialect.prompt_vocabulary(),
                        ctx.turn_id(),
                        ctx.protocol_iteration(),
                        &state,
                        Some(CellOutcome::Failed(error_text.clone())),
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
                Some(CellOutcome::Finished(finish_value.clone())),
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
            trajectory_events(
                self.dialect.prompt_vocabulary(),
                ctx.turn_id(),
                ctx.protocol_iteration(),
                &state,
                None,
            ),
            Vec::new(),
            if state.outcome.is_failed() {
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
            LlmOutputPart::ToolCall { tool_name, .. } => {
                return Err(NativeToolCall { tool_name });
            }
        }
    }
    Ok(ProjectedResponse {
        assistant_text,
        reasoning,
    })
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
                    "RLM conformance history fixture contains native tool call `{}`",
                    tool_call.tool_name
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
///
/// Two consequences of that law are intended, not oversights. A loop that
/// raises while genuinely converging is bounded the same as one that is not —
/// the bound is set far above ordinary repair traffic precisely so that
/// convergence has room, and a repair that needs more attempts than the bound
/// is indistinguishable from a stall from outside the model. And an attempt
/// whose cell executed but only printed observations resets nothing here; that
/// is real work, and bounding *it* is the turn budget's job, not this one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptProgress {
    /// A cell executed and reported no error.
    Executed,
    /// No cell executed, or the one that did reported an error.
    Stalled,
}

fn continue_or_stop_after_nonterminal(
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

    if !retry_events.is_empty() {
        actions.push(DriverAction::AppendEvents(retry_events));
    }

    actions.push(DriverAction::StartCheckpoint {
        checkpoint: CheckpointKind::AfterWork,
        on_empty: CheckpointResumeAction::PrepareIteration,
    });
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
    entry_outcome: Option<CellOutcome<String>>,
) -> RlmTrajectoryEntry {
    // A step the driver adjudicated on the spot (schema-mismatch failure,
    // validated finish) names its outcome explicitly; otherwise the entry
    // records the state's failure, and a pending finish never leaks in.
    let outcome = entry_outcome.unwrap_or_else(|| match &*state.outcome {
        CellOutcome::Failed(failure) => {
            CellOutcome::Failed(crate::feedback::render(failure, vocabulary.cell_noun))
        }
        CellOutcome::Running | CellOutcome::Finished(_) => CellOutcome::Running,
    });
    RlmTrajectoryEntry {
        id: format!("lashlang_step_{turn_id}_{protocol_iteration}"),
        protocol_iteration,
        code: state.code.clone(),
        output: state.output.clone(),
        images: state.images.clone(),
        calls: state.calls.clone(),
        calls_omitted: state.calls_omitted,
        outcome,
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
    entry_outcome: Option<CellOutcome<String>>,
) -> Vec<SessionHistoryRecord> {
    let mut events = Vec::new();
    if let Some(event) =
        assistant_content_event(turn_id, protocol_iteration, &state.reasoning, &state.prose)
    {
        events.push(event);
    }
    events.push(trajectory_event(trajectory_entry(
        vocabulary,
        turn_id,
        protocol_iteration,
        state,
        entry_outcome,
    )));
    events
}

fn assistant_content_event(
    turn_id: &TurnId,
    protocol_iteration: usize,
    reasoning: &[RlmReasoningPart],
    prose: &str,
) -> Option<SessionHistoryRecord> {
    let id = rlm_message_id(turn_id, protocol_iteration, "assistant_content");
    let prose = prose.trim();
    (!reasoning.is_empty() || !prose.is_empty()).then(|| {
        conversation_event(internal_assistant_prose_message_for_turn(
            turn_id,
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

/// The counts for an attempt whose reply carried no executable cell.
fn prose_only_counts<'a>(
    language_id: &'a str,
    assistant_text: &str,
    reasoning: &[RlmReasoningPart],
) -> ExtractionCounts<'a> {
    let chars = assistant_text.chars().count();
    ExtractionCounts::prose(language_id, chars, chars, reasoning_chars(reasoning))
}

/// The counts for an attempt whose reply carried a cell. `full_text_chars`
/// covers the fences the cell arrived in, so it exceeds `prose + code`.
fn cell_counts<'a>(
    language_id: &'a str,
    assistant_text: &str,
    reasoning: &[RlmReasoningPart],
    cell: &CellExtraction,
) -> ExtractionCounts<'a> {
    ExtractionCounts::program(
        language_id,
        assistant_text.chars().count(),
        cell.prose.chars().count(),
        reasoning_chars(reasoning),
        cell.code.chars().count(),
        cell.cell_count,
    )
}

fn reasoning_chars(reasoning: &[RlmReasoningPart]) -> usize {
    crate::protocol::stall::reasoning_diagnostic_chars(
        reasoning
            .iter()
            .map(|part| (part.text.as_str(), part.replay.as_ref())),
    )
}

/// One stall-retry branch's contribution to the shared epilogue: which
/// diagnostic decision to record, which reply projection (if any) becomes the
/// durable assistant message, and the retry message the model sees next.
struct StallRetry<'a> {
    /// Diagnostic decision token (`retry_unclosed_cell`, `request_finish`, …).
    decision: &'static str,
    /// Fingerprint of the reply as received.
    fingerprint: &'a str,
    /// Decoded termination options, recorded in the diagnostic.
    termination: &'a RlmTermination,
    /// Raw assistant text, feeding the diagnostic's character counters.
    raw_text: &'a str,
    /// Reply reasoning, counted in the diagnostic and carried into the
    /// retained assistant message.
    reasoning: &'a [RlmReasoningPart],
    /// `(projection, message purpose)` for the retained assistant message. The
    /// same string is guard and content; `None` emits no assistant message.
    assistant_message: Option<(&'a str, &'static str)>,
    /// The retry or reminder message the model sees next.
    retry: Message,
}

fn llm_extraction_payload(
    turn_id: &TurnId,
    reply_fingerprint: &str,
    decision: &str,
    termination: &RlmTermination,
    counts: ExtractionCounts<'_>,
) -> Value {
    ExtractionDiagnostic::new(turn_id, reply_fingerprint, decision, termination, counts).payload()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{
        AttachmentId, AttachmentSource, AttachmentTypeMetadata, MediaType, ToolCancellation,
        ToolFailureClass, facade_support::AttachmentMeta,
    };

    fn image_ref(id: &str) -> AttachmentSource {
        AttachmentSource::stored(
            AttachmentMeta::new(
                AttachmentId::parse(id).expect("valid attachment id"),
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
        let first = rlm_message_id(&TurnId::from("turn-1"), 0, "assistant_content");
        let replay = rlm_message_id(&TurnId::from("turn-1"), 0, "assistant_content");
        let next_turn = rlm_message_id(&TurnId::from("turn-2"), 0, "assistant_content");

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

        assert_eq!(reasoning_chars(&reasoning), 1);
        assert_eq!(
            serde_json::to_value(prose_only_counts("typescript", "", &reasoning)).unwrap(),
            serde_json::json!({
                "full_text_chars": 0,
                "prose_chars": 0,
                "code_chars": 0,
                "reasoning_chars": 1,
                "typescript_cell_count": 0,
            }),
            "an attempt that ran no program renders its code counters as zero"
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

    fn call(index: usize, output: ToolCallOutput) -> lash_core::ExecutedCall {
        let outcome = if output.is_success() {
            lash_core::ExecutedCallOutcome::Ok
        } else {
            lash_core::ExecutedCallOutcome::Err
        };
        lash_core::ExecutedCall {
            operation: format!("module.call_{index}"),
            outcome,
            host_record: Some(record(index, output)),
        }
    }

    #[test]
    fn bounded_output_replaces_oversized_scalars_without_losing_structure_or_attachments() {
        let attachment = image_ref("nested-attachment");
        let oversized = "x".repeat(MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES + 17);
        let output = ToolCallOutput::success_tool_value(ToolValue::Object(BTreeMap::from([
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
            "failure recovered by the cell program",
        );
        failure.raw = Some(ToolValue::Array(vec![
            ToolValue::String(oversized.clone()),
            ToolValue::Attachment(failure_attachment.clone()),
        ]));
        let cancellation = ToolCancellation {
            origin: None,
            message: "cancelled".to_string(),
            source: lash_core::ToolFailureSource::Cancellation,
            raw: Some(ToolValue::Array(vec![
                ToolValue::String(oversized.clone()),
                ToolValue::Attachment(cancellation_attachment.clone()),
            ])),
        };

        let (bounded, omitted) = bounded_exec_tool_call_records(&[
            call(0, ToolCallOutput::failure(failure)),
            call(1, ToolCallOutput::cancelled(cancellation)),
        ]);

        assert!(omitted.is_none());
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
    fn typed_omission_preserves_counts_failures_and_attachments() {
        let attachment = image_ref("overflow-attachment");
        let mut calls = (0..MAX_EXEC_TOOL_CALL_RECORDS + 3)
            .map(|index| call(index, ToolCallOutput::success(serde_json::json!(index))))
            .collect::<Vec<_>>();
        calls[MAX_EXEC_TOOL_CALL_RECORDS + 1] = call(
            MAX_EXEC_TOOL_CALL_RECORDS + 1,
            ToolCallOutput::failure(ToolFailure::tool(
                ToolFailureClass::Execution,
                "recovered_failure",
                "failure recovered by the cell program",
            )),
        );
        calls[MAX_EXEC_TOOL_CALL_RECORDS + 2] = call(
            MAX_EXEC_TOOL_CALL_RECORDS + 2,
            ToolCallOutput::success_tool_value(ToolValue::Attachment(attachment.clone())),
        );

        let (bounded, omitted) = bounded_exec_tool_call_records(&calls);

        assert_eq!(bounded.len(), MAX_EXEC_TOOL_CALL_RECORDS);
        assert_eq!(
            omitted,
            Some(OmittedToolCalls {
                count: 3,
                failures: 1,
                attachments: vec![attachment],
            })
        );
    }

    #[test]
    fn executed_call_ledger_elides_arguments_and_keeps_the_diagnostic_tail() {
        let records = (0..MAX_EXEC_TOOL_CALL_RECORDS + 3)
            .map(|index| lash_core::ExecutedCall {
                operation: format!("module.call_{index}"),
                outcome: if index == MAX_EXEC_TOOL_CALL_RECORDS + 2 {
                    lash_core::ExecutedCallOutcome::Err
                } else {
                    lash_core::ExecutedCallOutcome::Ok
                },
                host_record: None,
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
    fn trajectory_capture_preserves_model_visible_error() {
        let raw_error = "read failed at /workspace/private/secret.txt";
        let state = RlmDriverState {
            code: "read()".to_string(),
            outcome: CellOutcome::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                raw_error,
            ))
            .into(),
            ..RlmDriverState::default()
        };

        let vocabulary = crate::dialect::DialectPromptVocabulary::default();
        let entry = trajectory_entry(vocabulary, &TurnId::from("turn"), 0, &state, None);
        let error = entry.outcome.error().expect("captured public error");

        assert_eq!(entry.code, "read()");
        assert_eq!(
            error,
            &format!(
                "{raw_error}\n\nNext: the host failed while handling this cell. Retry it; if the failure persists, report the host problem."
            )
        );
    }

    #[test]
    fn all_success_omission_does_not_report_a_failure() {
        let calls = (0..MAX_EXEC_TOOL_CALL_RECORDS + 1)
            .map(|index| call(index, ToolCallOutput::success(serde_json::json!(index))))
            .collect::<Vec<_>>();

        let (bounded, omitted) = bounded_exec_tool_call_records(&calls);
        let omitted = omitted.expect("typed omission");

        assert_eq!(bounded.len(), MAX_EXEC_TOOL_CALL_RECORDS);
        assert_eq!(omitted.count, 1);
        assert_eq!(omitted.failures, 0);
        assert!(omitted.attachments.is_empty());
    }
}
