//! LLM stream accumulation: folding one provider call's stream events into
//! the response, debug and publication state the driver keeps for it.
//!
//! These types are crate-internal — sibling modules (`turn_driver.rs`,
//! `tests.rs`) import them via `use super::*` in `mod.rs`. A turn's committed
//! content is assembled elsewhere, from recorded state
//! (`turn_boundary::recorded_assembly`).

use std::collections::HashSet;
use std::time::Instant;

use lash_sansio::core_support::Blake3DomainHasher;
use serde_json::json;

use crate::llm::types::{
    LlmOutputPart, LlmResponse, LlmStreamEvent, LlmStreamEvidence, LlmUsage,
    ProviderReasoningReplay, ProviderReplayMeta, ResponseTextMeta, StreamBlockIdentity,
    StreamBlockKind,
};

#[derive(Clone, Debug, Default)]
pub struct LlmStreamAccumulator {
    pub parts: Vec<LlmOutputPart>,
    /// Provider-minted block id → index into `parts`, so deltas and
    /// authoritative block ends land on their own block rather than the tail.
    block_parts: std::collections::HashMap<String, usize>,
}

/// Reasoning blocks already published as live activity during one LLM
/// attempt.
///
/// Reconciliation is by block identity alone: a completed item-level
/// `LlmOutputPart::Reasoning` is already published when every summary entry
/// of its item had a streamed block. Completed-part text is never compared
/// with streamed text; the block's `item_id` + position in its item's block
/// order is the join. Blocks without an `item_id` reconcile positionally
/// against unstamped completed parts — the one case identity cannot cover.
#[derive(Clone, Debug, Default)]
pub(super) struct ReasoningPublicationState {
    published_blocks: Vec<StreamBlockIdentity>,
}

impl ReasoningPublicationState {
    /// The state a recorded LLM-call outcome carries: the blocks its live
    /// stream published.
    pub(super) fn from_published_blocks(published_blocks: Vec<StreamBlockIdentity>) -> Self {
        Self { published_blocks }
    }

    pub(super) fn into_published_blocks(self) -> Vec<StreamBlockIdentity> {
        self.published_blocks
    }

    /// Records a reasoning block that streamed (start or delta), so the
    /// completed part for its item does not re-publish it.
    pub(super) fn record_streamed_block(&mut self, block: &StreamBlockIdentity) {
        if self
            .published_blocks
            .iter()
            .any(|published| published.id == block.id)
        {
            return;
        }
        self.published_blocks.push(block.clone());
    }

    /// The ordinal where runtime-minted blocks begin: after every live block
    /// ordinal seen so far.
    pub(super) fn next_block_ordinal(&self) -> u64 {
        self.published_blocks
            .iter()
            .map(|block| block.ordinal + 1)
            .max()
            .unwrap_or(0)
    }

