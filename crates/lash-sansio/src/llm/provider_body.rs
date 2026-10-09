//! Recorded JSON literals and attachment slots; deliveries exist only while sending.

use super::attachment_delivery::{AttachmentPosition, Delivery, DeliveryForms, ProviderAccepts};
use super::types::{GenerationReceipt, LlmContentBlock, LlmRequest, ProviderRouteIdentity};
use crate::AttachmentRef;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotCodec {
    pub name: Box<str>,
    pub revision: u32,
}

/// The codec of the canonical body: a request's own JSON.
const CANONICAL_CODEC: &str = "lash.canonical";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSlot {
    pub reference: AttachmentRef,
    pub position: AttachmentPosition,
    pub accepts: ProviderAccepts,
    pub codec: SlotCodec,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "segment", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestSegment {
    Literal { text: Arc<str> },
    Attachment { slot: Box<AttachmentSlot> },
}

/// The request an admitted call sends: literal JSON text and attachment
/// slots, in wire order. A value of this type is valid by construction: its
/// literals are non-empty and never adjacent, no slot sits inside a string or
/// has empty acceptance, and the literals with every slot filled are one JSON
/// value. Every constructor and its `Deserialize` check that once, so a send
/// never re-parses the body to prove it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "TemplateParts", into = "TemplateParts")]
pub struct RecordedRequestTemplate {
    /// Recorded fetch slack; omission on reopen is refused, never filled from the host.
    pub fetch_horizon: super::attachment_delivery::DeliveryFetchHorizon,
    pub route: ProviderRouteIdentity,
    response_mode: ResponseMode,
    pub generation: Option<GenerationReceipt>,
    /// What the lowering route read from the body it built and needs again
    /// at every send (Anthropic: the beta headers the body requires), so no
    /// attempt parses the body to learn it. Empty for most routes.
    pub wire_features: Vec<Box<str>>,
    segments: Vec<RequestSegment>,
}

// Body is a derived cache; only Transport is serialized as mode metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseMode {
    Body(bool),
    Transport(bool),
}

/// The serialized shape of a [`RecordedRequestTemplate`], unvalidated.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateParts {
    fetch_horizon: super::attachment_delivery::DeliveryFetchHorizon,
    route: ProviderRouteIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<GenerationReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    wire_features: Vec<Box<str>>,
    segments: Vec<RequestSegment>,
}

impl TryFrom<TemplateParts> for RecordedRequestTemplate {
    type Error = TemplateError;
    fn try_from(parts: TemplateParts) -> Result<Self, TemplateError> {
        let mut template = Self::from_recorded_segments(
            parts.route,
            parts.transport_stream,
            parts.generation,
            parts.segments,
        )?;
        template.fetch_horizon = parts.fetch_horizon;
        template.wire_features = parts.wire_features;
        Ok(template)
    }
}

impl From<RecordedRequestTemplate> for TemplateParts {
    fn from(template: RecordedRequestTemplate) -> Self {
        let transport_stream = template.transport_stream();
        Self {
            fetch_horizon: template.fetch_horizon,
            route: template.route,
            transport_stream,
            generation: template.generation,
            wire_features: template.wire_features,
            segments: template.segments,
        }
    }
}

/// A JSON tree as an adapter's request builder emits it: ordinary JSON, with
/// each attachment a typed node where the builder placed it. No JSON value a
/// tool or a host supplies can be an attachment node, and no node is found
/// by searching: [`RequestTemplateBuilder::json`] writes the tree once, each
/// attachment node as one slot.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplateJson {
    /// A subtree that holds no attachment.
    Json(serde_json::Value),
    Array(Vec<TemplateJson>),
    /// Keys in the order `serde_json` serializes a map.
    Object(std::collections::BTreeMap<String, TemplateJson>),
    Attachment {
        reference: Box<AttachmentRef>,
        position: AttachmentPosition,
    },
}

impl From<serde_json::Value> for TemplateJson {
    fn from(value: serde_json::Value) -> Self {
        Self::Json(value)
    }
}

impl From<Vec<TemplateJson>> for TemplateJson {
    fn from(values: Vec<TemplateJson>) -> Self {
        Self::Array(values)
    }
}

