//! The admission and buffering bounds of the tracing seam.

use serde::Serialize;

use crate::TraceSpanKind;
use crate::telemetry::metrics::registry::attribute_reserve;

use super::{PolicyOperand, PolicyRelation, PolicyValue, TracePolicyError, TracePolicyField};

/// The bound on one execution's hold account: every hold of one body
/// execution, or one unanchored cell's, overhead included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PublicationHold {
    records: u32,
    bytes: u64,
}

impl PublicationHold {
    /// Held records of one account.
    pub const fn records(self) -> u32 {
        self.records
    }

    /// Held bytes of one account.
    pub const fn bytes(self) -> u64 {
        self.bytes
    }
}

macro_rules! trace_limits {
    ($($field:ident: $ty:ty => $id:ident, $doc:literal;)+) => {
        /// The admission and buffering bounds of the tracing seam: how much a
        /// shift holds, how many links a span keeps, and what every host or
        /// plugin contribution to a span may take.
        ///
        /// Sealed: built only through [`TraceLimits::builder`], which checks
        /// every row against its documented range and then the relations
        /// between rows. Each accessor documents its default and range.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        pub struct TraceLimits {
            $($field: $ty,)+
            publication_hold: PublicationHold,
        }

        impl TraceLimits {
            /// Every row at its documented default.
            pub const fn standard() -> Self {
                Self {
                    $($field: TracePolicyField::$id.default_value() as $ty,)+
                    publication_hold: PublicationHold {
                        records: TracePolicyField::PublicationHoldRecords.default_value() as u32,
                        bytes: TracePolicyField::PublicationHoldBytes.default_value(),
                    },
                }
            }

            $(
                #[doc = $doc]
                pub const fn $field(&self) -> $ty {
                    self.$field
                }
            )+

            /// The bound on one execution's hold account
            /// (`trace_limits.publication_hold.records`: default 256,
            /// 1..=4096; `.bytes`: default 8 MiB, 128 KiB..=64 MiB).
            pub const fn publication_hold(&self) -> PublicationHold {
                self.publication_hold
            }

            fn rows(&self) -> [(TracePolicyField, u64); trace_limits!(@count $($field)+) + 2] {
                [
                    $((TracePolicyField::$id, self.$field as u64),)+
                    (TracePolicyField::PublicationHoldRecords, u64::from(self.publication_hold.records)),
                    (TracePolicyField::PublicationHoldBytes, self.publication_hold.bytes),
                ]
            }
        }

        /// Builds a [`TraceLimits`] from [`TraceLimits::standard`].
        pub struct TraceLimitsBuilder {
            $($field: u64,)+
            publication_hold_records: u64,
            publication_hold_bytes: u64,
        }

        impl TraceLimitsBuilder {
            $(
                #[doc = $doc]
                #[must_use]
                pub fn $field(mut self, value: u64) -> Self {
                    self.$field = value;
                    self
                }
            )+

            /// The bound on one execution's hold account.
            #[must_use]
            pub fn publication_hold(mut self, records: u64, bytes: u64) -> Self {
                self.publication_hold_records = records;
                self.publication_hold_bytes = bytes;
                self
            }

            /// Checks each row against its range in table order, then the
            /// relations between rows and against the registry's reserve, and
            /// returns the first refusal.
            #[allow(clippy::cast_possible_truncation, reason = "every row was checked against a range that fits its type")]
            pub fn build(self) -> Result<TraceLimits, TracePolicyError> {
                $(TracePolicyField::$id.check(self.$field)?;)+
                TracePolicyField::PublicationHoldRecords.check(self.publication_hold_records)?;
                TracePolicyField::PublicationHoldBytes.check(self.publication_hold_bytes)?;
                let limits = TraceLimits {
                    $($field: self.$field as $ty,)+
                    publication_hold: PublicationHold {
                        records: self.publication_hold_records as u32,
                        bytes: self.publication_hold_bytes,
                    },
                };
                limits.check_relations()?;
                Ok(limits)
            }
        }

        impl TraceLimits {
            pub fn builder() -> TraceLimitsBuilder {
                let standard = Self::standard();
                TraceLimitsBuilder {
                    $($field: standard.$field as u64,)+
                    publication_hold_records: u64::from(standard.publication_hold.records),
                    publication_hold_bytes: standard.publication_hold.bytes,
                }
            }
        }
    };
    (@count) => { 0 };
    (@count $head:ident $($tail:ident)*) => { 1 + trace_limits!(@count $($tail)*) };
}

