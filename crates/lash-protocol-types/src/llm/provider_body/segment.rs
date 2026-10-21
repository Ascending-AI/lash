//! Immutable, shared pieces of a request body and the writer that builds
//! them from a [`TemplateJson`] tree.

use super::{AttachmentSlot, TemplateError, TemplateJson};
use crate::AttachmentRef;
use crate::derived::DerivedMemo;
use crate::llm::attachment_delivery::AttachmentPosition;
use std::any::Any;
use std::sync::Arc;

/// One part of a [`RequestSegment`], in wire order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentPart {
    /// JSON text, sent byte for byte. Never empty.
    Literal(Arc<str>),
    /// Where one attachment's delivered value goes: one JSON value per
    /// attempt, never inside a JSON string.
    Slot(Box<AttachmentSlot>),
}

/// How a segment stands in its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegmentKind {
    /// Any run of JSON text, read in context with its neighbours: the
    /// envelope of a request.
    Text,
    /// Exactly one complete JSON value standing as an array element (or as
    /// one value). The body writes `,` between two adjacent items, so items
    /// are inserted, dropped and reordered without touching separators.
    Item,
}

/// An immutable piece of a request body: JSON text and attachment slots,
/// shared by every template and call that holds it.
///
/// A segment is its own identity: a provider that lowers an unchanged
/// message reuses the segment it built before (through a
/// [`SegmentCache`](super::SegmentCache)), and what was derived from that
/// segment ([`Self::derived`]) is reused with it. A segment a plugin
/// rewrites is a new segment with nothing derived.
///
/// ```
/// use lash_protocol_types::llm::types::{RequestSegment, SegmentKind, TemplateJson};
///
/// let item = RequestSegment::item_json(&serde_json::json!({"role": "user"})).unwrap();
/// assert_eq!(item.kind(), SegmentKind::Item);
/// assert_eq!(item.text_lossy(), r#"{"role":"user"}"#);
/// // The same segment, not a copy: derived facts are shared with it.
/// let shared = item.clone();
/// assert!(RequestSegment::same(&item, &shared));
/// ```
#[derive(Clone)]
pub struct RequestSegment(Arc<SegmentInner>, Option<std::ops::Range<usize>>);

struct SegmentInner {
    kind: SegmentKind,
    parts: Box<[SegmentPart]>,
    literal_bytes: usize,
    derived: DerivedMemo,
    _hold: crate::allocation::AllocationHold,
}

impl RequestSegment {
    /// A segment of `parts` known to be valid for `kind`.
    pub(super) fn trusted(kind: SegmentKind, parts: Vec<SegmentPart>) -> Self {
        let literal_bytes = parts
            .iter()
            .map(|part| match part {
                SegmentPart::Literal(text) => text.len(),
                SegmentPart::Slot(_) => 0,
            })
            .sum();
        Self(
            Arc::new(SegmentInner {
                kind,
                parts: parts.into_boxed_slice(),
                literal_bytes,
                derived: DerivedMemo::default(),
                _hold: Default::default(),
            }),
            None,
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "the lowerer transfers a token before sharing the newly built segment"
    )]
    pub fn with_allocation(mut self, hold: crate::allocation::AllocationHold) -> Self {
        // New lowerings are still private. The token moves into the same
        // allocation that every template/cache reader subsequently shares.
        Arc::get_mut(&mut self.0)
            .expect("a newly lowered segment is private")
            ._hold = hold;
        self
    }

    /// Original source messages for this lowered item; never serialized.
    pub fn with_source_range(mut self, range: std::ops::Range<usize>) -> Self {
        self.1 = Some(range);
        self
    }
    pub fn source_range(&self) -> Option<std::ops::Range<usize>> {
        self.1.clone()
    }

    /// A text segment of `text`.
    ///
    /// # Errors
    ///
    /// [`TemplateError::EmptyLiteral`] when `text` is empty.
    pub fn text(text: impl Into<Arc<str>>) -> Result<Self, TemplateError> {
        Self::from_parts(SegmentKind::Text, vec![SegmentPart::Literal(text.into())])
    }

    /// A segment of `parts`. A text segment needs only non-empty literals:
    /// its JSON is checked in its template. An item must be one JSON value
    /// with no slot inside a string, checked here once.
    ///
    /// # Errors
    ///
    /// [`TemplateError`] naming the rule `parts` break.
    pub fn from_parts(kind: SegmentKind, parts: Vec<SegmentPart>) -> Result<Self, TemplateError> {
        if parts.is_empty() {
            return Err(TemplateError::EmptySegment);
        }
        for part in &parts {
            match part {
                SegmentPart::Literal(text) if text.is_empty() => {
                    return Err(TemplateError::EmptyLiteral);
                }
                SegmentPart::Slot(slot) if slot.accepts.is_empty() => {
                    return Err(TemplateError::EmptyAcceptance);
                }
                _ => {}
            }
        }
        if kind == SegmentKind::Item {
            validate_item(&parts)?;
        }
        Ok(Self::trusted(kind, parts))
    }

