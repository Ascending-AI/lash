//! Turn stream assembly and output classification.
//!
//! Extracted from `runtime/mod.rs`. These types are crate-internal — sibling
//! modules (`turn_driver.rs`, `tests.rs`) import them via `use super::*` in
//! `mod.rs`. No public API is exposed here.

use std::time::Instant;

use serde_json::json;

use std::collections::BTreeMap;

use crate::ToolCallRecord;
use crate::llm::types::{
    LlmOutputPart, LlmResponse, LlmStreamEvent, LlmStreamEvidence, LlmUsage,
    ProviderReasoningReplay, ProviderReplayMeta, ResponseTextMeta,
};
use crate::session_model::{MessageRole, PartKind, SessionStreamEvent, TokenUsage};
use crate::{TurnFinish, TurnOutcome, TurnStop};

use super::usage::TokenLedgerEntry;
use super::{
    AssembledTurn, AssistantOutput, ExecutionSummary, OutputState, TerminationPolicy, TurnIssue,
};

#[derive(Clone, Debug, Default)]
pub(super) struct LlmStreamAccumulator {
    pub(super) parts: Vec<LlmOutputPart>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct LlmStreamDebugState {
    pub(super) created_at: Instant,
    pub(super) sequence: u64,
    pub(super) summary: LlmStreamSummary,
}

#[derive(Clone, Copy)]
pub(super) struct LlmDebugText<'a> {
    pub(super) raw: Option<&'a str>,
    pub(super) visible: Option<&'a str>,
}

#[derive(Clone, Copy)]
pub(super) struct LlmDebugToolCall<'a> {
    pub(super) call_id: &'a str,
    pub(super) tool_name: &'a str,
    pub(super) input_json: &'a str,
}

#[derive(Clone, Copy)]
pub(super) struct LlmStreamEventLog<'a> {
    pub(super) protocol_iteration: usize,
    pub(super) event_type: &'a str,
    pub(super) text: LlmDebugText<'a>,
    pub(super) item_id: Option<&'a str>,
    pub(super) usage: Option<&'a LlmUsage>,
    pub(super) tool_call: Option<LlmDebugToolCall<'a>>,
}

pub(super) struct LlmStreamState<'a> {
    pub(super) text_streamed: &'a mut bool,
    pub(super) streamed_usage: &'a mut LlmUsage,
    pub(super) stream_accumulator: &'a mut LlmStreamAccumulator,
    pub(super) stream_evidence: &'a mut LlmStreamEvidence,
    pub(super) debug: &'a mut LlmStreamDebugState,
    pub(super) protocol_iteration: usize,
    pub(super) assistant_prose_correlation: &'a mut Option<crate::TurnActivityId>,
    pub(super) reasoning_correlation: &'a mut Option<crate::TurnActivityId>,
    pub(super) assistant_prose_attempt_correlations: &'a mut Vec<crate::TurnActivityId>,
    pub(super) reasoning_attempt_correlations: &'a mut Vec<crate::TurnActivityId>,
    /// Set to `true` by `forward_provider_stream_event` when a plugin
    /// stream hook has raised `AssistantStreamTransform.abort_stream`.
    /// The LLM runner checks this after each stream event and
    /// short-circuits the select loop, synthesizing a response from the
    /// already-streamed parts.
    pub(super) abort_requested: &'a mut bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LlmStreamSummary {
    pub(super) first_visible_token_latency_ms: Option<u64>,
    pub(super) last_visible_chunk_latency_ms: Option<u64>,
    pub(super) text_delta_count: u64,
    pub(super) visible_chunk_count: u64,
    pub(super) total_visible_chars: u64,
    pub(super) max_visible_chunk_chars: u64,
}

impl LlmStreamDebugState {
    pub(super) fn new(created_at: Instant) -> Self {
        Self {
            created_at,
            sequence: 0,
            summary: LlmStreamSummary::default(),
        }
    }

    pub(super) fn next_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        sequence
    }

    pub(super) fn elapsed_ms(&self, clock: &dyn crate::Clock) -> u64 {
        clock
            .now()
            .saturating_duration_since(self.created_at)
            .as_millis() as u64
    }
}

