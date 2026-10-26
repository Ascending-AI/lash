//! The machine's implementation, for the other kernel crates only: plans
//! that perturb how a run executes, and the step-structure checks kernel
//! laws pin.
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_state::paged::{CheckpointRoot, PageSource};
use std::sync::Arc;

pub use crate::compile::Layout;
pub use crate::observe::RegisterWork;
use crate::{CellStart, KernelMachine, OpenError, PreparedLibrary, Program, StartError};

/// Prepares `registry` under `layout`; library bodies compile on first use.
pub fn prepare_with_layout(registry: Arc<FunctionRegistry>, layout: Layout) -> PreparedLibrary {
    PreparedLibrary::with_layout(registry, layout)
}

/// Reprepare an installed library under another layout, preserving its helper image.
pub fn in_layout(library: &PreparedLibrary, layout: Layout) -> PreparedLibrary {
    library.in_layout(layout)
}

/// `library` preparing code for the `charge` policy: under Off, lowering
/// emits no charge regions, and charged code runs paced.
pub fn in_charge(library: &PreparedLibrary, charge: crate::Charge) -> PreparedLibrary {
    library.in_charge(charge)
}

/// Starts a run whose program executes in `layout`.
///
/// # Errors
///
/// [`crate::Machine::start`]'s.
pub fn start_with_layout(
    program: Program,
    bounds: crate::Bounds,
    start: CellStart,
    layout: Layout,
    source: Box<dyn PageSource>,
) -> Result<KernelMachine, StartError> {
    KernelMachine::start_with_layout(program, bounds, start, layout, source)
}

/// Opens a root whose program executes in `layout`.
///
/// # Errors
///
/// [`crate::Machine::open`]'s.
pub fn open_with_layout(
    program: Program,
    bounds: crate::Bounds,
    root: &CheckpointRoot,
    source: Box<dyn PageSource>,
    layout: Layout,
) -> Result<KernelMachine, OpenError> {
    KernelMachine::open_with_layout(program, bounds, root, source, layout)
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

impl StepStructure for KernelMachine {
    fn verify_stream(&self) -> Result<usize, lash_kernel_exec::KirError> {
        KernelMachine::verify_stream(self)
    }
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

/// Task identity resolution for the kernel's persisted-state laws.
pub fn task_identity(
    machine: &KernelMachine,
    identity: lash_kernel_state::paged::TaskIdentityRef,
) -> Result<lash_kernel_value::sites::TaskIdentity, lash_kernel_doc::heap::HeapFault> {
    machine.task_identity(identity)
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
/// `result` from the values alone (`Fast::immediate`), the arguments at the
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
    crate::compile::Fast::new(definition.charge()?, params, &plan, false).immediate(
        args,
        crate::compile::Fresh::of(fresh),
        result,
    )
}

/// How many call islands lowering forms for `program`'s document under its
/// library's layout (`docs/design/kernel-regions.md` §5 R4): what the
/// regions law checks its island-shaped programs form, and a layout
/// without regions does not.
///
/// # Errors
///
/// [`crate::Machine::start`]'s admission refusals.
pub fn call_islands(program: &Program) -> Result<usize, StartError> {
    let admitted = program.library.admit(
        &program.document,
        lash_kernel_value::data_limits::KernelDataLimits::default(),
        &program.selection,
    )?;
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .map_err(|missing| StartError::MissingFunction {
            function: missing.0,
        })?;
    Ok(exe.islands.plans.len())
}

/// The work the register program committed while `tape` recorded: the
/// calls islands entered and the typed runs scalar loops ran whole.
pub fn register_work(tape: &crate::ChargeTape) -> RegisterWork {
    tape.work()
}
