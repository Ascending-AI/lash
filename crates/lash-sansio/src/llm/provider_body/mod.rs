//! Request bodies as persistent segments with attachment slots; deliveries
//! exist only while sending (docs/design/provider-body-segments.md).

use super::attachment_delivery::{
    AttachmentPosition, Delivery, DeliveryForms, ProviderAccepts, ProviderFileScope,
};
use super::types::{GenerationReceipt, ProviderRouteIdentity};
use crate::AttachmentRef;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

mod cache;
mod canonical;
mod segment;
#[cfg(test)]
mod tests;

pub use cache::{
    LoweredUnit, SEGMENT_CACHE_BUDGET_BYTES, SegmentCache, SegmentCacheStats, SegmentContext,
    SegmentContextKey, SegmentKey,
};
use segment::{Lexer, Writer};
pub use segment::{LoweredItem, RequestSegment, SegmentKind, SegmentPart};

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

/// One piece of a body in wire order, as [`RecordedRequestTemplate::parts`]
/// reads it: the literals and slots of its segments, with the separator the
/// body writes between two adjacent items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyPart<'a> {
    Literal(&'a Arc<str>),
    /// The `,` between two adjacent items.
    Separator,
    Slot(&'a AttachmentSlot),
}

/// The request an admitted call sends: a sequence of shared
/// [`RequestSegment`]s, each JSON text or one item, with attachment slots
/// in wire order. A value of this type is valid by construction: its
/// literals are non-empty, no slot sits inside a string or has empty
/// acceptance, no item run stands at the top level and a run of several
/// items stands in an array, and the body with every slot filled is one
/// JSON value. Every constructor and its `Deserialize` check that once,
/// reading each item only when the item is built, so a send never re-parses
/// the body to prove it.
///
/// Two templates are equal when their metadata, bytes and slots are, however
/// they are segmented.
///
/// A provider plugin controls every byte: [`Self::segments`] reads each
/// segment, [`Self::edit_segments`] maps, replaces, inserts or drops them,
/// and [`Self::to_json`] / [`Self::from_json`] hand the whole body over as
/// one tree.
///
/// ```
/// use lash_sansio::llm::types::{
///     ProviderRouteIdentity, RecordedRequestTemplate, RequestSegment,
/// };
///
/// let route = ProviderRouteIdentity::for_endpoint("example", "https://example.test", "model");
/// let mut builder = RecordedRequestTemplate::builder(route, true, None);
/// builder.literal(r#"{"stream":true,"input":["#);
/// builder.segment(&RequestSegment::item_json(&serde_json::json!("a")).unwrap());
/// builder.segment(&RequestSegment::item_json(&serde_json::json!("b")).unwrap());
/// builder.literal("]}");
/// let template = builder.finish().unwrap();
/// // Adjacent items are joined by the body.
/// assert_eq!(template.redacted(), r#"{"stream":true,"input":["a","b"]}"#);
/// // Drop the first item; the separator goes with it.
/// let edited = template
///     .edit_segments(|segments| {
///         segments.remove(1);
///     })
///     .unwrap();
/// assert_eq!(edited.redacted(), r#"{"stream":true,"input":["b"]}"#);
/// ```
#[derive(Clone, Debug)]
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
    /// Literal bytes and separators: the body's length without slot values.
    literal_len: usize,
}

// Body is a derived cache; only Transport is serialized as mode metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseMode {
    Body(bool),
    Transport(bool),
}

impl PartialEq for RecordedRequestTemplate {
    fn eq(&self, other: &Self) -> bool {
        self.fetch_horizon == other.fetch_horizon
            && self.route == other.route
            && self.response_mode == other.response_mode
            && self.generation == other.generation
            && self.wire_features == other.wire_features
            && self.literal_len == other.literal_len
            && same_wire(self, other)
    }
}

impl Eq for RecordedRequestTemplate {}

