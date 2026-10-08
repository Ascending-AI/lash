use thiserror::Error;

#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "RlmSnapshotEnvelope"
pub const RLM_SNAPSHOT_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 26's shape; its `Lift::Decoder` row admits N's
/// roots, which the canonical decoder reads natively.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "RlmSnapshotEnvelope"
pub const RLM_SNAPSHOT_VERSION: u32 = 2;

const CUTOVER_REMEDY: &str = "drain in-flight sessions on the old build before deploying this build, or recreate development/test stores";

#[derive(Debug, Error)]
pub(crate) enum RlmSnapshotError {
    #[error("worker snapshot service is unavailable: {0}")]
    WorkerUnavailable(lash_vm_client::PoolError),
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
    #[error("RLM snapshot engine `{found}` is unsupported; expected `{expected}`")]
    EngineMismatch { expected: String, found: String },
    #[error(
        "RLM snapshot logical key `{logical_key}` references missing leaf component `{component}`"
    )]
    MissingLeaf {
        logical_key: String,
        component: lash_core::plugin::ExecutionLeafName,
    },
    #[error(
        "RLM snapshot logical key `{logical_key}` references leaf component `{component}` whose content address is `{actual_component}`"
    )]
    LeafHashMismatch {
        logical_key: String,
        component: lash_core::plugin::ExecutionLeafName,
        actual_component: lash_core::plugin::ExecutionLeafName,
    },
    #[error(
        "RLM snapshot root/leaf set is inconsistent; missing={missing:?}, unexpected={unexpected:?}"
    )]
    LeafSetMismatch {
        missing: Vec<lash_core::plugin::ExecutionLeafName>,
        unexpected: Vec<lash_core::plugin::ExecutionLeafName>,
    },
    #[error("RLM canonical Lashlang snapshot is invalid: {0}")]
    Lashlang(#[from] lashlang::SnapshotDecodeError),
}

impl From<RlmSnapshotError> for lash_core::SessionError {
    fn from(error: RlmSnapshotError) -> Self {
        match error {
            RlmSnapshotError::WorkerUnavailable(error) => {
                Self::Plugin(lash_core::PluginError::Runtime(error.into_runtime_error()))
            }
            error => Self::Protocol(error.to_string()),
        }
    }
}