    /// The item `json` writes, each attachment node a slot `slot` pins.
    ///
    /// # Errors
    ///
    /// What `slot` refuses a node with.
    pub fn item<E>(
        json: &TemplateJson,
        slot: &mut impl FnMut(&AttachmentRef, AttachmentPosition) -> Result<AttachmentSlot, E>,
    ) -> Result<Self, E> {
        let mut writer = Writer::new(true);
        writer.json(json, slot)?;
        Ok(writer.into_item())
    }

    /// The item of plain JSON `value`, which holds no slot.
    ///
    /// # Errors
    ///
    /// Never: a value with no attachment node has no slot to refuse. The
    /// result keeps [`Self::item`]'s shape.
    pub fn item_json(value: &serde_json::Value) -> Result<Self, TemplateError> {
        Self::item(&TemplateJson::Json(value.clone()), &mut |_, _| {
            Err(TemplateError::InvalidJson)
        })
    }

    pub fn kind(&self) -> SegmentKind {
        self.0.kind
    }

    pub fn is_item(&self) -> bool {
        self.0.kind == SegmentKind::Item
    }

    pub fn parts(&self) -> &[SegmentPart] {
        &self.0.parts
    }

    pub fn slots(&self) -> impl Iterator<Item = &AttachmentSlot> {
        self.0.parts.iter().filter_map(|part| match part {
            SegmentPart::Slot(slot) => Some(slot.as_ref()),
            SegmentPart::Literal(_) => None,
        })
    }

    /// The bytes of this segment's literals.
    pub fn literal_len(&self) -> usize {
        self.0.literal_bytes
    }

    /// Whether `left` and `right` are the same segment, not merely equal
    /// ones: what reuse is keyed by.
    pub fn same(left: &Self, right: &Self) -> bool {
        Arc::ptr_eq(&left.0, &right.0)
    }

    /// This segment's text with each slot as the redacted marker
    /// [`super::RecordedRequestTemplate::redacted`] writes.
    pub fn text_lossy(&self) -> String {
        let mut text = String::with_capacity(self.literal_len());
        for part in self.parts() {
            match part {
                SegmentPart::Literal(literal) => text.push_str(literal),
                SegmentPart::Slot(slot) => {
                    text.push_str(
                        &super::redacted_slot(&slot.reference, slot.position).to_string(),
                    );
                }
            }
        }
        text
    }

    /// This segment with `map` applied to each of its slots: a new segment,
    /// with nothing derived, when a slot changed; this one otherwise.
    ///
    /// # Errors
    ///
    /// [`TemplateError::EmptyAcceptance`] when a mapped slot accepts nothing.
    pub fn map_slots(
        &self,
        mut map: impl FnMut(&mut AttachmentSlot),
    ) -> Result<Self, TemplateError> {
        let mut changed = false;
        let parts = self
            .parts()
            .iter()
            .map(|part| match part {
                SegmentPart::Slot(slot) => {
                    let mut mapped = slot.clone();
                    map(&mut mapped);
                    changed |= mapped != *slot;
                    SegmentPart::Slot(mapped)
                }
                literal => literal.clone(),
            })
            .collect();
        if changed {
            Self::from_parts(self.kind(), parts)
        } else {
            Ok(self.clone())
        }
    }

    /// The `T` derived from this segment: computed by `compute` the first
    /// time it is asked for and kept as long as the segment lives. The
    /// segment is immutable, so a derived value never goes stale; a
    /// rewritten segment is a new segment and derives afresh. Two callers
    /// that race may both compute; the first stored value wins.
    pub fn derived<T: Any + Send + Sync>(&self, compute: impl FnOnce(&Self) -> T) -> Arc<T> {
        self.0.derived.get_or_compute(|| compute(self))
    }
}

impl PartialEq for RequestSegment {
    fn eq(&self, other: &Self) -> bool {
        Self::same(self, other) || (self.0.kind == other.0.kind && self.0.parts == other.0.parts)
    }
}

impl Eq for RequestSegment {}

impl std::fmt::Debug for RequestSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestSegment")
            .field("kind", &self.0.kind)
            .field("parts", &self.0.parts)
            .finish()
    }
}

/// An item standing in a [`TemplateJson`] tree as the JSON value it
/// already is: a provider's reused lowering of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoweredItem(RequestSegment);

impl LoweredItem {
    /// `segment` as a tree node.
    ///
    /// # Errors
    ///
    /// [`TemplateError::NotAnItem`] when `segment` is text.
    pub fn new(segment: RequestSegment) -> Result<Self, TemplateError> {
        if segment.is_item() {
            Ok(Self(segment))
        } else {
            Err(TemplateError::NotAnItem)
        }
    }

    pub fn segment(&self) -> &RequestSegment {
        &self.0
    }
}

