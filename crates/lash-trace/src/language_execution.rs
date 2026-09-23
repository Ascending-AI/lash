use serde::{Deserialize, Serialize};

use crate::{
    TraceBranchSelection, TraceLanguageChildExecution, TraceLanguageExecutionFailure,
    TraceLanguageExecutionMap, TraceLanguageExecutionStatus,
};

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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    /// The named occurrence has parked on an observed wait. Its `since` is
    /// the enclosing trace record timestamp, not a separately sampled clock.
    NodeWaiting {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        awaited: TraceNodeAwaited,
    },
    /// A parked occurrence resolved, including cancellation. Its node terminal
    /// remains a separate fact.
    NodeResumed {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        resolution: TraceNodeWaitResolution,
    },
    /// Only an occurrence observed in flight may be cancelled.
    NodeCancelled {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
    },
    NodeCompleted {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    NodeFailed {
        node_id: String,
        node_kind: lash_sansio::ExecutionNodeKind,
        label: String,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        failure: TraceLanguageExecutionFailure,
    },
    BranchSelected {
        node_id: String,
        occurrence: u64,
        edge_id: String,
        selected: TraceBranchSelection,
    },
    ChildStarted {
        parent_node_id: String,
        occurrence: u64,
        child: TraceLanguageChildExecution,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceNodeWaitKind {
    Sleep,
    Signal,
    ChildProcess,
    ToolBatch,
    EffectGroup,
}

impl TraceNodeWaitKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Signal => "signal",
            Self::ChildProcess => "child_process",
            Self::ToolBatch => "tool_batch",
            Self::EffectGroup => "effect_group",
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
    EffectGroup {
        group_key: String,
        position: usize,
        wake: lash_sansio::GroupWakePolicy,
    },
}

impl TraceNodeAwaited {
    pub const fn kind(&self) -> TraceNodeWaitKind {
        match self {
            Self::Sleep { .. } => TraceNodeWaitKind::Sleep,
            Self::Signal { .. } => TraceNodeWaitKind::Signal,
            Self::ChildProcesses { .. } => TraceNodeWaitKind::ChildProcess,
            Self::ToolBatch { .. } => TraceNodeWaitKind::ToolBatch,
            Self::EffectGroup { .. } => TraceNodeWaitKind::EffectGroup,
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
