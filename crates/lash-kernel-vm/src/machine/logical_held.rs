//! Source ownership's bridge to the ledger. Executors supply values and source
//! coordinates; prices and identity reach are the state ledger's Holding.
use super::{KernelMachine, project::SharedArena};
use crate::compile::{Stmt, Stream};
use lash_kernel_doc::{Value, heap::HeapFault};
use lash_kernel_state::kernel_internals::{PagedHeap, logical::Holding};
mod frame;
pub(super) use frame::FrameRoots;
mod operands;
pub(super) use operands::BankRoots;

pub(super) struct Held {
    pub(super) value: Value,
    pub(super) holding: Holding,
    pub(super) owner: Option<lash_kernel_state::kernel_internals::logical::Owner>,
}
pub(super) struct Projection<'a> {
    pub(super) arena: &'a SharedArena,
    pub(super) heap: &'a PagedHeap,
}
impl Projection<'_> {
    pub(super) fn hold(&self, value: Value) -> Result<Held, HeapFault> {
        Ok(Held {
            owner: None,
            holding: Holding::from_native(&mut self.arena.borrow_mut(), self.heap.pages(), &value)?,
            value,
        })
    }
}
impl KernelMachine {
    #[cfg(test)]
    pub(crate) fn source_law_heap(&self) -> &PagedHeap {
        &self.heap
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn logical_balance(
        &self,
    ) -> Result<lash_kernel_state::kernel_internals::logical::Balance, HeapFault> {
        self.logical.ledger().borrow().balance()
    }

    pub(super) fn logical_incoming(
        &self,
        task: lash_kernel_doc::TaskId,
        incoming: &super::Incoming,
    ) -> Result<(), HeapFault> {
        use lash_kernel_state::kernel_internals::logical::{Owner, SourceValue};
        let value = match incoming {
            super::Incoming::Value(value) | super::Incoming::Raise(value) => Some(value.clone()),
            super::Incoming::Joined(joined) => self.tasks.ended_result(*joined)?,
            super::Incoming::Outcome(wait) => {
                let pending = self
                    .waits
                    .get(
                        &lash_kernel_state::kernel_internals::directory::ordinal_key(
                            lash_kernel_doc::kernel_internals::wait_number(*wait),
                        ),
                        self.heap.pages(),
                    )?
                    .ok_or_else(|| {
                        HeapFault::Malformed("logical incoming outcome has no wait".into())
                    })?;
                let lash_kernel_state::paged::WaitPhase::Committed(outcome) = pending.phase else {
                    return Err(HeapFault::Malformed(
                        "logical incoming outcome is not committed".into(),
                    ));
                };
                use lash_kernel_state::paged::CommittedOutcomeRecord;
                let (CommittedOutcomeRecord::Completed(frozen)
                | CommittedOutcomeRecord::Failed(frozen)
                | CommittedOutcomeRecord::Rejected(frozen)) = outcome;
                self.logical
                    .replace(Owner::Incoming(task), Some(SourceValue::Frozen(frozen)));
                return Ok(());
            }
        };
        if let Some(value) = value {
            self.logical
                .replace(Owner::Incoming(task), Some(SourceValue::Page(value)));
        }
        Ok(())
    }
    pub(super) fn logical_flush(&self) -> Result<(), HeapFault> {
        self.logical
            .flush(&mut self.arena.borrow_mut(), &self.heap)?;
        self.publish_logical_meter()
    }
    pub(super) fn publish_logical_meter(&self) -> Result<(), HeapFault> {
        self.logical_memory
            .set(self.heap.logical_balance()?.total()?);
        Ok(())
    }
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
        self.regs.source_entry(values, &projection)?;
        self.regs.source_entry_roots(self.logical.clone());
        self.logical_flush()
    }
    pub(super) fn source_begin(
        &mut self,
        task: lash_kernel_doc::TaskId,
        stream: &Stream,
        stmt: &Stmt,
        pc: u32,
    ) -> Result<(), super::Halt> {
        let exe = std::sync::Arc::clone(&self.exe);
        self.logical_flush_controls(task, &exe)?;
        self.logical_flush()?;
        let access = self.heap.pages().clone();
        let roots = if let Some(roots) = &self.frame(task)?.logical {
            roots.clear_operands(&access)?;
            Some(roots.bank())
        } else {
            None
        };
        self.regs.source_context(roots);
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_begin(stream, stmt, pc, &projection)?;
        self.logical_flush()?;
        Ok(())
    }
    pub(super) fn source_complete_joins(
        &mut self,
        stream: &Stream,
        pc: u32,
    ) -> Result<(), HeapFault> {
        self.regs.source_complete_joins(stream, pc)?;
        self.logical_flush()
    }
    pub(super) fn source_complete(&mut self, stream: &Stream, pc: u32) -> Result<(), HeapFault> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_complete(stream, pc, &projection)?;
        self.logical_flush()
    }
    pub(super) fn source_atom(&mut self, index: usize, value: Value) -> Result<(), HeapFault> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_atom(index, value, &projection)
    }
}
