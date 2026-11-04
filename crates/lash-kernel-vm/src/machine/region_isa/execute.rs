use super::code::{Cost, Fee, Op, Register};
use super::registers::Registers;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Counters {
    pub(super) charge: u64,
    pub(super) balance: u64,
    pub(super) peak: u64,
    pub(super) depth: u32,
    pub(super) debt: u64,
}
pub(super) struct Frame {
    pub(super) invocation: u64,
    pub(super) pc: u32,
    pub(super) prefix: u32,
    pub(super) return_pc: u32,
    pub(super) activation: Option<CallActivation>,
}
pub(super) struct Execution {
    pub(super) code: Arc<Code>,
    pub(super) registers: Registers,
    pub(super) frames: Vec<Frame>,
    pub(super) reached: ReachedPrefix,
    pub(super) counters: Counters,
    pub(super) limits: Limits,
    pub(super) attempted: Option<Point>,
    pub(super) stop: Option<RegionStop>,
    pub(super) payload: Option<Value>,
    pub(super) faulted: bool,
    lease: Rc<()>,
    next_invocation: u64,
    #[cfg(test)]
    pub(super) executed: u64,
}

impl Execution {
    pub(super) fn acquire(code: Arc<Code>, limits: Limits) -> Result<Self, RegionError> {
        if code.frame_capacity == 0
            || code.invocation == 0
            || limits.entry_balance > limits.memory
            || limits.entry_charge > limits.charge
            || limits.entry_depth > limits.depth
            || code.instructions.get(code.entry as usize).is_none()
            || code.recovery.get(code.entry_prefix as usize).is_none()
        {
            return Err(invalid(Refused::Facts));
        }
        let mut registers = Registers::new(code.registers, code.frame_capacity)?;
        for (reg, value) in &code.initial {
            registers.write(0, code.invocation, *reg, *value)?;
        }
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(code.frame_capacity)
            .map_err(|_| invalid(Refused::Scratch))?;
        frames.push(Frame {
            invocation: code.invocation,
            pc: code.entry,
            prefix: code.entry_prefix,
            return_pc: 0,
            activation: None,
        });
        let reached = ReachedPrefix {
            invocation: code.invocation,
            occurrence: 0,
            prefix: code.entry_prefix,
            debt: 0,
        };
        let counters = Counters {
            charge: limits.entry_charge,
            balance: limits.entry_balance,
            peak: limits.entry_balance,
            depth: limits.entry_depth,
            debt: 0,
        };
        let next_invocation = code
            .invocation
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        Ok(Self {
            code,
            registers,
            frames,
            reached,
            counters,
            limits,
            attempted: None,
            stop: None,
            payload: None,
            faulted: false,
            lease: Rc::new(()),
            next_invocation,
            #[cfg(test)]
            executed: 0,
        })
    }

    pub(super) fn read(&self, reg: Register) -> Result<Value, RegionError> {
        let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        self.registers
            .read(self.frames.len() - 1, frame.invocation, reg)
    }

    fn fee(&self, fee: Fee) -> Result<u64, RegionError> {
        match fee {
            Fee::Fixed(units) => Ok(units),
            Fee::WordSize {
                value,
                zero,
                nonzero,
            } => match self.read(value)? {
                Value::Int(0) => Ok(zero),
                Value::Int(_) => Ok(nonzero),
                _ => Err(invalid(Refused::Facts)),
            },
        }
    }

    fn affordable(&self, cost: Cost) -> Result<Option<Counters>, RegionError> {
        let Some(charge) = self.counters.charge.checked_add(self.fee(cost.fee)?) else {
            return Ok(None);
        };
        let Some(balance) = i128::from(self.counters.balance)
            .checked_add(cost.delta)
            .and_then(|n| u64::try_from(n).ok())
        else {
            return Ok(None);
        };
        let Some(temporary) = self.counters.balance.checked_add(cost.peak) else {
            return Ok(None);
        };
        let Some(depth) = i64::from(self.counters.depth)
            .checked_add(i64::from(cost.depth))
            .and_then(|n| u32::try_from(n).ok())
        else {
            return Ok(None);
        };
        let Some(debt) = self.counters.debt.checked_add(1) else {
            return Ok(None);
        };
        let peak = self.counters.peak.max(temporary).max(balance);
        Ok((charge <= self.limits.charge
            && peak <= self.limits.memory
            && depth <= self.limits.depth)
            .then_some(Counters {
                charge,
                balance,
                peak,
                depth,
                debt,
            }))
    }

    fn refuse(&mut self, prefix: u32) -> Result<(), RegionError> {
        self.attempted = Some(
            self.code
                .recovery
                .get(prefix as usize)
                .ok_or_else(|| invalid(Refused::Exit))?
                .continuation
                .clone(),
        );
        self.stop = Some(RegionStop::Refusal);
        Ok(())
    }