impl LlmStreamSummary {
    pub(super) fn record_text_chunk(&mut self, visible_text: Option<&str>, elapsed_ms: u64) {
        self.text_delta_count += 1;

        let visible_chars = visible_text
            .map(|text| text.chars().count() as u64)
            .unwrap_or(0);
        if visible_chars == 0 {
            return;
        }

        if self.first_visible_token_latency_ms.is_none() {
            self.first_visible_token_latency_ms = Some(elapsed_ms);
        }
        self.last_visible_chunk_latency_ms = Some(elapsed_ms);
        self.visible_chunk_count += 1;
        self.total_visible_chars += visible_chars;
        self.max_visible_chunk_chars = self.max_visible_chunk_chars.max(visible_chars);
    }

    pub(super) fn to_json(self) -> serde_json::Value {
        let avg_visible_chunk_chars = if self.visible_chunk_count == 0 {
            None
        } else {
            Some(self.total_visible_chars as f64 / self.visible_chunk_count as f64)
        };
        let stream_duration_ms = match (
            self.first_visible_token_latency_ms,
            self.last_visible_chunk_latency_ms,
        ) {
            (Some(first), Some(last)) => Some(last.saturating_sub(first)),
            _ => None,
        };
        json!({
            "first_visible_token_latency_ms": self.first_visible_token_latency_ms,
            "stream_duration_ms": stream_duration_ms,
            "text_delta_count": self.text_delta_count,
            "visible_chunk_count": self.visible_chunk_count,
            "avg_visible_chunk_chars": avg_visible_chunk_chars,
            "max_visible_chunk_chars": if self.visible_chunk_count == 0 {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(self.max_visible_chunk_chars)
            },
        })
    }
}

impl LlmStreamAccumulator {
    pub(super) fn push_text(&mut self, piece: &str) {
        if piece.is_empty() {
            return;
        }
        match self.parts.last_mut() {
            Some(LlmOutputPart::Text { text, .. }) => append_stream_piece(text, piece),
            _ => self.parts.push(LlmOutputPart::Text {
                text: piece.to_string(),
                response_meta: None,
            }),
        }
    }

    pub(super) fn push_text_part(&mut self, text: String, response_meta: Option<ResponseTextMeta>) {
        if text.is_empty() && response_meta.is_none() {
            return;
        }

        let incoming_id = response_meta
            .as_ref()
            .and_then(|meta| meta.id.as_deref())
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let incoming_has_id = incoming_id.is_some();
        let target_index = incoming_id
            .as_deref()
            .and_then(|id| {
                self.parts.iter().position(|part| {
                    matches!(
                        part,
                        LlmOutputPart::Text {
                            response_meta: Some(meta),
                            ..
                        } if meta.id.as_deref() == Some(id)
                    )
                })
            })
            .or_else(|| {
                self.parts.iter().rposition(|part| match part {
                    LlmOutputPart::Text { response_meta, .. } if incoming_has_id => {
                        response_meta.is_none()
                    }
                    LlmOutputPart::Text { .. } => true,
                    _ => false,
                })
            });

        let Some(index) = target_index else {
            self.parts.push(LlmOutputPart::Text {
                text,
                response_meta,
            });
            return;
        };

        if let Some(LlmOutputPart::Text {
            text: existing,
            response_meta: existing_meta,
        }) = self.parts.get_mut(index)
        {
            if incoming_has_id {
                reconcile_text_snapshot(existing, &text);
            } else {
                append_stream_piece(existing, &text);
            }
            if response_meta.is_some() {
                *existing_meta = response_meta;
            }
        }
    }

    pub(super) fn push_tool_call(
        &mut self,
        call_id: String,
        tool_name: String,
        input_json: String,
        replay: Option<ProviderReplayMeta>,
    ) {
        if self.parts.iter().any(|part| {
            matches!(part, LlmOutputPart::ToolCall { call_id: existing, .. } if existing == &call_id)
        }) {
            return;
        }
        self.parts.push(LlmOutputPart::ToolCall {
            call_id,
            tool_name,
            input_json,
            replay,
        });
    }

    pub(super) fn push_reasoning(
        &mut self,
        text: String,
        item_id: Option<String>,
        summary: Vec<String>,
        encrypted_content: Option<String>,
    ) {
        let replay = ProviderReasoningReplay {
            item_id,
            encrypted_content,
            signature: None,
            redacted: false,
            summary,
        };
        self.push_reasoning_with_replay(text, (!replay.is_empty()).then_some(replay));
    }

