use serde::{Deserialize, Serialize};

use super::ProcessRecord;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "completion", rename_all = "snake_case")]
pub enum ProcessCompletionOutcome {
    Committed(ProcessRecord),
    AlreadyApplied { stored: ProcessRecord },
    Superseded { stored: ProcessRecord },
}

impl ProcessCompletionOutcome {
    /// Classifies a repeated completion as already applied only when the stored terminal outcome
    /// exactly equals the proposal; a different retained outcome is superseding evidence.
    pub fn from_stored(record: ProcessRecord, proposed: &super::ProcessAwaitOutput) -> Self {
        if record.outcome().as_ref() == Some(proposed) {
            Self::AlreadyApplied { stored: record }
        } else {
            Self::Superseded { stored: record }
        }
    }

    pub fn stored(&self) -> &ProcessRecord {
        match self {
            Self::Committed(record)
            | Self::AlreadyApplied { stored: record }
            | Self::Superseded { stored: record } => record,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn into_record(self) -> ProcessRecord {
        match self {
            Self::Committed(record)
            | Self::AlreadyApplied { stored: record }
            | Self::Superseded { stored: record } => record,
        }
    }
}

impl std::ops::Deref for ProcessCompletionOutcome {
    type Target = ProcessRecord;

    fn deref(&self) -> &Self::Target {
        self.stored()
    }
}

/// Execution-local context for process runners. Durable edges live on the
/// process record.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessExecutionContext {
    /// Replay-stable segment ordinal supplied by the engine's admitted start.
    #[serde(skip)]
    pub segment_ordinal: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal_invocation: Option<crate::RuntimeInvocation>,
}

impl ProcessExecutionContext {
    /// Sets the causal invocation carried by a `ProcessExecutionContext` for store and
    /// process-engine implementors while persisting and coordinating durable process execution.
    pub fn with_causal_invocation(mut self, invocation: Option<crate::RuntimeInvocation>) -> Self {
        self.causal_invocation = invocation;
        self
    }

    /// Lets store and process-engine implementors test whether this `ProcessExecutionContext` is
    /// empty while persisting and coordinating durable process execution.
    pub fn is_empty(&self) -> bool {
        self.causal_invocation.is_none()
    }
}
