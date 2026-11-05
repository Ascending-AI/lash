use super::code::{Cost, Fee};
use super::operands::Operand;
use super::storage::View;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReturnPhase {
    FrameLeave,
    Completion,
    CallerRestore,
}

pub(super) struct Returning<V: Copy> {
    activation: CallActivation,
    value: V,
    completion: Cost,
    resume_pc: u32,
    phase: ReturnPhase,
}

#[derive(Clone, Copy)]
pub(super) struct ReturnImage<V: Copy> {
    pub(super) callee_invocation: u64,
    pub(super) completion: Cost,
    pub(super) caller_program: CodeId,
    pub(super) site: u32,
    pub(super) value: V,
    pub(super) phase: ReturnPhase,
    pub(super) resume_pc: u32,
}

impl<V: Copy> Returning<V> {
    pub(super) fn image(&self) -> ReturnImage<V> {
        ReturnImage {
            callee_invocation: self.activation.invocation,
            completion: self.completion,
            caller_program: self.activation.caller_program,
            site: self.activation.plan,
            value: self.value,
            phase: self.phase,
            resume_pc: self.resume_pc,
        }
    }
    pub(super) fn incoming(&self) -> Result<V, RegionError> {
        if self.phase == ReturnPhase::FrameLeave {
            return Err(invalid(Refused::Exit));
        }
        Ok(self.value)
    }
}

impl<V: View> Execution<V> {
    pub(super) fn begin_return(&mut self, activation: CallActivation) -> Result<(), RegionError> {
        use super::code::Op;
        let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        if self.returning.is_some()
            || !Rc::ptr_eq(&activation.lease, &self.lease)
            || activation.depth.checked_add(1) != Some(self.frames.len())
            || frame.invocation != activation.invocation
            || self.frames.len() <= 1
            || self
                .frames
                .get(activation.depth - 1)
                .map(|frame| frame.program)
                != Some(activation.caller_program)
        {
            return Err(invalid(Refused::Facts));
        }
        let caller = &self.frames[activation.depth - 1];
        let caller_code = self.programs.code(activation.caller_program)?;
        let entered = caller_code
            .instructions
            .get(caller.pc as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        if !matches!(entered.op, Op::Call { site } if site == activation.plan)
            || entered.next != frame.return_pc
        {
            return Err(invalid(Refused::Facts));
        }
        let plan = caller_code
            .calls
            .get(activation.plan as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let instruction = self
            .code
            .instructions
            .get(frame.pc as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        if frame.program != plan.target
            || plan.leave.depth != -1
            || !matches!(instruction.op, Op::Return { value } if value == plan.result)
        {
            return Err(invalid(Refused::Facts));
        }
        let prefix = instruction.prefix;
        let resume_pc = frame.return_pc;
        let value = self
            .read(plan.result)?
            .bind(&mut self.view)
            .map_err(RegionError::Heap)?;
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
        let caller_code = self.programs.code(returning.activation.caller_program)?;
        let plan = caller_code
            .calls
            .get(returning.activation.plan as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let (cost, prefix) = match returning.phase {
            ReturnPhase::FrameLeave => (plan.leave, plan.leave_prefix),
            ReturnPhase::Completion => (returning.completion, plan.completion_prefix),
            ReturnPhase::CallerRestore => (plan.restore, plan.restore_prefix),
        };
        let Some(counters) = self.affordable(cost)? else {
            self.attempted = Some(
                caller_code
                    .recovery
                    .get(prefix as usize)
                    .ok_or_else(|| invalid(Refused::Exit))?
                    .continuation
                    .clone(),
            );
            self.stop = Some(RegionStop::Refusal);
            return Err(invalid(Refused::Peak));
        };
        match returning.phase {
            ReturnPhase::FrameLeave => {
                self.registers.clear(self.frames.len() - 1)?;
                self.frames.pop();
                self.code = Arc::clone(caller_code);
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
                    &mut self.view,
                    caller_index,
                    caller.invocation,
                    plan.destination,
                    Operand::Dynamic(returning.value),
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
