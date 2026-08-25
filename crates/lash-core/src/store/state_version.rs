use super::StoreError;

/// Oldest session-state generation this runtime can admit.
///
/// Version zero is also the durable meaning of a physically absent marker.
pub const OLDEST_SUPPORTED_SESSION_STATE_VERSION: u32 = 0;

/// Complete mutable-continuation generation emitted and admitted by this runtime.
/// FIG-1901 advances this with the first adjacent converter.
pub const CURRENT_SESSION_STATE_VERSION: u32 = 0;

/// Successful lease-fenced admission of one complete session-state generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStateAdmission {
    pub session_id: String,
    pub version: u32,
    pub lease_fencing_token: u64,
}

/// Interpret an independently read physical marker.
pub fn resolve_session_state_version(marker: Option<u32>) -> Result<u32, StoreError> {
    let version = marker.unwrap_or(OLDEST_SUPPORTED_SESSION_STATE_VERSION);
    if version == CURRENT_SESSION_STATE_VERSION {
        Ok(version)
    } else {
        Err(StoreError::SessionStateVersionNewerThanRuntime {
            found: version,
            current: CURRENT_SESSION_STATE_VERSION,
        })
    }
}
