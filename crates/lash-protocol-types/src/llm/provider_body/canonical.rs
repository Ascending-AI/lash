//! The canonical body: a request's own JSON, one item per message.

use super::segment::Lexer;
use super::{
    AttachmentSlot, CANONICAL_CODEC, LoweredUnit, RecordedRequestTemplate, RequestSegment,
    SegmentCache, SegmentContext, SegmentKey, SegmentKind, SegmentPart, SlotCodec, TemplateError,
    TemplateJson,
};
use crate::llm::attachment_delivery::{AttachmentPosition, ProviderAccepts};
use crate::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, ProviderRouteIdentity};
use std::sync::Arc;

/// The request without its messages: what the canonical envelope writes.
/// A struct literal, so a new request field cannot be left out.
fn envelope(request: &LlmRequest) -> LlmRequest {
    let LlmRequest {
        instructions,
        model,
        messages: _,
        tools,
        tool_choice,
        attachment_acceptance,
        generation,
        scope,
        output_spec,
        stream_events,
        provider_trace,
        attempt_control,
    } = request;
    LlmRequest {
        instructions: instructions.clone(),
        model: model.clone(),
        messages: Vec::new(),
        tools: tools.clone(),
        tool_choice: tool_choice.clone(),
        attachment_acceptance: attachment_acceptance.clone(),
        generation: generation.clone(),
        scope: scope.clone(),
        output_spec: output_spec.clone(),
        stream_events: stream_events.clone(),
        provider_trace: provider_trace.clone(),
        attempt_control: attempt_control.clone(),
    }
}

fn not_canonical(reason: impl Into<String>) -> TemplateError {
    TemplateError::NotCanonical {
        reason: reason.into(),
    }
}

/// The JSON of block `bi` of a serialized message.
fn block_node(tree: &mut TemplateJson, bi: usize) -> Option<&mut TemplateJson> {
    tree.field_mut("blocks")?.as_array_mut()?.get_mut(bi)
}

/// `message`'s canonical item: its own JSON, each attachment a slot where
/// the message names it.
fn message_item(
    message: &LlmMessage,
    slot: &mut impl FnMut(
        &crate::AttachmentRef,
        AttachmentPosition,
    ) -> Result<AttachmentSlot, TemplateError>,
) -> Result<RequestSegment, TemplateError> {
    let value = serde_json::to_value(message).map_err(|error| not_canonical(error.to_string()))?;
    let mut tree = TemplateJson::from(value);
    let unplaced =
        || not_canonical("the request's JSON does not hold an attachment where the request does");
    for (bi, block) in message.blocks.iter().enumerate() {
        match block {
            LlmContentBlock::Attachment { reference } => {
                *block_node(&mut tree, bi)
                    .and_then(|block| block.field_mut("Attachment"))
                    .ok_or_else(unplaced)? =
                    TemplateJson::attachment(reference, AttachmentPosition::Message);
            }
            LlmContentBlock::ToolResult { content, .. } => {
                for (pi, part) in content.iter().enumerate() {
                    let Some(reference) = part.attachment() else {
                        continue;
                    };
                    *block_node(&mut tree, bi)
                        .and_then(|block| {
                            block
                                .field_mut("ToolResult")?
                                .field_mut("content")?
                                .as_array_mut()?
                                .get_mut(pi)
                        })
                        .ok_or_else(unplaced)? =
                        TemplateJson::attachment(reference, AttachmentPosition::ToolResult);
                }
            }
            _ => {}
        }
    }
    RequestSegment::item(&tree, slot)
}

/// Each immutable chunk owns its sources and derived items together. An
/// append shares complete chunks and copies at most this many records from
/// the final chunk. A branch keeps only its matching prefix, so a rejected
/// oversized suffix cannot remain reachable through an earlier cache entry.
const HISTORY_CHUNK_MESSAGES: usize = 32;

#[derive(Clone)]
struct LoweredMessage {
    source: LlmMessage,
    item: RequestSegment,
}

struct HistoryChunk {
    previous: Option<Arc<HistoryChunk>>,
    messages: Vec<LoweredMessage>,
    end: usize,
    bytes: usize,
}

impl Clone for HistoryChunk {
    fn clone(&self) -> Self {
        let mut messages = Vec::with_capacity(HISTORY_CHUNK_MESSAGES);
        messages.extend_from_slice(&self.messages);
        Self {
            previous: self.previous.clone(),
            messages,
            end: self.end,
            bytes: self.bytes,
        }
    }
}

