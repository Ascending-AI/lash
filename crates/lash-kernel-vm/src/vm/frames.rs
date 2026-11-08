//! Frames and the operand stack (cutover contract §1, §5).
//!
//! A task's frames share one value stack: a frame's locals start at its
//! `base`, its operands above them. Persisted state never names a pc or a
//! stack position: the resume table maps a frame's site and phase to them
//! (contract §5).

use lash_kernel_doc::arena::ArenaHandle;

use crate::bytecode::program::UnitId;

/// One activation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub unit: UnitId,
    /// The next op to run.
    pub pc: u32,
    /// Where the frame's locals start on the stack.
    pub base: u32,
    /// The closure the frame runs, whose captures it reads.
    pub closure: Option<ArenaHandle>,
    /// Whether the frame runs inside a covered library extent
    /// (`K-CHG-007`): its own fixed events bill nothing.
    pub covered: bool,
}
