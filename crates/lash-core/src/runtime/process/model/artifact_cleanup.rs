use super::{
    ProcessExecutionEnvRef, ProcessId, ProcessIncarnation, ProcessInput, ProcessRecord, ProcessRef,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Exact artifact-release inputs retained durably when Process Prune removes
/// the authoritative process row. The evidence is deleted only after every
/// configured artifact store has severed that process owner's edges.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessArtifactCleanup {
    pub process_id: ProcessId,
    pub incarnation: ProcessIncarnation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessExecutionEnvRef>,
    pub input: Arc<ProcessInput>,
}

impl ProcessArtifactCleanup {
    pub fn from_record(record: &ProcessRecord) -> Self {
        Self {
            process_id: record.id.clone(),
            incarnation: record.incarnation,
            env_ref: record.env_ref.clone(),
            input: Arc::clone(&record.input),
        }
    }
}

/// Result of acknowledging one durable process-artifact cleanup record.
///
/// Acknowledgement observes the current process incarnation and the exact
/// cleanup row within one statement or transaction snapshot. When that
/// snapshot contains a successor incarnation, implementations prioritize
/// [`ProcessArtifactCleanupAck::StaleIncarnation`] whether or not this call
/// deleted the predecessor row. Repeated calls therefore remain idempotently
/// stale after the cleanup row is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "stale process incarnations must not be acknowledged silently"]
pub enum ProcessArtifactCleanupAck {
    /// The exact cleanup record was removed and no successor is live.
    Acknowledged { process_ref: ProcessRef },
    /// A successor incarnation was detected; the cleanup row may already be absent.
    StaleIncarnation {
        expected: ProcessRef,
        found: ProcessRef,
    },
    /// No exact cleanup record or successor incarnation was found.
    Unknown { process_ref: ProcessRef },
}
