use lashlang::State as FlowState;
use thiserror::Error;

// v6 cuts persisted Lashlang state over from marker-based JSON to canonical
// typed MessagePack. Older snapshots are rejected, never compatibility-decoded.
pub(super) const RLM_SNAPSHOT_VERSION: u32 = 6;

const CUTOVER_REMEDY: &str = "drain in-flight sessions on the old build before deploying this build, or recreate development/test stores";

#[derive(Debug, Error)]
pub(crate) enum RlmSnapshotError {
    #[error("RLM snapshot envelope exceeds the maximum MessagePack nesting depth of {limit}")]
    EnvelopeDepthLimitExceeded { limit: usize },
    #[error("non-canonical RLM snapshot envelope at `{location}`: {reason}")]
    NonCanonicalEnvelope { location: String, reason: String },
    #[error(
        "RLM snapshot format is incompatible with canonical typed MessagePack: {details}; {CUTOVER_REMEDY}"
    )]
    FormatMismatch { details: String },
    #[error(
        "RLM snapshot version {found} is incompatible with version {expected}; {CUTOVER_REMEDY}"
    )]
    VersionMismatch { expected: u32, found: u32 },
    #[error("RLM snapshot engine `{found}` is unsupported; expected `lashlang`")]
    EngineMismatch { found: String },
    #[error("RLM canonical Lashlang snapshot is invalid: {0}")]
    Lashlang(#[from] lashlang::SnapshotDecodeError),
}

pub(super) fn snapshot_runtime(rlm: &FlowState) -> Result<Vec<u8>, lashlang::ContinuationError> {
    rlm.snapshot().to_canonical_bytes()
}

pub(super) fn restore_runtime(data: &[u8]) -> Result<FlowState, RlmSnapshotError> {
    let snapshot = lashlang::Snapshot::from_canonical_bytes(data)?;
    Ok(FlowState::from_snapshot(snapshot))
}
