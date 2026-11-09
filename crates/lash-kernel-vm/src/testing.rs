//! Differential execution controls for embedding laws. All strategies keep
//! the same semantic result, effects and exact charge; interpreted execution
//! exercises the stored helper bodies instead of their native shortcuts.
use crate::{Bounds, CellStart, KernelMachine, OpenError, PreparedLibrary, Program, StartError};
use lash_kernel_state::paged::{Checkpoint, PageSource};

pub mod region_isa;

/// The execution paths an embedder can compare without naming VM layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionStrategy {
    #[default]
    Optimized,
    /// The independent canonical source evaluator.
    Source,
    /// The region executor with specialized kernels disabled.
    GenericRegions,
    /// Helper bodies with their native shortcuts disabled.
    Interpreted,
}

impl ExecutionStrategy {
    fn layout(self) -> crate::compile::Layout {
        crate::compile::Layout(match self {
            Self::Optimized | Self::Source | Self::GenericRegions => 0,
            Self::Interpreted => 3,
        })
    }

    fn select(self, mut machine: KernelMachine) -> KernelMachine {
        match self {
            Self::Source => machine.force_source_execution(),
            Self::GenericRegions => machine.force_generic_regions(),
            Self::Optimized | Self::Interpreted => {}
        }
        machine
    }
}

/// The source ledger's entity bytes, root/control bytes and entity count.
/// Physical residency and saved capacity floors do not participate.
///
/// # Errors
/// The source ledger could not read its authenticated balance.
pub fn owner_balance(
    machine: &KernelMachine,
) -> Result<[u64; 3], lash_kernel_doc::heap::HeapFault> {
    let balance = machine.logical_balance()?;
    Ok([
        balance.entity_local_bytes,
        balance.root_and_control_bytes,
        balance.entities,
    ])
}

/// Seals and reads the canonical cold equality domain at a legal boundary.
///
/// # Errors
/// The seal or semantic reader could not read the complete checkpoint.
pub fn semantic_checkpoint(
    machine: &mut KernelMachine,
) -> Result<lash_kernel_state::semantic::Checkpoint, crate::SealError> {
    machine.try_semantic_checkpoint()
}

/// Prepares the same installed definitions under the selected strategy.
pub fn library(library: &PreparedLibrary, strategy: ExecutionStrategy) -> PreparedLibrary {
    library.in_layout(strategy.layout())
}

/// Starts a law's run under the selected strategy.
///
/// # Errors
/// The embedding API's normal admission refusals.
pub fn start(
    program: Program,
    bounds: Bounds,
    start: CellStart,
    strategy: ExecutionStrategy,
    source: Box<dyn PageSource>,
) -> Result<KernelMachine, StartError> {
    KernelMachine::start_with_layout(program, bounds, start, strategy.layout(), source)
        .map(|machine| strategy.select(machine))
}

/// Reopens an opaque parked run under the selected strategy.
///
/// # Errors
/// The embedding API's normal checkpoint refusals.
pub fn open(
    program: Program,
    bounds: Bounds,
    checkpoint: &Checkpoint,
    source: Box<dyn PageSource>,
    strategy: ExecutionStrategy,
) -> Result<KernelMachine, OpenError> {
    KernelMachine::open_with_layout(
        program,
        bounds,
        lash_kernel_state::kernel_internals::checkpoint_root(checkpoint),
        source,
        strategy.layout(),
    )
    .map(|machine| strategy.select(machine))
}

/// A compiled and validated bytecode program, without a running machine.
pub struct BytecodeProgram {
    _code: crate::bytecode::program::Program,
}

/// Compiles every unit reached by this document and validates the bytecode.
/// No session, heap, host operations or machine start is required.
///
/// # Errors
/// Returns a typed start refusal for an unsupported document or malformed code.
pub fn compile_document(
    document: &lash_kernel_doc::Document,
    registry: &lash_kernel_doc::FunctionRegistry,
    link: lash_kernel_value::LinkId,
    entry: Option<&lash_kernel_doc::FunctionExport>,
) -> Result<BytecodeProgram, StartError> {
    let mut code = crate::bytecode::conventions::compile_selected(
        document,
        registry,
        link,
        &[],
        entry.map(|export| export.function),
        entry.map(|export| &export.convention),
        None,
    )
    .map_err(|problem| StartError::Linked {
        problem: problem.to_string(),
    })?;
    crate::bytecode::inline::inline(&mut code);
    crate::bytecode::validate::validate(&code).map_err(|problem| StartError::Linked {
        problem: problem.to_string(),
    })?;
    Ok(BytecodeProgram { _code: code })
}
