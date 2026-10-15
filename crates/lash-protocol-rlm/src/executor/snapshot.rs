use thiserror::Error;

/// A session's descriptor page: the dialect, number policy, kernel and
/// cell count the session was recorded under, the carried `SessionRoot`
/// (heap, bindings and functions as page roots) and the names it binds.
/// The page carries no version of its own; this constant names the surface
/// for the formats manifest.
///
/// version_guard(
///     items(
///         path = "crates/lash-protocol-rlm/src/executor/session/graph.rs",
///         SessionDescriptor, encode, decode, seal, read,
///     ),
///     items(path = "crates/lash-kernel-state/src/paged/source.rs", PAGE_FORMAT),
/// )
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

/// Why a saved code mode session could not be restored. No bindings are
/// adopted when any check fails.
///
/// [`lash_core::SessionError::ExecutionStateRestore`] retains this error as
/// its source, which hosts can downcast to inspect the failed check.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SessionOpenError {
    /// The recorded dialect differs from the session's dialect.
    #[error(
        "code mode session was recorded by a `{found}` session; this session's dialect is `{expected}`"
    )]
    Dialect { expected: String, found: String },
    /// The recorded number policy differs from the session's.
    #[error(
        "code mode session decodes numbers by {found:?}; this session decodes them by {expected:?}"
    )]
    Numbers {
        expected: lash_kernel_doc::NumberPolicy,
        found: lash_kernel_doc::NumberPolicy,
    },
    /// The state was written by a kernel version this build does not run.
    #[error("code mode session was written by kernel {found}; this build runs kernel {expected}")]
    Kernel { expected: u32, found: u32 },
    /// The descriptor page is not one a session writes.
    #[error("code mode session descriptor is refused: {0}")]
    Heap(#[from] lash_kernel_doc::heap::HeapFault),
}

impl From<SessionOpenError> for lash_core::SessionError {
    fn from(error: SessionOpenError) -> Self {
        Self::ExecutionStateRestore {
            source: Box::new(error),
        }
    }
}
