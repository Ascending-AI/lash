//! Loop regions (`docs/design/kernel-regions.md` §2, §3): a `while` whose
//! plan lowering formed runs whole iterations inside one step, from the
//! plan's compiled form, while each fits the charge headroom and every
//! guard holds. The reference interpreter stays the oracle and the
//! fallback: a region exits at the last statement boundary it committed,
//! with the exact control the reference would hold there, and the
//! reference runs on from it.

use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::Weight;

use super::ops::Flow;
use super::{Control, Halt, KernelMachine, SlotState, fault};
use crate::compile::{Event, Executable, Meter, Summary};
use crate::observe::ChargeTape;

/// What a region's statements may hold while it runs, admitted once at its
/// entry (§3.5): their scalar pins, which net to zero at each statement's
/// end.
const REGION_HOLD: u64 = 1 << 16;

impl KernelMachine {
    /// Runs whole iterations of the innermost `while` of `task` as its loop
    /// region `region`, entered where the reference has charged its test's
    /// unit and runs the rest of the test. `None`: the loop stands there
    /// again, for the reference. `Some(Step)`: an iteration stopped before
    /// a body statement, which the reference runs from.
    pub(super) fn loop_region(
        &mut self,
        task: TaskId,
        exe: &Executable,
        region: u32,
    ) -> Result<Option<Flow>, Halt> {
        // The environment guard: nothing observes the run but a charge
        // tape, which the region records.
        if !self.charging
            || !self.ran.is_empty()
            || self.steps.verify
            || self
                .observer
                .as_deref()
                .is_some_and(|observer| !observer.only_charges())
        {
            return Ok(None);
        }
        #[cfg(test)]
        if self.boundary.is_some() {
            return Ok(None);
        }
        let plan = &exe.regions[region as usize];
        // The memory headroom guard: no admission inside refuses.
        if self
            .bounds
            .memory
            .saturating_sub(self.heap.certificate().upper.0)
            < REGION_HOLD
        {
            return Ok(None);
        }
        // The depth guard: every site's depth check passes in this frame,
        // whose depth the region does not change.
        if let Some(need) = plan.depth {
            let frames = self
                .tasks
                .get_mut(task.0 as usize)?
                .ok_or_else(|| fault("no task"))?
                .frames
                .len();
            let depth = frames
                + self.plans as usize
                + usize::from(task != TaskId::MAIN || !self.session_cell);
            if depth as u64 + u64::from(need) > u64::from(self.bounds.call_depth) {
                return Ok(None);
            }
        }
        let Some(compiled) = plan.compiled(exe) else {
            return Ok(None);
        };
        let tape = self
            .observer
            .as_deref()
            .and_then(|observer| observer.charges.as_ref());
        let mut meter = Meter::new(tape.is_some());
        let (bound, slice_end) = (self.bounds.charge_limit, self.slice_end);
        let arena = self.arena.borrow();
        let frame = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?
            .frames
            .last_mut()?
            .ok_or_else(|| fault("no frame"))?;
        let mut started = 0_u64;
        let mut stopped = None;
        'iterations: loop {
            // The charge headroom guard: no event of the iteration passes
            // the bound, and no step boundary inside reaches the slice's
            // end.
            match self.charged.checked_add(plan.iteration_max) {
                Some(end) if end <= bound && end < slice_end => {}
                _ => break,
            }
            meter.reset();
            if !matches!(
                (compiled.test)(frame.slot_states(), &mut meter),
                Some(Value::Bool(true))
            ) {
                break;
            }
            commit(&mut self.charged, tape, &plan.test, &meter);
            started += 1;
            for (next, (statement, value)) in
                plan.statements.iter().zip(&compiled.statements).enumerate()
            {
                meter.reset();
                let (Some(value), Some(at)) =
                    (value(frame.slot_states(), &mut meter), statement.write)
                else {
                    stopped = Some(next);
                    break 'iterations;
                };
                // A write that grows its slot is the reference's to admit.
                match frame.slot(at) {
                    SlotState::Value(old) if arena.cost_of(&value) <= arena.cost_of(old) => {}
                    _ => {
                        stopped = Some(next);
                        break 'iterations;
                    }
                }
                frame.set_slot(at, SlotState::Value(value));
                commit(&mut self.charged, tape, statement, &meter);
            }
            // The next test's unit, which the reference charges first.
            self.charged += plan.loop_test;
            if let Some(tape) = tape
                && plan.loop_test != 0
            {
                tape.push(Weight::Atomic(plan.loop_test));
            }
        }
        drop(arena);
        // The exit materializes the control the reference holds there:
        // the loop's iterations started, and the body block standing at
        // the statement the region did not run.
        if let Some(Control::While { started: total, .. }) = frame.control.last_mut() {
            *total += started;
        }
        Ok(stopped.map(|next| {
            frame.control.push(Control::Block {
                block: plan.body,
                next,
            });
            Flow::Step
        }))
    }
}

/// Commits a region statement that ran metered by `meter`: its fixed units
/// and formulas, and, with a charge tape attached, its static charge
/// program as the reference charges it (§3.1).
#[inline(always)]
fn commit(charged: &mut u64, tape: Option<&ChargeTape>, statement: &Summary, meter: &Meter) {
    *charged += statement.fixed + meter.units;
    if let Some(tape) = tape {
        record(tape, &statement.program, &meter.formulas);
    }
}

/// Records `program` with its formulas' units `formulas`, in order: what
/// the reference's anchors record for the same statement.
#[cold]
#[inline(never)]
fn record(tape: &ChargeTape, program: &[Event], formulas: &[u64]) {
    let mut formulas = formulas.iter();
    for event in program {
        let weight = match event {
            Event::Fixed(weight) => *weight,
            Event::Formula => Weight::Atomic(formulas.next().copied().unwrap_or(0)),
        };
        if weight != Weight::Atomic(0) {
            tape.push(weight);
        }
    }
}
