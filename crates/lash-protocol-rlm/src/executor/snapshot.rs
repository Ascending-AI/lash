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
///         path = "crates/lash-protocol-rlm/src/executor/session/graph.rs",
///         path = "crates/lash-protocol-rlm/src/deferred.rs",
///         cover(SessionDescriptor),
///     ),
///     roots(path = "crates/lash-rlm-types/src/lib.rs", CodeModeProjectedSeedEntry),
///     roots(path = "crates/lash-sansio/src/causal.rs", CausalRef),
/// )
// The root is a session's descriptor page (FIG-6193): the session's heap,
// bindings and saved functions as page roots, with the names it binds. The
// page carries no format version of its own; this constant names the
// surface for the formats manifest.
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

/// Why a saved code mode session could not be restored. No bindings are
/// adopted when any check fails.
///
/// [`lash_core::SessionError::ExecutionStateRestore`] retains this error as
/// its source, which hosts can downcast to inspect the failed check.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodeModeSnapshotError {
    /// The graph is well formed, but its bindings, objects or alias table
    /// are not a session's.
    #[error("code mode snapshot root is not this build's: {details}; {CUTOVER_REMEDY}")]
    FormatMismatch { details: String },
    /// The recorded dialect differs from the session's dialect.
    #[error(
        "code mode snapshot was recorded by a `{found}` session; this session's dialect is `{expected}`"
    )]
    DialectMismatch { expected: String, found: String },
    /// The recorded number policy differs from the session's.
    #[error(
        "code mode snapshot decodes numbers by {found:?}; this session decodes them by {expected:?}"
    )]
    NumberPolicyMismatch {
        expected: lash_kernel_doc::NumberPolicy,
        found: lash_kernel_doc::NumberPolicy,
    },
    /// A node of the state's graph is missing, malformed, of the wrong kind
    /// or not the node its name names.
    #[error("code mode snapshot graph is refused: {0}")]
    Graph(#[from] lash_core_store::checkpoint_graph::GraphRefusal),
    /// The state was written by a kernel version this build does not run.
    #[error("code mode snapshot was written by kernel {found}; this build runs kernel {expected}")]
    KernelMismatch { expected: u32, found: u32 },
    /// The descriptor page is not one a session writes.
    #[error("code mode snapshot descriptor is refused: {0}")]
    Descriptor(lash_kernel_doc::heap::HeapFault),
}

impl From<CodeModeSnapshotError> for lash_core::SessionError {
    fn from(error: CodeModeSnapshotError) -> Self {
        Self::ExecutionStateRestore {
            source: Box::new(error),
        }
    }
}
