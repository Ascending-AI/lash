//! The step structure: where a statement's step leaves its frame.
//!
//! The prepared form states each statement's [`StepEdge`]: the blocks its
//! step leaves when it completes in its own block, and the Boundary the
//! frame is at then. The machine takes the edge and derives nothing from
//! the frame.

use lash_kernel_doc::TaskId;
use lash_kernel_exec::StepEdge;

use super::{Control, Halt, KernelMachine, SlotState};
use crate::compile::{Executable, StmtId};

impl KernelMachine {
    /// Takes the step edge of statement `stmt`, which has completed in its
    /// own block.
    pub(super) fn take_edge_of(
        &mut self,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
    ) -> Result<(), Halt> {
        self.take_edge(task, exe, exe.statement(stmt).2)
    }

    /// Takes `edge`, the step edge of a statement that has completed in its
    /// own block: leaves the blocks the edge says, ending their variables.
    #[inline]
    pub(super) fn take_edge(
        &mut self,
        task: TaskId,
        exe: &Executable,
        edge: StepEdge,
    ) -> Result<(), Halt> {
        if edge.leaves == 0 {
            return Ok(());
        }
        self.leave_blocks(task, exe, edge)
    }

    #[inline(never)]
    fn leave_blocks(&mut self, task: TaskId, exe: &Executable, edge: StepEdge) -> Result<(), Halt> {
        for _ in 0..edge.leaves {
            if self.publish_source_scope_leave(task, exe)? {
                continue;
            }
            let frame = self.frame(task)?;
            if let Some(Control::Block { block, .. }) = frame.control.pop() {
                for at in &exe.block(block).declares {
                    frame.set_slot(*at, SlotState::Empty);
                }
            }
        }
        Ok(())
    }
}