/// Check that `parts`, slots standing for one value each, are exactly one
/// JSON value with no slot inside a string.
fn validate_item(parts: &[SegmentPart]) -> Result<(), TemplateError> {
    let mut text = String::new();
    let mut lexer = Lexer::default();
    for part in parts {
        match part {
            SegmentPart::Literal(literal) => {
                lexer.scan(literal.as_bytes());
                text.push_str(literal);
            }
            SegmentPart::Slot(_) => {
                if lexer.in_string {
                    return Err(TemplateError::InvalidJson);
                }
                text.push_str("null");
            }
        }
    }
    serde_json::from_str::<serde::de::IgnoredAny>(&text)
        .map(|_| ())
        .map_err(|_| TemplateError::InvalidJson)
}

/// The lexical state of JSON text read so far: whether it stands in a
/// string, and the containers open outside strings.
#[derive(Default)]
pub(super) struct Lexer {
    pub(super) in_string: bool,
    escape: bool,
    pub(super) open: Vec<u8>,
}

impl Lexer {
    pub(super) fn scan(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.in_string {
                if self.escape {
                    self.escape = false;
                } else if byte == b'\\' {
                    self.escape = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
            } else {
                match byte {
                    b'"' => self.in_string = true,
                    b'{' | b'[' => self.open.push(byte),
                    b'}' | b']' => {
                        self.open.pop();
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Writes a [`TemplateJson`] tree as `serde_json` serializes it: into one
/// item's parts, or into a template's segments, where each lowered item
/// node stays the segment it is.
pub(super) struct Writer {
    item: bool,
    segments: Vec<RequestSegment>,
    parts: Vec<SegmentPart>,
    literal: Vec<u8>,
}

impl Writer {
    pub(super) fn new(item: bool) -> Self {
        Self {
            item,
            segments: Vec::new(),
            parts: Vec::new(),
            literal: Vec::new(),
        }
    }

    pub(super) fn literal(&mut self, text: &str) {
        self.literal.extend_from_slice(text.as_bytes());
    }

    #[expect(
        clippy::expect_used,
        reason = "the buffer holds only whole UTF-8 strings and serde_json's UTF-8 output"
    )]
    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            let text = String::from_utf8(std::mem::take(&mut self.literal))
                .expect("literal text is UTF-8");
            self.parts.push(SegmentPart::Literal(Arc::from(text)));
        }
    }

    fn flush_text(&mut self) {
        self.flush_literal();
        if !self.parts.is_empty() {
            let parts = std::mem::take(&mut self.parts);
            self.segments
                .push(RequestSegment::trusted(SegmentKind::Text, parts));
        }
    }

    pub(super) fn slot(&mut self, slot: AttachmentSlot) {
        self.flush_literal();
        self.parts.push(SegmentPart::Slot(Box::new(slot)));
    }

    /// Append `segment` whole: in an item, its parts; in a template, the
    /// segment itself, shared.
    pub(super) fn segment(&mut self, segment: &RequestSegment) {
        if self.item {
            self.flush_literal();
            self.parts.extend(segment.parts().iter().cloned());
        } else {
            self.flush_text();
            self.segments.push(segment.clone());
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "serde_json writes any Value and any string into a Vec"
    )]
    fn value(&mut self, value: &impl serde::Serialize) {
        serde_json::to_writer(&mut self.literal, value).expect("JSON writes into a Vec");
    }

    /// Write `tree`, each attachment node as the slot `slot` pins for it.
    pub(super) fn json<E>(
        &mut self,
        tree: &TemplateJson,
        slot: &mut impl FnMut(&AttachmentRef, AttachmentPosition) -> Result<AttachmentSlot, E>,
    ) -> Result<(), E> {
        match tree {
            TemplateJson::Json(value) => self.value(value),
            TemplateJson::Array(values) => {
                self.literal("[");
                let mut previous_item = false;
                for (index, value) in values.iter().enumerate() {
                    let item = matches!(value, TemplateJson::Lowered(_));
                    // Two adjacent items in a template are joined by the
                    // body itself.
                    if index != 0 && !(previous_item && item && !self.item) {
                        self.literal(",");
                    }
                    self.json(value, slot)?;
                    previous_item = item;
                }
                self.literal("]");
            }
            TemplateJson::Object(fields) => {
                self.literal("{");
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        self.literal(",");
                    }
                    self.value(key);
                    self.literal(":");
                    self.json(value, slot)?;
                }
                self.literal("}");
            }
            TemplateJson::Attachment {
                reference,
                position,
            } => {
                let pinned = slot(reference, *position)?;
                self.slot(pinned);
            }
            TemplateJson::Lowered(item) => self.segment(item.segment()),
        }
        Ok(())
    }

    fn into_item(mut self) -> RequestSegment {
        self.flush_literal();
        RequestSegment::trusted(SegmentKind::Item, self.parts)
    }

    pub(super) fn into_segments(mut self) -> Vec<RequestSegment> {
        self.flush_text();
        self.segments
    }
}
