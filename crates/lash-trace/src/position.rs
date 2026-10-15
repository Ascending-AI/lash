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

/// The kind of span a record names: one row of the span taxonomy.
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
    /// One real provider attempt of a model call.
    ModelAttempt,
    /// One tool call.
    Tool,
    /// One code cell.
    Cell,
    /// A process that is not a user-facing workflow.
    Process,
    /// A workflow process.
    Workflow,
    /// One occurrence of a workflow node.
    WorkflowNode,
    /// A host-submitted tool intent.
    ToolIntent,
    /// A span an extension opened.
    Extension,
    /// A zero-duration point an extension recorded.
    Point,
}

impl TraceSpanKind {
    /// How many kinds there are.
    pub const COUNT: usize = 12;

    /// Every kind, in declaration order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Producer,
        Self::Agent,
        Self::Model,
        Self::ModelAttempt,
        Self::Tool,
        Self::Cell,
        Self::Process,
        Self::Workflow,
        Self::WorkflowNode,
        Self::ToolIntent,
        Self::Extension,
        Self::Point,
    ];

    /// The kind's position in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The kind's snake-case name, as it serializes.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Producer => "producer",
            Self::Agent => "agent",
            Self::Model => "model",
            Self::ModelAttempt => "model_attempt",
            Self::Tool => "tool",
            Self::Cell => "cell",
            Self::Process => "process",
            Self::Workflow => "workflow",
            Self::WorkflowNode => "workflow_node",
            Self::ToolIntent => "tool_intent",
            Self::Extension => "extension",
            Self::Point => "point",
        }
    }

    /// Whether other owners or executions parent to spans of this kind. A
    /// container is never off and never skipped from export: turning it off
    /// would orphan or reparent its children.
    pub const fn is_container(self) -> bool {
        !matches!(
            self,
            Self::Model | Self::ModelAttempt | Self::Extension | Self::Point
        )
    }

    /// The kinds a span of this kind can parent, from the span taxonomy.
    /// Only leaf kinds can be off, so only the leaf rows enter the off rule;
    /// the container rows follow the cause table.
    pub const fn children(self) -> TraceSpanKindSet {
        use TraceSpanKind as K;
        match self {
            K::Producer => TraceSpanKindSet::of(&[K::Agent, K::Process, K::ToolIntent]),
            K::Agent => TraceSpanKindSet::of(&[
                K::Agent,
                K::Model,
                K::Tool,
                K::Cell,
                K::Extension,
                K::Point,
            ]),
            K::Model => TraceSpanKindSet::of(&[K::ModelAttempt]),
            K::Tool => TraceSpanKindSet::of(&[K::Process, K::Model, K::Extension, K::Point]),
            K::Cell => TraceSpanKindSet::of(&[K::Tool, K::Extension, K::Point]),
            K::Process => {
                TraceSpanKindSet::of(&[K::Agent, K::Tool, K::Model, K::Extension, K::Point])
            }
            K::Workflow => TraceSpanKindSet::of(&[
                K::WorkflowNode,
                K::Agent,
                K::Tool,
                K::Model,
                K::Extension,
                K::Point,
            ]),
            K::WorkflowNode => TraceSpanKindSet::of(&[K::Tool, K::Model, K::Extension, K::Point]),
            K::ToolIntent => TraceSpanKindSet::of(&[K::Tool, K::Process]),
            K::Extension => TraceSpanKindSet::of(&[K::Extension, K::Point, K::Model]),
            K::ModelAttempt | K::Point => TraceSpanKindSet::EMPTY,
        }
    }
}

impl std::fmt::Display for TraceSpanKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of span kinds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TraceSpanKindSet(u16);

impl TraceSpanKindSet {
    pub const EMPTY: Self = Self(0);
    /// Every kind.
    pub const ALL: Self = Self::of(&TraceSpanKind::ALL);

    /// The set of `kinds`.
    pub const fn of(kinds: &[TraceSpanKind]) -> Self {
        let mut bits = 0_u16;
        let mut index = 0;
        while index < kinds.len() {
            bits |= 1 << kinds[index].index();
            index += 1;
        }
        Self(bits)
    }

    pub const fn contains(self, kind: TraceSpanKind) -> bool {
        self.0 & (1 << kind.index()) != 0
    }

    /// The kinds of the set, in [`TraceSpanKind::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = TraceSpanKind> {
        TraceSpanKind::ALL
            .into_iter()
            .filter(move |kind| self.contains(*kind))
    }
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
