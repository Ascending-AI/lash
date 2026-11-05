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
    ) -> Result<bool, Halt> {
        if self.task(task)?.frames.is_empty() {
            self.refresh_charging(task)?;
            let Some(Incoming::Value(value)) = self.task(task)?.take_incoming() else {
                return Err(fault("a completed canonical frame has no incoming result"));
            };
            let value = self.admit_page(&value)?;
            self.end_task(task, Ok(value))?;
            return Ok(true);
        }
        let frame = self.frame(task)?;
        if frame.pending.source.is_none() {
            return Ok(false);
        }
        let id = frame.code;
        self.active_link = frame.link;
        self.refresh_charging(task)?;
        let draft = compile_region(&self.exe, id).map_err(|error| halt(invalid(error)))?;
        let calls = compile_calls(&self.exe, &draft).map_err(|error| halt(invalid(error)))?;
        let code =
            verify_region(&self.exe, id, draft, &calls).map_err(|error| halt(invalid(error)))?;
        let mut lease = RegionLease::acquire(self, code).map_err(halt)?;
        let outcome = lease.run(StopAt::NextBoundary);
        let stop = outcome.map_err(|error| {
            let at = match &lease.execution {
                Executing::Machine(execution) => execution
                    .code
                    .program
                    .step(execution.pc)
                    .map(|step| &step.at),
                #[cfg(test)]
                Executing::Fixture(_) => None,
            };
            match error {
                RegionError::Heap(error) => error.into(),
                RegionError::Invalid(reason) => fault(&format!(
                    "canonical region execution refused at {at:?}: {reason:?}"
                )),
            }
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
            RegionStop::Boundary | RegionStop::Throw => {
                return Err(fault("a canonical region reached an unintegrated boundary"));
            }
        }
        Ok(true)
    }
}
