use serde::{Deserialize, Serialize};

use crate::{
    TraceBranchSelection, TraceLanguageChildExecution, TraceLanguageExecutionFailure,
    TraceLanguageExecutionStatus,
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
    /// The source dialect, absent for a dialect-free graph execution.
    pub language: Option<String>,
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
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct WorkflowDocumentRef {
    /// The definition identity of the admitted module the document
    /// projects; the document's own `source_identity`.
    pub source_identity: String,
    /// The stored module the document is read from.
    pub module_ref: lash_sansio::ModuleRef,
    /// Where the execution enters the document.
    pub entry: WorkflowDocumentEntry,
    /// The interpretation of the IR the document is written under.
    pub ir_version: u32,
}

/// The entry of a [`WorkflowDocumentRef`].
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowDocumentEntry {
    /// The module's main body.
    Main,
    /// One exported process, by the persisted reference the module's
    /// exports name it with.
    Process { process_ref: String },
}

/// The admitted body of a process step started: the step's actor committed
/// its admission, bound it to a call, and is about to run it. A step that is
/// refused never produces one, and a retried body produces one more with the
/// same site, occurrence and call and the next attempt.
///
/// It names no language and no document: the process's own definition scopes
/// the site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StepBodyStarted {
    pub process_id: lash_sansio::ProcessId,
    /// The occurrence of the site in the process's workflow document the
    /// step runs for.
    pub at: lash_sansio::WorkflowOccurrence,
    /// The call the admission bound the step to.
    pub call_id: lash_sansio::ToolCallId,
    /// The one-based attempt of the admitted body.
    pub attempt: u32,
}

impl StepBodyStarted {
    /// The identity a redelivery of this fact repeats: one per attempt of
    /// one call.
    pub fn event_key(&self) -> String {
        format!(
            "step_body:process:{}:{}:attempt:{}",
            self.process_id, self.call_id, self.attempt
        )
    }
}

/// One [`StepBodyStarted`] as an observer receives it, with when it was
/// observed. `observed_at_ms` is not part of the fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepBodyStartedObservation {
    pub step: StepBodyStarted,
    pub observed_at_ms: u64,
}

#[expect(
    clippy::large_enum_variant,
    reason = "nearly every payload is a node fact, so boxing it would allocate once per record"
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceLanguageExecutionPayload {
    /// The execution began. Its identity names the workflow document and
    /// entry; the document itself is read through facade workflow inspection.
    ExecutionStarted,
    ExecutionFinished {
        status: TraceLanguageExecutionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// One fact about one occurrence of one execution site.
    Node {
        at: lash_sansio::WorkflowOccurrence,
        fact: TraceNodeFact,
    },
}

impl TraceLanguageExecutionPayload {
    /// The occurrence this fact is about; `None` for a fact about the whole
    /// execution.
    pub fn at(&self) -> Option<&lash_sansio::WorkflowOccurrence> {
        match self {
            Self::ExecutionStarted | Self::ExecutionFinished { .. } => None,
            Self::Node { at, .. } => Some(at),
        }
    }

    /// What happened to the occurrence this fact is about; `None` for a fact
    /// about the whole execution.
    pub fn node_fact(&self) -> Option<&TraceNodeFact> {
        match self {
            Self::ExecutionStarted | Self::ExecutionFinished { .. } => None,
            Self::Node { fact, .. } => Some(fact),
        }
    }
}

/// What happened to the occurrence a
/// [`TraceLanguageExecutionPayload::Node`] names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceNodeFact {
    Started {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<lash_sansio::ToolCallId>,
    },
    /// The occurrence has parked on an observed wait. Its `since` is the
    /// enclosing trace record timestamp, not a separately sampled clock.
    Waiting { awaited: TraceNodeAwaited },
    /// A parked occurrence resolved, including cancellation. Its node
    /// terminal remains a separate fact.
    Resumed { resolution: TraceNodeWaitResolution },
    /// Only an occurrence observed in flight may be cancelled.
    Cancelled,
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<lash_sansio::ToolCallId>,
    },
    Failed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<lash_sansio::ToolCallId>,
        failure: TraceLanguageExecutionFailure,
    },
    BranchSelected {
        /// The arm of the branch this occurrence took.
        selected: TraceBranchSelection,
    },
    /// The occurrence started a child execution.
    ChildStarted { child: TraceLanguageChildExecution },
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