/// Whether `left` and `right` hold the same bytes and slots in order.
fn same_wire(left: &RecordedRequestTemplate, right: &RecordedRequestTemplate) -> bool {
    let mut left = WireCursor::new(left.parts());
    let mut right = WireCursor::new(right.parts());
    loop {
        left.fill();
        right.fill();
        match (left.slot.take(), right.slot.take()) {
            (Some(a), Some(b)) if a == b => {}
            (Some(_), _) | (_, Some(_)) => return false,
            (None, None) => {
                let shared = left.bytes.len().min(right.bytes.len());
                if shared == 0 {
                    return left.bytes.is_empty() && right.bytes.is_empty();
                }
                if left.bytes[..shared] != right.bytes[..shared] {
                    return false;
                }
                left.bytes = &left.bytes[shared..];
                right.bytes = &right.bytes[shared..];
            }
        }
    }
}

/// Reads a body's parts as runs of bytes and slots.
struct WireCursor<'a, I> {
    parts: I,
    bytes: &'a [u8],
    slot: Option<&'a AttachmentSlot>,
}

impl<'a, I: Iterator<Item = BodyPart<'a>>> WireCursor<'a, I> {
    fn new(parts: I) -> Self {
        Self {
            parts,
            bytes: &[],
            slot: None,
        }
    }

    /// Load the next bytes or slot once the current ones are consumed.
    fn fill(&mut self) {
        while self.bytes.is_empty() && self.slot.is_none() {
            match self.parts.next() {
                None => return,
                Some(BodyPart::Literal(text)) => self.bytes = text.as_bytes(),
                Some(BodyPart::Separator) => self.bytes = b",",
                Some(BodyPart::Slot(slot)) => self.slot = Some(slot),
            }
        }
    }
}

/// One serialized part of a [`RecordedRequestTemplate`]: the body flattened
/// to literal text and slots.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "segment", rename_all = "snake_case", deny_unknown_fields)]
enum FlatPart {
    Literal { text: Arc<str> },
    Attachment { slot: Box<AttachmentSlot> },
}

/// The serialized shape of a [`RecordedRequestTemplate`], unvalidated.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateParts {
    fetch_horizon: super::attachment_delivery::DeliveryFetchHorizon,
    route: ProviderRouteIdentity,
    #[serde(default)]
    transport_stream: Option<bool>,
    #[serde(default)]
    generation: Option<GenerationReceipt>,
    #[serde(default)]
    wire_features: Vec<Box<str>>,
    segments: Vec<FlatPart>,
}

impl Serialize for RecordedRequestTemplate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        /// [`TemplateParts`] by reference: the body is written as it is
        /// read, never copied whole.
        #[derive(Serialize)]
        struct TemplatePartsRef<'a> {
            fetch_horizon: super::attachment_delivery::DeliveryFetchHorizon,
            route: &'a ProviderRouteIdentity,
            #[serde(skip_serializing_if = "Option::is_none")]
            transport_stream: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            generation: Option<GenerationReceipt>,
            #[serde(skip_serializing_if = "<[_]>::is_empty")]
            wire_features: &'a [Box<str>],
            segments: FlatParts<'a>,
        }
        TemplatePartsRef {
            fetch_horizon: self.fetch_horizon,
            route: &self.route,
            transport_stream: self.transport_stream(),
            generation: self.generation,
            wire_features: &self.wire_features,
            segments: FlatParts(self),
        }
        .serialize(serializer)
    }
}

/// A template's body as its serialized parts: each run of literal text
/// between slots one coalesced literal, written piece by piece.
struct FlatParts<'a>(&'a RecordedRequestTemplate);

