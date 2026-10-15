//! Every tracing knob is a sealed, validated policy: private fields, a
//! documented default and range, a constructor or builder that refuses a
//! value with a typed [`TracePolicyError`] naming the field, the value and
//! the bound it breaks, and accessors for the effective values.
//!
//! Refusals that must not fail execution are a typed [`TraceOmission`]
//! instead: counted, reported and logged, never an error.

use serde::Serialize;

use crate::{TelemetryContent, TraceSpanKind};

mod detail;
mod error;
mod limits;
mod omission;
mod writer;

pub use detail::{
    TraceDetail, TraceDetailPolicy, TraceDetailPolicyBuilder, TraceExportPolicy,
    TraceExportPolicyBuilder, TraceMetrics,
};
pub use error::{
    FieldBounds, PolicyOperand, PolicyRelation, PolicyValue, TracePolicyError, TracePolicyField,
    TracePolicySelection,
};
pub use limits::{PublicationHold, TraceLimits, TraceLimitsBuilder};
pub use omission::{
    TraceOmission, TraceOmissionCounts, TraceOmissionDiagnostic, TraceOmissionLedger,
    TraceOmissionReport, TraceOmissionSite, TraceOmissionSiteKind,
};
pub use writer::{AttrKey, Contributions, TraceAttributeWriter, TraceValue, WriterRemaining};

/// The off rule over one policy: a kind that is off while a kind in its
/// [`children`](TraceSpanKind::children) is on is refused. `off` says
/// whether a kind is off; `side` names a kind's field and value.
fn check_off_rule(
    off: impl Fn(TraceSpanKind) -> bool,
    side: impl Fn(TraceSpanKind) -> (TracePolicyField, PolicyValue),
) -> Result<(), TracePolicyError> {
    for parent in TraceSpanKind::ALL {
        if !off(parent) {
            continue;
        }
        if let Some(child) = parent
            .children()
            .iter()
            .find(|child| *child != parent && !off(*child))
        {
            return Err(detail::off_rule_relation(
                parent,
                child,
                side(parent),
                side(child),
            ));
        }
    }
    Ok(())
}

/// The bounds lash puts on the content a record carries.
///
/// Sealed: built through [`ContentLimits::new`], which refuses a value
/// outside its range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ContentLimits {
    max_provider_body_bytes: u64,
}

impl ContentLimits {
    /// A 2048-byte provider request body.
    pub const fn standard() -> Self {
        Self {
            max_provider_body_bytes: TracePolicyField::MaxProviderBodyBytes.default_value(),
        }
    }

    /// `max_provider_body_bytes`: the largest whole provider request body an
    /// extended model trace keeps (`content_limits.max_provider_body_bytes`:
    /// default 2048, 0..=8 MiB).
    pub fn new(max_provider_body_bytes: u64) -> Result<Self, TracePolicyError> {
        TracePolicyField::MaxProviderBodyBytes.check(max_provider_body_bytes)?;
        Ok(Self {
            max_provider_body_bytes,
        })
    }

    pub const fn max_provider_body_bytes(&self) -> u64 {
        self.max_provider_body_bytes
    }
}

impl Default for ContentLimits {
    fn default() -> Self {
        Self::standard()
    }
}

/// Every tracing policy in effect on a core, after validation: what the
/// enricher reads, and what a host can log or assert on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TracePolicySnapshot {
    pub content: TelemetryContent,
    pub content_limits: ContentLimits,
    pub detail: TraceDetailPolicy,
    pub limits: TraceLimits,
    /// `None` without the OpenTelemetry exporter.
    pub export: Option<TraceExportPolicy>,
    /// `None` without the OpenTelemetry exporter.
    pub metrics: Option<TraceMetrics>,
    /// Whether a span enricher is installed.
    pub enricher: bool,
}

impl TracePolicySnapshot {
    /// The policies a core has with no host choice and no exporter.
    pub fn standard() -> Self {
        Self {
            content: TelemetryContent::standard(),
            content_limits: ContentLimits::standard(),
            detail: TraceDetailPolicy::standard(),
            limits: TraceLimits::standard(),
            export: None,
            metrics: None,
            enricher: false,
        }
    }

    /// The value in effect of a numeric field the core holds: a content or
    /// trace limit. `None` for a field a host builds outside the core
    /// (tracestate limits, flight-recorder settings) and for a per-kind
    /// selection, which [`Self::detail`] and [`Self::export`] report.
    pub fn value(&self, field: TracePolicyField) -> Option<u64> {
        if field == TracePolicyField::MaxProviderBodyBytes {
            return Some(self.content_limits.max_provider_body_bytes());
        }
        self.limits
            .values()
            .find_map(|(row, value)| (row == field).then_some(value))
    }
}

impl Default for TracePolicySnapshot {
    fn default() -> Self {
        Self::standard()
    }
}
