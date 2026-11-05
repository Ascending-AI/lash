use super::code::{Cost, Fee};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReturnPhase {
    FrameLeave,
    Completion,
    CallerRestore,
}

pub(super) struct Returning {
    activation: CallActivation,
    value: Value,
    completion: Cost,
    resume_pc: u32,
    phase: ReturnPhase,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ReturnImage {
    pub(super) site: u32,
    pub(super) value: Value,
    pub(super) phase: ReturnPhase,
    pub(super) resume_pc: u32,
}

impl Returning {
    pub(super) fn image(&self) -> ReturnImage {
        ReturnImage {
            site: self.activation.plan,
            value: self.value,
            phase: self.phase,
            resume_pc: self.resume_pc,
        }
    }
    pub(super) fn incoming(&self) -> Result<Value, RegionError> {
        if self.phase == ReturnPhase::FrameLeave {
            return Err(invalid(Refused::Exit));
        }
        Ok(self.value)
    }
}

impl Execution {
    pub(super) fn begin_return(&mut self, activation: CallActivation) -> Result<(), RegionError> {
        use super::code::Op;
        let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        if self.returning.is_some()
            || !Rc::ptr_eq(&activation.lease, &self.lease)
            || activation.depth + 1 != self.frames.len()
            || frame.invocation != activation.invocation
            || self.frames.len() <= 1
        {
            return Err(invalid(Refused::Facts));
        }
        let plan = self
            .code
            .calls
            .get(activation.plan as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let instruction = self
            .code
            .instructions
            .get(frame.pc as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        if plan.leave.depth != -1
            || !matches!(instruction.op, Op::Return { value } if value == plan.result)
        {
            return Err(invalid(Refused::Facts));
        }
        let prefix = instruction.prefix;
        let resume_pc = frame.return_pc;
        let value = self.read(plan.result)?;
        let Some(counters) = self.affordable(instruction.cost)? else {
            self.refuse(prefix)?;
            return Err(invalid(Refused::Peak));
        };
        let completion = Cost {
            fee: Fee::Fixed(if self.limits.charge_enabled {
                self.fee(plan.completion.fee)?
            } else {
                0
            }),
            ..plan.completion
        };
        self.reached(prefix, counters)?;
        self.returning = Some(Returning {
            activation,
            value,
            completion,
            resume_pc,
            phase: ReturnPhase::FrameLeave,
        });
        Ok(())
    }

    pub(super) fn advance_return(&mut self) -> Result<(), RegionError> {
        let returning = self
            .returning
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?;
        let plan = self
            .code
            .calls
            .get(returning.activation.plan as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let (cost, prefix) = match returning.phase {
            ReturnPhase::FrameLeave => (plan.leave, plan.leave_prefix),
            ReturnPhase::Completion => (returning.completion, plan.completion_prefix),
            ReturnPhase::CallerRestore => (plan.restore, plan.restore_prefix),
        };
        let Some(counters) = self.affordable(cost)? else {
            self.refuse(prefix)?;
            return Err(invalid(Refused::Peak));
        };
        match returning.phase {
            ReturnPhase::FrameLeave => {
                self.registers.clear(self.frames.len() - 1)?;
                self.frames.pop();
                self.returning
                    .as_mut()
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .phase = ReturnPhase::Completion;
            }
            ReturnPhase::Completion => {
                self.returning
                    .as_mut()
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .phase = ReturnPhase::CallerRestore;
            }
            ReturnPhase::CallerRestore => {
                let caller_index = self.frames.len() - 1;
                let caller = self
                    .frames
                    .last_mut()
                    .ok_or_else(|| invalid(Refused::Facts))?;
                self.registers.write(
                    caller_index,
                    caller.invocation,
                    plan.destination,
                    returning.value,
                )?;
                caller.pc = returning.resume_pc;
                self.returning = None;
            }
        }
        self.reached(prefix, counters)
    }

    pub(super) fn leave_call(&mut self, activation: CallActivation) -> Result<(), RegionError> {
        self.begin_return(activation)?;
        while self.returning.is_some() {
            self.advance_return()?;
        }
        Ok(())
    }
}
