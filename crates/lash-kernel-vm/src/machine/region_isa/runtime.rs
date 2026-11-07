//! Resume canonical semantic phases through the verified executor.
use super::*;
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
    /// `task`'s top frame, if it stands suspended at a region call, takes
    /// `value` as the call's result: the callee left through the machine,
    /// which already gave the value to the task's incoming root, and the
    /// frame stands at the call's Restore as a lease stopped there leaves
    /// it, its completion billed as the callee left. `false` when the frame
    /// waits otherwise. A frame standing at a statement call the machine
    /// served takes the value as the call's delivered Incoming instead.
    pub(in crate::machine) fn return_into_region(
        &mut self,
        task: TaskId,
        value: lash_kernel_doc::Value,
    ) -> Result<bool, Halt> {
        if self.deliver_served_call(task, value)? {
            return Ok(true);
        }
        let frame = self.frame(task)?;
        let code = frame.code;
        let Some(at) = frame
            .pending
            .source
            .as_ref()
            .filter(|source| source.progress == 3)
            .map(|source| source.at.clone())
        else {
            return Ok(false);
        };
        let program = self
            .exe
            .region(code)
            .map_err(|error| halt(invalid(error)))?;
        let tables = self.bound.get(&self.exe, &program).map_err(halt)?;
        let resume = tables
            .calls
            .iter()
            .flatten()
            .find(|call| call.plan.point() == &at)
            .map(|call| call.plan.resume().clone())
            .ok_or_else(|| fault("a suspended canonical call has no plan"))?;
        let frame = self.frame(task)?;
        let mut source = frame
            .pending
            .leave_source()
            .ok_or_else(|| fault("a suspended canonical call has no continuation"))?;
        source.at = resume;
        source.progress = 4;
        source.pending = Some(value);
        frame.pending.source = Some(source);
        Ok(true)
    }

    /// `task`'s top frame stands at a statement call the machine served:
    /// the callee's value, already the task's incoming root, wakes the
    /// task, and the frame takes it at the call's Commit when it resumes.
    fn deliver_served_call(
        &mut self,
        task: TaskId,
        value: lash_kernel_doc::Value,
    ) -> Result<bool, Halt> {
        let frame = self.frame(task)?;
        let code = frame.code;
        let Some(source) = frame
            .pending
            .source
            .as_ref()
            .filter(|source| source.progress == 2)
        else {
            return Ok(false);
        };
        let source = source.clone();
        let program = self
            .exe
            .region(code)
            .map_err(|error| halt(invalid(error)))?;
        let tables = self.bound.get(&self.exe, &program).map_err(halt)?;
        let Some(standing) = verified::standing(&tables, &source).map_err(halt)? else {
            return Ok(false);
        };
        let calls = standing.act.is_none()
            && matches!(
                &self.exe.statement(standing.statement).1.form,
                crate::compile::Form::Action(crate::compile::Action {
                    kind: crate::compile::ActionKind::Call { .. },
                    ..
                })
            );
        if !calls {
            return Ok(false);
        }
        let incoming = Incoming::Value(self.page_form(&value)?);
        self.task(task)?.wake(incoming, false);
        self.frame(task)?.awaiting = Some(standing.statement);
        Ok(true)
    }

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
            let code = self.exe.region(id).map_err(|error| halt(invalid(error)))?;
            #[cfg(any(test, feature = "testing"))]
            let code = if self.islands.generic_only {
                code.as_generic()
            } else {
                code
            };
            // The driver performs one semantic step even for a zero-size slice.
            // Direct lease callers retain their exact, possibly exhausted cutoff.
            let tables = self.bound.get(&self.exe, &code).map_err(halt)?;
            let standing = match &self.frame(task)?.pending.source {
                Some(source) => verified::standing(&tables, source).map_err(halt)?,
                None => None,
            };
            if let Some(standing) = standing {
                match self.task(task)?.take_incoming() {
                    Some(incoming) => {
                        if let Err(raised) = self.deliver_action(task, standing, incoming)
                            && !self.raise_out(task, host, raised)?
                        {
                            return Ok(true);
                        }
                    }
                    None => {
                        if let Err(raised) = self.perform_action(task, host, standing)
                            && self.raise_out(task, host, raised)?
                        {
                            continue;
                        }
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
            let execution = &lease.execution;
            let program = &execution.code.program;
            let (at, native, effect, checked) = (
                program.step(execution.pc).map(|step| step.at.clone()),
                program.native_arguments(execution.pc),
                program.effect_argument(execution.pc),
                program.checked_call(execution.pc),
            );
            let stop = outcome.map_err(|error| match error {
                RegionError::Heap(error) => error.into(),
                RegionError::Invalid(reason) => fault(&format!(
                    "canonical region execution refused at {at:?}: {reason:?}"
                )),
            })?;
            let prefix = lease.materialize().map_err(halt)?;
            let (charge_limit, memory) = prefix.limits();
            let attempted = prefix.reconstruction().attempted.clone();
            let result = prefix.reconstruction().payload;
            prefix
                .prepare_exit()
                .map_err(halt)?
                .commit()
                .map_err(halt)?;
            // A limiter grew the limits in the run; the machine keeps them.
            if !matches!(self.bounds.charge, crate::Charge::Off) {
                self.bounds.charge_limit = charge_limit;
            }
            if self.bounds.memory_meter != crate::MemoryMeter::Off {
                self.bounds.memory = memory;
            }
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
                    // The nearest authored statement: the one the attempted
                    // step lies in when its body is authored; otherwise, or
                    // at a body's entry, the machine locates it from the
                    // frames and the task's spawn origin.
                    let code = self.frame(task)?.code;
                    exceeded.site = attempted
                        .filter(|point| self.exe.authored_unit(&point.site.unit))
                        .and_then(|point| self.exe.stmt_within(code, &point.site))
                        .map(Box::new);
                    return Err(Halt::Bound(exceeded));
                }
                RegionStop::Slice | RegionStop::Handoff => {}
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
                    let datum = match self.serve_effect(task, at, slot) {
                        Ok(datum) => datum,
                        Err(raised) => {
                            if self.raise_out(task, host, raised)? {
                                continue;
                            }
                            return Ok(true);
                        }
                    };
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
                    if let Err(raised) = self.answer_native(task, at, lib, &slots)
                        && !self.raise_out(task, host, raised)?
                    {
                        return Ok(true);
                    }
                    continue;
                }
                RegionStop::Check => {
                    self.regs.clear();
                    let (lib, slots) =
                        checked.ok_or_else(|| fault("a check stop stands at no checked call"))?;
                    let at = at.ok_or_else(|| fault("a check stop has no point"))?;
                    if let Err(raised) = self.check_arguments(task, at, lib, &slots)
                        && !self.raise_out(task, host, raised)?
                    {
                        return Ok(true);
                    }
                    continue;
                }
                // The next lease binds the dynamic target the call reached.
                RegionStop::Bind => {
                    self.regs.clear();
                    continue;
                }
                RegionStop::Refused { kind, message } => {
                    self.regs.clear();
                    let refused = crate::machine::Interrupt::refused(kind, message);
                    if !self.raise_out(task, host, refused)? {
                        return Ok(true);
                    }
                    continue;
                }
                // A raise no try of the frame takes leaves the frame.
                RegionStop::Throw => {
                    self.regs.clear();
                    self.raise_from_region(task, host)?;
                }
                // The lease's root returns into a caller the machine holds.
                RegionStop::Leave => {
                    self.regs.clear();
                    self.leave_from_region(task, host)?;
                }
                RegionStop::Boundary => {
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