impl Serialize for FlatParts<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeSeq, SerializeStruct};

        /// One coalesced literal: its pieces, in order.
        struct Run<'a>(&'a [&'a str]);
        impl std::fmt::Display for Run<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.iter().try_for_each(|piece| f.write_str(piece))
            }
        }
        impl Serialize for Run<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }
        struct Literal<'a>(Run<'a>);
        impl Serialize for Literal<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut part = serializer.serialize_struct("FlatPart", 2)?;
                part.serialize_field("segment", "literal")?;
                part.serialize_field("text", &self.0)?;
                part.end()
            }
        }
        struct Attachment<'a>(&'a AttachmentSlot);
        impl Serialize for Attachment<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut part = serializer.serialize_struct("FlatPart", 2)?;
                part.serialize_field("segment", "attachment")?;
                part.serialize_field("slot", self.0)?;
                part.end()
            }
        }

        let mut seq = serializer.serialize_seq(None)?;
        let mut run: Vec<&str> = Vec::new();
        for part in self.0.parts() {
            match part {
                BodyPart::Literal(text) => run.push(text),
                BodyPart::Separator => run.push(","),
                BodyPart::Slot(slot) => {
                    if !run.is_empty() {
                        seq.serialize_element(&Literal(Run(&run)))?;
                        run.clear();
                    }
                    seq.serialize_element(&Attachment(slot))?;
                }
            }
        }
        if !run.is_empty() {
            seq.serialize_element(&Literal(Run(&run)))?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for RecordedRequestTemplate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let parts = TemplateParts::deserialize(deserializer)?;
        // The recorded shape is coalesced: two literals are never adjacent.
        if parts
            .segments
            .windows(2)
            .any(|pair| matches!(pair, [FlatPart::Literal { .. }, FlatPart::Literal { .. }]))
        {
            return Err(serde::de::Error::custom(TemplateError::AdjacentLiterals));
        }
        let segment_parts = parts
            .segments
            .into_iter()
            .map(|part| match part {
                FlatPart::Literal { text } => SegmentPart::Literal(text),
                FlatPart::Attachment { slot } => SegmentPart::Slot(slot),
            })
            .collect();
        let segment = RequestSegment::from_parts(SegmentKind::Text, segment_parts)
            .map_err(serde::de::Error::custom)?;
        let mut template = Self::from_recorded_segments(
            parts.route,
            parts.transport_stream,
            parts.generation,
            vec![segment],
        )
        .map_err(serde::de::Error::custom)?;
        template.fetch_horizon = parts.fetch_horizon;
        template.wire_features = parts.wire_features;
        Ok(template)
    }
}

/// A JSON tree as an adapter's request builder emits it: ordinary JSON, with
/// each attachment a typed node where the builder placed it and each reused
/// item the segment it already is. No JSON value a tool or a host supplies
/// can be an attachment node, and no node is found by searching:
/// [`RequestTemplateBuilder::json`] writes the tree once, each attachment
/// node as one slot and each lowered item as itself.
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
    /// An item already lowered: written as its segment, shared, never read
    /// again. Opaque to the field helpers below.
    Lowered(LoweredItem),
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

