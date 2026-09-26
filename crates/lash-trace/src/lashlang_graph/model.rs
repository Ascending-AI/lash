use chrono::{DateTime, Utc};
use lash_sansio::ExecutionNodeKind;
use serde::{Deserialize, Serialize};

use crate::{
    TraceBranchSelection, TraceLabelMetadata, TraceLanguageExecution,
    TraceLanguageExecutionFailure, TraceLanguageExecutionGeneration,
    TraceLanguageExecutionMap as LanguageExecutionMap,
    TraceLanguageExecutionStatus as LanguageExecutionStatus, TraceRuntimeScope,
    TraceRuntimeSubject, ensure_trace_schema_version,
};

/// Default number of occurrence histories retained for each graph node.
pub const DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT: usize = 256;

/// Whether the static execution map was available to the fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceLashlangGraphCompleteness {
    Complete,
    IncompleteMap,
}

/// Canonical identity of one fold input.
///
/// Node transitions use `(node_id, node_kind, occurrence, attempt)` plus
/// the transition kind. The transition kind lets a start and its terminal
/// fact merge monotonically while still making a second, different start or
/// terminal a typed conflict.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct TraceLashlangEventIdentity {
    #[serde(flatten)]
    pub generation: Option<TraceLanguageExecutionGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_kind: Option<ExecutionNodeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<u64>,
    pub transition: TraceLashlangEventTransition,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TraceLashlangEventTransition {
    ExecutionStarted,
    ExecutionFinished,
    NodeStarted,
    NodeWaiting,
    NodeResumed,
    NodeTerminal,
    BranchSelected,
    ChildStarted,
}

/// One canonical event retained so a persisted snapshot can be folded again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLashlangGraphHistoryEvent {
    pub identity: TraceLashlangEventIdentity,
    pub timestamp: DateTime<Utc>,
    pub event: TraceLanguageExecution,
}

/// Why two records under one logical identity did not deduplicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceLashlangGraphConflictKind {
    ConflictingDuplicate,
}

/// A bounded, typed record of divergent values under one logical identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLashlangGraphConflict {
    pub identity: TraceLashlangEventIdentity,
    pub kind: TraceLashlangGraphConflictKind,
    pub variants: Vec<String>,
}

/// Trace-derived Lashlang execution graph snapshot for hosts and debugging tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct TraceLashlangGraph {
    pub schema_version: u32,
    pub graph_key: String,
    pub scope: TraceRuntimeScope,
    pub subject: TraceRuntimeSubject,
    pub source_identity: String,
    pub module_ref: String,
    pub entry_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_ref: Option<String>,
    pub entry_name: String,
    pub status: LanguageExecutionStatus,
    pub completeness: TraceLashlangGraphCompleteness,
    pub nodes: Vec<TraceLashlangGraphNode>,
    pub edges: Vec<TraceLashlangGraphEdge>,
    pub children: Vec<TraceLashlangGraphChildLink>,
    pub history_limit: usize,
    pub node_retention: Vec<TraceLashlangNodeRetention>,
    pub conflicts: Vec<TraceLashlangGraphConflict>,
    pub history: Vec<TraceLashlangGraphHistoryEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_map: Option<LanguageExecutionMap>,
}

#[derive(Deserialize)]
struct TraceLashlangGraphWire {
    schema_version: u32,
    graph_key: String,
    scope: TraceRuntimeScope,
    subject: TraceRuntimeSubject,
    source_identity: String,
    module_ref: String,
    entry_kind: String,
    entry_ref: Option<String>,
    entry_name: String,
    status: LanguageExecutionStatus,
    completeness: TraceLashlangGraphCompleteness,
    nodes: Vec<TraceLashlangGraphNode>,
    edges: Vec<TraceLashlangGraphEdge>,
    children: Vec<TraceLashlangGraphChildLink>,
    history_limit: usize,
    node_retention: Vec<TraceLashlangNodeRetention>,
    conflicts: Vec<TraceLashlangGraphConflict>,
    history: Vec<TraceLashlangGraphHistoryEvent>,
    execution_map: Option<LanguageExecutionMap>,
}

