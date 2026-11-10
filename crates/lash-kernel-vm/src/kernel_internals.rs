//! The machine's implementation, for the other kernel crates only: plans
//! that perturb how a run executes, and the step-structure checks kernel
//! laws pin.
use lash_kernel_state::paged::CheckpointRoot;

use crate::{KernelMachine, PreparedLibrary};

/// `library` preparing code for the `charge` policy: under Off, lowering
/// emits no charge regions, and charged code runs paced.
pub fn in_charge(library: &PreparedLibrary, charge: crate::Charge) -> PreparedLibrary {
    library.in_charge(charge)
}

/// The executable's own check of its statement structure.
pub trait StepStructure {
    /// Checks the ops of every statement the run executes; gives how many
    /// it checked.
    ///
    /// # Errors
    ///
    /// The first malformed range.
    fn verify_stream(&self) -> Result<usize, lash_kernel_exec::KirError>;
}

/// Checks the ops of every statement of `library`'s bodies; gives how many
/// it checked.
///
/// # Errors
///
/// The first malformed range.
pub fn verify_library_stream(
    library: &PreparedLibrary,
) -> Result<usize, lash_kernel_exec::KirError> {
    library.verify_stream()
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

/// The prepared scalar completion formula, for the owning library's
/// independent charge oracle. A missing specialization uses the normal
/// machine formula instead.
pub fn prepared_scalar_charge(
    definition: &lash_kernel_doc::FunctionDefinition,
    mask: u8,
) -> Option<u64> {
    let params = &definition.signature.params;
    let plan = crate::compile::Plan::new(definition.charge()?, params);
    crate::compile::Fast::new(definition.charge()?, params, &plan, true).scalar_units(mask)
}

/// What a region charges a call of `definition` on `args` that returned
/// `result` from the values alone (`Plan::immediate`), the arguments at the
/// positions `fresh` sets being constructions of no members, for the owning
/// library's independent charge oracle; `None` where the region measures the
/// formula in the arena instead.
pub fn immediate_charge(
    definition: &lash_kernel_doc::FunctionDefinition,
    args: &[lash_kernel_doc::Value],
    fresh: u16,
    result: &lash_kernel_doc::Value,
) -> Option<u64> {
    let params = &definition.signature.params;
    let plan = crate::compile::Plan::new(definition.charge()?, params);
    plan.immediate(args, crate::compile::Fresh::of(fresh), result)
}
