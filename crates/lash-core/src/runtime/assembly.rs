//! Runtime stream coordination and trace timing.

use crate::llm::types::{
    LlmOutputPart, LlmStreamEvent, LlmStreamEvidence, LlmUsage, StreamBlockEvent,
};
use lash_core_llm::stream_assembly::{LlmStreamAccumulator, ReasoningPublicationState};
use serde_json::json;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub(super) struct LlmStreamDebugState {
    pub(super) created_at: Instant,
    pub(super) sequence: u64,
    pub(super) summary: LlmStreamSummary,
}

pub(super) struct LlmStreamState<'a> {
    pub(super) address: crate::plugin::ModelCallAddress,
    pub(super) trace: crate::trace::TraceStanding,
    pub(super) trace_context: lash_trace::TraceContext,
    pub(super) text_streamed: &'a mut bool,
    pub(super) streamed_usage: &'a mut LlmUsage,
    pub(super) stream_accumulator: &'a mut LlmStreamAccumulator,
    pub(super) stream_evidence: &'a mut LlmStreamEvidence,
    pub(super) debug: &'a mut LlmStreamDebugState,
    pub(super) protocol_iteration: usize,
    /// Reasoning blocks the runtime itself mints for plugin-emitted reasoning
    /// deltas (providers mint every other block). Counted per call so ids are
    /// deterministic: `plugin-reasoning:{protocol_iteration}:{n}`.
    pub(super) plugin_reasoning_blocks: &'a mut u64,
    /// Position of the next completed `Part` event in the response, used to
    /// mint deterministic `part:{n}` identities for unstamped completed parts.
    pub(super) completed_part_index: &'a mut usize,
    pub(super) reasoning_publication: &'a mut ReasoningPublicationState,
    pub(super) assistant_prose_attempt_correlations: &'a mut Vec<crate::TurnActivityId>,
    pub(super) reasoning_attempt_correlations: &'a mut Vec<crate::TurnActivityId>,
    /// The LLM runner checks this after each stream event and short-circuits the select loop,
    /// synthesizing a response from the already-streamed parts.
    pub(super) abort_requested: &'a mut bool,
    /// Pre-transform text accumulated per streamed block id. The
    /// authoritative block-completion payload is reconciled against this raw
    /// accumulation: a prefix-extending completion forwards only the unseen
    /// tail through the plugin transform, while a non-prefix correction seals
    /// with the provider's text verbatim.
    pub(super) block_raw_text: &'a mut std::collections::BTreeMap<String, String>,
    /// Every visible chunk handed to the stream hooks since the attempt
    /// began, raw: the receipt's capsule, which a derivation replays.
    pub(super) stream_chunks: &'a mut Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LlmStreamSummary {
    pub(super) first_chunk_latency_ms: Option<u64>,
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
            "first_chunk_latency_ms": self.first_chunk_latency_ms,
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
        LlmStreamEvent::Block(StreamBlockEvent::Started { kind, block }) => {
            accumulator.open_block(block, *kind);
        }
        LlmStreamEvent::Block(StreamBlockEvent::Delta { kind, block, text }) => {
            accumulator.push_block_piece(block, *kind, text, false);
        }
        LlmStreamEvent::Block(StreamBlockEvent::Completed { kind, block, text }) => {
            accumulator.push_block_piece(block, *kind, text, true);
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
            let item_id = replay
                .as_ref()
                .and_then(|meta| meta.item_id.clone())
                .filter(|item_id| !item_id.is_empty());
            accumulator.push_reasoning_with_replay(text.clone(), replay.clone());
            // Streamed blocks of this item sit in per-block part slots. The
            // completed item part is authoritative at item granularity, so it
            // supersedes its blocks here — one reasoning item stays one part
            // with one set of replay material.
            if let Some(item_id) = item_id {
                accumulator.consolidate_reasoning_item(&item_id);
            }
        }
        LlmStreamEvent::Usage(streamed) => *usage = streamed.clone(),
        LlmStreamEvent::Evidence(_) => {}
        LlmStreamEvent::RetryStatus(_) => {}
    }
}