    fn reached(&mut self, prefix: u32, counters: Counters) -> Result<(), RegionError> {
        self.code
            .recovery
            .get(prefix as usize)
            .ok_or_else(|| invalid(Refused::Exit))?;
        let frame = self
            .frames
            .last_mut()
            .ok_or_else(|| invalid(Refused::Facts))?;
        frame.prefix = prefix;
        self.reached = ReachedPrefix {
            invocation: frame.invocation,
            occurrence: self
                .reached
                .occurrence
                .checked_add(1)
                .ok_or_else(|| invalid(Refused::Facts))?,
            prefix,
            debt: counters.debt,
        };
        self.counters = counters;
        Ok(())
    }

    pub(super) fn run(&mut self, request: StopAt) -> Result<RegionStop, RegionError> {
        if self.faulted {
            return Err(invalid(Refused::Facts));
        }
        if let Some(stop) = self.stop {
            if stop != RegionStop::Handoff {
                return Ok(stop);
            }
            self.stop = None;
        }
        let result = self.run_inner(request);
        if result.is_err() {
            self.faulted = true;
        }
        result
    }

    fn run_inner(&mut self, request: StopAt) -> Result<RegionStop, RegionError> {
        loop {
            if self.counters.charge >= self.limits.slice {
                self.stop = Some(RegionStop::Slice);
                return Ok(RegionStop::Slice);
            }
            let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
            let pc = frame.pc;
            let instruction = self
                .code
                .instructions
                .get(pc as usize)
                .ok_or_else(|| invalid(Refused::Path))?;
            let (op, next, prefix, cost) = (
                instruction.op,
                instruction.next,
                instruction.prefix,
                instruction.cost,
            );
            if matches!(op, Op::Boundary) {
                self.stop = Some(RegionStop::Boundary);
                return Ok(RegionStop::Boundary);
            }
            if let Op::Call { site } = op {
                match self.enter_call(site) {
                    Ok(activation) => {
                        self.frames
                            .last_mut()
                            .ok_or_else(|| invalid(Refused::Facts))?
                            .activation = Some(activation)
                    }
                    Err(_) if self.stop == Some(RegionStop::Refusal) => {
                        return Ok(RegionStop::Refusal);
                    }
                    Err(error) => return Err(error),
                }
            } else if let Op::Return { value } = op {
                if self.frames.len() == 1 {
                    let answer = self.read(value)?;
                    let Some(counters) = self.affordable(cost)? else {
                        self.refuse(prefix)?;
                        return Ok(RegionStop::Refusal);
                    };
                    self.reached(prefix, counters)?;
                    self.payload = Some(answer);
                    self.stop = Some(RegionStop::Complete);
                    return Ok(RegionStop::Complete);
                }
                let activation = self
                    .frames
                    .last_mut()
                    .and_then(|f| f.activation.take())
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let plan = self
                    .code
                    .calls
                    .get(activation.plan as usize)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                if plan.result != value {
                    return Err(invalid(Refused::Facts));
                }
                match self.leave_call(activation) {
                    Ok(()) => (),
                    Err(_) if self.stop == Some(RegionStop::Refusal) => {
                        return Ok(RegionStop::Refusal);
                    }
                    Err(error) => return Err(error),
                }
            } else {
                let Some(counters) = self.affordable(cost)? else {
                    self.refuse(prefix)?;
                    return Ok(RegionStop::Refusal);
                };
                let mut jump = next;
                let mut thrown = None;
                let write = match op {
                    Op::Constant { dst, value } => Some((dst, value)),
                    Op::Move { dst, src } | Op::Pack { dst, src } => Some((dst, self.read(src)?)),
                    Op::Int {
                        kernel,
                        lhs,
                        rhs,
                        dst,
                    } => {
                        let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
                        let index = self.frames.len() - 1;
                        let a = self.registers.integer(index, frame.invocation, lhs)?;
                        let b = self.registers.integer(index, frame.invocation, rhs)?;
                        #[cfg(test)]
                        self.code
                            .semantic_operations
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let answer = if kernel.compares() {
                            lash_kernel_doc::scalar::int_truth(kernel, a, b).map(Value::Bool)
                        } else {
                            lash_kernel_doc::scalar::int_number(kernel, a, b).map(Value::Int)
                        };
                        let Some(answer) = answer else {
                            self.stop = Some(RegionStop::Boundary);
                            return Ok(RegionStop::Boundary);
                        };
                        if self.code.safe_integers && [Value::Int(a), Value::Int(b), answer].iter().any(|value| matches!(value, Value::Int(n) if n.unsigned_abs() > 9_007_199_254_740_991)) {
                            self.stop = Some(RegionStop::Boundary); return Ok(RegionStop::Boundary);
                        }
                        Some((dst, answer))
                    }
                    Op::Branch { test, yes, no } => {
                        jump = match self.read(Register::Bool(test))? {
                            Value::Bool(true) => yes,
                            Value::Bool(false) => no,
                            _ => return Err(invalid(Refused::Facts)),
                        };
                        None
                    }
                    Op::Read { base, member, dst } => {
                        #[cfg(test)]
                        self.code
                            .semantic_operations
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let actual = self.read(Register::Value(base))?;
                        let dependency = self
                            .code
                            .dependencies
                            .get(base as usize)
                            .ok_or_else(|| invalid(Refused::Facts))?;
                        if !dependency.identity.same(&actual) {
                            return Err(invalid(Refused::Facts));
                        }
                        Some((
                            dst,
                            *dependency
                                .members
                                .get(member as usize)
                                .ok_or_else(|| invalid(Refused::Facts))?,
                        ))
                    }
                    Op::Throw { value } => {
                        thrown = Some(self.read(value)?);
                        None
                    }
                    Op::Meter | Op::Exit => None,
                    Op::Call { .. } | Op::Return { .. } | Op::Boundary => {
                        unreachable!("handled before scalar dispatch")
                    }
                };
                if let Some((dst, value)) = write {
                    let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
                    self.registers
                        .write(self.frames.len() - 1, frame.invocation, dst, value)?;
                }
                self.reached(prefix, counters)?;
                self.frames
                    .last_mut()
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .pc = jump;
                if matches!(op, Op::Throw { .. } | Op::Exit) {
                    self.payload = thrown;
                    let stop = if matches!(op, Op::Exit) {
                        RegionStop::Complete
                    } else {
                        RegionStop::Throw
                    };
                    self.stop = Some(stop);
                    return Ok(stop);
                }
            }
            #[cfg(test)]
            {
                self.executed += 1;
            }
            if matches!(request, StopAt::NextLegalPrefix) {
                self.stop = Some(RegionStop::Handoff);
                return Ok(RegionStop::Handoff);
            }
        }
    }

