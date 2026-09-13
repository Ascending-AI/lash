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
/// Acknowledgement removes the exact cleanup record even when the host-facing
/// process id has since been registered again. The stale outcome makes that
/// replacement visible without allowing the predecessor cleanup to retain
/// dead artifact-owner edges indefinitely.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "stale process incarnations must not be acknowledged silently"]
pub enum ProcessArtifactCleanupAck {
    /// The exact cleanup record was removed and no successor is live.
    Acknowledged { process_ref: ProcessRef },
    /// The exact cleanup record was removed while a successor incarnation is live.
    StaleIncarnation {
        expected: ProcessRef,
        found: ProcessRef,
    },
    /// No exact cleanup record or successor incarnation was found.
    Unknown { process_ref: ProcessRef },
}
