use crate::dialect::SessionDialect;
use crate::tool_records::{bounded_exec_tool_call_records, bounded_executed_calls};
use lash_sansio::TurnId;
use std::sync::Arc;

use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, PendingWork, ProtocolDriverHandle,
};
use lash_core::session_model::{
    ConversationRecord, Message, SessionHistoryRecord, SessionStreamEvent, TurnFailureCode,
    TurnFailureKind, make_error_event,
};
use lash_core::{
    CellOutcome, CellRecord, CheckpointKind, DriverAction, DriverContextView, ExecResponse,
    LlmResponse, ToolCallOutcome, ToolCallRecord, facade_support::TurnFinish,
    facade_support::TurnOutcome, facade_support::TurnStop,
    facade_support::normalized_response_parts,
};
use lash_rlm_types::{RlmDiagnosticEvent, RlmProtocolEvent};
use serde_json::Value;

use crate::projection::rlm_protocol_event;
use crate::rlm_support::decode_rlm_termination_options;

use super::finish::{
    finish_required_reminder_message, finish_schema_mismatch_message,
    internal_assistant_prose_message_for_turn, no_progress_stop_message,
    text_cell_correction_message, validate_finish_value,
};
use super::stall::{
    LLM_EXTRACTION_PHASE, NO_PROGRESS_BUDGET_PHASE, native_reply_fingerprint, stalled_attempts,
};
use crate::driver_state::{
    RLM_DRIVER_STATE_VERSION, RlmDriverState, RlmReasoningPart, check_parked_rlm_state,
    handed_back_rlm_state, rlm_driver_state,
};
use crate::protocol::actions::invalid_turn_options_actions;
use crate::protocol::stall::{ExtractionCounts, ExtractionDiagnostic};

#[derive(Clone)]
pub struct NativeDriver {
    dialect: Arc<SessionDialect>,
}

