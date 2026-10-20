//! The single fleet-gate answer carried by cells and workflow import.

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
    async fn writable(&self) -> Result<CellPackages, crate::RuntimeEffectControllerError>;
}
