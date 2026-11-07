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
        self.logical.borrow().balance()
    }

    pub(super) fn logical_incoming(
        &self,
        task: lash_kernel_doc::TaskId,
        incoming: &super::Incoming,
    ) -> Result<(), HeapFault> {
        use lash_kernel_state::kernel_internals::logical::{Owner, RootValue};
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
                let store = self.logical.clone();
                let mut ledger = store.borrow_mut();
                let owner = Owner::Incoming(task);
                let inputs = if ledger.holding(owner, self.heap.pages())?.is_some() {
                    vec![owner]
                } else {
                    Vec::new()
                };
                ledger.transfer_source(
                    &mut self.arena.borrow_mut(),
                    &self.heap,
                    &inputs,
                    &[(owner, RootValue::Frozen(&frozen))],
                )?;
                return Ok(());
            }
        };
        if let Some(value) = value {
            let store = self.logical.clone();
            let mut ledger = store.borrow_mut();
            let owner = Owner::Incoming(task);
            let inputs = if ledger.holding(owner, self.heap.pages())?.is_some() {
                vec![owner]
            } else {
                Vec::new()
            };
            ledger.transfer_source(
                &mut self.arena.borrow_mut(),
                &self.heap,
                &inputs,
                &[(owner, RootValue::Page(&value))],
            )?;
        }
        Ok(())
    }
    pub(super) fn publish_logical_meter(&self) -> Result<(), HeapFault> {
        self.logical_memory
            .set(self.heap.logical_balance()?.total()?);
        Ok(())
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
        // Decoding and ABI packing extend the same entry prefix. Transfer its
        // previous accepted receipts together with the new argument roots.
        let inputs = self.regs.source_owners();
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_entry(values, &projection)?;
        self.publish_logical_meter()?;
        let store = self.logical.clone();
        let outputs = self
            .regs
            .source_entry_roots(&store.borrow(), &projection, &inputs)?;
        let roots = outputs
            .iter()
            .map(|(owner, value)| {
                (
                    *owner,
                    lash_kernel_state::kernel_internals::logical::RootValue::Value(value),
                )
            })
            .collect::<Vec<_>>();
        store.borrow_mut().transfer_source(
            &mut self.arena.borrow_mut(),
            &self.heap,
            &inputs,
            &roots,
        )?;
        self.publish_logical_meter()
    }
    /// Opens the statement's source bank at `pc`. Gives whether it adopted
    /// a returned call's accepted resumption there rather than projecting
    /// the bank afresh.
    pub(super) fn source_begin(
        &mut self,
        task: lash_kernel_doc::TaskId,
        stream: &Stream,
        stmt: &Stmt,
        pc: u32,
    ) -> Result<bool, super::Halt> {
        let invocation = self.frame(task)?.invocation;
        let store = self.logical.clone();
        if self
            .regs
            .begin_source_resume(task, invocation, &stmt.site, pc, &store.borrow())?
        {
            return Ok(true);
        }
        let exe = std::sync::Arc::clone(&self.exe);
        self.logical_flush_controls(task, &exe)?;
        self.publish_logical_meter()?;
        let roots = self.frame(task)?.logical.as_ref().map(|roots| roots.bank());
        let preserve = if let Some(roots) = &roots {
            stmt.source_roles_at(stream, pc)
                .ok_or_else(|| HeapFault::Malformed("source resume has no role map".into()))?
                .iter()
                .map(|(_, site)| roots.operand(site))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        self.retire_source_bank(&preserve)?;
        if let Some(roots) = &roots {
            let roles = stmt.source_roles_at(stream, pc).ok_or_else(|| {
                HeapFault::Malformed("source resume has no canonical role map".into())
            })?;
            let mut ledger = store.borrow_mut();
            let mut inputs = Vec::new();
            let mut outputs = Vec::new();
            for (_, site) in roles.iter() {
                let output = roots.operand(site)?;
                if ledger.holding(output, self.heap.pages())?.is_some() {
                    continue;
                }
                let input = roots.continuation(site)?;
                if ledger.holding(input, self.heap.pages())?.is_none() {
                    return Err(HeapFault::Malformed(
                        format!("source resume has no accepted operand or continuation: statement {:?}, pc {pc}, role {site:?}, owner {output:?}", stmt.site),
                    )
                    .into());
                }
                inputs.push(input);
                outputs.push((
                    output,
                    lash_kernel_state::kernel_internals::logical::RootValue::Existing(input),
                ));
            }
            ledger.transfer_source(&mut self.arena.borrow_mut(), &self.heap, &inputs, &outputs)?;
        }
        self.regs.source_context(roots);
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_begin(stream, stmt, pc, &projection)?;
        self.publish_logical_meter()?;
        Ok(false)
    }
    pub(super) fn source_complete_joins(
        &mut self,
        running: &super::stream::Running<'_>,
        pc: u32,
    ) -> Result<(), super::Halt> {
        self.publish_source_joins(running, pc)
    }
    pub(super) fn source_complete(
        &mut self,
        running: &super::stream::Running<'_>,
        pc: u32,
    ) -> Result<(), super::Halt> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_complete(running.stream, pc, &projection)?;
        self.publish_logical_meter()?;
        self.source_complete_joins(running, pc)
    }
    /// The statement owns these exact receipts, including refusal exits. A
    /// prior frame/return publication may already have consumed a receipt.
    pub(super) fn retire_source_bank(
        &self,
        preserve: &[lash_kernel_state::kernel_internals::logical::Owner],
    ) -> Result<(), HeapFault> {
        self.logical.borrow_mut().transfer_source(
            &mut self.arena.borrow_mut(),
            &self.heap,
            &[],
            &[],
        )?;
        let store = self.logical.clone();
        let mut ledger = store.borrow_mut();
        let mut inputs = Vec::new();
        for owner in self.regs.source_owners() {
            if !preserve.contains(&owner)
                && !inputs.contains(&owner)
                && ledger.holding(owner, self.heap.pages())?.is_some()
            {
                inputs.push(owner);
            }
        }
        ledger.transfer_source(&mut self.arena.borrow_mut(), &self.heap, &inputs, &[])
    }
    /// Restore the permanent evaluator's continuation from accepted source
    /// roots. The result and saved operands move together; no physical value
    /// is reintroduced as a replacement for a missing continuation owner.
    pub(super) fn transfer_precise_result(
        &mut self,
        running: &super::stream::Running<'_>,
        pc: u32,
        result: lash_kernel_exec::Reg,
        value: &Value,
    ) -> Result<(), super::Halt> {
        use lash_kernel_state::kernel_internals::logical::{Owner, RootValue};
        let bank = self
            .frame(running.task)?
            .logical
            .as_ref()
            .ok_or_else(|| HeapFault::Malformed("source result has no invocation".into()))?
            .bank();
        let roles = running
            .stmt
            .source_roles_at(running.stream, pc)
            .ok_or_else(|| HeapFault::Malformed("source result has no continuation map".into()))?;
        let store = self.logical.clone();
        let mut ledger = store.borrow_mut();
        let incoming = Owner::Incoming(running.task);
        let held = ledger.holding(incoming, self.heap.pages())?.is_some();
        let mut inputs = if held { vec![incoming] } else { Vec::new() };
        let mut outputs = Vec::with_capacity(roles.len());
        for (reg, site) in roles.iter() {
            let output = bank.operand(site)?;
            if *reg == result {
                outputs.push((
                    output,
                    if held {
                        RootValue::Existing(incoming)
                    } else {
                        RootValue::Value(value)
                    },
                ));
            } else {
                let input = bank.continuation(site)?;
                inputs.push(input);
                outputs.push((output, RootValue::Existing(input)));
            }
        }
        let transferred =
            ledger.transfer_source(&mut self.arena.borrow_mut(), &self.heap, &inputs, &outputs);
        transferred?;
        Ok(())
    }
    pub(super) fn source_atom(&mut self, index: usize, value: Value) -> Result<(), HeapFault> {
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        self.regs.source_atom(index, value, &projection)
    }
}