impl NativeDriver {
    pub(crate) fn with_dialect(dialect: Arc<SessionDialect>) -> Self {
        Self { dialect }
    }
}

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for NativeDriver {
    fn project_visible_assistant_prose(&self, text: &str) -> String {
        text.to_string()
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
        if let Err(err) = decode_rlm_termination_options(ctx.termination()) {
            return invalid_turn_options_actions(err);
        }
        let mut actions = Vec::new();
        let request = match ctx.project_llm_request(false) {
            Ok(request) => request,
            Err(error) => return lash_sansio::sansio::stored_history_refusal_actions(error),
        };
        actions.push(DriverAction::Start(PendingWork::Llm {
            request,
            driver_state: Some(rlm_driver_state(
                RlmDriverState::default(),
                lash_core::driver_writer_version!(ctx, RLM_DRIVER_STATE_VERSION),
            )),
        }));
        actions
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _request: Arc<lash_core::LlmRequest>,
        driver_state: Option<lash_core::ProtocolDriverState>,
        llm_response: LlmResponse,
        calls: &lash_core::sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let mut actions = Vec::new();
        let parts = super::tool::assistant_parts(
            normalized_response_parts(&llm_response),
            calls.call_ids(&llm_response),
        );
        let fingerprint = native_reply_fingerprint(&parts);
        let prose = llm_response.full_text();
        let reasoning = parts
            .iter()
            .filter(|part| part.kind() == lash_core::PartKind::Reasoning)
            .map(|part| RlmReasoningPart {
                text: part.content().to_string(),
                replay: part.reasoning_meta().cloned(),
            })
            .collect::<Vec<_>>();
        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: prose.clone(),
        }));
        let action = super::tool::normalize(&parts);
        // This channel runs code only through `execute_code`. A reply with no
        // call that writes a cell in its text meant to run it: read as prose,
        // a natural turn would commit the cell as its answer, unrun
        // (FIG-5302), so it is corrected toward the tool instead.
        let text_cell = matches!(action, super::tool::NativeAction::ProseOnly)
            && crate::cell_scan::malformed_cell_fence(&prose, self.dialect.cell_tags());
        if matches!(action, super::tool::NativeAction::ProseOnly) && prose.trim().is_empty() {
            actions.push(DriverAction::Emit(make_error_event(
                TurnFailureKind::LlmProvider,
                Some(TurnFailureCode::EmptyResponse.into()),
                "Model returned no assistant text.",
                None,
                lash_sansio::session_model::RuntimeOutputCuts::standard(),
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
                ExtractionDiagnostic::new(
                    ctx.turn_id(),
                    &fingerprint,
                    decision,
                    &termination,
                    native_counts(self.dialect.language_id(), &prose, &action, &reasoning),
                )
                .payload(),
                lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
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
                    reply_marker: None,
                }));
            } else {
                durable.extend(refused_call_events(
                    ctx.turn_id(),
                    ctx.protocol_iteration(),
                    parts,
                    copy,
                ));
            }
            if let Err(error) = continue_or_stop_after_nonterminal(
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
            super::tool::NativeAction::ProseOnly => if text_cell {
                "retry_text_cell"
            } else if termination.prose_ends_turn() {
                "prose_only"
            } else {
                "request_finish"
            }
            .to_string(),
        };
        actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
            LLM_EXTRACTION_PHASE,
            ExtractionDiagnostic::new(
                ctx.turn_id(),
                &fingerprint,
                &decision,
                &termination,
                native_counts(self.dialect.language_id(), &prose, &action, &reasoning),
            )
            .payload(),
            lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
        )]));
        match action {
            super::tool::NativeAction::Malformed { repair_copy, .. } => {
                let events = refused_call_events(
                    ctx.turn_id(),
                    ctx.protocol_iteration(),
                    parts,
                    repair_copy,
                );
                if let Err(error) = continue_or_stop_after_nonterminal(
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
                if termination.prose_ends_turn() && !text_cell {
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
                    actions.push(DriverAction::Start(PendingWork::Checkpoint {
                        checkpoint: CheckpointKind::BeforeCompletion,
                        on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                            TurnFinish::AssistantMessage { text: prose },
                        )),
                    }));
                } else {
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
                        conversation_event(if text_cell {
                            text_cell_correction_message(
                                self.dialect.as_ref(),
                                rlm_message_id(
                                    ctx.turn_id(),
                                    ctx.protocol_iteration(),
                                    "text_cell",
                                ),
                            )
                        } else {
                            finish_required_reminder_message(
                                self.dialect.as_ref(),
                                rlm_message_id(
                                    ctx.turn_id(),
                                    ctx.protocol_iteration(),
                                    "finish_reminder",
                                ),
                                termination.finish_schema().is_some(),
                            )
                        }),
                    ];
                    if let Err(error) = continue_or_stop_after_nonterminal(
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
                let mut state = handed_back_rlm_state(
                    driver_state,
                    lash_core::driver_writer_version!(ctx, RLM_DRIVER_STATE_VERSION),
                );
                state.code = code.clone();
                state.reasoning = reasoning;
                state.assistant_parts = parts;
                actions.push(DriverAction::Emit(SessionStreamEvent::Message {
                    text: code.clone(),
                    kind: lash_core::session_model::StreamMessageKind::Code,
                }));
                actions.push(DriverAction::Start(PendingWork::Exec {
                    language: self.dialect.language_id().to_string(),
                    code,
                    driver_state: rlm_driver_state(
                        state,
                        lash_core::driver_writer_version!(ctx, RLM_DRIVER_STATE_VERSION),
                    ),
                }));
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
        driver_state: lash_core::ProtocolDriverState,
        result: Result<ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        let state = handed_back_rlm_state(
            Some(driver_state),
            lash_core::driver_writer_version!(ctx, RLM_DRIVER_STATE_VERSION),
        );
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
            language: self.dialect.language_id().to_string(),
            code: state.code,
            ..CellRecord::default()
        };
        let commit = |record: CellRecord| {
            cell_events(
                ctx.turn_id(),
                record,
                state.assistant_parts.clone(),
                lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION),
            )
        };
        // The finish value itself: the turn's answer, also when history
        // records only its retention (FIG-1643).
        let mut finish_value = None;
        match result {
            Ok(response) => {
                finish_value = response.finish_value().cloned();
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
                let (host_records, omitted) = bounded_exec_tool_call_records(
                    &response.tool_calls,
                    &self.dialect.presentation(),
                );
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
                (record.calls, record.calls_omitted) =
                    bounded_executed_calls(response.calls, &self.dialect.presentation());
                record.images = response.printed_images;
                record.prints = response.prints;
                record.prints_retained = response.prints_retained;
                record.result = response.result;
                if let Some(outcome) = terminal_outcome {
                    // A tool ended the turn: the cell's own finish never
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

        if let Some(finish_value) = finish_value {
            // Typed-RLM: validate against the declared schema, under either
            // termination (FIG-5104). If it fails, surface the error to the
            // model and loop; otherwise fall through to the shared
            // terminate-with-value path below.
            let termination = match decode_rlm_termination_options(ctx.termination()) {
                Ok(termination) => termination,
                Err(err) => return invalid_turn_options_actions(err),
            };
            if let Some(schema) = termination.finish_schema()
                && let Err(error_text) = validate_finish_value(&finish_value, schema)
            {
                // The program finished with a value its declared schema
                // refuses: a defect in the program.
                record.result = CellOutcome::Failed(
                    lash_core::CellFailure::new(
                        lash_core::CellFailureKind::Program,
                        error_text.to_string(),
                    )
                    .with_value_mismatch(error_text),
                );
                if let Err(err) = continue_or_stop_after_nonterminal(
                    &ctx,
                    &mut actions,
                    commit(record),
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

            actions.push(DriverAction::AppendEvents(commit(record)));
            actions.push(DriverAction::Start(PendingWork::Checkpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                    TurnFinish::FinalValue {
                        value: finish_value,
                    },
                )),
            }));
            return actions;
        }

        let progress = if record.result.is_failed() {
            AttemptProgress::Stalled
        } else {
            AttemptProgress::Executed
        };
        if let Err(err) = continue_or_stop_after_nonterminal(
            &ctx,
            &mut actions,
            commit(record),
            Vec::new(),
            progress,
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
        let attempts = stalled_attempts(ctx, actions).map_err(|error| error.to_string())?;
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
    lash_core::turn_outcome_from_tool_control(&record.tool, record.output.control.as_ref()?)
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

/// A cell's committed history: the reply that ran it, as the assistant
/// message it was, then the cell's record.
fn cell_events(
    turn_id: &TurnId,
    record: CellRecord,
    assistant_parts: Vec<lash_core::Part>,
    schema_version: u32,
) -> Vec<SessionHistoryRecord> {
    vec![
        conversation_event(super::transport::cell_context_message(
            turn_id,
            rlm_message_id(turn_id, record.protocol_iteration, "assistant_content"),
            record.id.clone(),
            assistant_parts,
        )),
        trajectory_event(record, schema_version),
    ]
}

/// A reply whose calls ran nothing, and the correction that answers them.
fn refused_call_events(
    turn_id: &TurnId,
    protocol_iteration: usize,
    parts: Vec<lash_core::Part>,
    correction: String,
) -> Vec<SessionHistoryRecord> {
    super::transport::refused_call_messages(
        turn_id,
        rlm_message_id(turn_id, protocol_iteration, "refused_call"),
        rlm_message_id(turn_id, protocol_iteration, "refused_call_result"),
        parts,
        correction,
    )
    .into_iter()
    .map(conversation_event)
    .collect()
}

fn conversation_event(message: Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(message))
}

fn trajectory_event(entry: CellRecord, schema_version: u32) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmTrajectoryEntry(Box::new(entry)),
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

/// A native reply says in two parts what a cell reply says in one, so
/// `full_text_chars` is the prose and the program together; there are no fences
/// between them to account for. An attempt whose call did not parse committed
/// no program and is counted as the prose it did say.
fn native_counts<'a>(
    language_id: &'a str,
    prose: &str,
    action: &super::tool::NativeAction,
    reasoning: &[RlmReasoningPart],
) -> ExtractionCounts<'a> {
    let prose_chars = prose.chars().count();
    let reasoning_chars = crate::protocol::stall::reasoning_diagnostic_chars(
        reasoning
            .iter()
            .map(|part| (part.text.as_str(), part.replay.as_ref())),
    );
    match action {
        super::tool::NativeAction::Execute { code } => {
            let code_chars = code.chars().count();
            ExtractionCounts::program(
                language_id,
                prose_chars + code_chars,
                prose_chars,
                reasoning_chars,
                code_chars,
                1,
            )
        }
        super::tool::NativeAction::ProseOnly | super::tool::NativeAction::Malformed { .. } => {
            ExtractionCounts::prose(language_id, prose_chars, prose_chars, reasoning_chars)
        }
    }
}