impl From<LoweredItem> for TemplateJson {
    fn from(item: LoweredItem) -> Self {
        Self::Lowered(item)
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
            Self::Lowered(item) => {
                serde_json::from_str(&item.segment().text_lossy()).unwrap_or_default()
            }
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
        Self::from_segments(route, stream, generation, vec![RequestSegment::text(body)?])
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

    /// The template of the whole-body tree `json`, each attachment node a
    /// slot `slot` pins: for a plugin that rewrites the body as one value
    /// ([`Self::to_json`]). The result is one text segment, so nothing in
    /// it is reused by the next call.
    ///
    /// # Errors
    ///
    /// What `slot` refuses a node with, or the [`TemplateError`] the body
    /// breaks.
    pub fn from_json<E: From<TemplateError>>(
        route: ProviderRouteIdentity,
        stream: bool,
        generation: Option<GenerationReceipt>,
        json: &TemplateJson,
        slot: &mut impl FnMut(&AttachmentRef, AttachmentPosition) -> Result<AttachmentSlot, E>,
    ) -> Result<Self, E> {
        let mut builder = Self::builder(route, stream, generation);
        builder.json(json, slot)?;
        Ok(builder.finish()?)
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
        let mut template = Self {
            fetch_horizon: super::attachment_delivery::DeliveryFetchHorizon::standard(),
            route,
            response_mode,
            generation,
            wire_features: Vec::new(),
            segments,
            literal_len: 0,
        };
        template.literal_len = template
            .parts()
            .map(|part| match part {
                BodyPart::Literal(text) => text.len(),
                BodyPart::Separator => 1,
                BodyPart::Slot(_) => 0,
            })
            .sum();
        Ok(template)
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
            writer: Writer::new(false),
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

    /// This template with its segments edited by `edit`: a plugin maps,
    /// replaces, inserts or drops segments, and the result is checked again
    /// as a new template. Its metadata stays. Adjacent items are written
    /// with a `,` between them, so an edit never writes separators.
    ///
    /// ```
    /// use lash_sansio::llm::types::{ProviderRouteIdentity, RecordedRequestTemplate, RequestSegment};
    ///
    /// let route = ProviderRouteIdentity::for_endpoint("example", "https://example.test", "model");
    /// let item = |value| RequestSegment::item_json(&serde_json::json!(value)).unwrap();
    /// let template = RecordedRequestTemplate::from_segments(
    ///     route,
    ///     false,
    ///     None,
    ///     vec![
    ///         RequestSegment::text(r#"{"messages":["#).unwrap(),
    ///         item("first"),
    ///         item("second"),
    ///         RequestSegment::text("]}").unwrap(),
    ///     ],
    /// )
    /// .unwrap();
    /// let edited = template
    ///     .edit_segments(|segments| {
    ///         segments.remove(1);
    ///         segments.insert(2, item("third"));
    ///     })
    ///     .unwrap();
    /// assert_eq!(edited.redacted(), r#"{"messages":["second","third"]}"#);
    /// ```
    ///
    /// # Errors
    ///
    /// [`TemplateError`] naming the rule the edited segments break.
    pub fn edit_segments(
        self,
        edit: impl FnOnce(&mut Vec<RequestSegment>),
    ) -> Result<Self, TemplateError> {
        let stream = self.stream();
        let Self {
            fetch_horizon,
            route,
            generation,
            wire_features,
            mut segments,
            ..
        } = self;
        edit(&mut segments);
        let mut template = Self::from_segments(route, stream, generation, segments)?;
        template.fetch_horizon = fetch_horizon;
        template.wire_features = wire_features;
        Ok(template)
    }

    /// This template with `map` applied to each slot, in wire order: a
    /// plugin that re-pins what a slot accepts. Segments with no slot
    /// change stay shared.
    ///
    /// ```
    /// use lash_sansio::llm::types::{ProviderRouteIdentity, RecordedRequestTemplate};
    ///
    /// let route = ProviderRouteIdentity::for_endpoint("example", "https://example.test", "model");
    /// let template = RecordedRequestTemplate::literal(route, false, None, "{}").unwrap();
    /// let mut seen = 0;
    /// let mapped = template.map_slots(|_slot| seen += 1).unwrap();
    /// assert_eq!(seen, 0);
    /// assert_eq!(mapped.redacted(), "{}");
    /// ```
    ///
    /// # Errors
    ///
    /// [`TemplateError`] when a mapped slot accepts nothing.
    pub fn map_slots(
        self,
        mut map: impl FnMut(&mut AttachmentSlot),
    ) -> Result<Self, TemplateError> {
        let mapped = self
            .segments
            .iter()
            .map(|segment| segment.map_slots(&mut map))
            .collect::<Result<Vec<_>, _>>()?;
        self.edit_segments(|segments| *segments = mapped)
    }

    /// The whole body as one tree, each slot an attachment node: for a
    /// plugin that reads or rewrites the body as one value. Build the
    /// rewritten template with [`Self::from_json`].
    ///
    /// ```
    /// use lash_sansio::llm::types::{ProviderRouteIdentity, RecordedRequestTemplate, TemplateError};
    ///
    /// let route = ProviderRouteIdentity::for_endpoint("example", "https://example.test", "model");
    /// let template =
    ///     RecordedRequestTemplate::literal(route.clone(), false, None, r#"{"messages":[]}"#).unwrap();
    /// let mut tree = template.to_json().unwrap();
    /// tree.set("metadata", serde_json::json!({ "plugin": "example" }));
    /// let rewritten = RecordedRequestTemplate::from_json::<TemplateError>(
    ///     route,
    ///     template.stream(),
    ///     template.generation,
    ///     &tree,
    ///     // A plugin pins each attachment node it writes; this body has none.
    ///     &mut |_, _| Err(TemplateError::InvalidJson),
    /// )
    /// .unwrap();
    /// assert_eq!(rewritten.redacted(), r#"{"messages":[],"metadata":{"plugin":"example"}}"#);
    /// ```
    ///
    /// # Errors
    ///
    /// [`TemplateError::InvalidJson`] never for a valid template; the
    /// error is the reader's.
    pub fn to_json(&self) -> Result<TemplateJson, TemplateError> {
        // Each slot stands as a marker no literal can spell: it names a
        // digest of the body's own literals.
        let mut digest = blake3::Hasher::new_derive_key("lash request body slot marker v1");
        for part in self.parts() {
            if let BodyPart::Literal(text) = part {
                digest.update(text.as_bytes());
            }
        }
        let marker = format!("$lash_slot:{}", digest.finalize().to_hex());
        let mut text = String::with_capacity(self.literal_len);
        let mut slots = Vec::new();
        for part in self.parts() {
            match part {
                BodyPart::Literal(literal) => text.push_str(literal),
                BodyPart::Separator => text.push(','),
                BodyPart::Slot(slot) => {
                    text.push_str(&serde_json::json!({ &marker: slots.len() }).to_string());
                    slots.push(slot);
                }
            }
        }
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| TemplateError::InvalidJson)?;
        Ok(place_slots(value, &marker, &slots))
    }

    /// The body's parts in wire order: each segment's literals and slots,
    /// and the separator between two adjacent items.
    pub fn parts(&self) -> impl Iterator<Item = BodyPart<'_>> {
        let mut previous_item = false;
        self.segments.iter().flat_map(move |segment| {
            let separator = previous_item && segment.is_item();
            previous_item = segment.is_item();
            separator
                .then_some(BodyPart::Separator)
                .into_iter()
                .chain(segment.parts().iter().map(|part| match part {
                    SegmentPart::Literal(text) => BodyPart::Literal(text),
                    SegmentPart::Slot(slot) => BodyPart::Slot(slot),
                }))
        })
    }

