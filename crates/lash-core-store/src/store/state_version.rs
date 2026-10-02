use super::StoreError;
use crate::SessionId;

/// Oldest session-state generation this runtime can admit.
pub const OLDEST_SUPPORTED_SESSION_STATE_VERSION: u32 = 1;

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
///
/// version_guard(
///     roots(path = "crates/lash-core-store/src/plugin_state.rs", PluginState),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "SessionStateGeneration"
pub const CURRENT_SESSION_STATE_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 3's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "SessionStateGeneration"
pub const CURRENT_SESSION_STATE_VERSION: u32 = 2;

/// Successful drive-fenced admission of one complete session-state generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStateAdmission {
    pub session_id: SessionId,
    pub version: u32,
    pub drive_epoch: u64,
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
///
/// A build's drain generation `G` hashes it, so two builds whose session
/// admission differs never serve the same lane: a child or a successor sent
/// on its opener's lane runs on a build that admits every marker the
/// opener's build admitted.
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