impl Drop for HistoryChunk {
    fn drop(&mut self) {
        // Releasing a long history must not recurse through its chunk chain.
        let mut previous = self.previous.take();
        while let Some(chunk) = previous {
            match Arc::try_unwrap(chunk) {
                Ok(mut chunk) => previous = chunk.previous.take(),
                Err(_) => break,
            }
        }
    }
}

/// The bounded segment cache owns the latest immutable history. Calls and
/// concurrent branches can hold earlier snapshots without copying history.
#[derive(Default)]
struct LoweredHistory {
    tail: Option<Arc<HistoryChunk>>,
}

impl LoweredUnit for LoweredHistory {
    fn retained_bytes(&self) -> usize {
        self.tail.as_ref().map_or(0, |tail| tail.bytes)
    }
}

impl LoweredHistory {
    fn len(&self) -> usize {
        self.tail.as_ref().map_or(0, |tail| tail.end)
    }

    fn records(&self) -> impl Iterator<Item = &LoweredMessage> {
        let mut chunks = Vec::with_capacity(self.len().div_ceil(HISTORY_CHUNK_MESSAGES));
        let mut chunk = self.tail.as_deref();
        while let Some(held) = chunk {
            chunks.push(held);
            chunk = held.previous.as_deref();
        }
        chunks
            .into_iter()
            .rev()
            .flat_map(|chunk| chunk.messages.iter())
    }

    fn shared_prefix(&self, messages: &[LlmMessage]) -> usize {
        let mut shared = messages.len().min(self.len());
        let mut chunk = self.tail.as_deref();
        while let Some(held) = chunk {
            let start = held.end - held.messages.len();
            if start < shared {
                let end = held.end.min(shared);
                let matched = held
                    .messages
                    .iter()
                    .zip(&messages[start..end])
                    .take_while(|(held, message)| {
                        held.source.role == message.role
                            && held.source.starts_user_segment == message.starts_user_segment
                            && Arc::ptr_eq(&held.source.blocks, &message.blocks)
                    })
                    .count();
                if matched < end - start {
                    shared = start + matched;
                }
            }
            chunk = held.previous.as_deref();
        }
        shared
    }

    fn items(&self) -> impl Iterator<Item = &RequestSegment> {
        self.records().map(|record| &record.item)
    }

    fn prefix(&self, len: usize) -> Self {
        let mut chunk = self.tail.as_ref();
        while let Some(held) = chunk {
            let start = held.end - held.messages.len();
            if len == held.end {
                return Self {
                    tail: Some(held.clone()),
                };
            }
            if len > start {
                let mut messages = Vec::with_capacity(HISTORY_CHUNK_MESSAGES);
                messages.extend_from_slice(&held.messages[..len - start]);
                let bytes = held.previous.as_ref().map_or(0, |previous| previous.bytes)
                    + messages
                        .iter()
                        .map(|message| message.item.literal_len())
                        .sum::<usize>();
                return Self {
                    tail: Some(Arc::new(HistoryChunk {
                        previous: held.previous.clone(),
                        messages,
                        end: len,
                        bytes,
                    })),
                };
            }
            chunk = held.previous.as_ref();
        }
        Self::default()
    }

    fn push(&mut self, source: LlmMessage, item: RequestSegment) {
        let end = self.len();
        let bytes = self.retained_bytes();
        let previous = if self
            .tail
            .as_ref()
            .is_some_and(|tail| tail.messages.len() == HISTORY_CHUNK_MESSAGES)
        {
            self.tail.take()
        } else {
            None
        };
        let tail = self.tail.get_or_insert_with(|| {
            Arc::new(HistoryChunk {
                end,
                bytes,
                previous,
                messages: Vec::with_capacity(HISTORY_CHUNK_MESSAGES),
            })
        });
        let tail = Arc::make_mut(tail);
        tail.end += 1;
        tail.bytes += item.literal_len();
        tail.messages.push(LoweredMessage { source, item });
    }

    fn extend(
        prior: Option<Arc<Self>>,
        messages: &[LlmMessage],
        slot: &mut impl FnMut(
            &crate::AttachmentRef,
            AttachmentPosition,
        ) -> Result<AttachmentSlot, TemplateError>,
    ) -> Result<Arc<Self>, TemplateError> {
        let shared = prior
            .as_ref()
            .map_or(0, |prior| prior.shared_prefix(messages));
        if let Some(prior) = &prior
            && shared == messages.len()
            && shared == prior.len()
        {
            return Ok(Arc::clone(prior));
        }
        Self::from_suffix(prior.as_deref(), shared, messages, slot)
    }