    pub(super) fn push_reasoning_with_replay(
        &mut self,
        text: String,
        replay: Option<ProviderReasoningReplay>,
    ) {
        let replay_value = replay.clone().unwrap_or_default();
        if let Some(LlmOutputPart::Reasoning {
            text: existing,
            replay: existing_replay,
        }) = self.parts.last_mut()
            && existing_replay
                .as_ref()
                .is_none_or(ProviderReasoningReplay::is_empty)
            && replay_value.is_empty()
        {
            append_stream_piece(existing, &text);
            return;
        }
        if let Some(LlmOutputPart::Reasoning {
            text: existing,
            replay: existing_replay,
        }) = self.parts.last_mut()
            && !replay_value.is_empty()
            && existing_replay
                .as_ref()
                .is_none_or(ProviderReasoningReplay::is_empty)
            && !existing.trim().is_empty()
            && (text.trim().is_empty() || text.contains(existing.as_str()))
        {
            if !text.trim().is_empty() && text != *existing {
                *existing = text;
            }
            *existing_replay = replay;
            return;
        }
        if let Some(LlmOutputPart::Reasoning {
            text: existing,
            replay: existing_replay,
        }) = self.parts.last_mut()
            && replay_value.is_empty()
            && existing_replay
                .as_ref()
                .is_some_and(|meta| !meta.is_empty())
            && !text.trim().is_empty()
            && existing.trim().is_empty()
        {
            append_stream_piece(existing, &text);
            return;
        }
        self.parts.push(LlmOutputPart::Reasoning { text, replay });
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.parts.iter().any(|part| match part {
            LlmOutputPart::Text { text, .. } => !text.is_empty(),
            LlmOutputPart::Reasoning { .. } => true,
            LlmOutputPart::ToolCall { .. } => true,
        })
    }

    pub(super) fn full_text(&self) -> String {
        crate::visible_response_text_from_parts(&self.parts)
    }

    pub(super) fn apply_to_response(&self, response: &mut LlmResponse) {
        if self.is_empty() {
            return;
        }
        if response.parts.is_empty() {
            response.parts = self.parts.clone();
            if response.full_text.is_empty() {
                response.full_text = self.full_text();
            }
            return;
        }

        if response_contains_accumulated_parts(response, &self.parts) {
            return;
        }

        response.parts = reconcile_accumulated_parts(&self.parts, &response.parts);
        if response.full_text.is_empty() {
            response.full_text = crate::visible_response_text_from_parts(&response.parts);
        }
    }
}

pub(super) fn fold_llm_stream_event(
    accumulator: &mut LlmStreamAccumulator,
    usage: &mut LlmUsage,
    event: &LlmStreamEvent,
) {
    match event {
        LlmStreamEvent::AttemptReset => {
            *accumulator = LlmStreamAccumulator::default();
            *usage = LlmUsage::default();
        }
        LlmStreamEvent::Delta(text) => accumulator.push_text(text),
        LlmStreamEvent::ReasoningDelta(text) => {
            accumulator.push_reasoning(text.clone(), None, Vec::new(), None);
        }
        LlmStreamEvent::Part(LlmOutputPart::Text {
            text,
            response_meta,
        }) => accumulator.push_text_part(text.clone(), response_meta.clone()),
        LlmStreamEvent::Part(LlmOutputPart::ToolCall {
            call_id,
            tool_name,
            input_json,
            replay,
        }) => accumulator.push_tool_call(
            call_id.clone(),
            tool_name.clone(),
            input_json.clone(),
            replay.clone(),
        ),
        LlmStreamEvent::Part(LlmOutputPart::Reasoning { text, replay }) => {
            accumulator.push_reasoning_with_replay(text.clone(), replay.clone());
        }
        LlmStreamEvent::Usage(streamed) => *usage = streamed.clone(),
        LlmStreamEvent::Evidence(_) => {}
        LlmStreamEvent::RetryStatus { .. } => {}
    }
}

fn response_contains_accumulated_parts(
    response: &LlmResponse,
    accumulated_parts: &[LlmOutputPart],
) -> bool {
    accumulated_parts
        .iter()
        .filter(|part| part_has_visible_or_tool_content(part))
        .all(|part| response_contains_part(response, part))
}

