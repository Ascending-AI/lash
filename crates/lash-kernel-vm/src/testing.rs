//! Differential execution controls for embedding laws. All strategies keep
//! the same semantic result, effects and exact charge; interpreted execution
//! exercises the stored helper bodies instead of their native shortcuts.
use crate::{
    Bounds, CellStart, KernelMachine, Machine as _, OpenError, PreparedLibrary, Program, StartError,
};
use lash_kernel_state::paged::{Checkpoint, PageSource};

/// The execution paths an embedder can compare without naming VM layout.
/// Every strategy runs the one bytecode core: a start or open takes no
/// strategy, and [`library`] alone reprepares helpers under its layout.
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
}

/// The source ledger's entity bytes, root/control bytes and entity count.
/// The bytecode core keeps no source ledger, so this always refuses.
///
/// # Errors
/// Always: [`HeapFault::Malformed`] naming the missing ledger.
pub fn owner_balance(
    machine: &KernelMachine,
) -> Result<[u64; 3], lash_kernel_doc::heap::HeapFault> {
    let _ = machine;
    Err(lash_kernel_doc::heap::HeapFault::Malformed(
        "the bytecode core keeps no source ledger".into(),
    ))
}

/// Seals and reads the canonical cold equality domain at a legal boundary.
/// The bytecode core seals site-keyed run records, which this reader does
/// not read, so this always refuses.
///
/// # Errors
/// Always: [`crate::SealError::Fault`] naming the format.
pub fn semantic_checkpoint(
    machine: &mut KernelMachine,
) -> Result<lash_kernel_state::semantic::Checkpoint, crate::SealError> {
    let _ = machine;
    Err(crate::SealError::Fault {
        problem: "the bytecode core's checkpoints are run records".into(),
    })
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
    let _ = strategy;
    KernelMachine::start(program, bounds, start, source)
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
    let _ = strategy;
    KernelMachine::open(program, bounds, checkpoint, source)
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
