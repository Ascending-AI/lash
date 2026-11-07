//! Initial resident for headers publish only an already paid iterable prefix.
use super::*;
use crate::admission::SourceResources;
use crate::compile::room::{
    placement::BindingPlacement,
    publication::boundary::{LoopEntry, PaidLoopInput},
};
use lash_kernel_state::kernel_internals::logical::{PublicationValue, RootValue};

impl KernelMachine {
    pub(in crate::machine) fn publish_initial_for_entry(
        &mut self,
        task_id: TaskId,
        exe: &Executable,
        statement: StmtId,
        cursor: &Cursor,
        paid: Option<&PaidLoopInput>,
    ) -> Result<bool, Halt> {
        #[cfg(any(test, feature = "testing"))]
        if self.islands.source_only || self.islands.generic_only {
            return Ok(false);
        }
        let Some(Ok((next_cursor, Some(value)))) = cursor.resident_step(&self.arena.borrow())
        else {
            return Ok(false);
        };
        let Some(walked) = cursor.walked() else {
            return Ok(false);
        };
        let Form::For {
            binding,
            body,
            via: None,
            ..
        } = &exe.stmt(statement).form
        else {
            return Ok(false);
        };
        if binding.shared {
            return Ok(false);
        }
        let (binding_at, body) = (binding.at, *body);
        self.publish_logical_meter()?;
        let store = self.logical.clone();
        let mut arena = self.arena.borrow_mut();
        let task = self
            .tasks
            .get_mut(task_id.0 as usize)?
            .ok_or_else(|| fault("initial for task disappeared"))?;
        let depth =
            (task.frames.len() + usize::from(task_id != TaskId::MAIN || !self.session_cell)) as u64;
        let frame = task
            .frames
            .last_mut()?
            .ok_or_else(|| fault("initial for frame disappeared"))?;
        let Some(frontier) = frame.source_frontier.as_mut() else {
            return Ok(false);
        };
        let Some(roots) = frame.logical.as_ref() else {
            return Ok(false);
        };
        let Ok(placement) = BindingPlacement::compile(exe, frame.code) else {
            return Ok(false);
        };
        let placement = Arc::new(placement);
        let Ok(entry) = LoopEntry::compile(exe, &placement, statement) else {
            return Ok(false);
        };
        entry.adopt_paid_input(
            frontier,
            roots.invocation(),
            &mut store.borrow_mut(),
            &mut arena,
            &self.heap,
            frame.slots.states(),
            walked,
            paid,
            self.charged,
        )?;
        let ledger = store.borrow();
        let Some(bound) = entry.bind(
            frontier,
            roots.invocation(),
            &ledger,
            &mut arena,
            &self.heap,
            frame.slots.states(),
            0,
            Some((walked, 0)),
            Some(value),
            paid,
            self.charged,
        )?
        else {
            return Ok(false);
        };
        let (inputs, outputs) = bound.roots();
        let outputs: Vec<_> = outputs
            .iter()
            .map(|(owner, value)| {
                (
                    *owner,
                    match value {
                        PublicationValue::Existing(from) => RootValue::Existing(*from),
                        PublicationValue::Native(value) => RootValue::Value(value),
                        _ => unreachable!("initial for roots are resident"),
                    },
                )
            })
            .collect();
        let registration = arena
            .registration()
            .map_err(crate::machine::source_publication::native_fault)?;
        let transaction =
            ledger.prepare_source_boundary(registration, &self.heap, inputs, &outputs)?;
        let safe_point = self.charged;
        let Some(accepted) = bound.prepare_external(
            frontier,
            &ledger,
            transaction,
            SourceResources {
                charged: &mut self.charged,
                counters: &mut task.counters,
                bound: if self.bounds.charge == crate::Charge::Exact {
                    self.bounds.charge_limit
                } else {
                    u64::MAX
                },
                slice: self.slice_end,
                depth,
                limit: u64::from(self.bounds.call_depth),
                safe_point,
            },
            if self.bounds.memory_meter == crate::MemoryMeter::On {
                self.bounds.memory
            } else {
                u64::MAX
            },
        )?
        else {
            return Ok(false);
        };
        drop(ledger);
        let frontier = frame
            .source_frontier
            .as_mut()
            .ok_or_else(|| fault("accepted initial for lost its frontier"))?;
        let balance = accepted.publish_boundary(
            frontier,
            &mut store.borrow_mut(),
            |_, edits| {
                assert!(edits.is_empty());
            },
            self.observer
                .as_deref()
                .and_then(|observer| observer.charges.as_ref()),
        )?;
        frame.control.push(Control::For {
            stmt: statement,
            cursor: next_cursor,
            started: 1,
            header: Header::Body,
            guard: None,
        });
        frame.control.push(Control::Block {
            block: body,
            next: 0,
        });
        frame.restore_slot(binding_at, crate::machine::SlotState::Value(value));
        self.logical_memory.set(balance.total()?);
        Ok(true)
    }
}
