//! Which package set a session's cells are written under (FIG-6918, S1-F1).
//!
//! A node holds its own package set and the retained sets whose artifacts
//! it still holds. While a live node serving these sessions lacks a set, a
//! cell written under it could not run there, so the writer lowers each
//! cell under the first candidate every live serving node holds: its own
//! set first, then the others newest first (FLEET-SET). The answer is the
//! cell's one key: the worker lowers against that set's names, the cell's
//! saved functions relink to it, and an actor whose state pins the cell's
//! code is stamped with it.

use lash_kernel_doc::PackageSetDigest;

/// The package set a cell is written under: the gate's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellPackages(pub PackageSetDigest);

impl CellPackages {
    /// The set's digest.
    #[must_use]
    pub fn digest(&self) -> PackageSetDigest {
        self.0
    }
}

/// The package set a cell lowered now is written under.
#[async_trait::async_trait]
pub trait PackageGate: Send + Sync {
    /// The first candidate set every live node serving these sessions
    /// holds. A typed retryable error defers lowering when none is held by
    /// every serving node or when the live capability survey is
    /// unavailable.
    async fn writable(&self) -> Result<CellPackages, lash_core::RuntimeEffectControllerError>;
}

/// A gate that always answers one set: a host whose fleet runs one build,
/// or one that stands in for a build of it.
pub(crate) struct OneSet(pub(crate) CellPackages);

#[async_trait::async_trait]
impl PackageGate for OneSet {
    async fn writable(&self) -> Result<CellPackages, lash_core::RuntimeEffectControllerError> {
        Ok(self.0)
    }
}