impl TemplateJson {
    pub fn attachment(reference: &AttachmentRef, position: AttachmentPosition) -> Self {
        Self::Attachment {
            reference: Box::new(reference.clone()),
            position,
        }
    }

    /// An object of `fields`.
    pub fn object<K: Into<String>>(fields: impl IntoIterator<Item = (K, TemplateJson)>) -> Self {
        Self::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }

    pub fn is_object(&self) -> bool {
        matches!(
            self,
            Self::Object(_) | Self::Json(serde_json::Value::Object(_))
        )
    }

    /// The plain JSON at `key` of this object: `None` when this is no
    /// object, has no such key, or holds an attachment under it.
    pub fn json_field(&self, key: &str) -> Option<&serde_json::Value> {
        match self {
            Self::Json(value) => value.get(key),
            Self::Object(fields) => match fields.get(key)? {
                Self::Json(value) => Some(value),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether this is an object with `key`.
    pub fn has_field(&self, key: &str) -> bool {
        match self {
            Self::Json(value) => value.get(key).is_some(),
            Self::Object(fields) => fields.contains_key(key),
            _ => false,
        }
    }

    /// The string at `key` of this object.
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.json_field(key)?.as_str()
    }

    /// Open one level of a plain JSON object or array so a child can be
    /// addressed as a node.
    fn open(&mut self) {
        if let Self::Json(value) = self {
            match std::mem::take(value) {
                serde_json::Value::Object(fields) => {
                    *self = Self::Object(
                        fields
                            .into_iter()
                            .map(|(key, value)| (key, Self::Json(value)))
                            .collect(),
                    );
                }
                serde_json::Value::Array(values) => {
                    *self = Self::Array(values.into_iter().map(Self::Json).collect());
                }
                other => *value = other,
            }
        }
    }

    /// The node at `key` of this object.
    pub fn field_mut(&mut self, key: &str) -> Option<&mut TemplateJson> {
        self.open();
        match self {
            Self::Object(fields) => fields.get_mut(key),
            _ => None,
        }
    }

    /// Set `key` of this object, replacing what it held. Returns whether
    /// this is an object.
    pub fn set(&mut self, key: &str, value: impl Into<TemplateJson>) -> bool {
        self.open();
        match self {
            Self::Object(fields) => {
                fields.insert(key.to_owned(), value.into());
                true
            }
            _ => false,
        }
    }

    /// Remove `key` from this object.
    pub fn remove(&mut self, key: &str) {
        match self {
            Self::Json(serde_json::Value::Object(fields)) => {
                fields.remove(key);
            }
            Self::Object(fields) => {
                fields.remove(key);
            }
            _ => {}
        }
    }

    /// The elements of this array, as nodes.
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<TemplateJson>> {
        self.open();
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    /// The tree as plain JSON, each attachment node shown as the marker
    /// [`RecordedRequestTemplate::redacted`] writes for its slot.
    pub fn redacted(&self) -> serde_json::Value {
        match self {
            Self::Json(value) => value.clone(),
            Self::Array(values) => values.iter().map(Self::redacted).collect(),
            Self::Object(fields) => serde_json::Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), value.redacted()))
                    .collect(),
            ),
            Self::Attachment {
                reference,
                position,
            } => redacted_slot(reference, *position),
        }
    }
}

fn redacted_slot(reference: &AttachmentRef, position: AttachmentPosition) -> serde_json::Value {
    serde_json::json!({"$lash_attachment": {
        "id": reference.id, "media_type": reference.media_type,
        "byte_len": reference.byte_len, "position": position
    }})
}

impl RecordedRequestTemplate {
    /// A template with no slot: `body` is the whole request.
    ///
    /// # Errors
    ///
    /// [`TemplateError`] when `body` is empty or not one JSON value.
    pub fn literal(
        route: ProviderRouteIdentity,
        stream: bool,
        generation: Option<GenerationReceipt>,
        body: impl Into<Arc<str>>,
    ) -> Result<Self, TemplateError> {
        Self::from_segments(
            route,
            stream,
            generation,
            vec![RequestSegment::Literal { text: body.into() }],
        )
    }

