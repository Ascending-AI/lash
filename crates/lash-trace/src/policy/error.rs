//! The one error every tracing policy refuses a value with, and the field
//! table it names: each numeric field's documented default and bounds.

use crate::TraceSpanKind;

use super::TraceDetail;

/// One refused tracing configuration value: the field, the value supplied
/// and the bound, relation or selection it breaks.
///
/// `LashCoreBuilder::build` returns it as `EmbedError::TracePolicy`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TracePolicyError {
    /// A value outside its documented range. Durations are in milliseconds.
    #[error("{field} = {value} is outside {min}..={max}")]
    Range {
        field: TracePolicyField,
        value: u64,
        min: u64,
        max: u64,
    },
    /// Two values that break a documented relation: a count over what the
    /// registry's reserve leaves, a byte bound over the budget that contains
    /// it, or a kind off while a kind it can parent is on.
    #[error("{field} = {value} must be {relation} {other} = {other_value}")]
    Relation {
        field: TracePolicyField,
        value: PolicyValue,
        relation: PolicyRelation,
        other: PolicyOperand,
        other_value: PolicyValue,
    },
    /// A selection the span structure forbids, such as turning a container
    /// off.
    #[error("{field} cannot select {selection}")]
    Structural {
        field: TracePolicyField,
        selection: TracePolicySelection,
    },
}

/// The value on either side of a relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyValue {
    Count(u64),
    Detail(TraceDetail),
    /// Whether the exporter exports the kind.
    Exported(bool),
}

impl std::fmt::Display for PolicyValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count(count) => write!(f, "{count}"),
            Self::Detail(detail) => f.write_str(detail.as_str()),
            Self::Exported(true) => f.write_str("exported"),
            Self::Exported(false) => f.write_str("skipped"),
        }
    }
}

/// The other side of a relation: another field, or a value the generated
/// attribute registry derives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyOperand {
    Field(TracePolicyField),
    /// `attribute_reserve(kind)`: the attributes lash may set on a span of
    /// `kind`.
    Reserve(TraceSpanKind),
    /// `attributes_per_span − attribute_reserve(kind)`: what a span's
    /// contributions share.
    Remaining(TraceSpanKind),
}

impl std::fmt::Display for PolicyOperand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Field(field) => write!(f, "{field}"),
            Self::Reserve(kind) => write!(f, "attribute_reserve({kind})"),
            Self::Remaining(kind) => {
                write!(
                    f,
                    "trace_limits.attributes_per_span - attribute_reserve({kind})"
                )
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyRelation {
    AtMost,
    AtLeast,
    /// The field turns a kind off (detail `Off`, skipped from export, or no
    /// extension spans) while the other, a kind in its
    /// [`children`](TraceSpanKind::children), is still on.
    ParentOffChildOn,
}

impl std::fmt::Display for PolicyRelation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AtMost => "at most",
            Self::AtLeast => "at least",
            Self::ParentOffChildOn => "off only with",
        })
    }
}

/// A selection a kind cannot take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TracePolicySelection {
    Off,
    Extended,
}

impl std::fmt::Display for TracePolicySelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Extended => "extended",
        })
    }
}

/// A numeric field's documented default and inclusive range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldBounds {
    pub default: u64,
    pub min: u64,
    pub max: u64,
}

impl FieldBounds {
    /// Refuses `value` for `field` outside the range.
    pub(crate) fn check(self, field: TracePolicyField, value: u64) -> Result<(), TracePolicyError> {
        if (self.min..=self.max).contains(&value) {
            Ok(())
        } else {
            Err(TracePolicyError::Range {
                field,
                value,
                min: self.min,
                max: self.max,
            })
        }
    }
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

macro_rules! numeric_fields {
    ($($id:ident => ($name:literal, $default:expr, $min:expr, $max:expr)),+ $(,)?) => {
        /// A field of a tracing policy: what a [`TracePolicyError`] names.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum TracePolicyField {
            /// The `TraceDetailPolicy` selection of a kind.
            Detail(TraceSpanKind),
            /// The `TraceExportPolicy` selection of a kind.
            Export(TraceSpanKind),
            /// A capture selector skipping an optional leaf.
            Selector(TraceSpanKind),
            $($id),+
        }

        impl TracePolicyField {
            /// The numeric fields, in policy table order.
            pub const NUMERIC: &[Self] = &[$(Self::$id),+];

            /// The documented default and range of a numeric field; `None`
            /// for a per-kind selection.
            pub const fn bounds(self) -> Option<FieldBounds> {
                match self {
                    Self::Detail(_) | Self::Export(_) | Self::Selector(_) => None,
                    $(Self::$id => Some(FieldBounds { default: $default, min: $min, max: $max })),+
                }
            }

            fn numeric_name(self) -> &'static str {
                match self {
                    Self::Detail(_) | Self::Export(_) | Self::Selector(_) => "",
                    $(Self::$id => $name),+
                }
            }
        }
    };
}

