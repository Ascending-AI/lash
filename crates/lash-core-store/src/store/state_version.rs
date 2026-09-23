use super::StoreError;
use crate::SessionId;

/// Oldest session-state generation this runtime can admit.
pub const OLDEST_SUPPORTED_SESSION_STATE_VERSION: u32 = 3;

/// Complete mutable-continuation generation emitted and admitted by this runtime.
/// ADR 0078 refuses the snapshot generation; no converter crosses this cutover.
/// Version 2 (FIG-1961) carries `last_prompt_usage` as the checked `TokenUsage`
/// shape; generation-1 snapshots holding the retired `PromptUsage` fields are
/// refused rather than remapped.
/// Version 3 (FIG-3571) is the carrier IR cutover. A generation-2 session's
/// continuation was written under the retired lashlang node vocabulary: its
/// cells' globals follow the old export rules and its journaled effects carry
/// replay keys under the old node ids, so a redrive would miss them and
/// dispatch again. Under the current, temporary cutover policy, generation-2
/// sessions are refused at lease admission and recovery, before any turn,
/// model, tool or provider effect; the refusal names the found generation so
/// a later migration or drain can identify them.
pub const CURRENT_SESSION_STATE_VERSION: u32 = 3;

/// Successful lease-fenced admission of one complete session-state generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStateAdmission {
    pub session_id: SessionId,
    pub version: u32,
    pub lease_fencing_token: u64,
}

/// Interpret an independently read physical marker.
pub fn resolve_session_state_version(marker: Option<u32>) -> Result<u32, StoreError> {
    let version = marker.unwrap_or(0);
    if version == CURRENT_SESSION_STATE_VERSION {
        Ok(version)
    } else if version < CURRENT_SESSION_STATE_VERSION {
        Err(StoreError::SessionStateVersionUnsupported {
            found: version,
            current: CURRENT_SESSION_STATE_VERSION,
        })
    } else {
        Err(StoreError::SessionStateVersionNewerThanRuntime {
            found: version,
            current: CURRENT_SESSION_STATE_VERSION,
        })
    }
}