    /// The template of `segments`, checked once (see the type).
    ///
    /// # Errors
    ///
    /// [`TemplateError`] naming the rule `segments` break.
    pub fn from_segments(
        route: ProviderRouteIdentity,
        stream: bool,
        generation: Option<GenerationReceipt>,
        segments: Vec<RequestSegment>,
    ) -> Result<Self, TemplateError> {
        let body_stream = validate(&segments)?;
        if body_stream.is_some_and(|body_stream| body_stream != stream) {
            return Err(TemplateError::ResponseMode);
        }
        Self::from_validated_segments(
            route,
            body_stream,
            body_stream.is_none().then_some(stream),
            generation,
            segments,
        )
    }

    /// Reopen a record with mode metadata only when the body carries no mode.
    /// A second statement of the body's mode is refused, even when equal.
    ///
    /// # Errors
    ///
    /// [`TemplateError`] when the segments or mode are not canonical.
    pub fn from_recorded_segments(
        route: ProviderRouteIdentity,
        transport_stream: Option<bool>,
        generation: Option<GenerationReceipt>,
        segments: Vec<RequestSegment>,
    ) -> Result<Self, TemplateError> {
        let body_stream = validate(&segments)?;
        Self::from_validated_segments(route, body_stream, transport_stream, generation, segments)
    }

    fn from_validated_segments(
        route: ProviderRouteIdentity,
        body_stream: Option<bool>,
        transport_stream: Option<bool>,
        generation: Option<GenerationReceipt>,
        segments: Vec<RequestSegment>,
    ) -> Result<Self, TemplateError> {
        let response_mode = match (body_stream, transport_stream) {
            (Some(stream), None) => ResponseMode::Body(stream),
            (None, Some(stream)) => ResponseMode::Transport(stream),
            _ => return Err(TemplateError::ResponseMode),
        };
        Ok(Self {
            fetch_horizon: super::attachment_delivery::DeliveryFetchHorizon::standard(),
            route,
            response_mode,
            generation,
            wire_features: Vec::new(),
            segments,
        })
    }

    pub fn builder(
        route: ProviderRouteIdentity,
        stream: bool,
        generation: Option<GenerationReceipt>,
    ) -> RequestTemplateBuilder {
        RequestTemplateBuilder {
            route,
            stream,
            generation,
            segments: Vec::new(),
            literal: String::new(),
        }
    }

    /// The immutable mode, derived from the body when it names `stream`.
    pub fn stream(&self) -> bool {
        match self.response_mode {
            ResponseMode::Body(stream) | ResponseMode::Transport(stream) => stream,
        }
    }

    /// Mode recorded outside JSON (for example Google's URL method or a
    /// canonical in-process request). Absent when the body owns the mode.
    pub fn transport_stream(&self) -> Option<bool> {
        match self.response_mode {
            ResponseMode::Body(_) => None,
            ResponseMode::Transport(stream) => Some(stream),
        }
    }

    pub fn segments(&self) -> &[RequestSegment] {
        &self.segments
    }

