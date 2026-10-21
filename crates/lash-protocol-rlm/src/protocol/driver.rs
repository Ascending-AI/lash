use lash_protocol_types::TurnId;
use std::sync::Arc;

use crate::rlm_support::RlmCompletion;
use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, PendingWork, ProtocolDriverHandle,
};
use lash_core::session_model::{
    ConversationRecord, Message, SessionHistoryRecord, SessionStreamEvent, TurnFailureCode,
    TurnFailureKind, make_error_event,
};
use lash_core::{
    CellRecord, CheckpointKind, DriverAction, DriverContextView, ExecResponse, LlmOutputPart,
    LlmResponse, LlmTerminalReason, facade_support::TurnFinish, facade_support::TurnOutcome,
    facade_support::TurnStop, facade_support::append_assistant_text_part,
    facade_support::normalized_response_parts,
};
use lash_rlm_types::{RlmDiagnosticEvent, RlmProtocolEvent};
use serde_json::Value;

#[cfg(feature = "testing")]
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};

use crate::dialect::SessionDialect;
use crate::projection::rlm_protocol_event;
use crate::rlm_support::decode_rlm_termination_options;
use crate::settlement::{AttemptProgress, continue_or_stop_after_nonterminal, settle_execution};

use super::actions::invalid_turn_options_actions;
use super::cell::{
    CellExtraction, CellExtractionError, extract_cell, malformed_cell_fence,
    project_visible_assistant_prose_with_tags,
};
#[cfg(feature = "testing")]
use super::finish::internal_assistant_prose_message;
use super::finish::{
    cell_context_message, finish_required_reminder_message,
    internal_assistant_prose_message_for_turn, invalid_cell_message, output_limit_retry_message,
};
use super::stall::{
    ExtractionCounts, ExtractionDiagnostic, LLM_EXTRACTION_PHASE, reply_fingerprint,
};
use crate::driver_state::{
    RLM_DRIVER_STATE_VERSION, RlmDriverState, RlmReasoningPart, check_parked_rlm_state,
    handed_back_rlm_state, rlm_driver_state,
};

#[derive(Clone)]
pub struct RlmDriver {
    dialect: Arc<SessionDialect>,
}

impl RlmDriver {
    /// A prompt-only driver in the host's selected `dialect` (ADR 0096).
    pub fn new(dialect: crate::dialect::CellDialect) -> Self {
        Self {
            dialect: Arc::new(crate::dialect::SessionDialect::prompt_only(dialect)),
        }
    }

    pub(crate) fn with_dialect(dialect: Arc<SessionDialect>) -> Self {
        Self { dialect }
    }

