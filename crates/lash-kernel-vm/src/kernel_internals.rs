//! The machine's implementation, for the other kernel crates only: plans
//! that perturb how a run executes, and the step-structure checks kernel
//! laws pin.
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_state::paged::{CheckpointRoot, PageSource};
use std::sync::Arc;

pub use crate::compile::Layout;
pub use crate::observe::RegionWork;
use crate::{CellStart, KernelMachine, OpenError, PreparedLibrary, Program, StartError};

/// Prepares `registry` under `layout`; library bodies compile on first use.
pub fn prepare_with_layout(registry: Arc<FunctionRegistry>, layout: Layout) -> PreparedLibrary {
    PreparedLibrary::with_layout(registry, layout)
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

/// The executable's own checks of its statement and step structure.
pub trait StepStructure {
    /// Has the run check, at the end of every statement's step, the step
    /// structure the executable states against the one its frame shows.
    fn verify_step_structure(&mut self);
    /// Checks the ops of every statement the run executes; gives how many
    /// it checked.
    ///
    /// # Errors
    ///
    /// The first malformed range.
    fn verify_stream(&self) -> Result<usize, lash_kernel_exec::KirError>;
    /// How many statement ends the step-structure check has passed.
    fn verified_steps(&self) -> u64;
}

impl StepStructure for KernelMachine {
    fn verify_step_structure(&mut self) {
        KernelMachine::verify_step_structure(self);
    }
    fn verify_stream(&self) -> Result<usize, lash_kernel_exec::KirError> {
        KernelMachine::verify_stream(self)
    }
    fn verified_steps(&self) -> u64 {
        KernelMachine::verified_steps(self)
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

/// Exact semantic equality for the kernel conformance laws.
pub mod conformance {
    pub use crate::conformance::*;
}

/// The projected records inspected by kernel-owned format laws.
pub fn sealed_root(sealed: &crate::SealedRoot) -> &CheckpointRoot {
    &sealed.root
}

/// Task identity resolution for the kernel's persisted-state laws.
pub fn task_identity(
    machine: &KernelMachine,
    identity: lash_kernel_state::paged::TaskIdentityRef,
) -> Result<lash_vocabulary::sites::TaskIdentity, lash_kernel_doc::heap::HeapFault> {
    machine.task_identity(identity)
}

/// How many installed segments `library` runs, and each one whose callback
/// sites its body does not state, with why: that function runs its body
/// (FIG-6709).
pub fn segments(library: &PreparedLibrary) -> (usize, Vec<String>) {
    library.segments()
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

/// How many loop regions lowering forms for `program`'s document under its
/// library's layout (`docs/design/kernel-regions.md` §1): what the regions
/// law checks its region-shaped programs form, and a layout without
/// regions does not.
///
/// # Errors
///
/// [`crate::Machine::start`]'s admission refusals.
pub fn loop_regions(program: &Program) -> Result<usize, StartError> {
    let admitted = program.library.admit(
        &program.document,
        lash_vocabulary::data_limits::KernelDataLimits::default(),
    )?;
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.instances,
        )
        .map_err(|missing| StartError::MissingFunction {
            function: missing.0,
        })?;
    Ok(exe.regions.len())
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
        lash_vocabulary::data_limits::KernelDataLimits::default(),
    )?;
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.instances,
        )
        .map_err(|missing| StartError::MissingFunction {
            function: missing.0,
        })?;
    Ok(exe.islands.plans.len())
}

/// The work the regions tier committed while `tape` recorded: the loop
/// iterations regions ran to their end and the calls islands entered. The regions law requires it nonzero where regions form, so a
/// run that leaves everything to the reference cannot pass as agreement.
pub fn region_work(tape: &crate::ChargeTape) -> RegionWork {
    tape.work()
}

/// The most a plan holds beyond what the reference holds at the same
/// point (`docs/design/kernel-regions.md` §3.5): a loop region's nursery,
/// or an island's transient holdings and its virtual frames. Where one
/// plan refuses memory and the other runs on, their held bytes there
/// differ by no more.
pub fn transient_hold() -> u64 {
    crate::machine::transient_hold()
}
