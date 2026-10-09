use serde::{Deserialize, Serialize};

use crate::{
    TraceBranchSelection, TraceLanguageChildExecution, TraceLanguageExecutionFailure,
    TraceLanguageExecutionMap, TraceLanguageExecutionStatus,
};

/// One language execution fact as an observer receives it: the execution
/// event itself, the language that produced it and when it was observed. It
/// carries no trace envelope, so a consumer never filters unrelated trace
/// variants to find it.
///
/// `execution.event_key` is the producer's stable identity of the
/// observation: a publication retry keeps it, and `observed_at_ms` is not
/// part of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageExecutionObservation {
    pub language: String,
    pub execution: crate::TraceLanguageExecution,
    pub observed_at_ms: u64,
}

impl LanguageExecutionObservation {
    /// Whether `other` states the same fact: the same language and
    /// execution event, whenever each was observed.
    pub fn same_fact(&self, other: &Self) -> bool {
        self.language == other.language && self.execution == other.execution
    }
}

/// Which workflow document an execution runs, and where it enters it. It
/// names the document; it is never the document. A host reads the graph the
/// reference names through the facade's workflow inspection and may cache
/// it under this value, which is immutable for a process.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowDocumentRef {
    /// The definition identity of the admitted module the document
    /// projects; the document's own `source_identity`.
    pub source_identity: String,
    /// The stored module the document is read from.
    pub module_ref: String,
    /// Where the execution enters the document.
    pub entry: WorkflowDocumentEntry,
    /// The interpretation of the IR the document is written under.
    pub ir_version: u32,
}

/// The entry of a [`WorkflowDocumentRef`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowDocumentEntry {
    /// The module's main body.
    Main,
    /// One exported process, by the persisted reference the module's
    /// exports name it with.
    Process { process_ref: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceLanguageExecutionPayload {
    ExecutionStarted {
        execution_map: TraceLanguageExecutionMap,
    },
    ExecutionFinished {
        status: TraceLanguageExecutionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    NodeStarted {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        /// The exact site inside the node and the loop activations around
        /// this occurrence. `occurrence` counts per site.
        #[serde(
            default,
            skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
        )]
        context: lash_sansio::WorkflowOccurrenceContext,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<lash_sansio::ToolCallId>,
    },
    /// The named occurrence has parked on an observed wait. Its `since` is
    /// the enclosing trace record timestamp, not a separately sampled clock.
    NodeWaiting {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        /// The exact site inside the node and the loop activations around
        /// this occurrence. `occurrence` counts per site.
        #[serde(
            default,
            skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
        )]
        context: lash_sansio::WorkflowOccurrenceContext,
        awaited: TraceNodeAwaited,
    },
    /// A parked occurrence resolved, including cancellation. Its node terminal
    /// remains a separate fact.
    NodeResumed {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        /// The exact site inside the node and the loop activations around
        /// this occurrence. `occurrence` counts per site.
        #[serde(
            default,
            skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
        )]
        context: lash_sansio::WorkflowOccurrenceContext,
        resolution: TraceNodeWaitResolution,
    },
    /// Only an occurrence observed in flight may be cancelled.
    NodeCancelled {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        /// The exact site inside the node and the loop activations around
        /// this occurrence. `occurrence` counts per site.
        #[serde(
            default,
            skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
        )]
        context: lash_sansio::WorkflowOccurrenceContext,
    },
    NodeCompleted {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        /// The exact site inside the node and the loop activations around
        /// this occurrence. `occurrence` counts per site.
        #[serde(
            default,
            skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
        )]
        context: lash_sansio::WorkflowOccurrenceContext,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<lash_sansio::ToolCallId>,
    },
    NodeFailed {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        /// The exact site inside the node and the loop activations around
        /// this occurrence. `occurrence` counts per site.
        #[serde(
            default,
            skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
        )]
        context: lash_sansio::WorkflowOccurrenceContext,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<lash_sansio::ToolCallId>,
        failure: TraceLanguageExecutionFailure,
    },
    BranchSelected {
        node_id: String,
        occurrence: u64,
        /// The exact site inside the node and the loop activations around
        /// this occurrence. `occurrence` counts per site.
        #[serde(
            default,
            skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
        )]
        context: lash_sansio::WorkflowOccurrenceContext,
        edge_id: String,
        selected: TraceBranchSelection,
    },
    ChildStarted {
        parent_node_id: String,
        occurrence: u64,
        /// The exact site inside the node and the loop activations around
        /// this occurrence. `occurrence` counts per site.
        #[serde(
            default,
            skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
        )]
        context: lash_sansio::WorkflowOccurrenceContext,
        child: TraceLanguageChildExecution,
    },
}