    fn from_suffix(
        prior: Option<&Self>,
        shared: usize,
        messages: &[LlmMessage],
        slot: &mut impl FnMut(
            &crate::AttachmentRef,
            AttachmentPosition,
        ) -> Result<AttachmentSlot, TemplateError>,
    ) -> Result<Arc<Self>, TemplateError> {
        let mut history = prior.map_or_else(Self::default, |prior| prior.prefix(shared));
        for message in &messages[shared..] {
            let item = message_item(message, slot)?;
            history.push(message.clone(), item);
        }
        Ok(Arc::new(history))
    }
}

#[derive(Default)]
struct RetainedHistory {
    source: crate::llm::types::RequestMessages,
    items: super::RequestItems,
    bytes: usize,
    _hold: crate::allocation::AllocationHold,
}
impl LoweredUnit for RetainedHistory {
    fn retained_bytes(&self) -> usize {
        self.bytes
    }
}

impl RecordedRequestTemplate {
    /// Canonical lowering from the owner's immutable request tree.
    pub fn of_retained_request(
        route: ProviderRouteIdentity,
        request: &crate::llm::types::RetainedLlmRequest,
        cache: &SegmentCache,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, TemplateError> {
        let envelope = request.envelope();
        let envelope_hold = budget.reserve(crate::allocation::payload_capacity(envelope, 2048))?;
        let provider = route.provider.clone();
        let context = SegmentContext::new("lash.canonical.retained.message/1")
            .u64(budget.identity() as u64)
            .str(&provider)
            .json(envelope.attachment_acceptance.as_ref())
            .finish();
        let mut slot = |reference: &crate::AttachmentRef, position| {
            let accepts = ProviderAccepts {
                bytes: true,
                url: true,
                provider_file: None,
            }
            .narrowed(envelope.attachment_acceptance.forms(
                &provider,
                &reference.media_type,
                position,
            ));
            if accepts.is_empty() {
                return Err(TemplateError::EmptyAcceptance);
            }
            Ok(AttachmentSlot {
                reference: reference.clone(),
                position,
                accepts,
                codec: SlotCodec {
                    name: CANONICAL_CODEC.into(),
                    revision: 1,
                },
            })
        };
        let history = match request.messages().iter().next() {
            Some(first) => cache.carried(
                SegmentKey::new(&context).str("history").message(first),
                |prior: Option<Arc<RetainedHistory>>| {
                    let metadata =
                        budget.reserve((std::mem::size_of::<RetainedHistory>() + 256) as u64)?;
                    let shared = prior
                        .as_ref()
                        .map_or(0, |prior| prior.source.shared_prefix(request.messages()));
                    let mut items = match &prior {
                        Some(prior) => prior.items.prefix(shared, budget)?,
                        None => super::RequestItems::default(),
                    };
                    let mut tail = Vec::new();
                    for message in request.messages().iter_from(shared) {
                        let hold = budget.reserve(crate::allocation::payload_capacity(
                            message,
                            std::mem::size_of::<RequestSegment>(),
                        ))?;
                        tail.push(message_item(message, &mut slot)?.with_allocation(hold));
                    }
                    items = items.append(&tail, budget)?;
                    let bytes = items.bytes;
                    Ok::<_, TemplateError>(Arc::new(RetainedHistory {
                        source: request.messages().clone(),
                        items,
                        bytes,
                        _hold: metadata,
                    }))
                },
            )?,
            None => Arc::new(RetainedHistory::default()),
        };
        let value =
            serde_json::to_value(envelope).map_err(|error| not_canonical(error.to_string()))?;
        let serde_json::Value::Object(fields) = value else {
            return Err(not_canonical("request envelope is not an object"));
        };
        let fields: std::collections::BTreeMap<_, _> = fields.into_iter().collect();
        let mut before = String::from("{");
        let mut after = String::new();
        let mut output = &mut before;
        for (index, (key, value)) in fields.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            output.push_str(
                &serde_json::to_string(key).map_err(|error| not_canonical(error.to_string()))?,
            );
            output.push(':');
            if key == "messages" {
                output.push('[');
                after.push(']');
                output = &mut after;
            } else {
                output.push_str(&value.to_string());
            }
        }
        after.push('}');
        Self::from_item_run(
            route,
            envelope.stream_events.is_some(),
            None,
            before,
            history.items.clone(),
            after,
            budget,
        )
        .map(|mut template| {
            template._hold = envelope_hold;
            template
        })
    }

