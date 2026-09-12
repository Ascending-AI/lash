use super::{ProcessExecutionEnvRef, ProcessId, ProcessIncarnation, ProcessInput, ProcessRecord};
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