    /// The body's length without slot values.
    pub fn literal_len(&self) -> usize {
        self.literal_len
    }

    pub fn slots(&self) -> impl Iterator<Item = &AttachmentSlot> {
        self.segments.iter().flat_map(RequestSegment::slots)
    }

    pub fn redacted(&self) -> String {
        let mut text = String::with_capacity(self.literal_len);
        for part in self.parts() {
            match part {
                BodyPart::Literal(span) => text.push_str(span),
                BodyPart::Separator => text.push(','),
                BodyPart::Slot(slot) => {
                    text.push_str(&redacted_slot(&slot.reference, slot.position).to_string());
                }
            }
        }
        text
    }
}

/// `value` with each slot marker object replaced by its slot's node; a
/// subtree with no slot stays plain JSON.
fn place_slots(value: serde_json::Value, marker: &str, slots: &[&AttachmentSlot]) -> TemplateJson {
    let plain = |node: &TemplateJson| matches!(node, TemplateJson::Json(_));
    let into_json = |node: TemplateJson| match node {
        TemplateJson::Json(value) => value,
        _ => serde_json::Value::Null,
    };
    match value {
        serde_json::Value::Object(fields) => {
            if fields.len() == 1
                && let Some(index) = fields.get(marker).and_then(serde_json::Value::as_u64)
                && let Some(slot) = usize::try_from(index)
                    .ok()
                    .and_then(|index| slots.get(index))
            {
                return TemplateJson::attachment(&slot.reference, slot.position);
            }
            let nodes: Vec<(String, TemplateJson)> = fields
                .into_iter()
                .map(|(key, value)| (key, place_slots(value, marker, slots)))
                .collect();
            if nodes.iter().all(|(_, node)| plain(node)) {
                TemplateJson::Json(serde_json::Value::Object(
                    nodes
                        .into_iter()
                        .map(|(key, node)| (key, into_json(node)))
                        .collect(),
                ))
            } else {
                TemplateJson::Object(nodes.into_iter().collect())
            }
        }
        serde_json::Value::Array(values) => {
            let nodes: Vec<TemplateJson> = values
                .into_iter()
                .map(|value| place_slots(value, marker, slots))
                .collect();
            if nodes.iter().all(plain) {
                TemplateJson::Json(serde_json::Value::Array(
                    nodes.into_iter().map(into_json).collect(),
                ))
            } else {
                TemplateJson::Array(nodes)
            }
        }
        other => TemplateJson::Json(other),
    }
}