    /// The canonical template of `request`: its own JSON, each message one
    /// item, each attachment a slot with the request's effective
    /// acceptance. The matching history prefix is carried in `cache` by
    /// identity; only the changed suffix lowers again. Cache loss simply
    /// rebuilds it from the request and never changes an admitted body.
    ///
    /// # Errors
    ///
    /// [`TemplateError`] when the request does not serialize or an
    /// attachment has empty acceptance.
    pub fn of_request(
        route: ProviderRouteIdentity,
        request: &LlmRequest,
        cache: &SegmentCache,
    ) -> Result<Self, TemplateError> {
        let provider = route.provider.clone();
        let mut slot = |reference: &crate::AttachmentRef, position| {
            let accepts = ProviderAccepts {
                bytes: true,
                url: true,
                provider_file: None,
            }
            .narrowed(request.attachment_acceptance.forms(
                &provider,
                &reference.media_type,
                position,
            ));
            if accepts.is_empty() {
                return Err(TemplateError::EmptyAcceptance);
            }
            Ok(AttachmentSlot {
                reference: reference.clone(),
                position,
                accepts,
                codec: SlotCodec {
                    name: CANONICAL_CODEC.into(),
                    revision: 1,
                },
            })
        };
        let context = SegmentContext::new("lash.canonical.message/1")
            .str(&provider)
            .json(request.attachment_acceptance.as_ref())
            .finish();
        let history = match request.messages.first() {
            Some(first) => cache.carried(
                SegmentKey::new(&context).str("history").message(first),
                |prior| LoweredHistory::extend(prior, &request.messages, &mut slot),
            )?,
            None => Arc::new(LoweredHistory::default()),
        };
        let value = serde_json::to_value(envelope(request))
            .map_err(|error| not_canonical(error.to_string()))?;
        let serde_json::Value::Object(fields) = value else {
            return Err(not_canonical("the request's JSON is not an object"));
        };
        // TemplateJson's object writer orders keys even when a consumer
        // enables serde_json's preserve_order feature.
        let fields: std::collections::BTreeMap<_, _> = fields.into_iter().collect();
        let mut builder = Self::builder(route, request.stream_events.is_some(), None);
        builder.literal("{");
        for (index, (key, value)) in fields.iter().enumerate() {
            if index != 0 {
                builder.literal(",");
            }
            builder.literal(
                serde_json::to_string(key).map_err(|error| not_canonical(error.to_string()))?,
            );
            builder.literal(":");
            if key == "messages" {
                builder.literal("[");
                for item in history.items() {
                    builder.segment(item);
                }
                builder.literal("]");
            } else {
                builder.literal(value.to_string());
            }
        }
        builder.literal("}");
        builder.finish()
    }

    /// The request a canonical template ([`Self::of_request`]) was lowered
    /// from, read back from its literals with each slot's ref restored where
    /// the request named it. It is the typed view an in-process model
    /// decides from: what the body says, never a request held beside it. The
    /// request carries no senders.
    ///
    /// Where the body's items are exactly its `messages` array, the envelope
    /// is read without them and each message is read once per item and kept
    /// with it; any other body is read whole.
    ///
    /// # Errors
    ///
    /// [`TemplateError::NotCanonical`] when a slot is another codec's or the
    /// literals are not a request.
    pub fn canonical_request(&self) -> Result<LlmRequest, TemplateError> {
        if let Some(request) = self.canonical_by_items()? {
            return Ok(request);
        }
        let mut text = String::with_capacity(self.literal_len);
        for part in self.parts() {
            match part {
                super::BodyPart::Literal(span) => text.push_str(span),
                super::BodyPart::Separator => text.push(','),
                super::BodyPart::Slot(slot) => text.push_str(&canonical_slot_value(slot)?),
            }
        }
        serde_json::from_str(&text).map_err(|error| not_canonical(error.to_string()))
    }