    pub(super) fn enter_call(&mut self, site: u32) -> Result<CallActivation, RegionError> {
        let caller = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        let instruction = self
            .code
            .instructions
            .get(caller.pc as usize)
            .ok_or_else(|| invalid(Refused::Path))?;
        if !matches!(instruction.op, Op::Call { site: actual } if actual == site)
            || instruction.cost.depth != 1
        {
            return Err(invalid(Refused::Facts));
        }
        let plan = self
            .code
            .calls
            .get(site as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let (next, prefix, cost) = (instruction.next, instruction.prefix, instruction.cost);
        if self.frames.len() == self.code.frame_capacity {
            self.refuse(prefix)?;
            return Err(invalid(Refused::Peak));
        }
        for (src, _) in &plan.arguments {
            self.read(*src)?;
        }
        let Some(counters) = self.affordable(cost)? else {
            self.refuse(prefix)?;
            return Err(invalid(Refused::Peak));
        };
        let invocation = self.next_invocation;
        let following = invocation
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let depth = self.frames.len();
        self.registers.clear(depth)?;
        for (src, dst) in &plan.arguments {
            let value = self.read(*src)?;
            self.registers.write(depth, invocation, *dst, value)?;
        }
        let (entry, entry_prefix) = (plan.entry, plan.prefix);
        self.frames
            .last_mut()
            .ok_or_else(|| invalid(Refused::Facts))?
            .prefix = prefix;
        self.frames.push(Frame {
            invocation,
            pc: entry,
            prefix: entry_prefix,
            return_pc: next,
            activation: None,
        });
        self.next_invocation = following;
        self.reached(entry_prefix, counters)?;
        Ok(CallActivation {
            lease: Rc::clone(&self.lease),
            invocation,
            depth,
            plan: site,
        })
    }

    pub(super) fn leave_call(&mut self, activation: CallActivation) -> Result<(), RegionError> {
        let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        if !Rc::ptr_eq(&activation.lease, &self.lease)
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
        if plan.leave.depth != -1
            || !matches!(self.code.instructions.get(frame.pc as usize).map(|op| op.op), Some(Op::Return { value }) if value == plan.result)
        {
            return Err(invalid(Refused::Facts));
        }
        let (value, destination, prefix) =
            (self.read(plan.result)?, plan.destination, plan.leave_prefix);
        let Some(counters) = self.affordable(plan.leave)? else {
            self.refuse(prefix)?;
            return Err(invalid(Refused::Peak));
        };
        let next = frame.return_pc;
        self.registers.clear(self.frames.len() - 1)?;
        self.frames.pop();
        let caller_index = self.frames.len() - 1;
        let caller = self
            .frames
            .last_mut()
            .ok_or_else(|| invalid(Refused::Facts))?;
        caller.pc = next;
        self.registers
            .write(caller_index, caller.invocation, destination, value)?;
        self.reached(prefix, counters)
    }
}
