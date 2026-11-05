//! Indexed dispatch advances only after its actual answer and local phase agree.
use super::super::*;
use super::{Running, charge};
use crate::compile::region_isa::{PrefixId, RegionOp};
use crate::compile::room::charges::region_isa::ChargeMoment;

impl Running<'_> {
    pub(super) fn execute(&mut self, request: StopAt) -> Result<RegionStop, RegionError> {
        if self.faulted {
            return Err(invalid(Refused::Facts));
        }
        if let Some(stop) = self.stop {
            return Ok(stop);
        }
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

    fn reach(&mut self, prefix: PrefixId) -> Result<(), RegionError> {
        self.reached.ordinal = self
            .reached
            .ordinal
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        self.prefix = prefix;
        self.reached.prefix = prefix.index() as u32;
        self.reached.debt = self.counters.debt;
        Ok(())
    }

    fn dispatch(&mut self, request: StopAt) -> Result<RegionStop, RegionError> {
        loop {
            let code = Arc::clone(&self.code);
            let step = code
                .program
                .step(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?;
            let op = code
                .program
                .op(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?;
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
                self.attempted = Some(step.at.clone());
                return self.halt(RegionStop::Refusal);
            }
            if let Some(guard) = &step.guard
                && !self.guard(guard)?
            {
                return Err(invalid(Refused::Facts));
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
            let after = charge::bill(
                self.counters,
                self.limits,
                &step.charges,
                ChargeMoment::After,
                answer.formula,
            )?;
            self.counters = after.counters;
            if let Some(crossing) = after.crossing {
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
                self.reach(step.pending)?;
                return self.halt(RegionStop::Refusal);
            }
            let flow = code
                .flows
                .get(self.pc.index())
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let Some(resources) = self.resources(flow, &answer)? else {
                self.attempted = Some(step.at.clone());
                return self.halt(RegionStop::Refusal);
            };
            let counters = resources.accept()?;
            if let Some((destination, value)) = answer.write {
                self.registers.write(
                    &mut self.view,
                    0,
                    self.reached.invocation,
                    destination,
                    value,
                )?;
            }
            self.counters = counters;
            self.reach(step.after)?;
            self.pc = answer.next;
            if matches!(request, StopAt::NextLegalPrefix) {
                return self.halt(RegionStop::Handoff);
            }
        }
    }
}