fn response_contains_part(response: &LlmResponse, part: &LlmOutputPart) -> bool {
    match part {
        LlmOutputPart::Text { text, .. } => {
            text.trim().is_empty()
                || response.full_text.contains(text)
                || response.parts.iter().any(|candidate| {
                    matches!(candidate, LlmOutputPart::Text { text: candidate, .. } if candidate.contains(text))
                })
        }
        LlmOutputPart::Reasoning { text, replay } => {
            text.trim().is_empty()
                || response.parts.iter().any(|candidate| match candidate {
                    LlmOutputPart::Reasoning {
                        text: candidate,
                        replay: candidate_replay,
                        ..
                    } => {
                        let item_id = replay.as_ref().and_then(|meta| meta.item_id.as_ref());
                        let candidate_id = candidate_replay
                            .as_ref()
                            .and_then(|meta| meta.item_id.as_ref());
                        candidate.contains(text)
                            || (item_id.is_some()
                                && candidate_id.is_some()
                                && item_id == candidate_id
                                && !candidate.trim().is_empty())
                    }
                    _ => false,
                })
        }
        LlmOutputPart::ToolCall {
            call_id, replay, ..
        } => response.parts.iter().any(|candidate| match candidate {
            LlmOutputPart::ToolCall {
                call_id: candidate_call_id,
                replay: candidate_replay,
                ..
            } => {
                let item_id = replay.as_ref().and_then(|meta| meta.item_id.as_ref());
                let candidate_item_id = candidate_replay
                    .as_ref()
                    .and_then(|meta| meta.item_id.as_ref());
                candidate_call_id == call_id
                    || (item_id.is_some() && candidate_item_id.is_some() && item_id == candidate_item_id)
            }
            _ => false,
        }),
    }
}

fn reconcile_accumulated_parts(
    accumulated_parts: &[LlmOutputPart],
    final_parts: &[LlmOutputPart],
) -> Vec<LlmOutputPart> {
    let mut out = accumulated_parts.to_vec();
    for final_part in final_parts {
        match final_part {
            LlmOutputPart::ToolCall { .. } => {
                if let Some(existing) = out
                    .iter_mut()
                    .find(|candidate| tool_calls_match(candidate, final_part))
                {
                    *existing = final_part.clone();
                } else {
                    out.push(final_part.clone());
                }
            }
            LlmOutputPart::Reasoning { .. } => {
                if !out.iter().any(|candidate| reasoning_matches(candidate, final_part)) {
                    out.push(final_part.clone());
                }
            }
            LlmOutputPart::Text { text, .. } => {
                if !text.trim().is_empty()
                    && !out
                        .iter()
                        .any(|candidate| matches!(candidate, LlmOutputPart::Text { text: candidate, .. } if candidate.contains(text)))
                {
                    out.push(final_part.clone());
                }
            }
        }
    }
    out
}

fn part_has_visible_or_tool_content(part: &LlmOutputPart) -> bool {
    match part {
        LlmOutputPart::Text { text, .. } => !text.trim().is_empty(),
        LlmOutputPart::Reasoning { text, replay, .. } => {
            !text.trim().is_empty() || replay.as_ref().is_some_and(|meta| !meta.is_empty())
        }
        LlmOutputPart::ToolCall { .. } => true,
    }
}

fn tool_calls_match(candidate: &LlmOutputPart, expected: &LlmOutputPart) -> bool {
    match (candidate, expected) {
        (
            LlmOutputPart::ToolCall {
                call_id, replay, ..
            },
            LlmOutputPart::ToolCall {
                call_id: expected_call_id,
                replay: expected_replay,
                ..
            },
        ) => {
            let item_id = replay.as_ref().and_then(|meta| meta.item_id.as_ref());
            let expected_item_id = expected_replay
                .as_ref()
                .and_then(|meta| meta.item_id.as_ref());
            call_id == expected_call_id
                || (item_id.is_some() && expected_item_id.is_some() && item_id == expected_item_id)
        }
        _ => false,
    }
}

fn reasoning_matches(candidate: &LlmOutputPart, expected: &LlmOutputPart) -> bool {
    match (candidate, expected) {
        (
            LlmOutputPart::Reasoning { text, replay, .. },
            LlmOutputPart::Reasoning {
                text: expected_text,
                replay: expected_replay,
                ..
            },
        ) => {
            let item_id = replay.as_ref().and_then(|meta| meta.item_id.as_ref());
            let expected_item_id = expected_replay
                .as_ref()
                .and_then(|meta| meta.item_id.as_ref());
            let encrypted_content = replay
                .as_ref()
                .and_then(|meta| meta.encrypted_content.as_ref());
            let expected_encrypted_content = expected_replay
                .as_ref()
                .and_then(|meta| meta.encrypted_content.as_ref());
            (!text.trim().is_empty()
                && !expected_text.trim().is_empty()
                && (text.contains(expected_text) || expected_text.contains(text)))
                || (item_id.is_some() && expected_item_id.is_some() && item_id == expected_item_id)
                || (encrypted_content.is_some()
                    && expected_encrypted_content.is_some()
                    && encrypted_content == expected_encrypted_content)
        }
        _ => false,
    }
}

