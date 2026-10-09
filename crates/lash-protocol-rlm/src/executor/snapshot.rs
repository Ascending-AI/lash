use thiserror::Error;

/// Version of the durable RLM snapshot envelope stored behind a session
/// checkpoint component.
///
/// Re-exported by the facade's `formats` manifest so a host can read it before
/// wiring a store; the history below is why each boundary is a version rather
/// than a decode failure.
///
///
/// version_guard(
///     shapes(
///         path = "crates/lash-protocol-rlm/src/executor/session.rs",
///         path = "crates/lash-protocol-rlm/src/deferred.rs",
///         cover(RlmSnapshotRoot),
///     ),
///     roots(path = "crates/lash-rlm-types/src/lib.rs", RlmProjectedSeedEntry),
///     roots(path = "crates/lash-sansio/src/causal.rs", CausalRef),
/// )
// The root is a session's bindings in the kernel's parked-run terms: a
// header and one fragment per binding (`lash-kernel-state`), each fragment
// inline or a content-addressed leaf. The kernel version the header states
// is the kernel's own; this version covers the root that carries it.
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
    #[error("RLM snapshot root is not this build's: {details}; {CUTOVER_REMEDY}")]
    FormatMismatch { details: String },
    #[error(
        "RLM snapshot version {found} is incompatible with version {expected}; {CUTOVER_REMEDY}"
    )]
    VersionMismatch { expected: u32, found: u32 },
    #[error(
        "RLM snapshot was recorded by a `{found}` session; this session's dialect is `{expected}`"
    )]
    DialectMismatch { expected: String, found: String },
    #[error(
        "RLM snapshot binding `{logical_key}` references missing leaf component `{component:?}`"
    )]
    MissingLeaf {
        logical_key: String,
        component: lash_core::plugin::ExecutionLeafName,
    },
    #[error(
        "RLM snapshot binding `{logical_key}` references leaf component `{component:?}` whose content address is `{actual_component:?}`"
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
    #[error("RLM session bindings are not a kernel state this build reads: {0}")]
    Kernel(#[from] lash_kernel_state::LoadError),
}

impl From<RlmSnapshotError> for lash_core::SessionError {
    fn from(error: RlmSnapshotError) -> Self {
        Self::Protocol(error.to_string())
    }
}