/// Check `segments` are a template (see [`RecordedRequestTemplate`]),
/// reading every text segment and no item: each run of items stands in the
/// skeleton as one value.
fn validate(segments: &[RequestSegment]) -> Result<Option<bool>, TemplateError> {
    let mut skeleton = String::new();
    let mut lexer = Lexer::default();
    let mut run = 0usize;
    for segment in segments {
        match segment.kind() {
            SegmentKind::Item => {
                if run == 0 {
                    if lexer.in_string || lexer.open.is_empty() {
                        return Err(TemplateError::InvalidJson);
                    }
                    skeleton.push_str(" 0 ");
                }
                run += 1;
                if run == 2 && lexer.open.last() != Some(&b'[') {
                    return Err(TemplateError::InvalidJson);
                }
            }
            SegmentKind::Text => {
                run = 0;
                for part in segment.parts() {
                    match part {
                        SegmentPart::Literal(text) => {
                            lexer.scan(text.as_bytes());
                            skeleton.push_str(text);
                        }
                        SegmentPart::Slot(slot) => {
                            if lexer.in_string {
                                return Err(TemplateError::InvalidJson);
                            }
                            if slot.accepts.is_empty() {
                                return Err(TemplateError::EmptyAcceptance);
                            }
                            skeleton.push_str("null");
                        }
                    }
                }
            }
        }
    }
    if skeleton.trim_start().starts_with('{') {
        serde_json::from_str::<BodyResponseMode>(&skeleton)
            .map(|body| body.stream)
            .map_err(|_| TemplateError::InvalidJson)
    } else {
        serde_json::from_str::<serde::de::IgnoredAny>(&skeleton)
            .map(|_| None)
            .map_err(|_| TemplateError::InvalidJson)
    }
}

// Read only the top-level mode; serde skips other fields without allocating
// a JSON tree and refuses duplicate `stream` fields. Slots and items cannot
// supply mode.
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

/// Builds a [`RecordedRequestTemplate`] in wire order.
pub struct RequestTemplateBuilder {
    route: ProviderRouteIdentity,
    stream: bool,
    generation: Option<GenerationReceipt>,
    writer: Writer,
}

impl RequestTemplateBuilder {
    /// Append JSON text to the current text segment.
    pub fn literal(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.writer.literal(text.as_ref());
        self
    }

    pub fn attachment(&mut self, slot: AttachmentSlot) -> &mut Self {
        self.writer.slot(slot);
        self
    }

    /// Append `segment` as it is, shared.
    pub fn segment(&mut self, segment: &RequestSegment) -> &mut Self {
        self.writer.segment(segment);
        self
    }

    /// Write `tree` as `serde_json` serializes it, each attachment node as
    /// the slot `slot` pins for it and each lowered item as its segment.
    ///
    /// # Errors
    ///
    /// What `slot` refuses a node with.
    pub fn json<E>(
        &mut self,
        tree: &TemplateJson,
        slot: &mut impl FnMut(&AttachmentRef, AttachmentPosition) -> Result<AttachmentSlot, E>,
    ) -> Result<&mut Self, E> {
        self.writer.json(tree, slot)?;
        Ok(self)
    }

    /// # Errors
    ///
    /// [`TemplateError`] naming the rule the written body breaks.
    pub fn finish(self) -> Result<RecordedRequestTemplate, TemplateError> {
        RecordedRequestTemplate::from_segments(
            self.route,
            self.stream,
            self.generation,
            self.writer.into_segments(),
        )
    }
}

/// One encoded JSON value for the live wire, with no serialized form.
pub struct TransientJson {
    value: Arc<str>,
    forms: DeliveryForms,
    valid_until_ms: Option<u64>,
    provider_file_scope: Option<ProviderFileScope>,
}
impl TransientJson {
    pub fn new(value: &serde_json::Value, delivery: &Delivery) -> Self {
        Self {
            value: Arc::from(value.to_string()),
            forms: delivery.forms(),
            valid_until_ms: match delivery {
                Delivery::Bytes(_) => None,
                Delivery::Url { valid_until_ms, .. }
                | Delivery::ProviderFile { valid_until_ms, .. } => *valid_until_ms,
            },
            provider_file_scope: match delivery {
                Delivery::ProviderFile { scope, .. } => Some(scope.clone()),
                _ => None,
            },
        }
    }
    /// The live encoded value, for the owning codec. May contain secrets;
    /// never record or trace it. Debug remains redacted.
    pub fn wire_value(&self) -> &str {
        &self.value
    }
    pub fn forms(&self) -> DeliveryForms {
        self.forms
    }
    /// The expiry the delivered value actually declared, retained only live.
    pub fn valid_until_ms(&self) -> Option<u64> {
        self.valid_until_ms
    }
    /// The actual file namespace, rather than an inferred endpoint label.
    pub fn provider_file_scope(&self) -> Option<&ProviderFileScope> {
        self.provider_file_scope.as_ref()
    }
}
impl std::fmt::Debug for TransientJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TransientJson(<redacted>)")
    }
}

