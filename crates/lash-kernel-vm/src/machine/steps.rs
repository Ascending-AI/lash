//! The step structure: where a statement's step leaves its frame.
//!
//! The prepared form states each statement's [`StepEdge`]: the blocks its
//! step leaves when it completes in its own block, and the Boundary the
//! frame is at then. The machine takes the edge and derives nothing from
//! the frame. [`KernelMachine::verify_step_structure`] has a run also
//! derive, at the end of every statement's step, what that step leaves from
//! the frame itself, as the machine once did at run time, and fault where
//! the two disagree.

use lash_kernel_doc::TaskId;
use lash_kernel_exec::{BoundaryKind, StepEdge};

use super::{Control, Halt, KernelMachine, SlotState};
use crate::compile::{Executable, StmtId};

/// The step-structure check of one run.
#[derive(Debug, Default)]
pub(super) struct StepCheck {
    pub(super) verify: bool,
    /// The statement ends checked so far.
    pub(super) verified: u64,
}

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
        if edge.leaves == 0 && !self.steps.verify {
            return Ok(());
        }
        self.leave_blocks(task, exe, edge)
    }

    #[inline(never)]
    fn leave_blocks(&mut self, task: TaskId, exe: &Executable, edge: StepEdge) -> Result<(), Halt> {
        if self.steps.verify {
            self.verify_leaves(task, exe, edge.leaves)?;
        }
        if edge.leaves > 0 {
            let frame = self.frame(task)?;
            for _ in 0..edge.leaves {
                if let Some(Control::Block { block, .. }) = frame.control.pop() {
                    for at in &exe.block(block).declares {
                        frame.set_slot(*at, SlotState::Empty)?;
                    }
                }
            }
        }
        if self.steps.verify {
            self.verify_boundary(task, exe, edge)?;
        }
        Ok(())
    }

    /// The check where the last statement of a block ended its step
    /// elsewhere: in a block, a loop, a `try` or a frame it entered, or in
    /// a wait. The frame leaves nothing.
    pub(super) fn verify_no_edge(&mut self, task: TaskId, exe: &Executable) -> Result<(), Halt> {
        self.verify_leaves(task, exe, 0)
    }

    fn verify_leaves(&mut self, task: TaskId, exe: &Executable, leaves: u32) -> Result<(), Halt> {
        let derived = self.derived_leaves(task, exe)?;
        if derived != leaves {
            return Err(Halt::Fault(format!(
                "the prepared step leaves {leaves} block(s) where the frame shows {derived}"
            )));
        }
        self.steps.verified += 1;
        Ok(())
    }

    fn verify_boundary(
        &mut self,
        task: TaskId,
        exe: &Executable,
        edge: StepEdge,
    ) -> Result<(), Halt> {
        let Some(Control::Block { block, next }) = self.frame(task)?.control.last() else {
            return Err(Halt::Fault(
                "a statement's step ended in no block".to_string(),
            ));
        };
        let derived = if *next < exe.block(*block).stmts.len() {
            BoundaryKind::StatementStart
        } else {
            BoundaryKind::BlockEnd
        };
        if derived != edge.next {
            return Err(Halt::Fault(format!(
                "the prepared step ends at {:?} where the frame is at {derived:?}",
                edge.next
            )));
        }
        Ok(())
    }

    /// What the step of the statement that has just completed leaves,
    /// derived from the frame: the blocks the statement was the last of,
    /// while each is inside another block of the same frame. A frame that
    /// waits in a statement, or whose block is a loop's or a `try`'s, leaves
    /// nothing.
    fn derived_leaves(&mut self, task: TaskId, exe: &Executable) -> Result<u32, Halt> {
        let Some(frame) = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| super::fault("a running task is missing"))?
            .frames
            .last_mut()?
        else {
            return Ok(0);
        };
        if frame.awaiting.is_some() {
            return Ok(0);
        }
        let mut control = frame.control.as_slice();
        let mut leaves = 0;
        while let [
            rest @ ..,
            Control::Block { .. },
            Control::Block { block, next },
        ] = control
        {
            if *next < exe.block(*block).stmts.len() {
                break;
            }
            leaves += 1;
            control = &control[..rest.len() + 1];
        }
        Ok(leaves)
    }
}
