//! Cell and capture reads and writes (`K-CLO-001`): they keep the running
//! frame, so the dispatch loop runs them without reading it again.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ArenaHandle;

use super::{Run, Vm, fault, read};
use crate::bytecode::op::Op;
use crate::heap::Heap;

impl<H: Heap> Vm<H> {
    /// The cell local `slot` of the running frame holds.
    pub(super) fn cell_at(&self, slot: u16) -> Run<ArenaHandle> {
        match self.cx.stack[self.base() + usize::from(slot)] {
            Value::Handle(cell) => Ok(cell),
            _ => Err(fault("a shared local holds no cell")),
        }
    }

    /// The cell of the running closure's capture `n`. A capture read while the arena has freed nothing since it was
    /// kept is the cell kept ([`Captured`]).
    pub(super) fn capture(&self, n: u16) -> Run<ArenaHandle> {
        let closure = self
            .cx
            .frames
            .last()
            .and_then(|frame| frame.closure)
            .ok_or_else(|| fault("a capture outside a closure"))?;
        let freed = self.heap.arena().freed();
        let kept = &self.captured[Captured::slot(closure, n)];
        if let Some(cell) = kept.get().cell(closure, n, freed) {
            return Ok(cell);
        }
        let (_, captures, _) = self.heap.arena().closure(closure).map_err(read)?;
        let cell = captures
            .get(usize::from(n))
            .copied()
            .ok_or_else(|| fault("a capture past the closure's cells"))?;
        kept.set(Captured::of(closure, n, freed, cell));
        Ok(cell)
    }

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

/// How many capture reads the instance keeps.
pub(super) const KEPT: usize = 8;

/// A capture read the arena answered: closure `closure`'s capture `n`
/// is cell `cell`. A closure's cells are fixed when it is made, so the
/// answer holds while the arena has freed nothing since (`freed`): no
/// handle was freed or given to another object.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Captured(Option<(ArenaHandle, u16, u64, ArenaHandle)>);

impl Captured {
    pub(super) fn of(closure: ArenaHandle, n: u16, freed: u64, cell: ArenaHandle) -> Self {
        Self(Some((closure, n, freed, cell)))
    }

    /// Where closure `closure`'s capture `n` is kept.
    #[inline(always)]
    pub(super) fn slot(closure: ArenaHandle, n: u16) -> usize {
        (closure.index() as usize ^ usize::from(n)) % KEPT
    }

    /// The cell kept for closure `closure`'s capture `n`, if it holds.
    #[inline(always)]
    pub(super) fn cell(self, closure: ArenaHandle, n: u16, freed: u64) -> Option<ArenaHandle> {
        match self.0 {
            Some((held, at, when, cell)) if held == closure && at == n && when == freed => {
                Some(cell)
            }
            _ => None,
        }
    }
}
