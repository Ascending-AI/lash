//! Thin frame placement adapter. It supplies canonical slot IDs and semantic
//! values; the state ledger owns every price and identity edge.
use super::*;
use crate::compile::{CodeId, Executable, room::Source};
use crate::machine::{Frame, Halt, SlotState};
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::{
    Invocation, Lifecycle, Owner, Slot, SourceValue,
};
use lash_kernel_state::kernel_internals::tree::Access;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::sync::Arc;

pub(in crate::machine) struct FrameRoots {
    lifecycle: Lifecycle,
    invocation: Invocation,
    source: Arc<Source>,
    by_at: Arc<[u32]>,
    control_owners: RefCell<BTreeSet<Owner>>,
    active_departure: Cell<bool>,
}
impl FrameRoots {
    pub(in crate::machine) fn bank(&self) -> Result<super::BankRoots, HeapFault> {
        let Owner::Frame { task, invocation } = self.invocation.frame() else {
            unreachable!("frame owner");
        };
        Ok(super::BankRoots {
            lifecycle: self.lifecycle.clone(),
            task,
            invocation,
            slots: self
                .source
                .owner_slots()
                .map_err(|_| HeapFault::Malformed("source ownership verification failed".into()))?,
        })
    }
    pub(in crate::machine) fn restore(
        lifecycle: Lifecycle,
        task: TaskId,
        frame: &Frame,
        exe: &Executable,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        let invocation = lifecycle
            .ledger()
            .borrow()
            .invocation(task, frame.invocation, access)?;
        Ok(Self {
            lifecycle,
            invocation,
            control_owners: RefCell::new(BTreeSet::new()),
            active_departure: Cell::new(
                frame.cleaning.is_some()
                    || frame.awaiting.is_some_and(|stmt| {
                        matches!(exe.stmt(stmt).form, crate::compile::Form::ReturnVia { .. })
                    }),
            ),
            source: Arc::clone(&exe.code(frame.code).source_room),
            by_at: Arc::clone(&exe.code(frame.code).slots_by_at),
        })
    }
    pub(in crate::machine) fn replace(&self, at: u32, old: SlotState, next: SlotState) {
        if matches!((old, next), (SlotState::Empty, SlotState::Empty)) {
            return;
        }
        let index = self.by_at[at as usize];
        let slot = binding_slot(&self.source, index);
        self.lifecycle
            .replace(self.invocation.owner(slot), source_value(slot, next));
    }
    fn role(
        &self,
        bank: &super::BankRoots,
        selector: crate::compile::room::Owner,
        value: Option<Value>,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        let owner = bank
            .slots
            .root(&selector, bank.task, bank.invocation)
            .ok_or_else(|| HeapFault::Malformed("source control has no canonical owner".into()))?;
        let mut owned = self.control_owners.borrow_mut();
        if let Some(value) = value {
            self.lifecycle
                .replace(owner, Some(SourceValue::Value(value)));
            owned.insert(owner);
        } else if owned.remove(&owner)
            || self
                .lifecycle
                .ledger()
                .borrow()
                .holding(owner, access)?
                .is_some()
        {
            self.lifecycle.replace(owner, None);
        }
        Ok(())
    }
    pub(in crate::machine) fn departure(
        &self,
        value: Option<Value>,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        if value.is_none() && !self.active_departure.get() {
            return Ok(());
        }
        self.role(
            &self.bank()?,
            crate::compile::room::Owner::ActiveDeparture(self.source.site.clone()),
            value,
            access,
        )?;
        self.active_departure.set(value.is_some());
        Ok(())
    }
    pub(in crate::machine) fn controls(
        &self,
        controls: &mut crate::machine::controls::Controls,
        cleaning: &Option<Result<Value, Value>>,
        reconstructed: bool,
        exe: &Executable,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        use crate::compile::room::Owner as Selector;
        use crate::machine::controls::Roles;
        if !controls.has_changes() && cleaning.is_none() {
            return Ok(());
        }
        let bank = self.bank()?;
        let record = |selector, value| self.role(&bank, selector, value, access);
        controls.flush(|role, keep| match role {
            Roles::For {
                stmt,
                cursor,
                header,
                initial,
            } => {
                // The physical cursor exists before its first source header
                // accepts. A refused header must not publish an extra owner.
                if reconstructed && initial && keep {
                    return Ok(());
                }
                let site = &exe.stmt(stmt).site;
                record(
                    Selector::LoopCursor(site.clone()),
                    if keep { cursor } else { None },
                )?;
                record(
                    Selector::LoopHeader(site.clone()),
                    if keep { header } else { None },
                )
            }
            Roles::Try { stmt, pending } => {
                let selector = Selector::PendingDeparture(exe.stmt(stmt).site.clone());
                if bank
                    .slots
                    .root(&selector, bank.task, bank.invocation)
                    .is_none()
                    && pending.is_none()
                {
                    return Ok(());
                }
                record(selector, if keep { pending } else { None })
            }
        })?;
        if let Some(value) = cleaning {
            self.departure(Some(*value.as_ref().unwrap_or_else(|value| value)), access)?;
        }
        Ok(())
    }
}
fn binding_slot(source: &Source, index: u32) -> Slot {
    let Some(Owner::Value { slot, .. }) = source.binding_root(index, TaskId::MAIN, 0) else {
        unreachable!("compiled binding has a canonical source slot");
    };
    slot
}
fn source_value(slot: Slot, state: SlotState) -> Option<SourceValue> {
    match state {
        SlotState::Empty => None,
        SlotState::Value(value) => Some(SourceValue::Value(value)),
        SlotState::Cell(handle) if matches!(slot, Slot::Capture(_)) => {
            Some(SourceValue::Capture(handle))
        }
        SlotState::Cell(handle) => Some(SourceValue::Value(Value::ObjectRef(handle))),
    }
}
impl KernelMachine {
    pub(in crate::machine) fn logical_departure(
        &mut self,
        task: TaskId,
        value: Option<Value>,
    ) -> Result<(), Halt> {
        let access = self.heap.pages().clone();
        if let Some(frame) = self.task(task)?.frames.last_mut()?
            && let Some(roots) = &frame.logical
        {
            roots.departure(value, &access)?;
        }
        Ok(())
    }

