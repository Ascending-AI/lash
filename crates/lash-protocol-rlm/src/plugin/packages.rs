//! Which package surface a session's cells are written under (FIG-6918,
//! FIG-6991).
//!
//! Every package set a node installs is stored with its surface snapshot,
//! what running its code requires and all its definitions. The gate
//! answers the surface each cell is written under, and the answer is the
//! cell's one key (P3): the prompt shows that surface, the worker lowers
//! against its names, the cell's saved functions relink to it, and its
//! declared operations judge the cell's runners. A node that does not hold
//! the set's artifacts reads the surface from its stored snapshot and the
//! definitions the store holds for it.

pub use lash_execution_contracts::{CellPackages, PackageGate};

/// What a worker lowering under `packages` is sent: the digest of a held
/// set, or a stored surface whole with the definitions the node lacks.
pub(crate) fn wire(packages: &CellPackages) -> lash_vm_client::service::SurfaceRef {
    lash_vm_client::service::SurfaceRef::of(packages.digest(), packages.stored_wire())
}

/// A gate that always answers one set: a host whose fleet runs one build,
/// or one that stands in for a build of it.
pub(crate) struct OneSet(pub(crate) CellPackages);

#[async_trait::async_trait]
impl PackageGate for OneSet {
    async fn writable(&self) -> Result<CellPackages, lash_core::RuntimeEffectControllerError> {
        Ok(self.0.clone())
    }
}