    /// The repair for a provider tool call on a request that declared no
    /// tools: malformed provider output gets the same one-round extraction
    /// repair as any other reply with no usable cell (FIG-2777).
    fn native_tool_call_prompt(
        &self,
        attempt: &AttemptContext<'_>,
        tool_name: &str,
    ) -> RepairPrompt<'static> {
        RepairPrompt {
            decision: "retry_native_tool_call",
            assistant_message: None,
            correction: invalid_cell_message(
                self.dialect.as_ref(),
                attempt.message_id("native_tool_call"),
                &self.dialect.native_tool_call_copy(tool_name),
            ),
        }
    }

    /// The one place a reply's extraction outcome is read as a decision:
    /// cell, natural finish, or which repair prompt to emit. The branches
    /// used to spell the decision string, the retained projection, the
    /// message-id purpose and the correction text separately; keeping them in
    /// one classification is what stops them drifting again.
    ///
    /// Decision strings and message-id purposes are durable history and do
    /// not change here; the table test pins them byte for byte.
    fn classify_reply<'a>(
        &self,
        attempt: &AttemptContext<'_>,
        extraction: Result<Option<CellExtraction>, CellExtractionError>,
        terminal_reason: LlmTerminalReason,
        termination: &RlmCompletion,
        reply: ReplyProjections<'a>,
    ) -> ReplyClass<'a> {
        match extraction {
            Ok(Some(cell)) => return ReplyClass::Cell(cell),
            Ok(None) => {}
            Err(err) => {
                let (decision, message) = match (err, terminal_reason) {
                    (CellExtractionError::UnclosedCell, LlmTerminalReason::OutputLimit) => (
                        "retry_output_limit_cell",
                        self.dialect
                            .output_limit_cell_copy(attempt.output_token_cap),
                    ),
                    (CellExtractionError::UnclosedCell, _) => {
                        ("retry_unclosed_cell", self.dialect.cell_error_message(err))
                    }
                };
                return ReplyClass::Repair(Box::new(RepairPrompt {
                    decision,
                    assistant_message: Some((reply.visible_prose, "assistant_response")),
                    correction: invalid_cell_message(
                        self.dialect.as_ref(),
                        attempt.message_id("invalid_cell"),
                        &message,
                    ),
                }));
            }
        }

        if terminal_reason == LlmTerminalReason::OutputLimit {
            return ReplyClass::Repair(Box::new(RepairPrompt {
                decision: "retry_output_limit_prose",
                assistant_message: Some((
                    reply.visible_assistant_text,
                    "truncated_assistant_response",
                )),
                correction: output_limit_retry_message(
                    self.dialect.prompt_vocabulary(),
                    attempt.message_id("output_limit_retry"),
                    attempt.output_token_cap,
                ),
            }));
        }
        // A reply that opened a line with the active dialect's tag and still
        // produced no cell put a fence somewhere the grammar refuses. Read
        // as prose it is silence: the model is asked to finish, answers with
        // the same reply, and nothing in the loop ever names what was wrong
        // with it (FIG-1475).
        //
        // On a `Natural` turn too: read as prose there, the refused cell
        // *is* the answer, committed verbatim as the turn's reply with its
        // program never run (FIG-5302). Prose that mentions the tags wraps
        // them in backticks or starts the line with something else, and the
        // correction says so.
        if malformed_cell_fence(reply.assistant_text, self.dialect.cell_tags()) {
            return ReplyClass::Repair(Box::new(RepairPrompt {
                decision: "retry_malformed_cell_fence",
                assistant_message: Some((reply.visible_prose, "assistant_response")),
                correction: invalid_cell_message(
                    self.dialect.as_ref(),
                    attempt.message_id("malformed_cell_fence"),
                    &self.dialect.malformed_cell_fence_retry_copy(),
                ),
            }));
        }
        if termination.prose_ends_turn() {
            return ReplyClass::Finish;
        }
        let assistant_message = if !reply.visible_assistant_text.trim().is_empty() {
            Some((reply.visible_assistant_text, "assistant_prose"))
        } else if !reply.reasoning.is_empty() {
            Some(("", "assistant_reasoning"))
        } else {
            None
        };
        ReplyClass::Repair(Box::new(RepairPrompt {
            decision: "request_finish",
            assistant_message,
            correction: finish_required_reminder_message(
                self.dialect.as_ref(),
                attempt.message_id("finish_reminder"),
                &attempt.finishing,
            ),
        }))
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
                retry.prompt.decision,
                retry.termination,
                prose_only_counts(self.dialect.language_id(), retry.raw_text, retry.reasoning),
            ),
            lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
        )]));
        let mut retry_events = Vec::new();
        if let Some((prose, purpose)) = retry.prompt.assistant_message
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
        retry_events.push(conversation_event(retry.prompt.correction));
        continue_or_stop_after_nonterminal(
            ctx,
            actions,
            Vec::new(),
            retry_events,
            AttemptProgress::Stalled,
            LLM_EXTRACTION_PHASE,
        )
    }
}

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for RlmDriver {
    fn project_visible_assistant_prose(&self, text: &str) -> String {
        super::cell::project_visible_assistant_prose_with_tags(text, self.dialect.cell_tags())
    }

    fn cell_trailing_discarded(
        &self,
        events: &[SessionHistoryRecord],
    ) -> Result<usize, lash_core::StoredDataCorruption> {
        for record in events.iter().rev() {
            let SessionHistoryRecord::Protocol(event) = record else {
                continue;
            };
            if let Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) =
                crate::projection::decode_rlm_protocol_event(event)?.as_deref()
                && diagnostic.phase == LLM_EXTRACTION_PHASE
            {
                let Some(note) = diagnostic.payload.get("discarded_after_cell") else {
                    return Ok(0);
                };
                let note: crate::cell_scan::DiscardedAfterCell =
                    serde_json::from_value(note.clone()).map_err(|error| {
                        lash_core::StoredDataCorruption {
                            record_kind: "discarded_after_cell".into(),
                            message: error.to_string(),
                        }
                    })?;
                return Ok(note.bytes);
            }
        }
        Ok(0)
    }

    fn handles_output_limit_response(&self) -> bool {
        true
    }

    fn check_parked_state(
        &self,
        ctx: DriverContextView<'_>,
        work: &PendingWork<lash_core::HostTurnProtocol>,
    ) -> Result<(), lash_sansio::UndecodableDriverState> {
        check_parked_rlm_state(&ctx, work)
    }

    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        match decode_rlm_termination_options(ctx.termination()) {
            Err(err) => return invalid_turn_options_actions(err),
            Ok(termination) => {
                if let Err(err) = crate::rlm_support::finish_available(&termination, &ctx) {
                    return invalid_turn_options_actions(err);
                }
            }
        }
        let request = match ctx.project_llm_request_retained(false) {
            Ok(request) => request,
            Err(error) => return vec![DriverAction::ProjectionRefused(error)],
        };
        vec![DriverAction::Start(PendingWork::Llm {
            request,
            driver_state: Some(rlm_driver_state(
                RlmDriverState::default(),
                lash_core::driver_writer_version!(ctx, RLM_DRIVER_STATE_VERSION),
            )),
        })]
    }

    fn handle_llm_success_retained(
        &self,
        ctx: DriverContextView<'_>,
        request: Arc<lash_core::llm::types::RetainedLlmRequest>,
        driver_state: Option<lash_core::ProtocolDriverState>,
        response: LlmResponse,
        calls: &lash_core::sansio::ResponseToolCalls,
        text_streamed: bool,
    ) -> Vec<DriverAction> {
        // This built-in response reducer consumes only the envelope's tool
        // contract (RLM consumes no request fields), never the prompt history.
        self.handle_llm_success(
            ctx,
            Arc::new(request.envelope().clone()),
            driver_state,
            response,
            calls,
            text_streamed,
        )
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _request: Arc<lash_core::LlmRequest>,
        driver_state: Option<lash_core::ProtocolDriverState>,
        llm_response: LlmResponse,
        _calls: &lash_core::sansio::ResponseToolCalls,
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
                let attempt = AttemptContext {
                    turn_id: ctx.turn_id(),
                    protocol_iteration: ctx.protocol_iteration(),
                    output_token_cap: ctx
                        .generation()
                        .output_token_cap
                        .map(std::num::NonZeroUsize::get),
                    finishing: self.dialect.offered_finish_calls(&ctx),
                };
                if let Err(err) = self.stall_retry_epilogue(
                    &ctx,
                    &mut actions,
                    StallRetry {
                        prompt: self.native_tool_call_prompt(&attempt, &tool_call.tool_name),
                        fingerprint: &reply_fingerprint(&full_text),
                        termination: &termination,
                        raw_text: &full_text,
                        reasoning: &[],
                    },
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
        let extraction = extract_cell(&assistant_text, tags).map(|cell| {
            cell.map(|mut cell| {
                if cell.discarded_after_cell.is_none() {
                    cell.discarded_after_cell = llm_response.discarded_after_cell;
                }
                cell
            })
        });
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
        }));

        if assistant_text.trim().is_empty()
            && reasoning.iter().all(|part| part.text.trim().is_empty())
        {
            actions.push(DriverAction::Emit(make_error_event(
                TurnFailureKind::LlmProvider,
                Some(TurnFailureCode::EmptyResponse.into()),
                "Model returned no assistant text.",
                None,
                lash_protocol_types::session_model::RuntimeOutputCuts::standard(),
            )));
            actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                TurnStop::ProviderError {
                    cause: lash_protocol_types::TurnErrorCause::new(
                        lash_protocol_types::TurnErrorCode::InvalidResponse,
                    ),
                },
            )));
            return actions;
        }

        let termination = match decode_rlm_termination_options(ctx.termination()) {
            Ok(termination) => termination,
            Err(err) => return invalid_turn_options_actions(err),
        };

        let attempt = AttemptContext {
            turn_id: ctx.turn_id(),
            protocol_iteration: ctx.protocol_iteration(),
            output_token_cap: ctx
                .generation()
                .output_token_cap
                .map(std::num::NonZeroUsize::get),
            finishing: self.dialect.offered_finish_calls(&ctx),
        };
        let cell = match self.classify_reply(
            &attempt,
            extraction,
            terminal_reason,
            &termination,
            ReplyProjections {
                assistant_text: &assistant_text,
                visible_prose: &visible_prose,
                visible_assistant_text: &visible_assistant_text,
                reasoning: &reasoning,
            },
        ) {
            ReplyClass::Cell(cell) => cell,
            ReplyClass::Repair(prompt) => {
                if let Err(err) = self.stall_retry_epilogue(
                    &ctx,
                    &mut actions,
                    StallRetry {
                        prompt: *prompt,
                        fingerprint: &fingerprint,
                        termination: &termination,
                        raw_text: &assistant_text,
                        reasoning: &reasoning,
                    },
                ) {
                    return invalid_turn_options_actions(err);
                }
                return actions;
            }
            ReplyClass::Finish => {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    LLM_EXTRACTION_PHASE,
                    llm_extraction_payload(
                        ctx.turn_id(),
                        &fingerprint,
                        "finish_prose",
                        &termination,
                        prose_only_counts(self.dialect.language_id(), &assistant_text, &reasoning),
                    ),
                    lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
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
                actions.push(DriverAction::Start(PendingWork::Checkpoint {
                    checkpoint: CheckpointKind::BeforeCompletion,
                    on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                        TurnFinish::AssistantMessage {
                            text: visible_assistant_text.clone(),
                        },
                    )),
                }));
                return actions;
            }
        };

        actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
            LLM_EXTRACTION_PHASE,
            ExtractionDiagnostic::new(
                ctx.turn_id(),
                &fingerprint,
                &self.dialect.execution_diagnostic_name(),
                &termination,
                cell_counts(
                    self.dialect.language_id(),
                    &assistant_text,
                    &reasoning,
                    &cell,
                ),
            )
            .with_discarded_after_cell(cell.discarded_after_cell)
            .payload(),
            lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
        )]));

        let mut state = handed_back_rlm_state(
            driver_state,
            lash_core::driver_writer_version!(ctx, RLM_DRIVER_STATE_VERSION),
        );
        state.code = cell.code.clone();
        state.reasoning = reasoning;
        state.assistant_parts = vec![lash_core::Part::text(
            rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), "prose"),
            cell.prose.clone(),
            None,
        )];

        // Emit the raw cell source as a `Message` with kind `code` so the
        // CLI can reveal it in the full-expand view (Alt+O) above the tool
        // activities it produced.
        actions.push(DriverAction::Emit(SessionStreamEvent::Message {
            text: cell.code.clone(),
            kind: lash_core::session_model::StreamMessageKind::Code,
        }));
        actions.push(DriverAction::Start(PendingWork::Exec {
            language: self.dialect.language_id().to_string(),
            code: cell.code,
            driver_state: rlm_driver_state(
                state,
                lash_core::driver_writer_version!(ctx, RLM_DRIVER_STATE_VERSION),
            ),
        }));
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
        driver_state: lash_core::ProtocolDriverState,
        result: Result<ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        let state = handed_back_rlm_state(
            Some(driver_state),
            lash_core::driver_writer_version!(ctx, RLM_DRIVER_STATE_VERSION),
        );
        settle_execution(
            &ctx,
            &self.dialect,
            state.code,
            result,
            LLM_EXTRACTION_PHASE,
            |record| {
                cell_events(
                    ctx.turn_id(),
                    record,
                    &state.reasoning,
                    &state.assistant_parts,
                    lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
                )
            },
        )
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