fn append_stream_piece(full: &mut String, piece: &str) {
    if piece.is_empty() {
        return;
    }
    if piece.starts_with(full.as_str()) {
        full.push_str(&piece[full.len()..]);
    } else {
        full.push_str(piece);
    }
}

fn reconcile_text_snapshot(existing: &mut String, snapshot: &str) {
    if snapshot.is_empty() || snapshot == existing {
        return;
    }
    if let Some(suffix) = snapshot.strip_prefix(existing.as_str()) {
        existing.push_str(suffix);
    } else {
        *existing = snapshot.to_string();
    }
}

pub(super) struct TurnAssembler {
    pub(super) tool_calls: Vec<ToolCallRecord>,
    pub(super) llm_calls: Vec<crate::LlmCallRecord>,
    pub(super) token_usage: TokenUsage,
    pub(super) last_llm_usage: Option<TokenUsage>,
    /// Latest `cumulative` reported by each child session, keyed by
    /// `(session_id, source, model)`. Cumulative is monotonically
    /// increasing per child session, so each new event for the same key
    /// supersedes the previous value. At `finish()` time we aggregate by
    /// `(source, model)` and sum across child sessions.
    pub(super) child_cumulatives: BTreeMap<(String, String, String), TokenUsage>,
    pub(super) issues: Vec<TurnIssue>,
    pub(super) saw_done: bool,
    pub(super) outcome: Option<TurnOutcome>,
}

impl Default for TurnAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnAssembler {
    pub(super) fn new() -> Self {
        Self {
            tool_calls: Vec::new(),
            llm_calls: Vec::new(),
            token_usage: TokenUsage::default(),
            last_llm_usage: None,
            child_cumulatives: BTreeMap::new(),
            issues: Vec::new(),
            saw_done: false,
            outcome: None,
        }
    }

    pub(super) fn push(&mut self, event: &SessionStreamEvent) {
        match event {
            SessionStreamEvent::ToolCall {
                call_id,
                name,
                args,
                output,
                duration_ms,
            } => {
                self.tool_calls.push(ToolCallRecord {
                    call_id: call_id.clone(),
                    tool: name.clone(),
                    args: args.clone(),
                    output: output.clone(),
                    duration_ms: *duration_ms,
                });
            }
            SessionStreamEvent::TokenUsage {
                usage, cumulative, ..
            } => {
                self.token_usage = cumulative.clone();
                self.last_llm_usage = Some(usage.clone());
            }
            SessionStreamEvent::ChildTokenUsage {
                session_id,
                source,
                model,
                cumulative,
                ..
            } => {
                self.child_cumulatives.insert(
                    (session_id.clone(), source.clone(), model.clone()),
                    cumulative.clone(),
                );
            }
            SessionStreamEvent::Error { message, envelope } => {
                let issue = if let Some(envelope) = envelope {
                    TurnIssue {
                        kind: envelope.kind.clone(),
                        code: envelope.code.clone(),
                        terminal_reason: envelope.terminal_reason,
                        message: message.clone(),
                        raw: envelope.raw.clone(),
                        retryable: envelope.retryable,
                        provider_failure_kind: envelope.provider_failure_kind,
                    }
                } else {
                    TurnIssue {
                        kind: "runtime".to_string(),
                        code: None,
                        terminal_reason: None,
                        message: message.clone(),
                        raw: None,
                        retryable: None,
                        provider_failure_kind: None,
                    }
                };
                self.issues.push(issue);
            }
            SessionStreamEvent::Done => {
                self.saw_done = true;
            }
            SessionStreamEvent::TurnOutcome { outcome } => {
                self.outcome = Some(outcome.clone());
            }
            _ => {}
        }
    }

    pub(super) fn with_llm_calls(mut self, llm_calls: Vec<crate::LlmCallRecord>) -> Self {
        self.llm_calls = llm_calls;
        self
    }