impl TraceLanguageExecutionPayload {
    /// Where the occurrence this fact is about ran inside its node; `None`
    /// for a fact about the whole execution.
    pub fn context(&self) -> Option<&lash_sansio::WorkflowOccurrenceContext> {
        match self {
            Self::ExecutionStarted { .. } | Self::ExecutionFinished { .. } => None,
            Self::NodeStarted { context, .. }
            | Self::NodeWaiting { context, .. }
            | Self::NodeResumed { context, .. }
            | Self::NodeCancelled { context, .. }
            | Self::NodeCompleted { context, .. }
            | Self::NodeFailed { context, .. }
            | Self::BranchSelected { context, .. }
            | Self::ChildStarted { context, .. } => Some(context),
        }
    }

    /// The occurrence this fact is about: its site and which run of that
    /// site it is. Occurrences count per site, so the node id alone does not
    /// name one.
    pub fn occurrence_key(&self) -> Option<(lash_sansio::WorkflowSiteRef, u64)> {
        let (node_id, occurrence, context) = match self {
            Self::ExecutionStarted { .. } | Self::ExecutionFinished { .. } => return None,
            Self::NodeStarted {
                node_id,
                occurrence,
                context,
                ..
            }
            | Self::NodeWaiting {
                node_id,
                occurrence,
                context,
                ..
            }
            | Self::NodeResumed {
                node_id,
                occurrence,
                context,
                ..
            }
            | Self::NodeCancelled {
                node_id,
                occurrence,
                context,
                ..
            }
            | Self::NodeCompleted {
                node_id,
                occurrence,
                context,
                ..
            }
            | Self::NodeFailed {
                node_id,
                occurrence,
                context,
                ..
            }
            | Self::BranchSelected {
                node_id,
                occurrence,
                context,
                ..
            }
            | Self::ChildStarted {
                parent_node_id: node_id,
                occurrence,
                context,
                ..
            } => (node_id, *occurrence, context),
        };
        Some((
            lash_sansio::WorkflowSiteRef::new(node_id.clone(), context.site_path.clone()),
            occurrence,
        ))
    }

    /// The exact site of the fact's occurrence inside its node: the node's
    /// own statement for a fact about the whole execution.
    pub fn site_path(&self) -> lash_sansio::WorkflowSitePath {
        self.context()
            .map(|context| context.site_path.clone())
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceNodeWaitKind {
    Sleep,
    Signal,
    ChildProcess,
    ToolBatch,
    RunAggregate,
}

impl TraceNodeWaitKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Signal => "signal",
            Self::ChildProcess => "child_process",
            Self::ToolBatch => "tool_batch",
            Self::RunAggregate => "run_aggregate",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceNodeAwaited {
    Sleep {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline_ms: Option<u64>,
    },
    Signal {
        name: String,
        key: String,
    },
    ChildProcesses {
        process_ids: Vec<lash_sansio::ProcessId>,
    },
    ToolBatch {
        batch_id: String,
        position: usize,
    },
    RunAggregate {
        group_key: String,
        position: usize,
        wake: lash_sansio::RunAggregateWakePolicy,
    },
}

impl TraceNodeAwaited {
    pub const fn kind(&self) -> TraceNodeWaitKind {
        match self {
            Self::Sleep { .. } => TraceNodeWaitKind::Sleep,
            Self::Signal { .. } => TraceNodeWaitKind::Signal,
            Self::ChildProcesses { .. } => TraceNodeWaitKind::ChildProcess,
            Self::ToolBatch { .. } => TraceNodeWaitKind::ToolBatch,
            Self::RunAggregate { .. } => TraceNodeWaitKind::RunAggregate,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceNodeWaitResolution {
    Resumed,
    TimedOut,
    Cancelled,
}

impl TraceNodeWaitResolution {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}