    pub(in crate::machine) fn logical_flush_controls(
        &mut self,
        task: TaskId,
        exe: &Executable,
    ) -> Result<(), Halt> {
        let access = self.heap.pages().clone();
        if let Some(frame) = self.task(task)?.frames.last_mut()?
            && let Some(roots) = &frame.logical
        {
            roots.controls(&mut frame.control, &frame.cleaning, false, exe, &access)?;
        }
        Ok(())
    }
    pub(in crate::machine) fn logical_reconstruct_controls(
        &mut self,
        task: TaskId,
        exe: &Executable,
    ) -> Result<(), Halt> {
        let access = self.heap.pages().clone();
        if let Some(frame) = self.task(task)?.frames.last_mut()?
            && let Some(roots) = &frame.logical
        {
            roots.controls(&mut frame.control, &frame.cleaning, true, exe, &access)?;
        }
        Ok(())
    }

    /// Packet reconstruction installs the same saved operand owners that
    /// source capture transfers out of its register bank.
    pub(in crate::machine) fn logical_flush_continuation(
        &mut self,
        task: TaskId,
        exe: &Executable,
    ) -> Result<(), Halt> {
        let frame = self.frame(task)?;
        let (Some(roots), Some(pending), Some(stmt), Some(act)) = (
            &frame.logical,
            frame.pending.peek(),
            frame.awaiting,
            frame.nested,
        ) else {
            return Ok(());
        };
        let entry = &exe.statement(stmt).0.nested[act.ix()];
        if pending.site != entry.suffix || pending.roles.len() != entry.roles.len() {
            return Err(
                HeapFault::Malformed("source continuation has another role map".into()).into(),
            );
        }
        let bank = roots.bank()?;
        for source in &entry.roles {
            let saved = pending
                .roles
                .iter()
                .find(|saved| saved.site == source.suffix)
                .ok_or_else(|| {
                    HeapFault::Malformed("source continuation role is missing".into())
                })?;
            bank.acquire(bank.continuation(&source.site)?, saved.value);
        }
        Ok(())
    }

    pub(in crate::machine) fn logical_enter_frame(
        &mut self,
        task: TaskId,
        exe: &Executable,
        code: CodeId,
        inputs: &[Owner],
    ) -> Result<(), Halt> {
        let compiled = exe.code(code);
        let bindings = self
            .frame(task)?
            .slot_states()
            .iter()
            .enumerate()
            .filter_map(|(at, state)| {
                let slot = binding_slot(&compiled.source_room, compiled.slots_by_at[at]);
                source_value(slot, *state).map(|value| (slot, value))
            })
            .collect::<Vec<_>>();
        let invocation = self.logical.enter(
            task,
            &mut self.arena.borrow_mut(),
            &self.heap,
            inputs,
            &bindings,
        )?;
        let roots = FrameRoots {
            lifecycle: self.logical.clone(),
            invocation,
            control_owners: RefCell::new(BTreeSet::new()),
            active_departure: Cell::new(false),
            source: Arc::clone(&compiled.source_room),
            by_at: Arc::clone(&compiled.slots_by_at),
        };
        let frame = self.frame(task)?;
        frame.invocation = roots.invocation.id();
        frame.logical = Some(roots);
        Ok(())
    }
    pub(in crate::machine) fn logical_leave_frame(
        &self,
        frame: &Frame,
        task: TaskId,
        value: Value,
    ) -> Result<(), HeapFault> {
        if let Some(roots) = &frame.logical {
            self.logical.leave(
                &roots.invocation,
                &mut self.arena.borrow_mut(),
                &self.heap,
                (Owner::Incoming(task), value),
            )?;
        }
        Ok(())
    }
}