fn rlm_message_id(turn_id: &TurnId, protocol_iteration: usize, purpose: &str) -> String {
    format!("m_rlm_{turn_id}_{protocol_iteration}_{purpose}")
}

/// A cell's committed history: the reply's reasoning and the prose around
/// the cell, as the assistant message that names the cell, then the cell's
/// record.
fn cell_events(
    turn_id: &TurnId,
    record: CellRecord,
    reasoning: &[RlmReasoningPart],
    assistant_parts: &[lash_core::Part],
    schema_version: u32,
) -> Vec<SessionHistoryRecord> {
    let prose = assistant_parts
        .iter()
        .filter_map(lash_core::Part::text_content)
        .collect::<Vec<_>>()
        .join("");
    let prose = prose.trim();
    let mut events = Vec::new();
    if !reasoning.is_empty() || !prose.is_empty() {
        events.push(conversation_event(cell_context_message(
            turn_id,
            rlm_message_id(turn_id, record.protocol_iteration, "assistant_content"),
            record.id.clone(),
            prose.to_string(),
            reasoning,
        )));
    }
    events.push(trajectory_event(record, schema_version));
    events
}

fn conversation_event(message: Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(message))
}

fn trajectory_event(entry: CellRecord, schema_version: u32) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmTrajectoryEntry(std::sync::Arc::new(entry)),
        schema_version,
    ))
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

