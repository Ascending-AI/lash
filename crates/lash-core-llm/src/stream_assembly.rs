//! Pure stream accumulation, reasoning publication identities and response reconciliation.

/// version_surface = "coexist"
/// version_guard(items(LASH_PROVIDER_CALL_CORRELATION_DOMAIN_VERSION, repair_tool_call_ids))
const LASH_PROVIDER_CALL_CORRELATION_DOMAIN_VERSION: &str = "lash-provider-call-correlation/v1";

use crate::llm::types::{
    LlmOutputPart, ProviderReasoningReplay, ProviderReplayMeta, ResponseTextMeta,
    StreamBlockIdentity, StreamBlockKind,
};
use lash_protocol_types::core_support::Blake3DomainHasher;
use std::collections::HashSet;

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
pub struct ReasoningPublicationState {
    published_blocks: Vec<StreamBlockIdentity>,
}

impl ReasoningPublicationState {
    /// The state a recorded LLM-call outcome carries: the blocks its live
    /// stream published.
    pub fn from_published_blocks(published_blocks: Vec<StreamBlockIdentity>) -> Self {
        Self { published_blocks }
    }

    pub fn into_published_blocks(self) -> Vec<StreamBlockIdentity> {
        self.published_blocks
    }

    /// Records a reasoning block that streamed (start or delta), so the
    /// completed part for its item does not re-publish it.
    pub fn record_streamed_block(&mut self, block: &StreamBlockIdentity) {
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
    pub fn next_block_ordinal(&self) -> u64 {
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
    pub fn unpublished_blocks(
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

impl LlmStreamAccumulator {
    /// Anonymous tail-append kept for tests that assemble accumulated parts
    /// without going through provider block events.
    #[cfg(any(test, feature = "testing"))]
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
    #[cfg(any(test, feature = "testing"))]
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
    pub fn open_block(&mut self, block: &StreamBlockIdentity, kind: StreamBlockKind) -> usize {
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
    pub fn push_block_piece(
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
    pub fn consolidate_reasoning_item(&mut self, item_id: &str) {
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

    pub fn is_empty(&self) -> bool {
        !self.parts.iter().any(|part| match part {
            LlmOutputPart::Text { text, .. } => !text.is_empty(),
            LlmOutputPart::Reasoning { .. } => true,
            LlmOutputPart::ToolCall { .. } => true,
        })
    }

    /// Reconciles the streamed parts with the provider's final parts and
    /// repairs call ids deterministically within this request.
    pub fn response_parts(
        &self,
        mut parts: Vec<LlmOutputPart>,
        request_id: &str,
    ) -> Vec<LlmOutputPart> {
        if !self.is_empty() {
            if parts.is_empty() {
                parts = self.parts.clone();
            } else if !response_contains_accumulated_parts(&parts, &self.parts)
                || !provider_call_ids_unique(&self.parts)
            {
                parts = reconcile_accumulated_parts(&self.parts, &parts);
            }
        }
        repair_tool_call_ids(&mut parts, request_id);
        parts
    }
}

fn provider_call_ids_unique(parts: &[LlmOutputPart]) -> bool {
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
            let mut hash = Blake3DomainHasher::new(LASH_PROVIDER_CALL_CORRELATION_DOMAIN_VERSION);
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

fn response_contains_accumulated_parts(
    response: &[LlmOutputPart],
    accumulated_parts: &[LlmOutputPart],
) -> bool {
    accumulated_parts
        .iter()
        .filter(|part| part_has_visible_or_tool_content(part))
        .all(|part| response_contains_part(response, part))
}

fn response_contains_part(response: &[LlmOutputPart], part: &LlmOutputPart) -> bool {
    match part {
        LlmOutputPart::Text { text, .. } => {
            text.trim().is_empty()
                || response.iter().any(|candidate| {
                    matches!(candidate, LlmOutputPart::Text { text: candidate, .. } if candidate.contains(text))
                })
        }
        LlmOutputPart::Reasoning { text, replay } => {
            text.trim().is_empty()
                || response.iter().any(|candidate| match candidate {
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
        } => response.iter().any(|candidate| match candidate {
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

#[cfg(test)]
mod tests;
