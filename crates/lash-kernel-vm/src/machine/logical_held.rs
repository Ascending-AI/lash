//! Source ownership's bridge to the ledger. Executors supply values and source
//! coordinates; prices and identity reach are the state ledger's Holding.
use super::{KernelMachine, project::SharedArena};
use crate::compile::{Stmt, Stream};
use lash_kernel_doc::{Value, heap::HeapFault};
use lash_kernel_state::kernel_internals::{PagedHeap, logical::Holding};

pub(super) struct Held {
    pub(super) value: Value,
    pub(super) holding: Holding,
}
pub(super) struct Projection<'a> {
    pub(super) arena: &'a SharedArena,
    pub(super) heap: &'a PagedHeap,
}
impl Projection<'_> {
    pub(super) fn hold(&self, value: Value) -> Result<Held, HeapFault> {
        Ok(Held {
            holding: Holding::from_native(&mut self.arena.borrow_mut(), self.heap.pages(), &value)?,
            value,
        })
    }
}
impl KernelMachine {
    pub(super) fn hold_value(&self, value: Value) -> Result<Held, HeapFault> {
        Projection {
            arena: &self.arena,
            heap: &self.heap,
        }
        .hold(value)
    }
    pub(super) fn take_source_role(
        &mut self,
        reg: lash_kernel_exec::Reg,
        site: &lash_kernel_value::sites::Site,
    ) -> Result<Held, HeapFault> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.take_source_role(reg, site, &projection)
    }
    pub(super) fn source_entry(&mut self, values: &[Value]) -> Result<(), HeapFault> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_entry(values, &projection)
    }
    pub(super) fn source_begin(
        &mut self,
        stream: &Stream,
        stmt: &Stmt,
        pc: u32,
    ) -> Result<(), HeapFault> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_begin(stream, stmt, pc, &projection)
    }
    pub(super) fn source_complete(&mut self, stream: &Stream, pc: u32) -> Result<(), HeapFault> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_complete(stream, pc, &projection)
    }
    pub(super) fn source_atom(&mut self, index: usize, value: Value) -> Result<(), HeapFault> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_atom(index, value, &projection)
    }
}
