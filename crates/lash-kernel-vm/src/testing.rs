//! Differential execution controls for embedding laws. All strategies keep
//! the same semantic result, effects and exact charge; interpreted execution
//! exercises the stored helper bodies instead of their native shortcuts.
use crate::{Bounds, CellStart, KernelMachine, OpenError, PreparedLibrary, Program, StartError};
use lash_kernel_state::paged::{Checkpoint, PageSource};

/// The execution paths an embedder can compare without naming VM layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionStrategy {
    #[default]
    Optimized,
    /// The canonical source evaluator with packet entry disabled.
    Source,
    /// The shared packet executor with all grouped kernels disabled.
    GenericPackets,
    /// Helper bodies with their native shortcuts disabled.
    Interpreted,
}

impl ExecutionStrategy {
    fn layout(self) -> crate::compile::Layout {
        crate::compile::Layout(match self {
            Self::Optimized | Self::Source | Self::GenericPackets => 0,
            Self::Interpreted => 3,
        })
    }

    fn select(self, mut machine: KernelMachine) -> KernelMachine {
        match self {
            Self::Source => machine.force_source_execution(),
            Self::GenericPackets => machine.force_generic_packets(),
            Self::Optimized | Self::Interpreted => {}
        }
        machine
    }
}

pub use crate::machine::islands::witness::{Counts as FamilyCounts, Exit as FamilyExit, Family};

/// Starts collecting actual grouped-kernel commits and exits on this thread.
pub fn begin_family_witnesses() {
    crate::machine::islands::witness::begin();
}

/// Takes the grouped-kernel events since the last collection began.
pub fn take_family_witnesses() -> FamilyCounts {
    crate::machine::islands::witness::take()
}

/// Takes the dispatched packet and specialized packet counts for this machine.
pub fn take_packet_counts(machine: &mut KernelMachine) -> (u64, u64) {
    machine.take_packet_counts()
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
