//! Why lash did not keep something a plugin, tool body, hook, selector or
//! enricher contributed, or a record it held, and where that happened.
//! An omission never fails execution: it is counted, reported and logged.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::{TraceScopeId, TraceSpanKind, W3cSpanId};

macro_rules! omissions {
    ($($id:ident => $name:literal),+ $(,)?) => {
        /// Why lash did not keep a contribution or a held record.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum TraceOmission { $($id),+ }

        impl TraceOmission {
            pub const ALL: &[Self] = &[$(Self::$id),+];
            pub const COUNT: usize = Self::ALL.len();

            /// The reason's snake-case name, as `lash.trace.omitted` and the
            /// counts' map spell it.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$id => $name),+ }
            }

            pub fn of_name(name: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|reason| reason.as_str() == name)
            }

            const fn index(self) -> usize {
                self as usize
            }
        }
    };
}

omissions! {
    Spans => "spans",
    Attributes => "attributes",
    Events => "events",
    ValueTruncated => "value_truncated",
    ValueTooLarge => "value_too_large",
    BodyBytes => "body_bytes",
    Name => "name",
    Key => "key",
    ReservedKey => "reserved_key",
    Immutable => "immutable",
    UseReplace => "use_replace",
    DisallowedKey => "disallowed_key",
    NonFinite => "non_finite",
    References => "references",
    ReferenceBytes => "reference_bytes",
    SpanBytes => "span_bytes",
    HeldObservations => "held_observations",
    PublicationHold => "publication_hold",
    Late => "late",
    Links => "links",
    SelectorFailed => "selector_failed",
    HookFailed => "hook_failed",
    EnricherFailed => "enricher_failed",
}

impl std::fmt::Display for TraceOmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where an omission happened: a stable lash address, never a host value.
/// Two bodies or two records never share one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "site", rename_all = "snake_case", deny_unknown_fields)]
pub enum TraceOmissionSite {
    /// An extension body or a tool body's annotations: the registered
    /// callback it was bound from, the span its capability was bound at,
    /// and the admitted attempt it ran for.
    Body {
        owner: String,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<W3cSpanId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<lash_sansio::ToolCallId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
    },
    /// One record's content-hook references or selector failure.
    Record {
        kind: TraceSpanKind,
        record: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span: Option<W3cSpanId>,
    },
    /// One exported span's enrichment, or a producer's sampler attributes.
    Span {
        kind: TraceSpanKind,
        span: W3cSpanId,
    },
    /// Observations a shift held behind its journal frontier.
    Shift { scope: Option<TraceScopeId> },
    /// Links over the limit at a scope's admission or close.
    Admission { scope: TraceScopeId },
    /// Records an unanchored cell shed under hold pressure.
    Cell { graph_key: String },
}

impl TraceOmissionSite {
    pub fn kind(&self) -> TraceOmissionSiteKind {
        match self {
            Self::Body { .. } => TraceOmissionSiteKind::Body,
            Self::Record { .. } => TraceOmissionSiteKind::Record,
            Self::Span { .. } => TraceOmissionSiteKind::Span,
            Self::Shift { .. } => TraceOmissionSiteKind::Shift,
            Self::Admission { .. } => TraceOmissionSiteKind::Admission,
            Self::Cell { .. } => TraceOmissionSiteKind::Cell,
        }
    }
}

/// The kind of an omission's site, which the core-wide report aggregates by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TraceOmissionSiteKind {
    Body,
    Record,
    Span,
    Shift,
    Admission,
    Cell,
}

impl TraceOmissionSiteKind {
    pub const ALL: [Self; 6] = [
        Self::Body,
        Self::Record,
        Self::Span,
        Self::Shift,
        Self::Admission,
        Self::Cell,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Record => "record",
            Self::Span => "span",
            Self::Shift => "shift",
            Self::Admission => "admission",
            Self::Cell => "cell",
        }
    }
}

/// Omissions by reason: one saturating counter per [`TraceOmission`].
/// Serialized as a map of the non-zero reasons.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TraceOmissionCounts([u32; TraceOmission::COUNT]);

impl TraceOmissionCounts {
    pub fn get(&self, reason: TraceOmission) -> u32 {
        self.0[reason.index()]
    }