    /// The fast read: `None` when the body's items are not exactly its
    /// top-level `messages` array.
    fn canonical_by_items(&self) -> Result<Option<LlmRequest>, TemplateError> {
        let Some(first) = self.segments.iter().position(RequestSegment::is_item) else {
            return Ok(None);
        };
        let end = self.segments[first..]
            .iter()
            .position(|segment| !segment.is_item())
            .map_or(self.segments.len(), |offset| first + offset);
        if self.segments[end..].iter().any(RequestSegment::is_item) {
            return Ok(None);
        }
        let mut envelope = String::new();
        for segment in &self.segments[..first] {
            if !push_text(&mut envelope, segment) {
                return Ok(None);
            }
        }
        let splice = envelope.len();
        for segment in &self.segments[end..] {
            if !push_text(&mut envelope, segment) {
                return Ok(None);
            }
        }
        if !messages_array_at(&envelope, splice) {
            return Ok(None);
        }
        let mut request: LlmRequest =
            serde_json::from_str(&envelope).map_err(|error| not_canonical(error.to_string()))?;
        request.messages.reserve_exact(end - first);
        for item in &self.segments[first..end] {
            let decoded = item.derived(|item| CanonicalMessage(decode_item(item)));
            match &decoded.0 {
                Ok(message) => request.messages.push(message.clone()),
                Err(reason) => return Err(not_canonical(reason.clone())),
            }
        }
        Ok(Some(request))
    }
}

/// The message an item says, kept with the item.
struct CanonicalMessage(Result<LlmMessage, String>);