    pub fn of_request(
        route: ProviderRouteIdentity,
        request: &LlmRequest,
    ) -> Result<Self, TemplateError> {
        let value = serde_json::to_value(request).map_err(|error| TemplateError::NotCanonical {
            reason: error.to_string(),
        })?;
        // Each attachment is placed where the request's own JSON names it.
        let mut tree = TemplateJson::from(value);
        let unplaced = || TemplateError::NotCanonical {
            reason: "the request's JSON does not hold an attachment where the request does".into(),
        };
        for (mi, message) in request.messages.iter().enumerate() {
            for (bi, block) in message.blocks.iter().enumerate() {
                match block {
                    LlmContentBlock::Attachment { reference } => {
                        *block_node(&mut tree, mi, bi)
                            .and_then(|block| block.field_mut("Attachment"))
                            .ok_or_else(unplaced)? =
                            TemplateJson::attachment(reference, AttachmentPosition::Message);
                    }
                    LlmContentBlock::ToolResult { content, .. } => {
                        for (pi, part) in content.iter().enumerate() {
                            let Some(reference) = part.attachment() else {
                                continue;
                            };
                            *block_node(&mut tree, mi, bi)
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
        }
        let provider = route.provider.clone();
        let mut builder = Self::builder(route, request.stream_events.is_some(), None);
        builder.json(&tree, &mut |reference, position| {
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
        })?;
        builder.finish()
    }

    /// The request a canonical template ([`Self::of_request`]) was lowered
    /// from, read back from its literals with each slot's ref restored where
    /// the request named it. It is the typed view an in-process model
    /// decides from: what the body says, never a request held beside it. The
    /// request carries no senders.
    ///
    /// # Errors
    ///
    /// [`TemplateError::NotCanonical`] when a slot is another codec's or the
    /// literals are not a request.
    pub fn canonical_request(&self) -> Result<LlmRequest, TemplateError> {
        let not_canonical = |reason: String| TemplateError::NotCanonical { reason };
        let mut text = String::new();
        for segment in &self.segments {
            match segment {
                RequestSegment::Literal { text: span } => text.push_str(span),
                RequestSegment::Attachment { slot } => {
                    if slot.codec.name.as_ref() != CANONICAL_CODEC {
                        return Err(not_canonical(format!(
                            "a slot is encoded by `{}`",
                            slot.codec.name
                        )));
                    }
                    let named = match slot.position {
                        AttachmentPosition::Message => {
                            serde_json::json!({ "reference": slot.reference })
                        }
                        AttachmentPosition::ToolResult => serde_json::to_value(
                            crate::ModelToolReturnPart::Attachment(slot.reference.clone()),
                        )
                        .map_err(|error| not_canonical(error.to_string()))?,
                    };
                    text.push_str(&named.to_string());
                }
            }
        }
        serde_json::from_str(&text).map_err(|error| not_canonical(error.to_string()))
    }

    pub fn slots(&self) -> impl Iterator<Item = &AttachmentSlot> {
        self.segments.iter().filter_map(|segment| match segment {
            RequestSegment::Attachment { slot } => Some(slot.as_ref()),
            RequestSegment::Literal { .. } => None,
        })
    }

    pub fn redacted(&self) -> String {
        let mut text = String::new();
        for segment in &self.segments {
            match segment {
                RequestSegment::Literal { text: span } => text.push_str(span),
                RequestSegment::Attachment { slot } => {
                    text.push_str(&redacted_slot(&slot.reference, slot.position).to_string());
                }
            }
        }
        text
    }
}

/// The JSON of block `bi` of message `mi` of a serialized request.
fn block_node(tree: &mut TemplateJson, mi: usize, bi: usize) -> Option<&mut TemplateJson> {
    tree.field_mut("messages")?
        .as_array_mut()?
        .get_mut(mi)?
        .field_mut("blocks")?
        .as_array_mut()?
        .get_mut(bi)
}

/// Check `segments` are a template (see [`RecordedRequestTemplate`]).
fn validate(segments: &[RequestSegment]) -> Result<Option<bool>, TemplateError> {
    let mut text = String::new();
    let mut literal = false;
    let mut in_string = false;
    let mut escape = false;
    for segment in segments {
        match segment {
            RequestSegment::Literal { text: span } => {
                if span.is_empty() {
                    return Err(TemplateError::EmptyLiteral);
                }
                if literal {
                    return Err(TemplateError::AdjacentLiterals);
                }
                literal = true;
                text.push_str(span);
                for byte in span.bytes() {
                    if escape {
                        escape = false;
                    } else if in_string && byte == b'\\' {
                        escape = true;
                    } else if byte == b'"' {
                        in_string = !in_string;
                    }
                }
            }
            RequestSegment::Attachment { slot } => {
                if in_string {
                    return Err(TemplateError::InvalidJson);
                }
                if slot.accepts.is_empty() {
                    return Err(TemplateError::EmptyAcceptance);
                }
                literal = false;
                text.push_str("null");
            }
        }
    }
    if text.trim_start().starts_with('{') {
        serde_json::from_str::<BodyResponseMode>(&text)
            .map(|body| body.stream)
            .map_err(|_| TemplateError::InvalidJson)
    } else {
        serde_json::from_str::<serde::de::IgnoredAny>(&text)
            .map(|_| None)
            .map_err(|_| TemplateError::InvalidJson)
    }
}

// Read only the top-level mode; serde skips other fields without allocating
// a JSON tree and refuses duplicate `stream` fields. Slots cannot supply mode.
#[derive(Deserialize)]
struct BodyResponseMode {
    #[serde(default, deserialize_with = "body_stream")]
    stream: Option<bool>,
}

fn body_stream<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}

pub struct RequestTemplateBuilder {
    route: ProviderRouteIdentity,
    stream: bool,
    generation: Option<GenerationReceipt>,
    segments: Vec<RequestSegment>,
    literal: String,
}
impl RequestTemplateBuilder {
    pub fn literal(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.literal.push_str(text.as_ref());
        self
    }
    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            self.segments.push(RequestSegment::Literal {
                text: Arc::from(std::mem::take(&mut self.literal)),
            });
        }
    }
    pub fn attachment(&mut self, slot: AttachmentSlot) -> &mut Self {
        self.flush_literal();
        self.segments.push(RequestSegment::Attachment {
            slot: Box::new(slot),
        });
        self
    }
    /// Write `tree` as `serde_json` serializes it, each attachment node as
    /// the slot `slot` pins for it.
    ///
    /// # Errors
    ///
    /// What `slot` refuses a node with.
    pub fn json<E>(
        &mut self,
        tree: &TemplateJson,
        slot: &mut impl FnMut(&AttachmentRef, AttachmentPosition) -> Result<AttachmentSlot, E>,
    ) -> Result<&mut Self, E> {
        match tree {
            TemplateJson::Json(value) => {
                self.literal(value.to_string());
            }
            TemplateJson::Array(values) => {
                self.literal("[");
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        self.literal(",");
                    }
                    self.json(value, slot)?;
                }
                self.literal("]");
            }
            TemplateJson::Object(fields) => {
                self.literal("{");
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        self.literal(",");
                    }
                    self.literal(serde_json::Value::String(key.clone()).to_string())
                        .literal(":");
                    self.json(value, slot)?;
                }
                self.literal("}");
            }
            TemplateJson::Attachment {
                reference,
                position,
            } => {
                let slot = slot(reference, *position)?;
                self.attachment(slot);
            }
        }
        Ok(self)
    }
    pub fn finish(mut self) -> Result<RecordedRequestTemplate, TemplateError> {
        self.flush_literal();
        RecordedRequestTemplate::from_segments(
            self.route,
            self.stream,
            self.generation,
            self.segments,
        )
    }
}

