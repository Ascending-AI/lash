//! Resume canonical semantic phases through the verified executor.
use super::*;
use crate::compile::region_isa::{compile_region, verify_region};
use crate::compile::room::region::calls::region_isa::compile_calls;
use crate::machine::{Halt, Incoming, fault};
use lash_kernel_doc::TaskId;

fn halt(error: RegionError) -> Halt {
    match error {
        RegionError::Heap(error) => error.into(),
        RegionError::Invalid(reason) => {
            fault(&format!("canonical region admission refused: {reason:?}"))
        }
    }
}

impl KernelMachine {
    pub(in crate::machine) fn resume_source_continuation(
        &mut self,
        task: TaskId,
        host: &mut dyn crate::interface::Host,
    ) -> Result<bool, Halt> {
        #[cfg(any(test, feature = "testing"))]
        if self.islands.source_only {
            return Ok(false);
        }
        if self.task(task)?.frames.is_empty() {
            self.refresh_charging(task)?;
            let Some(Incoming::Value(value)) = self.task(task)?.take_incoming() else {
                return Err(fault("a completed canonical frame has no incoming result"));
            };
            let value = self.admit_page(&value)?;
            self.end_task(task, Ok(value))?;
            return Ok(true);
        }
        // A native the region cannot answer in place is answered here and
        // taken by the region at once, before any other task runs.
        // A finish or fail served before its step, ending the run once taken.
        let mut ending = None;
        loop {
            let frame = self.frame(task)?;
            let id = frame.code;
            self.active_link = frame.link;
            self.refresh_charging(task)?;
            let draft = compile_region(&self.exe, id).map_err(|error| halt(invalid(error)))?;
            let calls = compile_calls(&self.exe, &draft).map_err(|error| halt(invalid(error)))?;
            let code = verify_region(&self.exe, id, draft, &calls)
                .map_err(|error| halt(invalid(error)))?;
            #[cfg(any(test, feature = "testing"))]
            let code = if self.islands.generic_only {
                code.as_generic()
            } else {
                code
            };
            // The driver performs one semantic step even for a zero-size slice.
            // Direct lease callers retain their exact, possibly exhausted cutoff.
            let tables = verified::Code::bind(self, &code).map_err(halt)?;
            let standing = match &self.frame(task)?.pending.source {
                Some(source) => verified::standing(&tables, source).map_err(halt)?,
                None => None,
            };
            if let Some(standing) = standing {
                match self.task(task)?.take_incoming() {
                    Some(incoming) => self.deliver_action(task, standing, incoming)?,
                    None => {
                        self.perform_action(task, host, standing)?;
                        return Ok(true);
                    }
                }
            }
            let mut limits = Limits::machine(self).map_err(halt)?;
            let request = if limits.slice <= limits.entry_progress {
                limits.slice = u64::MAX;
                StopAt::NextLegalPrefix
            } else {
                StopAt::NextBoundary
            };
            let mut lease = RegionLease::acquire_verified(self, tables, limits).map_err(halt)?;
            let outcome = lease.run(request);
            // The point the region stopped at, and, standing before a step
            // the machine serves, the slots its frame holds its operands in.
            let (at, native, effect) = match &lease.execution {
                Executing::Machine(execution) => (
                    execution
                        .code
                        .program
                        .step(execution.pc)
                        .map(|step| step.at.clone()),
                    execution.code.program.native_arguments(execution.pc),
                    execution.code.program.effect_argument(execution.pc),
                ),
                #[cfg(test)]
                Executing::Fixture(_) => (None, None, None),
            };
            let stop = outcome.map_err(|error| match error {
                RegionError::Heap(error) => error.into(),
                RegionError::Invalid(reason) => fault(&format!(
                    "canonical region execution refused at {at:?}: {reason:?}"
                )),
            })?;
            let prefix = lease.materialize().map_err(halt)?;
            let attempted = prefix.reconstruction().attempted.clone();
            let result = prefix.reconstruction().payload;
            prefix
                .prepare_exit()
                .map_err(halt)?
                .commit()
                .map_err(halt)?;
            match stop {
                RegionStop::Complete => {
                    self.end_task(
                        task,
                        Ok(result.ok_or_else(|| fault("a completed region has no result"))?),
                    )?;
                }
                RegionStop::Refusal => {
                    let mut exceeded = self
                        .regs
                        .take_region_refusal()
                        .ok_or_else(|| fault("a refused region has no bound payload"))?;
                    exceeded.site = attempted.map(|point| Box::new(point.site));
                    return Err(Halt::Bound(exceeded));
                }
                RegionStop::Slice | RegionStop::Handoff | RegionStop::Observer => {}
                // The frame stands at the action in its source continuation, a
                // canonical safe point: the physical cursor is discarded, since
                // performing the action may run another task before it resumes.
                RegionStop::Action => self.regs.clear(),
                // The effect's statement and copy charges fit: it is served
                // now, and the region takes its step at once.
                RegionStop::Effect(kind) => {
                    self.regs.clear();
                    let slot = effect.ok_or_else(|| fault("an effect stop stands at no effect"))?;
                    let at = at.ok_or_else(|| fault("an effect stop has no point"))?;
                    let datum = self.serve_effect(task, at, slot)?;
                    match kind {
                        crate::compile::region_isa::EffectKind::Print => host.print(&datum),
                        crate::compile::region_isa::EffectKind::Finish => {
                            ending = Some(Halt::Finish(Box::new(datum)));
                        }
                        crate::compile::region_isa::EffectKind::Fail => {
                            ending = Some(Halt::Fail(Box::new(datum)));
                        }
                    }
                    continue;
                }
                RegionStop::End => {
                    return Err(ending
                        .take()
                        .ok_or_else(|| fault("a program ended with no served effect"))?);
                }
                RegionStop::Native => {
                    self.regs.clear();
                    let (lib, slots) =
                        native.ok_or_else(|| fault("a native stop stands at no native call"))?;
                    let at = at.ok_or_else(|| fault("a native stop has no point"))?;
                    self.answer_native(task, at, lib, &slots)?;
                    continue;
                }
                RegionStop::Boundary | RegionStop::Throw => {
                    return Err(fault(&format!(
                        "a canonical region reached an unintegrated boundary at {at:?}"
                    )));
                }
            }
            if self.bounds.memory_meter != crate::MemoryMeter::Off {
                self.pins.clear();
            }
            return Ok(true);
        }
    }
}
