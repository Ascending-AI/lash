//! Indexed dispatch advances only after its actual answer and local phase agree.
use super::super::*;
use super::{Running, charge};
use crate::compile::region_isa::RegionOp;
use crate::compile::room::charges::region_isa::ChargeMoment;

impl Running<'_> {
    pub(super) fn execute(&mut self, request: StopAt) -> Result<RegionStop, RegionError> {
        if self.faulted {
            return Err(invalid(Refused::Facts));
        }
        if let Some(stop) = self.stop
            && stop != RegionStop::Handoff
        {
            return Ok(stop);
        }
        self.stop = None;
        let outcome = self.dispatch(request);
        if outcome.is_err() {
            self.faulted = true;
        }
        outcome
    }

    fn halt(&mut self, reason: RegionStop) -> Result<RegionStop, RegionError> {
        self.stop = Some(reason);
        Ok(reason)
    }

    fn prefix_stop(&mut self, request: StopAt) -> Option<RegionStop> {
        let stop = if self.counters.progress >= self.limits.slice {
            RegionStop::Slice
        } else if matches!(request, StopAt::NextLegalPrefix) {
            RegionStop::Handoff
        } else {
            return None;
        };
        self.stop = Some(stop);
        Some(stop)
    }

    fn dispatch(&mut self, request: StopAt) -> Result<RegionStop, RegionError> {
        loop {
            if self.counters.progress >= self.limits.slice {
                return self.halt(RegionStop::Slice);
            }
            if self.returning.is_some() {
                if self.complete {
                    return self.halt(RegionStop::Complete);
                }
                if !self.restore_call()? {
                    return self.halt(RegionStop::Refusal);
                }
                if let Some(stop) = self.prefix_stop(request) {
                    return Ok(stop);
                }
                continue;
            }
            let code = Arc::clone(&self.code);
            let step = code
                .program
                .step(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?;
            let op = code
                .program
                .op(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?;
            if let RegionOp::Call { site, .. } = op {
                let Some(activation) = self.prepare_call(site.index())? else {
                    return self.halt(self.stop.unwrap_or(RegionStop::Refusal));
                };
                self.activation = Some(activation);
                if let Some(stop) = self.prefix_stop(request) {
                    return Ok(stop);
                }
                continue;
            }
            if matches!(op, RegionOp::Boundary { .. }) {
                return self.halt(RegionStop::Boundary);
            }
            let before = charge::bill(
                self.counters,
                self.limits,
                &step.charges,
                ChargeMoment::Before,
                None,
            )?;
            self.counters = before.counters;
            if let Some(crossing) = before.crossing {
                self.refusal = Some(crate::Bound::Charge);
                self.crossing = Some(crossing);
                self.attempted = Some(step.at.clone());
                return self.halt(RegionStop::Refusal);
            }
            if self
                .counters
                .depth
                .checked_add(step.depth)
                .is_none_or(|depth| depth > self.limits.depth)
            {
                self.refusal = Some(crate::Bound::CallDepth);
                self.attempted = Some(step.at.clone());
                return self.halt(RegionStop::Refusal);
            }
            if let Some(guard) = &step.guard
                && !self.guard(guard)?
            {
                return Err(invalid(Refused::Facts));
            }
            if let RegionOp::Heap {
                kernel,
                args,
                dst,
                next,
            } = op
            {
                if !self.heap_step(kernel, args, *dst, *next)? {
                    return self.halt(self.stop.unwrap_or(RegionStop::Refusal));
                }
                if let Some(stop) = self.prefix_stop(request) {
                    return Ok(stop);
                }
                continue;
            }
            let answer = match op {
                RegionOp::Read {
                    spec,
                    base,
                    dst,
                    next,
                } => match self.read_answer(spec, *base, *dst, *next) {
                    Ok(answer) => answer,
                    Err(
                        lash_kernel_state::kernel_internals::logical::LeaseFault::BoundaryRequired,
                    ) => {
                        return self.halt(RegionStop::Boundary);
                    }
                    Err(fault) => return Err(storage_fault(fault)),
                },
                _ => self
                    .scalar_answer(op)?
                    .ok_or_else(|| invalid(Refused::Facts))?,
            };
            let departure = if matches!(op, RegionOp::Exit { .. }) && self.departing.is_none() {
                let map = code
                    .program
                    .recovery(step.before)
                    .ok_or_else(|| invalid(Refused::Exit))?;
                let value = map
                    .roles
                    .iter()
                    .find_map(|(slot, reg)| {
                        matches!(
                            slot,
                            lash_kernel_state::kernel_internals::logical::Slot::Departure(_)
                        )
                        .then_some(*reg)
                    })
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let value = self.value(value)?;
                Some(self.quote_return(value)?)
            } else if matches!(op, RegionOp::Return { .. }) {
                let value = answer
                    .write
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .1
                    .bind(&mut self.view)
                    .map_err(RegionError::Heap)?;
                if !self.export_return(value)? {
                    return self.halt(RegionStop::Refusal);
                }
                Some(self.quote_return(value)?)
            } else {
                None
            };
            let after = charge::bill(
                self.counters,
                self.limits,
                &step.charges,
                ChargeMoment::After,
                answer.formula,
            )?;
            self.counters = after.counters;
            if let Some(crossing) = after.crossing {
                self.refusal = Some(crate::Bound::Charge);
                self.crossing = Some(crossing);
                self.attempted = Some(step.at.clone());
                if let Some(pending) = code
                    .program
                    .recovery(step.pending)
                    .ok_or_else(|| invalid(Refused::Exit))?
                    .pending
                {
                    self.write(
                        pending,
                        answer.write.ok_or_else(|| invalid(Refused::Facts))?.1,
                    )?;
                }
                self.reached.ordinal = self
                    .reached
                    .ordinal
                    .checked_add(1)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                self.prefix = step.pending;
                self.reached.prefix = step.pending.index() as u32;
                self.reached.debt = self.counters.debt;
                return self.halt(RegionStop::Refusal);
            }
            let flow = code
                .flows
                .get(self.pc.index())
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let looping = code.program.loop_departure(self.pc);
            let control = self.prepare_control(op, &answer, flow.loop_branch, looping.as_ref())?;
            let ordinal = self
                .reached
                .ordinal
                .checked_add(1)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let write = answer
                .write
                .map(|(destination, value)| {
                    self.registers.prepare_write(
                        &mut self.view,
                        self.parked.len(),
                        self.reached.invocation,
                        destination,
                        value,
                    )
                })
                .transpose()?;
            let Some(resources) = self.resources(flow, &answer)? else {
                self.refusal = Some(crate::Bound::Memory);
                self.attempted = Some(step.at.clone());
                return self.halt(RegionStop::Refusal);
            };
            let counters = resources.accept()?;
            self.accept_control(control);
            if let Some(write) = write {
                self.registers.publish_write(write);
            }
            self.counters = counters;
            self.reached.ordinal = ordinal;
            self.prefix = step.after;
            self.reached.prefix = step.after.index() as u32;
            self.reached.debt = counters.debt;
            if let Some(departure) = departure {
                self.departing = Some(departure);
            }
            match looping {
                Some(crate::compile::region_isa::LoopDeparture::Origin(origin)) => {
                    self.looping = Some(origin);
                }
                Some(crate::compile::region_isa::LoopDeparture::End { .. }) => {
                    self.looping = None;
                }
                None => {}
            }
            if matches!(op, RegionOp::Exit { .. }) {
                self.finish_frame()?;
                if self.complete {
                    return self.halt(RegionStop::Complete);
                }
            } else {
                self.pc = answer.next;
            }
            if let Some(stop) = self.prefix_stop(request) {
                return Ok(stop);
            }
        }
    }
}