fn prose_only_counts<'a>(
    language_id: &'a str,
    assistant_text: &str,
    reasoning: &[RlmReasoningPart],
) -> ExtractionCounts<'a> {
    let chars = assistant_text.chars().count();
    ExtractionCounts::prose(language_id, chars, chars, reasoning_chars(reasoning))
}

/// `full_text_chars` covers the fences the cell arrived in, so it exceeds `prose + code`.
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

/// The per-attempt coordinates every repair message shares: the turn and
/// iteration that name message ids, and the request's output-token cap the
/// output-limit corrections cite.
struct AttemptContext<'a> {
    turn_id: &'a TurnId,
    protocol_iteration: usize,
    output_token_cap: Option<usize>,
    /// How the model calls each finish tool the turn's surface offers.
    finishing: Vec<String>,
}

impl AttemptContext<'_> {
    fn message_id(&self, purpose: &str) -> String {
        rlm_message_id(self.turn_id, self.protocol_iteration, purpose)
    }
}

/// The reply projections [`RlmDriver::classify_reply`] chooses between:
/// `assistant_text` is the raw reply the fence scan reads, `visible_prose`
/// the tag-stripped projection repairs journal as the assistant message, and
/// `visible_assistant_text` the normalized projection the output-limit and
/// finish-required repairs journal.
struct ReplyProjections<'a> {
    assistant_text: &'a str,
    visible_prose: &'a str,
    visible_assistant_text: &'a str,
    reasoning: &'a [RlmReasoningPart],
}

