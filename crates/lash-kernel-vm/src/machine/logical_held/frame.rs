//! Thin frame placement adapter. It supplies canonical slot IDs and semantic
//! values; the state ledger owns every price and identity edge.
use super::*;
use crate::compile::{CodeId, Executable, room::Source};
use crate::machine::{Frame, Halt, SlotState, controls::Departed};
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::{
    Invocation, Ledger, Owner, PublicationValue, Slot,
};
use lash_kernel_state::kernel_internals::tree::Access;
use std::cell::Cell;
use std::sync::Arc;

pub(in crate::machine) struct FrameRoots {
    bank: super::BankRoots,
    invocation: Invocation,
    source: Arc<Source>,
    by_at: Arc<[u32]>,
    active_departure: Cell<bool>,
}
impl FrameRoots {
    pub(in crate::machine) fn prepared(
        lifecycle: std::rc::Rc<std::cell::RefCell<Ledger>>,
        task: TaskId,
        invocation: Invocation,
        source: Arc<Source>,
        by_at: Arc<[u32]>,
    ) -> Result<Self, HeapFault> {
        let slots = source.owner_slots().map_err(|_| {
            HeapFault::Malformed("entry source ownership verification failed".into())
        })?;
        Ok(Self {
            bank: super::BankRoots {
                ledger: lifecycle.clone(),
                task,
                invocation: invocation.id(),
                slots,
            },
            invocation,
            source,
            by_at,
            active_departure: Cell::new(false),
        })
    }
    pub(in crate::machine) fn invocation(&self) -> &Invocation {
        &self.invocation
    }
    pub(in crate::machine) fn bank(&self) -> super::BankRoots {
        self.bank.clone()
    }
    pub(in crate::machine) fn restore(
        lifecycle: std::rc::Rc<std::cell::RefCell<Ledger>>,
        task: TaskId,
        frame: &Frame,
        exe: &Executable,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        let invocation = lifecycle
            .clone()
            .borrow()
            .invocation(task, frame.invocation, access)?;
        let source = Arc::clone(&exe.code(frame.code).source_room);
        let slots = source
            .owner_slots()
            .map_err(|_| HeapFault::Malformed("source ownership verification failed".into()))?;
        Ok(Self {
            bank: super::BankRoots {
                ledger: lifecycle.clone(),
                task,
                invocation: invocation.id(),
                slots,
            },
            invocation,
            active_departure: Cell::new(
                frame.cleaning.is_some()
                    || frame.awaiting.is_some_and(|stmt| {
                        matches!(exe.stmt(stmt).form, crate::compile::Form::ReturnVia { .. })
                    }),
            ),
            source,
            by_at: Arc::clone(&exe.code(frame.code).slots_by_at),
        })
    }
    pub(in crate::machine) fn binding(&self, at: u32) -> (Owner, Slot) {
        let slot = binding_slot(&self.source, self.by_at[at as usize]);
        (self.invocation.owner(slot), slot)
    }
    fn role(
        &self,
        selector: crate::compile::room::Owner,
        value: Option<PublicationValue>,
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        use lash_kernel_state::kernel_internals::logical::RootValue;
        let bank = &self.bank;
        let owner = bank
            .slots
            .root(&selector, bank.task, bank.invocation)
            .ok_or_else(|| HeapFault::Malformed("source control has no canonical owner".into()))?;
        let mut ledger = bank.ledger.borrow_mut();
        let inputs = if ledger.holding(owner, projection.heap.pages())?.is_some() {
            vec![owner]
        } else {
            Vec::new()
        };
        let outputs = value
            .as_ref()
            .map(|value| {
                (
                    owner,
                    match value {
                        PublicationValue::Native(value) => RootValue::Value(value),
                        PublicationValue::Departure => RootValue::Departure,
                        _ => unreachable!("source controls own values or departure records"),
                    },
                )
            })
            .into_iter()
            .collect::<Vec<_>>();
        ledger.transfer_source(
            &mut projection.arena.borrow_mut(),
            projection.heap,
            &inputs,
            &outputs,
        )
    }
    pub(in crate::machine) fn departure(
        &self,
        value: Option<Departed>,
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        if value.is_none() && !self.active_departure.get() {
            return Ok(());
        }
        self.role(
            crate::compile::room::Owner::ActiveDeparture(self.source.site.clone()),
            value.map(Departed::source),
            projection,
        )?;
        self.active_departure.set(value.is_some());
        Ok(())
    }
    pub(in crate::machine) fn controls(
        &self,
        controls: &mut crate::machine::controls::Controls,
        cleaning: &Option<Result<Value, Value>>,
        exe: &Executable,
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        use crate::compile::room::Owner as Selector;
        use crate::machine::controls::Roles;
        if !controls.has_changes() && cleaning.is_none() {
            return Ok(());
        }
        let bank = &self.bank;
        let record = |selector, value| self.role(selector, value, projection);
        controls.flush(|role, keep| match role {
            Roles::For {
                stmt,
                cursor,
                header,
                ..
            } => {
                let site = &exe.stmt(stmt).site;
                record(
                    Selector::LoopCursor(site.clone()),
                    cursor.filter(|_| keep).map(PublicationValue::Native),
                )?;
                record(
                    Selector::LoopHeader(site.clone()),
                    header.filter(|_| keep).map(PublicationValue::Native),
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
                record(selector, pending.filter(|_| keep).map(Departed::source))
            }
        })?;
        if let Some(value) = cleaning {
            self.departure(
                Some(Departed::Value(
                    *value.as_ref().unwrap_or_else(|value| value),
                )),
                projection,
            )?;
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
pub(in crate::machine) fn source_value(slot: Slot, state: SlotState) -> Option<PublicationValue> {
    match state {
        SlotState::Empty => None,
        SlotState::Value(value) => Some(PublicationValue::Native(value)),
        SlotState::Cell(handle) if matches!(slot, Slot::Capture(_)) => {
            Some(PublicationValue::Capture(handle))
        }
        SlotState::Cell(handle) => Some(PublicationValue::Native(Value::ObjectRef(handle))),
    }
}
impl KernelMachine {
    /// A precise source binding owns its old root until this transfer commits.
    pub(in crate::machine) fn set_source_slot(
        &mut self,
        task: TaskId,
        at: u32,
        value: SlotState,
    ) -> Result<(), Halt> {
        let change = {
            let frame = self.frame(task)?;
            frame.logical.as_ref().map(|roots| {
                let (owner, slot) = roots.binding(at);
                (owner, source_value(slot, value))
            })
        };
        if let Some((owner, value)) = change {
            self.publish_logical_meter()?;
            let store = self.logical.clone();
            let mut ledger = store.borrow_mut();
            let inputs = if ledger.holding(owner, self.heap.pages())?.is_some() {
                vec![owner]
            } else {
                Vec::new()
            };
            let output = value
                .as_ref()
                .map(|value| {
                    use lash_kernel_state::kernel_internals::logical::RootValue;
                    (
                        owner,
                        match value {
                            PublicationValue::Native(value) => RootValue::Value(value),
                            PublicationValue::Capture(handle) => RootValue::Capture(*handle),
                            _ => unreachable!("a source slot is a value or a capture"),
                        },
                    )
                })
                .into_iter()
                .collect::<Vec<_>>();
            ledger.transfer_source(&mut self.arena.borrow_mut(), &self.heap, &inputs, &output)?;
        }
        self.frame(task)?.restore_slot(at, value);
        Ok(())
    }

    pub(in crate::machine) fn logical_departure(
        &mut self,
        task: TaskId,
        value: Option<Departed>,
    ) -> Result<(), Halt> {
        self.task(task)?;
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        let task = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| crate::machine::fault("no task"))?;
        if let Some(frame) = task.frames.last_mut()?
            && let Some(roots) = &frame.logical
        {
            roots.departure(value, &projection)?;
        }
        Ok(())
    }

    pub(in crate::machine) fn logical_flush_controls(
        &mut self,
        task: TaskId,
        exe: &Executable,
    ) -> Result<(), Halt> {
        self.task(task)?;
        let projection = Projection {
            arena: &self.arena,
            heap: &self.heap,
        };
        let task = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| crate::machine::fault("no task"))?;
        if let Some(frame) = task.frames.last_mut()?
            && let Some(roots) = &frame.logical
        {
            roots.controls(&mut frame.control, &frame.cleaning, exe, &projection)?;
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
        let slots = compiled
            .source_room
            .owner_slots()
            .map_err(|_| HeapFault::Malformed("source ownership verification failed".into()))?;
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
        self.logical.borrow_mut().transfer_source(
            &mut self.arena.borrow_mut(),
            &self.heap,
            &[],
            &[],
        )?;
        let invocation = self.logical.borrow_mut().enter_source(
            task,
            &mut self.arena.borrow_mut(),
            &self.heap,
            inputs,
            &bindings,
        )?;
        let roots = FrameRoots {
            bank: super::BankRoots {
                ledger: self.logical.clone(),
                task,
                invocation: invocation.id(),
                slots,
            },
            invocation,
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
            self.logical.borrow_mut().transfer_source(
                &mut self.arena.borrow_mut(),
                &self.heap,
                &[],
                &[],
            )?;
            self.logical.borrow_mut().leave_source(
                &roots.invocation,
                &mut self.arena.borrow_mut(),
                &self.heap,
                (Owner::Incoming(task), value),
            )?;
        }
        Ok(())
    }
}
