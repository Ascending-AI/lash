//! The machine's implementation, for the other kernel crates only: plans
//! that perturb how a run executes, and the step-structure checks kernel
//! laws pin.
use lash_kernel_state::paged::CheckpointRoot;

use crate::{KernelMachine, PreparedLibrary};

/// The per-unit reference charger every charge tape equals (`K-CHG-002`).
pub use crate::observe::{Ending, ReferenceCharger};

/// `library` prepared for the `charge` policy.
pub fn in_charge(library: &PreparedLibrary, charge: crate::Charge) -> PreparedLibrary {
    library.in_charge(charge)
}

/// The projected records inspected by kernel-owned format laws.
pub fn sealed_root(sealed: &crate::SealedRoot) -> &CheckpointRoot {
    &sealed.root
}

/// A task's identity, as the frozen engine's paged task records named it.
/// The bytecode core keeps run records instead, so every reference refuses.
pub fn task_identity(
    _machine: &KernelMachine,
    _identity: lash_kernel_state::paged::TaskIdentityRef,
) -> Result<lash_kernel_value::sites::TaskIdentity, lash_kernel_doc::heap::HeapFault> {
    Err(lash_kernel_doc::heap::HeapFault::Malformed(
        "the bytecode core keeps no paged task identities".to_owned(),
    ))
}