fn decode_item(item: &RequestSegment) -> Result<LlmMessage, String> {
    let mut text = String::with_capacity(item.literal_len());
    for part in item.parts() {
        match part {
            SegmentPart::Literal(literal) => text.push_str(literal),
            SegmentPart::Slot(slot) => {
                text.push_str(&canonical_slot_value(slot).map_err(|error| error.to_string())?);
            }
        }
    }
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

/// The JSON a canonical request names `slot`'s attachment with.
fn canonical_slot_value(slot: &AttachmentSlot) -> Result<String, TemplateError> {
    if slot.codec.name.as_ref() != CANONICAL_CODEC {
        return Err(not_canonical(format!(
            "a slot is encoded by `{}`",
            slot.codec.name
        )));
    }
    Ok(match slot.position {
        AttachmentPosition::Message => serde_json::json!({ "reference": slot.reference }),
        AttachmentPosition::ToolResult => serde_json::to_value(
            crate::ModelToolReturnPart::Attachment(slot.reference.clone()),
        )
        .map_err(|error| not_canonical(error.to_string()))?,
    }
    .to_string())
}

/// Append a text segment with no slot; `false` for anything else.
fn push_text(text: &mut String, segment: &RequestSegment) -> bool {
    if segment.kind() != SegmentKind::Text {
        return false;
    }
    for part in segment.parts() {
        match part {
            SegmentPart::Literal(literal) => text.push_str(literal),
            SegmentPart::Slot(_) => return false,
        }
    }
    true
}

/// Whether offset `splice` of `envelope` stands inside an otherwise empty
/// array that is the value of the top-level object's `"messages"` key.
fn messages_array_at(envelope: &str, splice: usize) -> bool {
    let (before, after) = envelope.split_at(splice);
    let mut lexer = Lexer::default();
    let mut key_start = None;
    for (offset, byte) in before.bytes().enumerate() {
        if !lexer.in_string && byte == b'"' && lexer.open.as_slice() == b"{" {
            key_start = Some(offset);
        }
        lexer.scan(&[byte]);
    }
    if lexer.in_string || lexer.open.as_slice() != b"{[" {
        return false;
    }
    let Some(key_start) = key_start else {
        return false;
    };
    let Some(rest) = before[key_start..].strip_prefix("\"messages\"") else {
        return false;
    };
    let Some(rest) = rest.trim_start().strip_prefix(':') else {
        return false;
    };
    rest.trim_start()
        .strip_prefix('[')
        .is_some_and(|rest| rest.trim().is_empty())
        && after.trim_start().starts_with(']')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::LlmRole;
    use allocation_counter::Measurement;

    /// LOWERED-APPEND: carrying a validated prefix copies at most one
    /// bounded tail chunk, independently of the retained history depth.
    #[test]
    fn carrying_a_validated_canonical_prefix_has_bounded_allocation() {
        let counts = [1, 10, 1000].map(|len| {
            let messages: Vec<_> = (0..=len)
                .map(|index| LlmMessage::text(LlmRole::User, format!("message {index:04}")))
                .collect();
            let mut slot = |_: &crate::AttachmentRef, _: AttachmentPosition| {
                panic!("this history has no attachments")
            };
            let prior = LoweredHistory::extend(None, &messages[..len], &mut slot).unwrap();
            let measurement = Measurement::start();
            let next = LoweredHistory::extend(Some(prior.clone()), &messages, &mut slot).unwrap();
            let counts = measurement.finish();
            assert_eq!(prior.items().count(), len);
            assert_eq!(next.items().count(), len + 1);
            for (held, next) in prior.items().zip(next.items()) {
                assert!(RequestSegment::same(held, next));
            }
            let rebuilt = LoweredHistory::extend(None, &messages, &mut slot).unwrap();
            assert_eq!(next.retained_bytes(), rebuilt.retained_bytes());
            for (next, rebuilt) in next.items().zip(rebuilt.items()) {
                assert_eq!(next.text_lossy(), rebuilt.text_lossy());
            }
            // Edits and compaction may cut through any chunk. They keep
            // only the matching prefix, without changing a held snapshot.
            for edit in [0, len / 2, len - 1] {
                let mut branch = messages.clone();
                branch[edit] = LlmMessage::text(LlmRole::Assistant, "rewritten");
                let changed =
                    LoweredHistory::extend(Some(prior.clone()), &branch, &mut slot).unwrap();
                let rebuilt = LoweredHistory::extend(None, &branch, &mut slot).unwrap();
                assert_eq!(changed.retained_bytes(), rebuilt.retained_bytes());
                for (changed, rebuilt) in changed.items().zip(rebuilt.items()) {
                    assert_eq!(changed.text_lossy(), rebuilt.text_lossy());
                }
                for (held, changed) in prior.items().zip(changed.items()).take(edit) {
                    assert!(RequestSegment::same(held, changed));
                }
            }
            for end in [0, 1, len / 2] {
                let compacted =
                    LoweredHistory::extend(Some(prior.clone()), &messages[..end], &mut slot)
                        .unwrap();
                assert_eq!(compacted.items().count(), end);
                assert_eq!(
                    compacted.retained_bytes(),
                    prior
                        .items()
                        .take(end)
                        .map(RequestSegment::literal_len)
                        .sum::<usize>()
                );
                for (held, compacted) in prior.items().zip(compacted.items()) {
                    assert!(RequestSegment::same(held, compacted));
                }
            }
            assert_eq!(prior.items().count(), len);
            counts
        });
        eprintln!("canonical prefix carry at 1, 10 and 1000 messages: {counts:?}");
        assert!(
            counts
                .iter()
                .all(|count| count.allocated <= counts[0].allocated),
            "carrying an unchanged prefix must not copy its history vectors: {counts:?}"
        );
        assert!(
            counts
                .iter()
                .all(|count| count.allocations <= counts[0].allocations)
        );
    }

    pub(crate) mod allocation_counter {
        //! Allocation requests on the measuring thread.
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        #[derive(Clone, Copy, Debug, Default)]
        pub(crate) struct Counts {
            pub(crate) allocated: usize,
            pub(crate) allocations: usize,
        }

        thread_local! {
            static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) };
        }

        fn charge(bytes: usize) {
            let _ = COUNTS.try_with(|cell| {
                if let Some(mut counts) = cell.get() {
                    counts.allocated += bytes;
                    counts.allocations += 1;
                    cell.set(Some(counts));
                }
            });
        }

        struct Counter;

        #[global_allocator]
        static ALLOCATOR: Counter = Counter;

        #[expect(
            unsafe_code,
            reason = "test allocator forwards the unchanged GlobalAlloc contract to System"
        )]
        unsafe impl GlobalAlloc for Counter {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                charge(layout.size());
                // SAFETY: The caller supplies a valid layout; System receives it unchanged.
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                charge(layout.size());
                // SAFETY: The caller supplies a valid layout; System receives it unchanged.
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: Allocations came from System with the same layout.
                unsafe { System.dealloc(ptr, layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                charge(new_size);
                // SAFETY: The pointer, layout and new size keep the caller's contract.
                unsafe { System.realloc(ptr, layout, new_size) }
            }
        }

        pub(crate) struct Measurement;

        impl Measurement {
            pub(crate) fn start() -> Self {
                COUNTS.with(|cell| {
                    assert!(cell.get().is_none(), "allocation measurements cannot nest");
                    cell.set(Some(Counts::default()));
                });
                Self
            }

            pub(crate) fn finish(self) -> Counts {
                COUNTS.with(|cell| cell.take().expect("active allocation measurement"))
            }
        }

        impl Drop for Measurement {
            fn drop(&mut self) {
                COUNTS.with(|cell| cell.set(None));
            }
        }
    }
}