    /// The non-zero reasons and their counts.
    pub fn iter(&self) -> impl Iterator<Item = (TraceOmission, u32)> + '_ {
        TraceOmission::ALL
            .iter()
            .map(|reason| (*reason, self.get(*reason)))
            .filter(|(_, count)| *count > 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|count| *count == 0)
    }

    /// Every omission counted, saturating.
    pub fn total(&self) -> u32 {
        self.0
            .iter()
            .fold(0_u32, |total, count| total.saturating_add(*count))
    }

    pub fn add(&mut self, reason: TraceOmission) {
        self.add_n(reason, 1);
    }

    pub fn add_n(&mut self, reason: TraceOmission, count: u32) {
        let slot = &mut self.0[reason.index()];
        *slot = slot.saturating_add(count);
    }

    /// Adds `other`'s counts to these.
    pub fn merge(&mut self, other: &Self) {
        for (reason, count) in other.iter() {
            self.add_n(reason, count);
        }
    }
}

impl Serialize for TraceOmissionCounts {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter().map(|(reason, count)| (reason.as_str(), count)))
    }
}

impl<'de> Deserialize<'de> for TraceOmissionCounts {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = BTreeMap::<String, u32>::deserialize(deserializer)?;
        let mut counts = Self::default();
        for (name, count) in map {
            let reason = TraceOmission::of_name(&name).ok_or_else(|| {
                serde::de::Error::custom(format!("unknown omission reason `{name}`"))
            })?;
            counts.add_n(reason, count);
        }
        Ok(counts)
    }
}

impl schemars::JsonSchema for TraceOmissionCounts {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TraceOmissionCounts".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let properties = TraceOmission::ALL
            .iter()
            .map(|reason| {
                (
                    reason.as_str().to_owned(),
                    serde_json::json!({ "type": "integer", "format": "uint32", "minimum": 1 }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        schemars::json_schema!({
            "description": "Omissions by reason: the non-zero reasons and their counts.",
            "type": "object",
            "properties": properties,
            "additionalProperties": false,
        })
    }
}

/// One body's or one record's omissions with the address they happened at.
/// Bounded whatever the body did: one site and one fixed counter array.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceOmissionDiagnostic {
    pub site: TraceOmissionSite,
    pub counts: TraceOmissionCounts,
}

/// Core-wide omission counts since the core started, by site kind and
/// reason only: fixed-size whatever happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceOmissionReport {
    counts: [[u64; TraceOmission::COUNT]; TraceOmissionSiteKind::COUNT],
}

impl TraceOmissionReport {
    pub fn get(&self, site: TraceOmissionSiteKind, reason: TraceOmission) -> u64 {
        self.counts[site as usize][reason.index()]
    }

    /// Every omission counted at any site.
    pub fn total(&self) -> u64 {
        self.counts
            .iter()
            .flatten()
            .fold(0_u64, |total, count| total.saturating_add(*count))
    }
}

/// The core-wide omission counters every tracing path records into, which
/// [`TraceOmissionReport`] reads. Each `(site kind, reason)` logs once, when
/// it is first counted.
#[derive(Debug)]
pub struct TraceOmissionLedger {
    counts: [[AtomicU64; TraceOmission::COUNT]; TraceOmissionSiteKind::COUNT],
}

impl Default for TraceOmissionLedger {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}

impl TraceOmissionLedger {
    /// Counts `counts` at `site`.
    pub fn record(&self, site: &TraceOmissionSite, counts: &TraceOmissionCounts) {
        let kind = site.kind();
        for (reason, count) in counts.iter() {
            let previous = self.counts[kind as usize][reason.index()]
                .fetch_add(u64::from(count), Ordering::Relaxed);
            if previous == 0 {
                tracing::warn!(
                    event = "trace.omitted",
                    site = kind.as_str(),
                    reason = reason.as_str(),
                    "lash omitted a trace contribution or held record; later omissions of this reason and site kind are counted only"
                );
            }
        }
    }

    /// Counts one `reason` at `site`.
    pub fn record_one(&self, site: &TraceOmissionSite, reason: TraceOmission) {
        let mut counts = TraceOmissionCounts::default();
        counts.add(reason);
        self.record(site, &counts);
    }

    pub fn report(&self) -> TraceOmissionReport {
        TraceOmissionReport {
            counts: std::array::from_fn(|site| {
                std::array::from_fn(|reason| self.counts[site][reason].load(Ordering::Relaxed))
            }),
        }
    }
}
