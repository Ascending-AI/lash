//! Which helper release a session's cells are written against (FIG-5799).
//!
//! A build holds the functions of the helper releases it retains beside its
//! own, and a node of a build that holds only an earlier release cannot run
//! a function a later release added. So while such a node is live, a node
//! of the later build writes the newest release every live node holds: the
//! fleet-format gate (ADR 0106 §2), asked as each cell is lowered. A host
//! whose fleet runs one build installs no gate, and its cells are written
//! against the build's own release.

/// The helper release a cell lowered now is written against.
#[async_trait::async_trait]
pub trait HelperReleaseGate: Send + Sync {
    /// The newest helper release every live node serving these sessions holds.
    /// A typed retryable error defers lowering when none is fleet-readable or
    /// when the live capability survey is unavailable.
    async fn writable(&self) -> Result<u32, lash_core::RuntimeEffectControllerError>;
}

/// A gate that always answers one release: a host that stands in for a
/// build of it.
#[cfg(feature = "synthetic-next")]
pub(crate) struct WritingRelease(pub(crate) u32);

#[cfg(feature = "synthetic-next")]
#[async_trait::async_trait]
impl HelperReleaseGate for WritingRelease {
    async fn writable(&self) -> Result<u32, lash_core::RuntimeEffectControllerError> {
        Ok(self.0)
    }
}