    /// Blocks of `part`'s reasoning item that were never streamed and still
    /// owe the host visible text, minted deterministically from the part.
    ///
    /// `part_index` is the part's position in the completed response;
    /// `next_ordinal` hands out ordinals continuing after the live blocks, so
    /// persistence and replay order by `ordinal` without parsing `id`.
    pub(super) fn unpublished_blocks(
        &self,
        part_index: usize,
        part: &LlmOutputPart,
        next_ordinal: &mut u64,
    ) -> Vec<(StreamBlockIdentity, String)> {
        let LlmOutputPart::Reasoning { text, replay } = part else {
            return Vec::new();
        };
        let item_id = replay
            .as_ref()
            .and_then(|meta| meta.item_id.as_deref())
            .filter(|item_id| !item_id.is_empty());
        let summary = replay
            .as_ref()
            .map(|meta| meta.summary.as_slice())
            .unwrap_or_default();
        let mut minted = Vec::new();
        let mut mint = |id: String, item_id: Option<&str>, text: &str| {
            let block = StreamBlockIdentity {
                id,
                ordinal: *next_ordinal,
                item_id: item_id.map(str::to_string),
            };
            *next_ordinal += 1;
            minted.push((block, text.to_string()));
        };
        match item_id {
            Some(item_id) => {
                let streamed = self
                    .published_blocks
                    .iter()
                    .filter(|block| block.item_id.as_deref() == Some(item_id))
                    .count();
                if summary.is_empty() {
                    if streamed == 0 && !text.is_empty() {
                        mint(item_id.to_string(), Some(item_id), text);
                    }
                } else {
                    for (index, entry) in summary.iter().enumerate().skip(streamed) {
                        mint(format!("{item_id}:summary:{index}"), Some(item_id), entry);
                    }
                }
            }
            None => {
                let anonymous_streamed = self
                    .published_blocks
                    .iter()
                    .filter(|block| block.item_id.is_none())
                    .count();
                if anonymous_streamed == 0 && !text.is_empty() {
                    mint(format!("part:{part_index}"), None, text);
                }
            }
        }
        minted
    }
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
    /// The streamed block's own identity — distinct blocks can share one
    /// provider `item_id`, so traces need both to keep sub-blocks apart.
    pub(super) block_id: Option<&'a str>,
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
    /// authoritative `TextBlockEnd` payload is reconciled against this raw
    /// accumulation: a prefix-extending completion forwards only the unseen
    /// tail through the plugin transform, while a non-prefix correction seals
    /// with the provider's text verbatim.
    pub(super) block_raw_text: &'a mut std::collections::BTreeMap<String, String>,
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
    /// Anonymous tail-append kept for tests that assemble accumulated parts
    /// without going through provider block events.
    #[allow(dead_code)]
    pub fn push_text(&mut self, piece: &str) {
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

    pub fn push_text_part(&mut self, text: String, response_meta: Option<ResponseTextMeta>) {
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

    pub fn push_tool_call(
        &mut self,
        call_id: String,
        tool_name: String,
        input_json: String,
        replay: Option<ProviderReplayMeta>,
    ) {
        self.parts.push(LlmOutputPart::ToolCall {
            call_id,
            tool_name,
            input_json,
            replay,
        });
    }

    /// Anonymous reasoning append kept for tests; production paths carry a
    /// provider-minted block identity.
    #[allow(dead_code)]
    pub fn push_reasoning(
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
            origin: None,
        };
        self.push_reasoning_with_replay(text, (!replay.is_empty()).then_some(replay));
    }

    pub fn push_reasoning_with_replay(
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

    /// Opens the part slot for a just-started stream block, or returns the
    /// existing slot when the block id is already known (a delta or end can
    /// legally arrive at a slot the start already opened).
    ///
    /// A block keeps one part slot. For text blocks the provider's message
    /// item id becomes `response_meta.id` — correlation identity supplied by
    /// the provider, never minted here. Reasoning blocks carry `item_id` in
    /// their replay meta so the item-level `Part(Reasoning)` event can
    /// consolidate them (see [`Self::consolidate_reasoning_item`]); encrypted
    /// content, signatures, and summary arrive with that item part.
    fn open_block(&mut self, block: &StreamBlockIdentity, kind: StreamBlockKind) -> usize {
        if let Some(index) = self.block_parts.get(&block.id) {
            return *index;
        }
        let index = self.parts.len();
        self.parts.push(match kind {
            StreamBlockKind::AssistantText => LlmOutputPart::Text {
                text: String::new(),
                response_meta: block.item_id.clone().map(|id| ResponseTextMeta {
                    id: Some(id),
                    ..ResponseTextMeta::default()
                }),
            },
            StreamBlockKind::Reasoning => LlmOutputPart::Reasoning {
                text: String::new(),
                replay: block
                    .item_id
                    .clone()
                    .map(|item_id| ProviderReasoningReplay {
                        item_id: Some(item_id),
                        ..ProviderReasoningReplay::default()
                    }),
            },
        });
        self.block_parts.insert(block.id.clone(), index);
        index
    }

    /// The accumulated text currently held for `block`'s part slot — the
    /// post-transform total matching what deltas forwarded to hosts.
    pub fn block_text(&self, block: &StreamBlockIdentity) -> Option<String> {
        let index = self.block_parts.get(&block.id)?;
        match self.parts.get(*index) {
            Some(LlmOutputPart::Text { text, .. })
            | Some(LlmOutputPart::Reasoning { text, .. }) => Some(text.clone()),
            _ => None,
        }
    }

    /// Appends a delta to `block`'s part (`authoritative == false`), or writes
    /// the block's end-of-stream authoritative text over it
    /// (`authoritative == true`). The end event seals the block; its text is
    /// what the provider certifies, including the empty text of a zero-delta
    /// block (redacted or signed-empty thinking).
    fn push_block_piece(
        &mut self,
        block: &StreamBlockIdentity,
        kind: StreamBlockKind,
        text: &str,
        authoritative: bool,
    ) {
        let index = self.open_block(block, kind);
        match &mut self.parts[index] {
            LlmOutputPart::Text {
                text: part_text, ..
            } if kind == StreamBlockKind::AssistantText => {
                if authoritative {
                    *part_text = text.to_string();
                } else {
                    append_stream_piece(part_text, text);
                }
            }
            LlmOutputPart::Reasoning {
                text: part_text, ..
            } if kind == StreamBlockKind::Reasoning => {
                if authoritative {
                    *part_text = text.to_string();
                } else {
                    append_stream_piece(part_text, text);
                }
            }
            _ => {}
        }
    }

    /// Folds a completed item-level `Reasoning` part over the per-block slots
    /// its item accumulated while streaming.
    ///
    /// `Part(Reasoning)` is pushed first (the newest matching slot is the
    /// authoritative item part), then every earlier block slot owned by the
    /// item is removed and the item part takes the earliest slot's position,
    /// so `parts` stays at item granularity and keeps one copy of the item's
    /// replay material.
    fn consolidate_reasoning_item(&mut self, item_id: &str) {
        let owned = |part: &LlmOutputPart| {
            matches!(part, LlmOutputPart::Reasoning { replay: Some(replay), .. }
                if replay.item_id.as_deref() == Some(item_id))
        };
        let indices: Vec<usize> = self
            .parts
            .iter()
            .enumerate()
            .filter(|(_, part)| owned(part))
            .map(|(index, _)| index)
            .collect();
        let Some(&last) = indices.last() else {
            return;
        };
        if indices.len() <= 1 {
            return;
        }
        let item_part = self.parts[last].clone();
        let keep = indices[0];
        let removed: std::collections::HashSet<usize> = indices.iter().copied().collect();
        let mut remap: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::with_capacity(self.parts.len());
        let mut consolidated = Vec::with_capacity(self.parts.len() - indices.len() + 1);
        for (old_index, part) in std::mem::take(&mut self.parts).into_iter().enumerate() {
            if removed.contains(&old_index) {
                if old_index == keep {
                    remap.insert(old_index, consolidated.len());
                    consolidated.push(item_part.clone());
                }
                continue;
            }
            remap.insert(old_index, consolidated.len());
            consolidated.push(part);
        }
        self.parts = consolidated;
        // Remap surviving block slots; drop ids whose slot was folded into
        // the item so a late event for a consolidated block opens fresh
        // rather than landing on the item part.
        self.block_parts.retain(|_, index| match remap.get(index) {
            Some(new_index) => {
                *index = *new_index;
                !owned(&self.parts[*index])
            }
            None => false,
        });
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.parts.iter().any(|part| match part {
            LlmOutputPart::Text { text, .. } => !text.is_empty(),
            LlmOutputPart::Reasoning { .. } => true,
            LlmOutputPart::ToolCall { .. } => true,
        })
    }

    pub fn apply_to_response_for_request(&self, response: &mut LlmResponse, request_id: &str) {
        if !self.is_empty() {
            if response.parts.is_empty() {
                response.parts = self.parts.clone();
            } else if !response_contains_accumulated_parts(response, &self.parts)
                || !tool_call_ids_unique(&self.parts)
            {
                response.parts = reconcile_accumulated_parts(&self.parts, &response.parts);
            }
        }
        repair_tool_call_ids(&mut response.parts, request_id);
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn apply_to_response(&self, response: &mut LlmResponse) {
        self.apply_to_response_for_request(response, "test-request");
    }
}

fn tool_call_ids_unique(parts: &[LlmOutputPart]) -> bool {
    let mut seen = HashSet::new();
    parts.iter().all(|part| match part {
        LlmOutputPart::ToolCall { call_id, .. } => seen.insert(call_id),
        _ => true,
    })
}

fn repair_tool_call_ids(parts: &mut [LlmOutputPart], request_id: &str) {
    // Reserve every provider id first so a minted id cannot displace a later
    // valid id in this response.
    let mut used: HashSet<String> = parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::ToolCall { call_id, .. } if !call_id.trim().is_empty() => {
                Some(call_id.clone())
            }
            _ => None,
        })
        .collect();
    let mut seen = HashSet::new();
    for (index, part) in parts.iter_mut().enumerate() {
        let LlmOutputPart::ToolCall { call_id, .. } = part else {
            continue;
        };
        if !call_id.trim().is_empty() && seen.insert(call_id.clone()) {
            continue;
        }
        for attempt in 0u64.. {
            let mut hash = Blake3DomainHasher::new("lash-provider-call-correlation/v1");
            hash.update((request_id.len() as u64).to_le_bytes());
            hash.update(request_id.as_bytes());
            hash.update((index as u64).to_le_bytes());
            hash.update(attempt.to_le_bytes());
            let digest = hash.finalize_hex();
            let replacement = format!("lashcall_{}", &digest[..24]);
            if used.insert(replacement.clone()) {
                *call_id = replacement;
                break;
            }
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
        LlmStreamEvent::TextBlockStart { block } => {
            accumulator.open_block(block, StreamBlockKind::AssistantText);
        }
        LlmStreamEvent::ReasoningBlockStart { block } => {
            accumulator.open_block(block, StreamBlockKind::Reasoning);
        }
        LlmStreamEvent::Delta { block, text } => {
            accumulator.push_block_piece(block, StreamBlockKind::AssistantText, text, false);
        }
        LlmStreamEvent::ReasoningDelta { block, text } => {
            accumulator.push_block_piece(block, StreamBlockKind::Reasoning, text, false);
        }
        LlmStreamEvent::TextBlockEnd { block, text } => {
            accumulator.push_block_piece(block, StreamBlockKind::AssistantText, text, true);
        }
        LlmStreamEvent::ReasoningBlockEnd { block, text } => {
            accumulator.push_block_piece(block, StreamBlockKind::Reasoning, text, true);
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
        LlmStreamEvent::RetryStatus { .. } => {}
        // Argument streaming is capture evidence only: the semantic response
        // takes the call whole from `Part(ToolCall)` (ADR 0114 §2.1).
        LlmStreamEvent::ToolInputStart { .. }
        | LlmStreamEvent::ToolInputDelta { .. }
        | LlmStreamEvent::ToolInputEnd { .. } => {}
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
    let mut matched_tool_slots = HashSet::new();
    for final_part in final_parts
        .iter()
        .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
    {
        if let Some(index) = out.iter().enumerate().position(|(index, candidate)| {
            !matched_tool_slots.contains(&index) && tool_calls_match(candidate, final_part)
        }) {
            out[index] = final_part.clone();
            matched_tool_slots.insert(index);
        } else {
            out.push(final_part.clone());
            matched_tool_slots.insert(out.len() - 1);
        }
    }
    for final_part in final_parts {
        match final_part {
            LlmOutputPart::ToolCall { .. } => {}
            LlmOutputPart::Reasoning { .. } => {
                let final_item_id = reasoning_part_item_id(final_part);
                if let Some(item_id) = final_item_id
                    && let Some(first) = out
                        .iter()
                        .position(|candidate| {
                            reasoning_part_item_id(candidate) == Some(item_id)
                        })
                {
                    // Per-block slots streamed under this item consolidate
                    // back to the item-level part — one item, one set of
                    // replay material — replacing them at their position.
                    out.retain(|candidate| {
                        reasoning_part_item_id(candidate) != Some(item_id)
                    });
                    out.insert(first.min(out.len()), final_part.clone());
                } else if !out
                    .iter()
                    .any(|candidate| reasoning_matches(candidate, final_part))
                {
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
            if item_id.is_some() && expected_item_id.is_some() {
                item_id == expected_item_id
            } else {
                call_id == expected_call_id
            }
        }
        _ => false,
    }
}

fn reasoning_part_item_id(part: &LlmOutputPart) -> Option<&str> {
    match part {
        LlmOutputPart::Reasoning {
            replay: Some(replay),
            ..
        } => replay
            .item_id
            .as_deref()
            .filter(|item_id| !item_id.is_empty()),
        _ => None,
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
