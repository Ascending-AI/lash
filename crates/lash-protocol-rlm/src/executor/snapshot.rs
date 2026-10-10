use thiserror::Error;

/// Version of the durable code mode snapshot envelope stored behind a session
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
///         cover(CodeModeSnapshotRoot),
///     ),
///     roots(path = "crates/lash-rlm-types/src/lib.rs", CodeModeProjectedSeedEntry),
///     roots(path = "crates/lash-sansio/src/causal.rs", CausalRef),
/// )
// The root is a session's bindings in the kernel's parked-run terms: a
// header and one fragment per binding (`lash-kernel-state`), each fragment
// inline or a content-addressed leaf. The kernel version the header states
// is the kernel's own; this version covers the root that carries it.
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "CodeModeSnapshotEnvelope"
pub const CODEMODE_SNAPSHOT_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 26's shape; its `Lift::Decoder` row admits N's
/// roots, which the canonical decoder reads natively.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "CodeModeSnapshotEnvelope"
pub const CODEMODE_SNAPSHOT_VERSION: u32 = 2;

const CUTOVER_REMEDY: &str = "drain in-flight sessions on the old build before deploying this build, or recreate development/test stores";

/// Why a saved code mode session could not be restored. No bindings are adopted
/// when any check fails: fragments can share objects across bindings.
///
/// [`lash_core::SessionError::ExecutionStateRestore`] retains this error as
/// its source, which hosts can downcast to inspect the failed check.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodeModeSnapshotError {
    /// The root cannot decode as this build's snapshot envelope.
    #[error("code mode snapshot root is not this build's: {details}; {CUTOVER_REMEDY}")]
    FormatMismatch { details: String },
    /// The snapshot envelope version is outside the reader's window.
    #[error(
        "code mode snapshot version {found} is incompatible with version {expected}; {CUTOVER_REMEDY}"
    )]
    VersionMismatch { expected: u32, found: u32 },
    /// The recorded dialect differs from the session's dialect.
    #[error(
        "code mode snapshot was recorded by a `{found}` session; this session's dialect is `{expected}`"
    )]
    DialectMismatch { expected: String, found: String },
    /// A binding's content-addressed fragment was not supplied.
    #[error(
        "code mode snapshot binding `{logical_key}` references missing leaf component `{component:?}`"
    )]
    MissingLeaf {
        logical_key: String,
        component: lash_core::plugin::ExecutionLeafName,
    },
    /// A binding's fragment bytes do not match their content address.
    #[error(
        "code mode snapshot binding `{logical_key}` references leaf component `{component:?}` whose content address is `{actual_component:?}`"
    )]
    LeafHashMismatch {
        logical_key: String,
        component: lash_core::plugin::ExecutionLeafName,
        actual_component: lash_core::plugin::ExecutionLeafName,
    },
    /// The supplied leaf set differs from the set the root references.
    #[error(
        "code mode snapshot root/leaf set is inconsistent; missing={missing:?}, unexpected={unexpected:?}"
    )]
    LeafSetMismatch {
        missing: Vec<lash_core::plugin::ExecutionLeafName>,
        unexpected: Vec<lash_core::plugin::ExecutionLeafName>,
    },
    /// The kernel refuses the decoded fragments as stored state.
    #[error("code mode session bindings are not a kernel state this build reads: {0}")]
    Kernel(#[from] lash_kernel_state::LoadError),
}

impl From<CodeModeSnapshotError> for lash_core::SessionError {
    fn from(error: CodeModeSnapshotError) -> Self {
        Self::ExecutionStateRestore {
            source: Box::new(error),
        }
    }
}