numeric_fields! {
    // ContentLimits
    MaxPartBytes => ("content_limits.max_part_bytes", 16 * KIB, KIB, MIB),
    MaxAttributeBytes => ("content_limits.max_attribute_bytes", 128 * KIB, KIB, 8 * MIB),
    MaxProviderBodyBytes => ("content_limits.max_provider_body_bytes", 2048, 0, 8 * MIB),
    // TraceLimits, in table order
    HeldObservations => ("trace_limits.held_observations", 256, 0, 4096),
    Links => ("trace_limits.links", 64, 0, 128),
    ExtensionSpans => ("trace_limits.extension_spans", 256, 0, 4096),
    AttributesPerSpan => ("trace_limits.attributes_per_span", 128, 1, 1024),
    EventsPerSpan => ("trace_limits.events_per_span", 64, 0, 256),
    AttributeValueBytes => ("trace_limits.attribute_value_bytes", 4 * KIB, 64, 64 * KIB),
    ExtensionBytes => ("trace_limits.extension_bytes", MIB, 4 * KIB, 16 * MIB),
    ToolSpanAttributes => ("trace_limits.tool_span_attributes", 16, 0, 64),
    NameBytes => ("trace_limits.name_bytes", KIB, 16, 16 * KIB),
    AttributeKeyBytes => ("trace_limits.attribute_key_bytes", 128, 16, KIB),
    SpanContributionBytes => ("trace_limits.span_contribution_bytes", 256 * KIB, 4 * KIB, 8 * MIB),
    References => ("trace_limits.references", 16, 0, 64),
    ReferenceBytes => ("trace_limits.reference_bytes", 64 * KIB, 0, MIB),
    PublicationHoldRecords => ("trace_limits.publication_hold.records", 256, 1, 4096),
    PublicationHoldBytes => ("trace_limits.publication_hold.bytes", 8 * MIB, 128 * KIB, 64 * MIB),
    // TraceStateLimits: the W3C ceilings are the defaults and the maxima.
    TracestateMembers => ("tracestate_limits.members", 32, 0, 32),
    TracestateChars => ("tracestate_limits.chars", 512, 0, 512),
    TracestateKeyBytes => ("tracestate_limits.key_bytes", 256, 0, 256),
    TracestateTenantBytes => ("tracestate_limits.tenant_bytes", 241, 0, 241),
    TracestateSystemBytes => ("tracestate_limits.system_bytes", 14, 0, 14),
    TracestateValueBytes => ("tracestate_limits.value_bytes", 256, 0, 256),
    // FlightRecorderSettings; durations in milliseconds.
    RecorderCapacity => ("flight_recorder.capacity", 256, 1, 16384),
    RecorderSlowThreshold => ("flight_recorder.slow_threshold_ms", 5000, 1, DAY_MS),
    RecorderMinimumInterval => ("flight_recorder.minimum_interval_ms", 60_000, 0, DAY_MS),
}

impl TracePolicyField {
    /// Every field: the numeric fields, then each kind's detail and export
    /// selection.
    pub fn all() -> impl Iterator<Item = Self> {
        Self::NUMERIC.iter().copied().chain(
            TraceSpanKind::ALL
                .into_iter()
                .flat_map(|kind| [Self::Detail(kind), Self::Export(kind)]),
        )
    }

    /// The documented default of a numeric field.
    pub(crate) const fn default_value(self) -> u64 {
        match self.bounds() {
            Some(bounds) => bounds.default,
            None => 0,
        }
    }

    /// Refuses `value` outside the field's range.
    pub(crate) fn check(self, value: u64) -> Result<(), TracePolicyError> {
        match self.bounds() {
            Some(bounds) => bounds.check(self, value),
            None => Ok(()),
        }
    }
}

impl std::fmt::Display for TracePolicyField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Detail(kind) => write!(f, "trace_detail.{kind}"),
            Self::Selector(kind) => write!(f, "capture_selector.{kind}"),
            Self::Export(kind) => write!(f, "trace_export.{kind}"),
            field => f.write_str(field.numeric_name()),
        }
    }
}