impl<'de> Deserialize<'de> for TraceLashlangGraph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let schema_version = value
            .get("schema_version")
            .cloned()
            .ok_or_else(|| serde::de::Error::missing_field("schema_version"))
            .and_then(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))?;
        ensure_trace_schema_version(schema_version).map_err(serde::de::Error::custom)?;
        let wire: TraceLashlangGraphWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema_version: wire.schema_version,
            graph_key: wire.graph_key,
            scope: wire.scope,
            subject: wire.subject,
            source_identity: wire.source_identity,
            module_ref: wire.module_ref,
            entry_kind: wire.entry_kind,
            entry_ref: wire.entry_ref,
            entry_name: wire.entry_name,
            status: wire.status,
            completeness: wire.completeness,
            nodes: wire.nodes,
            edges: wire.edges,
            children: wire.children,
            history_limit: wire.history_limit,
            node_retention: wire.node_retention,
            conflicts: wire.conflicts,
            history: wire.history,
            execution_map: wire.execution_map,
        })
    }
}

/// Aggregate facts for occurrences evicted from one node's bounded history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLashlangNodeRetention {
    pub node_id: String,
    pub truncation_watermark: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_node: Option<Box<TraceLashlangGraphNode>>,
    pub watermark_history: Vec<TraceLashlangGraphHistoryEvent>,
    pub node: TraceLashlangGraphNode,
    pub selected_edge_ids: Vec<String>,
    pub children: Vec<TraceLashlangGraphChildLink>,
}

/// One occurrence's observed Lashlang graph node state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TraceLashlangNodeObservation {
    #[default]
    Unobserved,
    Running {
        occurrence: u64,
        start: DateTime<Utc>,
    },
    Waiting {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        since: DateTime<Utc>,
        awaited: crate::TraceNodeAwaited,
    },
    Completed {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        end: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
    },
    Failed {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        end: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
        failure: TraceLanguageExecutionFailure,
    },
    Cancelled {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        end: DateTime<Utc>,
    },
    Skipped {
        end: DateTime<Utc>,
        branch_node_id: String,
        branch_occurrence: u64,
    },
}

impl TraceLashlangNodeObservation {
    /// Whether no later transition for this occurrence may replace it.
    pub const fn is_terminal(&self) -> bool {
        match self {
            Self::Unobserved | Self::Running { .. } | Self::Waiting { .. } => false,
            Self::Completed { .. }
            | Self::Failed { .. }
            | Self::Cancelled { .. }
            | Self::Skipped { .. } => true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLashlangNodeSummary {
    pub retained_occurrences: u64,
    pub started_count: u64,
    pub terminal_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_terminal: Option<TraceLashlangNodeTerminalSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_terminal: Option<TraceLashlangNodeTerminalSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLashlangNodeTerminalSummary {
    pub occurrence: u64,
    pub status: TraceLashlangNodeTerminalStatus,
    pub end: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceLashlangNodeTerminalStatus {
    Completed,
    Failed,
    Cancelled,
    Skipped,
}

/// Observed branch-edge selection state.
///
/// Only the edge a `BranchSelected` event names is marked. There is no
/// `Rejected`: the execution map carries data-dependency and sequencing edges
/// (ADR 0037) and nothing marks an edge as a branch arm, so an unselected arm
/// is indistinguishable from an ordinary edge leaving the same node. Which arm
/// ran is read from the typed selection on the branch node itself
/// ([`TraceLashlangGraphNode::branch_selection`]).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TraceLashlangEdgeSelection {
    #[default]
    Unknown,
    Selected,
}

/// Trace-derived Lashlang graph node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLashlangGraphNode {
    pub id: String,
    pub kind: ExecutionNodeKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_metadata: Option<TraceLabelMetadata>,
    /// Which arm a branch node took, copied from the typed `selected` field of
    /// the `BranchSelected` event. Absent on every other node, and on a branch
    /// whose selection has not been observed yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_selection: Option<TraceBranchSelection>,
    #[serde(flatten)]
    pub observation: TraceLashlangNodeObservation,
    pub summary: TraceLashlangNodeSummary,
}

impl TraceLashlangGraphNode {
    pub(super) fn unobserved(
        id: impl Into<String>,
        kind: ExecutionNodeKind,
        label: impl Into<String>,
        label_metadata: Option<TraceLabelMetadata>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            label: label.into(),
            label_metadata,
            branch_selection: None,
            observation: TraceLashlangNodeObservation::Unobserved,
            summary: TraceLashlangNodeSummary::default(),
        }
    }
}

/// Trace-derived Lashlang graph edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLashlangGraphEdge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub label: String,
    pub selection: TraceLashlangEdgeSelection,
}

/// Link from an observed parent Lashlang node to a child execution graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLashlangGraphChildLink {
    pub parent_graph_key: String,
    pub parent_node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_graph_key: Option<String>,
    pub child_process_id: lash_sansio::ProcessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_module_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_entry_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_entry_name: Option<String>,
}