/// One chunk of a filled body, in wire order, as
/// [`LiveRequestBody::wire_chunks`] yields it. Literal text and slot values
/// are shared, so a transport can send them without copying.
#[derive(Clone, Copy, Debug)]
pub enum WireChunk<'a> {
    /// Recorded literal text.
    Literal(&'a Arc<str>),
    /// The `,` between two adjacent items.
    Separator,
    /// A slot's live encoded value. May contain secrets: never record or
    /// trace it.
    Value(&'a Arc<str>),
}

impl<'a> WireChunk<'a> {
    pub fn as_str(&self) -> &'a str {
        match self {
            Self::Literal(text) | Self::Value(text) => text,
            Self::Separator => ",",
        }
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
    /// The admitted template, shared.
    pub fn shared_template(&self) -> &Arc<RecordedRequestTemplate> {
        &self.template
    }
    /// Each admitted slot with its live encoded value and delivery metadata.
    /// Values may contain secrets; they have no serialized representation.
    pub fn encoded_slots(&self) -> impl Iterator<Item = (&AttachmentSlot, &TransientJson)> {
        self.template.slots().zip(&self.values)
    }

    /// See [`RecordedRequestTemplate::canonical_request`].
    ///
    /// # Errors
    ///
    /// [`TemplateError::NotCanonical`] when the body is not canonical.
    pub fn canonical_request(&self) -> Result<super::types::LlmRequest, TemplateError> {
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
    /// The bytes this body sends, in order, without copying any of them:
    /// write or stream them. The values may contain secrets.
    pub fn wire_chunks(&self) -> impl Iterator<Item = WireChunk<'_>> {
        let mut values = self.values.iter();
        self.template.parts().filter_map(move |part| match part {
            BodyPart::Literal(text) => Some(WireChunk::Literal(text)),
            BodyPart::Separator => Some(WireChunk::Separator),
            BodyPart::Slot(_) => values.next().map(|value| WireChunk::Value(&value.value)),
        })
    }
    /// The length of the bytes this body sends.
    pub fn wire_len(&self) -> usize {
        self.template.literal_len
            + self
                .values
                .iter()
                .map(|value| value.value.len())
                .sum::<usize>()
    }
    /// Write the bytes this body sends to `out`.
    ///
    /// # Errors
    ///
    /// What `out` refuses a write with.
    pub fn write_wire(&self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        for chunk in self.wire_chunks() {
            out.write_all(chunk.as_str().as_bytes())?;
        }
        Ok(())
    }
    /// The bytes this body sends as one string, for a consumer that needs
    /// the whole body as one value (a WebSocket frame, a test). It copies
    /// every byte; a transport that can stream reads
    /// [`Self::wire_chunks`] instead.
    pub fn to_wire_string(&self) -> String {
        let mut text = String::with_capacity(self.wire_len());
        for chunk in self.wire_chunks() {
            text.push_str(chunk.as_str());
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

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("a request template must name one response mode, agreeing with its body")]
    ResponseMode,
    #[error("a request template contains an empty literal")]
    EmptyLiteral,
    #[error("a request template contains adjacent literals")]
    AdjacentLiterals,
    #[error("a request segment has no parts")]
    EmptySegment,
    #[error("a lowered tree node is not an item segment")]
    NotAnItem,
    #[error("a request template's literals and slots are not one JSON value")]
    InvalidJson,
    #[error("a request template expects {expected} slots, received {actual}")]
    SlotCount { expected: usize, actual: usize },
    #[error("an attachment slot has empty acceptance")]
    EmptyAcceptance,
    #[error("the body is not a canonical request: {reason}")]
    NotCanonical { reason: String },
}
