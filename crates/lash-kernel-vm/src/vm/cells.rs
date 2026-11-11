//! Cell and capture reads and writes (`K-CLO-001`): they keep the running
//! frame, so the dispatch loop runs them without reading it again.

use super::{Run, Vm, fault};
use crate::bytecode::op::Op;
use crate::heap::Heap;

impl<H: Heap> Vm<H> {
    /// Runs cell or capture op `op` over the running frame. A write's
    /// value leaves the stack once the write commits: a cold cell runs
    /// the op again with its operand still in place.
    #[inline(never)]
    pub(super) fn cell_op(&mut self, op: Op) -> Run<()> {
        match op {
            Op::CellGet(n) => {
                let cell = self.cell_at(n)?;
                let value = self
                    .heap
                    .cell_get(cell)
                    .map_err(|error| self.native(error))?;
                self.push(value);
            }
            Op::CaptureGet(n) => {
                let cell = self.capture(n)?;
                let value = self
                    .heap
                    .cell_get(cell)
                    .map_err(|error| self.native(error))?;
                self.push(value);
            }
            Op::CellSet(n) | Op::CaptureSet(n) => {
                let cell = match op {
                    Op::CellSet(_) => self.cell_at(n)?,
                    _ => self.capture(n)?,
                };
                let [value] = self.operands::<1>();
                self.heap
                    .cell_set(cell, value)
                    .map_err(|error| self.native(error))?;
                self.pop();
            }
            _ => return Err(fault("a cell op of no cell")),
        }
        Ok(())
    }
}