    pub(super) fn finish(
        mut self,
        state: crate::SessionSnapshot,
        interrupted: bool,
        force_runtime_error: Option<TurnIssue>,
        termination: &TerminationPolicy,
    ) -> AssembledTurn {
        let mut issues = self.issues;
        if let Some(issue) = force_runtime_error {
            issues.push(issue);
        }
        let read_model = state.read_model();
        let max_turn_reached = read_model.messages.iter().rev().take(8).any(|msg| {
            msg.role == MessageRole::System
                && msg
                    .parts
                    .iter()
                    .any(|part| part.content.contains("Turn limit reached ("))
        });

        let raw_output = if let Some(output) =
            self.outcome.as_ref().and_then(render_outcome_for_output)
        {
            output
        } else {
            let recovered = recovered_assistant_output_from_state(&state);
            if !recovered.is_empty() {
                issues.push(TurnIssue {
                    kind: "runtime".to_string(),
                    code: Some(ASSISTANT_OUTPUT_RECOVERED_FROM_STATE_CODE.to_string()),
                    terminal_reason: None,
                    message: "assistant output was recovered from persisted messages because no explicit assistant output was assembled".to_string(),
                    raw: None,
                    retryable: None,
                    provider_failure_kind: None,
                });
            }
            recovered
        };
        let safe_output = sanitize_assistant_output(raw_output.clone());

        let outcome = if interrupted {
            TurnOutcome::Stopped(TurnStop::Cancelled)
        } else if let Some(outcome) = self.outcome.take() {
            match outcome {
                TurnOutcome::Finished(TurnFinish::AssistantMessage { .. }) => {
                    TurnOutcome::Finished(TurnFinish::AssistantMessage {
                        text: safe_output.clone(),
                    })
                }
                outcome => outcome,
            }
        } else if !self.saw_done && termination.treat_missing_done_as_failure {
            issues.push(TurnIssue {
                kind: "runtime".to_string(),
                code: Some("missing_done".to_string()),
                terminal_reason: None,
                message: "turn stream ended without a Done event".to_string(),
                raw: None,
                retryable: None,
                provider_failure_kind: None,
            });
            TurnOutcome::Stopped(TurnStop::RuntimeError)
        } else if has_blocking_turn_issue(&issues) {
            if self
                .tool_calls
                .iter()
                .any(|record| !record.output.is_success())
            {
                TurnOutcome::Stopped(TurnStop::ToolFailure)
            } else {
                TurnOutcome::Stopped(TurnStop::RuntimeError)
            }
        } else if max_turn_reached {
            TurnOutcome::Stopped(TurnStop::MaxTurns)
        } else {
            TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: safe_output.clone(),
            })
        };
        let output_state = classify_output_state(&raw_output, &safe_output, &issues);

        let children_usage = child_cumulative_entries(self.child_cumulatives);

        AssembledTurn {
            execution: ExecutionSummary {
                had_tool_calls: !self.tool_calls.is_empty(),
                had_code_execution: false,
                // Timing is stamped by the turn loop, which owns the
                // claim → final-commit measurement window.
                started_at_ms: 0,
                duration_ms: 0,
            },
            state,
            outcome,
            cancellation: None,
            assistant_output: AssistantOutput {
                safe_text: safe_output,
                raw_text: raw_output,
                state: output_state,
            },
            token_usage: self.token_usage,
            children_usage,
            llm_calls: self.llm_calls,
            tool_calls: self.tool_calls,
            errors: issues,
        }
    }

    pub(super) fn last_llm_usage(&self) -> Option<&TokenUsage> {
        self.last_llm_usage.as_ref()
    }
}

/// Sum the latest cumulative usage reported by each `(session_id, source,
/// model)` triple into `(source, model)` ledger entries.
fn child_cumulative_entries(
    cumulatives: BTreeMap<(String, String, String), TokenUsage>,
) -> Vec<TokenLedgerEntry> {
    // Preserve one row per child session. The strict durable ledger merge owns
    // aggregation by `(source, model)` and can therefore reject overflow with
    // the same typed pre-commit error used for parent usage.
    cumulatives
        .into_iter()
        .map(|((_session_id, source, model), usage)| TokenLedgerEntry {
            source,
            model,
            usage,
        })
        .collect()
}

