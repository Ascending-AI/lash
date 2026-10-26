use super::StoreError;
use lash_kernel_value::identity::SessionId;

/// Oldest session-state generation this runtime can admit.
pub const OLDEST_SUPPORTED_SESSION_STATE_VERSION: u32 = 1;

/// Newest mutable session-state generation this build can read.
/// Ordinary builds use the initial 1.0 baseline, version 1. Writers use the
/// version selected by the recorded fleet format; admission checks its read
/// window before guarded payload decoding (ADR 0077). A marker outside that
/// window is refused by [`resolve_session_state_version`].
///
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "SessionStateGeneration"
pub const CURRENT_SESSION_STATE_VERSION: u32 = 1;

/// The synthetic-next upgrade proof (ADR 0115 §6) uses version 2 with the
/// baseline's shape; its registered lift reads what version 1 wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "SessionStateGeneration"
pub const CURRENT_SESSION_STATE_VERSION: u32 = 2;

/// Successful admission of one complete session-state generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStateAdmission {
    pub session_id: SessionId,
    pub version: u32,
}

/// The session-state generations one build's admission gate can admit, as a
/// static descriptor (FIG-4454).
///
/// The gate ([`resolve_session_state_version`]) admits a marker inside the
/// build's supported range `[oldest, newest]`, or equal to the version the
/// recorded `F` pins the surface's writers to. The range is the build's own,
/// and every pin the recorded `F` could select is in the build's pin table,
/// so the descriptor reads no store: the live fleet row only chooses among
/// [`pins`](Self::pins), it never adds one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAdmissionWindow {
    /// The oldest marker the build's upcaster chain lifts from.
    pub oldest: u32,
    /// The newest marker the build knows: [`CURRENT_SESSION_STATE_VERSION`].
    pub newest: u32,
    /// Every writer pin the build's table holds for the session-state
    /// surface, by fleet generation.
    pub pins: Vec<super::fleet_format::WriterPin>,
}

impl SessionAdmissionWindow {
    /// This build's descriptor.
    pub fn of_this_build() -> Self {
        let fleet = super::FleetFormat::current();
        let surface = crate::surface_format!(CURRENT_SESSION_STATE_VERSION);
        let window = fleet.read_window(surface);
        Self {
            oldest: window.oldest(),
            newest: window.newest(),
            pins: fleet.writer_pins(surface),
        }
    }
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
