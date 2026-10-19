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

/// An immutable lowered history. The source Arcs keep identities alive and
/// the matching prefix's segments keep all their derived admission facts.
/// Only this call's changed suffix is lowered. The whole value is replaced
/// in the bounded cache, so growth and branches stay inside its budget.
#[derive(Default)]
struct LoweredHistory {
    sources: Vec<LlmMessage>,
    items: Vec<RequestSegment>,
}

impl LoweredUnit for LoweredHistory {
    fn retained_bytes(&self) -> usize {
        self.items.iter().map(RequestSegment::literal_len).sum()
    }
}

impl LoweredHistory {
    fn extend(
        prior: Option<Arc<Self>>,
        messages: &[LlmMessage],
        slot: &mut impl FnMut(
            &crate::AttachmentRef,
            AttachmentPosition,
        ) -> Result<AttachmentSlot, TemplateError>,
    ) -> Result<Arc<Self>, TemplateError> {
        let shared = prior.as_ref().map_or(0, |prior| {
            prior
                .sources
                .iter()
                .zip(messages)
                .take_while(|(held, message)| {
                    held.role == message.role
                        && held.starts_user_segment == message.starts_user_segment
                        && Arc::ptr_eq(&held.blocks, &message.blocks)
                })
                .count()
        });
        if let Some(prior) = &prior
            && shared == messages.len()
            && shared == prior.sources.len()
        {
            return Ok(Arc::clone(prior));
        }
        let mut items = Vec::with_capacity(messages.len());
        if let Some(prior) = prior {
            items.extend(prior.items[..shared].iter().cloned());
        }
        for message in &messages[shared..] {
            items.push(message_item(message, slot)?);
        }
        Ok(Arc::new(Self {
            sources: messages.to_vec(),
            items,
        }))
    }
}

impl RecordedRequestTemplate {
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
                for item in &history.items {
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