fn render_final_value_for_output(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn render_outcome_for_output(outcome: &TurnOutcome) -> Option<String> {
    match outcome {
        TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) => Some(text.clone()),
        TurnOutcome::Finished(TurnFinish::FinalValue { value })
        | TurnOutcome::Finished(TurnFinish::ToolValue { value, .. })
        | TurnOutcome::Stopped(TurnStop::SubmittedError { value })
        | TurnOutcome::Stopped(TurnStop::ToolError { value, .. }) => {
            Some(render_final_value_for_output(value))
        }
        TurnOutcome::AgentFrameSwitch { .. }
        | TurnOutcome::Stopped(
            TurnStop::Cancelled
            | TurnStop::Incomplete
            | TurnStop::InvalidInput
            | TurnStop::MaxTurns
            | TurnStop::ToolFailure
            | TurnStop::ProviderError
            | TurnStop::PluginAbort
            | TurnStop::RuntimeError,
        ) => None,
    }
}

pub(super) fn recovered_assistant_output_from_state(state: &crate::SessionSnapshot) -> String {
    let read_model = state.read_model();
    let messages = read_model.messages.as_slice();
    let latest_user_message_idx = messages
        .iter()
        .rposition(|message| matches!(message.role, MessageRole::User));
    let search_messages = latest_user_message_idx
        .map(|idx| &messages[idx.saturating_add(1)..])
        .unwrap_or(messages);
    search_messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| {
            message
                .parts
                .iter()
                .filter(|part| {
                    matches!(
                        part.kind,
                        PartKind::Text | PartKind::Prose | PartKind::Attachment
                    )
                })
                .map(|part| part.content.as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

pub(super) fn sanitize_assistant_output(text: String) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join(
            "
",
        )
        .trim()
        .to_string()
}

pub(super) fn classify_output_state(
    raw_text: &str,
    safe_text: &str,
    issues: &[TurnIssue],
) -> OutputState {
    if safe_text.is_empty() && raw_text.is_empty() {
        return OutputState::EmptyOutput;
    }
    if safe_text.is_empty() && contains_traceback_only(raw_text) {
        return OutputState::TracebackOnly;
    }
    if has_blocking_turn_issue(issues) && !safe_text.is_empty() {
        return OutputState::RecoveredFromError;
    }
    OutputState::Usable
}

const ASSISTANT_OUTPUT_RECOVERED_FROM_STATE_CODE: &str = "assistant_output_recovered_from_state";

fn has_blocking_turn_issue(issues: &[TurnIssue]) -> bool {
    issues.iter().any(turn_issue_blocks_completion)
}

fn turn_issue_blocks_completion(issue: &TurnIssue) -> bool {
    issue.code.as_deref() != Some(ASSISTANT_OUTPUT_RECOVERED_FROM_STATE_CODE)
}

fn contains_traceback_only(raw_text: &str) -> bool {
    if raw_text.is_empty() {
        return false;
    }
    let has_traceback = raw_text.contains("Traceback (most recent call last)")
        || raw_text.lines().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("Runtime error:")
                || trimmed.starts_with("NameError:")
                || trimmed.starts_with("TypeError:")
                || trimmed.starts_with("ValueError:")
                || trimmed.starts_with("KeyError:")
                || trimmed.starts_with("AttributeError:")
                || trimmed.starts_with("SyntaxError:")
                || trimmed.starts_with("ImportError:")
                || trimmed.starts_with("ModuleNotFoundError:")
        });
    if !has_traceback {
        return false;
    }
    // If no alphabetic prose besides traceback/exception formatting, treat as traceback-only.
    !raw_text.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("Traceback")
            || trimmed.starts_with("File ")
            || trimmed.starts_with("Runtime error:")
        {
            return false;
        }
        !trimmed.contains(':')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_usage_preserves_one_row_per_child_session() {
        let cumulatives = BTreeMap::from([
            (
                (
                    "child-a".to_string(),
                    "child".to_string(),
                    "shared-model".to_string(),
                ),
                TokenUsage {
                    input_tokens: 3,
                    ..TokenUsage::default()
                },
            ),
            (
                (
                    "child-b".to_string(),
                    "child".to_string(),
                    "shared-model".to_string(),
                ),
                TokenUsage {
                    input_tokens: 5,
                    ..TokenUsage::default()
                },
            ),
        ]);

        let entries = child_cumulative_entries(cumulatives);

        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| { entry.source == "child" && entry.model == "shared-model" })
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.usage.input_tokens)
                .collect::<Vec<_>>(),
            vec![3, 5]
        );
    }
}
