//! Differential execution controls for embedding laws. Both strategies keep
//! the same semantic result, effects and exact charge; interpreted execution
//! exercises the stored helper bodies instead of their native shortcuts.
use crate::{Bounds, CellStart, KernelMachine, OpenError, PreparedLibrary, Program, StartError};
use lash_kernel_state::paged::{Checkpoint, PageSource};

/// The two execution paths an embedder can compare without naming VM layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionStrategy {
    #[default]
    Optimized,
    Interpreted,
}

impl ExecutionStrategy {
    fn layout(self) -> crate::compile::Layout {
        crate::compile::Layout(match self {
            Self::Optimized => 0,
            Self::Interpreted => 3,
        })
    }
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
}