trace_limits! {
    held_observations: u32 => HeldObservations,
        "Records a shift holds behind its journal frontier, lash's and extensions' alike; the overflow is dropped and counted (`trace_limits.held_observations`: default 256, 0..=4096).";
    links: u32 => Links,
        "Links one span keeps, at its admission and at its close; the overflow is counted in `lash.links.omitted` (`trace_limits.links`: default 64, 0..=128).";
    extension_spans: u32 => ExtensionSpans,
        "Extension and point spans one body execution opens; 0 turns both off, which requires model calls and their attempts off too (`trace_limits.extension_spans`: default 256, 0..=4096).";
    attributes_per_span: u32 => AttributesPerSpan,
        "Attributes of one assembled span, lash's included; contributions share what the registry's reserve for the kind leaves. Also the attributes of one event (`trace_limits.attributes_per_span`: default 128, 1..=1024, at least every kind's reserve).";
    events_per_span: u32 => EventsPerSpan,
        "Events per extension span (`trace_limits.events_per_span`: default 64, 0..=256).";
    attribute_value_bytes: u64 => AttributeValueBytes,
        "Bytes of one contributed value: a longer string is cut, a longer JSON value refused (`trace_limits.attribute_value_bytes`: default 4 KiB, 64 B..=64 KiB, at most `span_contribution_bytes`).";
    extension_bytes: u64 => ExtensionBytes,
        "Every byte one body's contributions charge: names, keys, values and events (`trace_limits.extension_bytes`: default 1 MiB, 4 KiB..=16 MiB, at least `attribute_value_bytes`).";
    tool_span_attributes: u32 => ToolSpanAttributes,
        "Annotations a tool body sets on its tool span (`trace_limits.tool_span_attributes`: default 16, 0..=64, at most `attributes_per_span` less the tool kind's reserve).";
    name_bytes: u64 => NameBytes,
        "Bytes of one span, event or error-type name a plugin or the enricher supplies (`trace_limits.name_bytes`: default 1 KiB, 16 B..=16 KiB).";
    attribute_key_bytes: u64 => AttributeKeyBytes,
        "Bytes of one contributed key (`trace_limits.attribute_key_bytes`: default 128 B, 16 B..=1 KiB).";
    span_contribution_bytes: u64 => SpanContributionBytes,
        "Every byte the optional sources contribute to one span: body annotations, extension attributes and events, hook references, the sampler's attributes and enrichment (`trace_limits.span_contribution_bytes`: default 256 KiB, 4 KiB..=8 MiB).";
    references: u32 => References,
        "Content-hook references per record (`trace_limits.references`: default 16, 0..=64, at most `attributes_per_span` less the largest content-bearing kind's reserve).";
    reference_bytes: u64 => ReferenceBytes,
        "Content-hook reference keys and values per record (`trace_limits.reference_bytes`: default 64 KiB, 0..=1 MiB, at most `span_contribution_bytes`).";
}

/// The kinds whose spans carry content and so take content-hook references.
const CONTENT_KINDS: [TraceSpanKind; 4] = [
    TraceSpanKind::Model,
    TraceSpanKind::Tool,
    TraceSpanKind::Cell,
    TraceSpanKind::Extension,
];

/// The kind among `kinds` with the largest reserve, the first on a tie.
fn largest_reserve(kinds: impl IntoIterator<Item = TraceSpanKind>) -> (TraceSpanKind, u32) {
    kinds
        .into_iter()
        .map(|kind| (kind, attribute_reserve(kind)))
        .fold(None::<(TraceSpanKind, u32)>, |best, candidate| match best {
            Some(best) if best.1 >= candidate.1 => Some(best),
            _ => Some(candidate),
        })
        .unwrap_or((TraceSpanKind::Producer, 0))
}

