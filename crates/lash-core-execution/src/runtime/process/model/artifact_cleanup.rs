use super::{ProcessExecutionEnvRef, ProcessId, ProcessInput, ProcessRecord};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Exact artifact-release inputs retained durably when Process Prune removes
/// the authoritative process row. The evidence is deleted only after every
/// configured artifact store has severed that process owner's edges.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessArtifactCleanup {
    pub process_id: ProcessId,
    /// The key the process was started under: its start's staging owner is
    /// keyed by it, never by the id the start minted (ADR 0107).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<crate::StartKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessExecutionEnvRef>,
    pub input: Arc<ProcessInput>,
}

impl ProcessArtifactCleanup {
    pub fn from_record(record: &ProcessRecord) -> Self {
        Self {
            process_id: record.id.clone(),
            start_key: record.start_key.clone(),
            env_ref: record.env_ref.clone(),
            input: Arc::clone(&record.input),
        }
    }

    /// The owner the process's start staged its artifacts under, when it was
    /// started under a key. A process registered without a key staged nothing
    /// under a key, and the keyless owner is shared by every such start, so
    /// there is no staging owner of its own to retire.
    pub fn start_staging_owner(&self) -> Option<super::ArtifactOwner> {
        self.start_key.as_ref().map(|start_key| {
            super::ArtifactOwner::process_start(&crate::ProcessCommand::start_effect_id(Some(
                start_key,
            )))
        })
    }
}

/// Result of acknowledging one durable process-artifact cleanup record.
///
/// A process id is minted and never reused (ADR 0107), so no later process
/// can share the pruned process's cleanup: the acknowledgement either removed
/// the exact record or found none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessArtifactCleanupAck {
    /// The exact cleanup record was removed.
    Acknowledged { process_id: ProcessId },
    /// No cleanup record was found for the process.
    Unknown { process_id: ProcessId },
}