/// One encoded JSON value for the live wire, with no serialized form.
pub struct TransientJson {
    value: String,
    forms: DeliveryForms,
}
impl TransientJson {
    pub fn new(value: &serde_json::Value, delivery: &Delivery) -> Self {
        Self {
            value: value.to_string(),
            forms: delivery.forms(),
        }
    }
}
impl std::fmt::Debug for TransientJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TransientJson(<redacted>)")
    }
}

pub struct LiveRequestBody {
    template: Arc<RecordedRequestTemplate>,
    values: Vec<TransientJson>,
}
impl LiveRequestBody {
    pub fn fill(
        template: Arc<RecordedRequestTemplate>,
        values: Vec<TransientJson>,
    ) -> Result<Self, TemplateError> {
        let expected = template.slots().count();
        if expected != values.len() {
            return Err(TemplateError::SlotCount {
                expected,
                actual: values.len(),
            });
        }
        Ok(Self { template, values })
    }
    pub fn template(&self) -> &RecordedRequestTemplate {
        &self.template
    }
    /// See [`RecordedRequestTemplate::canonical_request`].
    ///
    /// # Errors
    ///
    /// [`TemplateError::NotCanonical`] when the body is not canonical.
    pub fn canonical_request(&self) -> Result<LlmRequest, TemplateError> {
        self.template.canonical_request()
    }
    pub fn route(&self) -> &ProviderRouteIdentity {
        &self.template.route
    }
    pub fn stream(&self) -> bool {
        self.template.stream()
    }
    pub fn generation(&self) -> Option<GenerationReceipt> {
        self.template.generation
    }
    /// The form each slot was delivered in, in slot order: the form's name,
    /// never its value.
    pub fn forms(&self) -> impl Iterator<Item = DeliveryForms> + '_ {
        self.values.iter().map(|value| value.forms)
    }
    /// The slots delivered as provider files, by slot index: the only slots
    /// a provider's refusal of a file id can name.
    pub fn provider_file_slots(&self) -> Vec<usize> {
        self.forms()
            .enumerate()
            .filter(|(_, forms)| forms.provider_file)
            .map(|(index, _)| index)
            .collect()
    }
    pub fn wire(&self) -> String {
        let mut text = String::new();
        let mut values = self.values.iter();
        for segment in &self.template.segments {
            match segment {
                RequestSegment::Literal { text: span } => text.push_str(span),
                RequestSegment::Attachment { .. } => {
                    if let Some(value) = values.next() {
                        text.push_str(&value.value);
                    }
                }
            }
        }
        text
    }
    pub fn redacted(&self) -> String {
        self.template.redacted()
    }
}
impl std::fmt::Debug for LiveRequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.redacted())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("a request template must name one response mode, agreeing with its body")]
    ResponseMode,
    #[error("a request template contains an empty literal")]
    EmptyLiteral,
    #[error("a request template contains adjacent literals")]
    AdjacentLiterals,
    #[error("a request template's literals and slots are not one JSON value")]
    InvalidJson,
    #[error("a request template expects {expected} slots, received {actual}")]
    SlotCount { expected: usize, actual: usize },
    #[error("an attachment slot has empty acceptance")]
    EmptyAcceptance,
    #[error("the body is not a canonical request: {reason}")]
    NotCanonical { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_mode_has_one_authority_and_preserves_literal_bytes() {
        let route = ProviderRouteIdentity {
            provider: "fixture".into(),
            endpoint: "https://fixture.test".into(),
            model: "model".into(),
        };
        let body = " { \"stream\" : true, \"input\" : \"literal\" } ";
        assert!(RecordedRequestTemplate::literal(route.clone(), false, None, body).is_err());
        let template = RecordedRequestTemplate::literal(route.clone(), true, None, body).unwrap();
        let encoded = serde_json::to_value(&template).unwrap();
        assert!(encoded.get("stream").is_none());
        assert!(encoded.get("transport_stream").is_none());
        let decoded: RecordedRequestTemplate = serde_json::from_value(encoded.clone()).unwrap();
        let live = LiveRequestBody::fill(Arc::new(decoded), vec![]).unwrap();
        assert!(live.stream());
        assert_eq!(live.wire(), body);
        for stream in [false, true] {
            let mut duplicate = encoded.clone();
            duplicate["transport_stream"] = serde_json::json!(stream);
            assert!(serde_json::from_value::<RecordedRequestTemplate>(duplicate).is_err());
        }
        let mut builder = RecordedRequestTemplate::builder(route.clone(), false, None);
        builder.literal(body);
        assert!(builder.finish().is_err());
        for invalid in [
            "{\"stream\":null}",
            "{\"stream\":1}",
            "{\"stream\":\"true\"}",
            "{\"stream\":true,\"stream\":false}",
        ] {
            assert!(RecordedRequestTemplate::literal(route.clone(), true, None, invalid).is_err());
        }
        let mut conflicting = encoded;
        conflicting["stream"] = serde_json::json!(false);
        assert!(serde_json::from_value::<RecordedRequestTemplate>(conflicting).is_err());
        for stream in [false, true] {
            let template =
                RecordedRequestTemplate::literal(route.clone(), stream, None, "{}").unwrap();
            let mut encoded = serde_json::to_value(template).unwrap();
            let decoded: RecordedRequestTemplate = serde_json::from_value(encoded.clone()).unwrap();
            encoded.as_object_mut().unwrap().remove("transport_stream");
            assert!(serde_json::from_value::<RecordedRequestTemplate>(encoded).is_err());
            assert_eq!(
                LiveRequestBody::fill(Arc::new(decoded), vec![])
                    .unwrap()
                    .stream(),
                stream
            );
        }
    }
}