fn relation(
    field: TracePolicyField,
    value: u64,
    relation: PolicyRelation,
    other: PolicyOperand,
    other_value: u64,
) -> TracePolicyError {
    TracePolicyError::Relation {
        field,
        value: PolicyValue::Count(value),
        relation,
        other,
        other_value: PolicyValue::Count(other_value),
    }
}

impl TraceLimits {
    /// The contribution count a span of `kind` has after the registry's
    /// reserve for lash's own attributes.
    pub fn contribution_attributes(&self, kind: TraceSpanKind) -> u32 {
        self.attributes_per_span
            .saturating_sub(attribute_reserve(kind))
    }

    fn check_relations(&self) -> Result<(), TracePolicyError> {
        use PolicyRelation::{AtLeast, AtMost};
        use TracePolicyField as F;
        let per_span = u64::from(self.attributes_per_span);
        let (widest, reserve) = largest_reserve(TraceSpanKind::ALL);
        if per_span < u64::from(reserve) {
            return Err(relation(
                F::AttributesPerSpan,
                per_span,
                AtLeast,
                PolicyOperand::Reserve(widest),
                u64::from(reserve),
            ));
        }
        if self.extension_bytes < self.attribute_value_bytes {
            return Err(relation(
                F::ExtensionBytes,
                self.extension_bytes,
                AtLeast,
                PolicyOperand::Field(F::AttributeValueBytes),
                self.attribute_value_bytes,
            ));
        }
        let tool = u64::from(self.contribution_attributes(TraceSpanKind::Tool));
        if u64::from(self.tool_span_attributes) > tool {
            return Err(relation(
                F::ToolSpanAttributes,
                u64::from(self.tool_span_attributes),
                AtMost,
                PolicyOperand::Remaining(TraceSpanKind::Tool),
                tool,
            ));
        }
        let (content, _) = largest_reserve(CONTENT_KINDS);
        let room = u64::from(self.contribution_attributes(content));
        if u64::from(self.references) > room {
            return Err(relation(
                F::References,
                u64::from(self.references),
                AtMost,
                PolicyOperand::Remaining(content),
                room,
            ));
        }
        for (field, value) in [
            (F::AttributeValueBytes, self.attribute_value_bytes),
            (F::ReferenceBytes, self.reference_bytes),
        ] {
            if value > self.span_contribution_bytes {
                return Err(relation(
                    field,
                    value,
                    AtMost,
                    PolicyOperand::Field(F::SpanContributionBytes),
                    self.span_contribution_bytes,
                ));
            }
        }
        Ok(())
    }

    /// Refuses limits that turn extension and point spans off
    /// (`extension_spans = 0`) while `detail` still builds a kind an
    /// extension span can parent: model calls and their attempts.
    pub fn check_against(&self, detail: &super::TraceDetailPolicy) -> Result<(), TracePolicyError> {
        if self.extension_spans > 0 {
            return Ok(());
        }
        for child in TraceSpanKind::Extension.children().iter() {
            if matches!(child, TraceSpanKind::Extension | TraceSpanKind::Point) {
                continue;
            }
            for kind in std::iter::once(child).chain(child.children().iter()) {
                if detail.builds(kind) {
                    return Err(TracePolicyError::Relation {
                        field: TracePolicyField::ExtensionSpans,
                        value: PolicyValue::Count(0),
                        relation: PolicyRelation::ParentOffChildOn,
                        other: PolicyOperand::Field(TracePolicyField::Detail(kind)),
                        other_value: PolicyValue::Detail(detail.get(kind)),
                    });
                }
            }
        }
        Ok(())
    }

    /// Every numeric row with its value, in table order.
    pub fn values(&self) -> impl Iterator<Item = (TracePolicyField, u64)> {
        self.rows().into_iter()
    }
}

impl Default for TraceLimits {
    fn default() -> Self {
        Self::standard()
    }
}
