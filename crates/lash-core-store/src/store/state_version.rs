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
///
/// `fleet` is the store's recorded `F`: the marker admits the build's newest
/// generation and the version `F` records for the session-state surface —
/// the `[N-1, N]` window the fleet reads through while a finalize is pending
/// (FIG-3796, ADR 0106 §2). Anything outside the pair reports the same
/// unsupported/newer refusal the exact-version gate reported.
pub fn resolve_session_state_version(
    marker: Option<u32>,
    fleet: super::FleetFormat,
) -> Result<u32, StoreError> {
    let version = marker.unwrap_or(0);
    let window = fleet.read_window(crate::surface_format!(CURRENT_SESSION_STATE_VERSION));
    if window.admits(version) {
        Ok(version)
    } else if version < window.newest() {
        Err(StoreError::SessionStateVersionUnsupported {
            found: version,
            current: window.newest(),
        })
    } else {
        Err(StoreError::SessionStateVersionNewerThanRuntime {
            found: version,
            current: window.newest(),
        })
    }
}
