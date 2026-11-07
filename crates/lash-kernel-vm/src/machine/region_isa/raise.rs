//! A raise leaves the region from its frame's departure role, or a raise
//! served to a frame's next step re-enters the region at the route of the
//! event it is born at.
use super::*;
use crate::machine::{Completion, Control, Halt, Interrupt, TryPhase, fault};
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::kernel_internals::logical::Slot;

impl KernelMachine {
    /// `task`'s top frame stands after a raise the region accepted that no
    /// try of the frame takes, its value in the frame's departure role. The
    /// machine carries it out of the frame, ending each frame it leaves, as
    /// any raise that frame cannot take; it is born in this step.
    pub(super) fn raise_from_region(
        &mut self,
        task: TaskId,
        host: &mut dyn crate::interface::Host,
    ) -> Result<(), Halt> {
        let value = self
            .frame(task)?
            .pending
            .source
            .as_ref()
            .ok_or_else(|| fault("an uncaught raise has no source continuation"))?
            .roles
            .iter()
            .find_map(|(slot, value)| matches!(slot, Slot::Departure(_)).then_some(*value))
            .ok_or_else(|| fault("an uncaught raise holds no departure"))?;
        self.raised = None;
        let exe = Arc::clone(&self.exe);
        self.complete(task, host, &exe, Completion::Throw(Box::new(value)))
    }

    /// `task`'s top frame stands before its Exit, its value in its departure
    /// role, and returns into a caller the machine holds: the machine
    /// carries the value out as any return, into that caller's wait.
    pub(super) fn leave_from_region(
        &mut self,
        task: TaskId,
        host: &mut dyn crate::interface::Host,
    ) -> Result<(), Halt> {
        let source = self
            .frame(task)?
            .pending
            .source
            .as_ref()
            .ok_or_else(|| fault("a leaving frame has no source continuation"))?;
        // A `return via` returns what its policy gave: the value its
        // Restore takes when its policy returned in the region, or else the
        // task's incoming.
        let value = if source.at.phase == crate::compile::room::Phase::Policy {
            let value = source
                .pending
                .ok_or_else(|| fault("a leaving policy return has no result"))?;
            // The frame takes the result its Restore would have: the task's
            // Incoming lets go of it.
            use lash_kernel_state::kernel_internals::logical::Owner;
            let store = self.logical.clone();
            let mut ledger = store.borrow_mut();
            let owner = Owner::Incoming(task);
            if ledger.holding(owner, self.heap.pages())?.is_some() {
                ledger.transfer_source(&mut self.arena.borrow_mut(), &self.heap, &[owner], &[])?;
            }
            value
        } else if source.at.phase == crate::compile::room::Phase::Restore {
            let Some(crate::machine::Incoming::Value(value)) = self.task(task)?.take_incoming()
            else {
                return Err(fault("a leaving policy return has no incoming result"));
            };
            self.admit_page(&value)?
        } else {
            source
                .roles
                .iter()
                .find_map(|(slot, value)| matches!(slot, Slot::Departure(_)).then_some(*value))
                .ok_or_else(|| fault("a leaving frame holds no departure"))?
        };
        let exe = Arc::clone(&self.exe);
        self.complete(task, host, &exe, Completion::Return(Box::new(value)))
    }

    /// A step the machine served for `task`'s top frame raised: an action,
    /// its delivery, a native answer or an effect's copy. The raise is born
    /// where the frame stands; `complete` routes it to a try of a canonical
    /// frame or out of each frame no try of it takes.
    /// Gives whether a try of the frame's own region took it, the region
    /// then running on in the caller's step.
    pub(super) fn raise_out(
        &mut self,
        task: TaskId,
        host: &mut dyn crate::interface::Host,
        interrupt: Interrupt,
    ) -> Result<bool, Halt> {
        self.regs.clear();
        self.raised = None;
        let value = self.thrown_by(task, interrupt)?;
        // A try of the frame's own region takes the raise at its route: the
        // region runs on in the same step, as after a delivered result.
        if self.catch_in_region(task, *value)? {
            return Ok(true);
        }
        let exe = Arc::clone(&self.exe);
        self.complete(task, host, &exe, Completion::Throw(value))?;
        if self.bounds.memory_meter != crate::MemoryMeter::Off {
            self.pins.clear();
        }
        Ok(false)
    }

    /// A raise `value` reached `task`'s top frame at the step it stands
    /// before: a standing action's Commit, or a suspended call's Restore.
    /// When the frame is canonical and its region routes that event's raise
    /// to a try, the frame re-enters there, the value delivered to the
    /// route's origin row as the action's or the call's result would be.
    /// `false` when no route takes it.
    pub(in crate::machine) fn catch_in_region(
        &mut self,
        task: TaskId,
        value: Value,
    ) -> Result<bool, Halt> {
        let frame = self.frame(task)?;
        if frame.pending.source.is_none() {
            return Ok(false);
        }
        let id = frame.code;
        let refused = |error| fault(&format!("canonical region admission refused: {error:?}"));
        let code = self.exe.region(id).map_err(refused)?;
        let tables = self
            .bound
            .get(&self.exe, &code)
            .map_err(|error| match error {
                RegionError::Heap(error) => error.into(),
                RegionError::Invalid(reason) => refused(reason),
            })?;
        let source = self
            .frame(task)?
            .pending
            .source
            .as_ref()
            .ok_or_else(|| fault("a canonical frame lost its source continuation"))?;
        let at = match verified::raised_at(&tables, source) {
            Ok(at) => at,
            Err(RegionError::Heap(error)) => return Err(error.into()),
            Err(RegionError::Invalid(reason)) => return Err(refused(reason)),
        };
        drop(tables);
        let Some(at) = at.filter(|at| code.raise_route(at)) else {
            return Ok(false);
        };
        // The innermost try in its body takes the raise; catch blocks the
        // raise leaves on its way let go of what they took.
        let frame = self.frame(task)?;
        let catching = frame
            .control
            .iter()
            .rposition(|control| {
                matches!(
                    control,
                    Control::Try {
                        phase: TryPhase::Body,
                        ..
                    }
                )
            })
            .ok_or_else(|| fault("a routed raise reaches no try of its frame"))?;
        let left: Vec<usize> = frame
            .control
            .iter()
            .enumerate()
            .skip(catching + 1)
            .filter(|(_, control)| {
                matches!(
                    control,
                    Control::Try {
                        phase: TryPhase::Catch,
                        ..
                    }
                )
            })
            .map(|(index, _)| index)
            .rev()
            .collect();
        for index in left {
            self.catch_left(task, index, Some(&value))?;
        }
        // The raised value is the task's Incoming, which the raising row
        // takes as a delivered result is taken.
        {
            use lash_kernel_state::kernel_internals::logical::{Owner, RootValue};
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
                &[(owner, RootValue::Value(&value))],
            )?;
        }
        self.publish_logical_meter()?;
        let charged = self.charged;
        let frame = self.frame(task)?;
        frame.awaiting = None;
        frame.nested = None;
        let source = frame
            .pending
            .source
            .as_mut()
            .ok_or_else(|| fault("a canonical frame lost its source continuation"))?;
        source.at = at.clone();
        source.unpaid = at.clone();
        source.progress = 0;
        source.pending = Some(value);
        source.departure = Some(at);
        // What the raising step charged outside the region is paid.
        source.debt.paid = charged;
        self.raise_held();
        self.catch_took(task, catching, &value)?;
        Ok(true)
    }
}
