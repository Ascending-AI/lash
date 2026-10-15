//! Where a record sits in the trace: its W3C span, parent, links, role and
//! kind, as every consumer reads it.

use serde::{Deserialize, Serialize};

use crate::TraceTimestamp;
use crate::telemetry::{TraceAttemptLink, TraceCarrier, TraceLinks, TraceScopeId};

/// What a record is to the span it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpanRole {
    /// A live hint that the span started. Its span may never close if its
    /// execution dies first.
    Opens,
    /// The span's one closing record, published under the closure rules: it
    /// carries the span's exported start and end.
    Closes,
    /// A span opened and closed by this one record.
    Whole,
    /// A live record inside the span.
    Event,
}

impl SpanRole {
    /// Whether the record publishes its span: its closing or whole record.
    pub fn publishes(self) -> bool {
        matches!(self, Self::Closes | Self::Whole)
    }
}

/// The kind of span a record names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceSpanKind {
    /// A host call that admits durable work: a trace's root or the caller's
    /// child.
    Producer,
    /// A turn.
    Agent,
    /// One logical model call.
    Model,
    /// One tool call.
    Tool,
    /// One code cell.
    Cell,
    /// A process.
    Process,
    /// A host-submitted tool intent.
    ToolIntent,
}

/// A record's position: the span it opens, closes, is, or is an event of,
/// with that span's parent, links and real start.
///
/// The span's flags are its trace's sampling decision. `parent` is `None`
/// only for a producer with no caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TracePosition {
    pub span: TraceCarrier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<TraceCarrier>,
    #[serde(default, skip_serializing_if = "TraceLinks::is_empty")]
    pub links: TraceLinks,
    pub role: SpanRole,
    pub kind: TraceSpanKind,
    /// The span's real start; on a closing or whole record, its exported
    /// start.
    pub started_at: TraceTimestamp,
    /// Set on a closing or whole record: the span's exported end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<TraceTimestamp>,
    /// The span's logical owner, when a durable scope owns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<TraceScopeId>,
    /// The substrate attempt that made the record, when the substrate
    /// reports one: a link, never a parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<TraceAttemptLink>,
}

impl TracePosition {
    /// Whether the record publishes its span ([`SpanRole::publishes`]).
    pub fn publishes(&self) -> bool {
        self.role.publishes()
    }

    /// Whether the span's trace was sampled at its producer.
    pub fn is_sampled(&self) -> bool {
        self.span.flags().is_sampled()
    }
}