/// One repair prompt for a reply that yielded no usable cell: the diagnostic
/// decision token, which projection (if any) is journaled as the durable
/// assistant message and under which message-id purpose, and the correction
/// the model reads next. [`RlmDriver::classify_reply`] is the only producer.
struct RepairPrompt<'a> {
    /// Diagnostic decision token (`retry_unclosed_cell`, `request_finish`, …).
    decision: &'static str,
    /// `(projection, message purpose)` for the retained assistant message. The
    /// same string is guard and content; `None` emits no assistant message.
    assistant_message: Option<(&'a str, &'static str)>,
    /// The retry or reminder correction the model sees next.
    correction: Message,
}

/// What a reply's extraction outcome means for the rest of
/// `handle_llm_success`.
enum ReplyClass<'a> {
    /// A usable cell arrived; the mainline executes it.
    Cell(CellExtraction),
    /// The reply is a `Natural`-termination finish: its prose is the answer
    /// and the finish path in `handle_llm_success` renders it.
    Finish,
    /// A stall-repair round; `stall_retry_epilogue` renders the prompt.
    Repair(Box<RepairPrompt<'a>>),
}

/// One stall-retry round as the shared epilogue consumes it: the classified
/// prompt plus the reply's diagnostic inputs.
struct StallRetry<'a> {
    /// The classified repair: decision, retained projection and correction.
    prompt: RepairPrompt<'a>,
    /// Fingerprint of the reply as received.
    fingerprint: &'a str,
    /// Decoded termination options, recorded in the diagnostic.
    termination: &'a RlmCompletion,
    /// Raw assistant text, feeding the diagnostic's character counters.
    raw_text: &'a str,
    /// Reply reasoning, counted in the diagnostic and carried into the
    /// retained assistant message.
    reasoning: &'a [RlmReasoningPart],
}

fn llm_extraction_payload(
    turn_id: &TurnId,
    reply_fingerprint: &str,
    decision: &str,
    termination: &RlmCompletion,
    counts: ExtractionCounts<'_>,
) -> Value {
    ExtractionDiagnostic::new(turn_id, reply_fingerprint, decision, termination, counts).payload()
}

#[cfg(test)]
mod classification_tests;

#[cfg(test)]
mod tests {
    use super::*;

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
}
