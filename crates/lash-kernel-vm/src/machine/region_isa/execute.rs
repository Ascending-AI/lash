use super::code::{Cost, Fee, Op, ReadSelector, Register};
use super::operands::Operand;
use super::registers::Registers;
use super::storage::View;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Counters {
    pub(super) charge: u64,
    pub(super) progress: u64,
    pub(super) balance: u64,
    pub(super) peak: u64,
    pub(super) depth: u32,
    pub(super) debt: u64,
}
pub(super) struct Frame {
    pub(super) program: CodeId,
    pub(super) invocation: u64,
    pub(super) pc: u32,
    pub(super) prefix: u32,
    pub(super) return_pc: u32,
    pub(super) activation: Option<CallActivation>,
}
pub(super) struct Execution<V: View> {
    pub(super) view: V,
    pub(super) arguments: Vec<V::Value>,
    pub(super) code: Arc<Code>,
    pub(super) programs: Arc<programs::Programs>,
    pub(super) registers: Registers<V::Value>,
    pub(super) source: super::source_counters::SourceCounters,
    pub(super) frames: Vec<Frame>,
    pub(super) reached: ReachedPrefix,
    pub(super) counters: Counters,
    pub(super) limits: Limits,
    pub(super) attempted: Option<Point>,
    pub(super) stop: Option<RegionStop>,
    pub(super) payload: Option<V::Value>,
    pub(super) faulted: bool,
    pub(super) lease: Rc<()>,
    pub(super) returning: Option<returns::Returning<V::Value>>,
    pub(super) entering: Option<u32>,
    pub(super) invocations: super::invocations::Invocations,
    #[cfg(test)]
    pub(super) executed: u64,
}

impl<V: View> Execution<V> {
    pub(super) fn acquire(
        code: Arc<Code>,
        limits: Limits,
        invocations: super::invocations::Invocations,
        view: V,
    ) -> Result<Self, RegionError> {
        let id = code.id;
        Self::acquire_programs(
            programs::Programs::bind(Box::new([code]))?,
            id,
            limits,
            invocations,
            view,
        )
    }
    pub(super) fn acquire_programs(
        programs: Arc<programs::Programs>,
        entry: CodeId,
        limits: Limits,
        invocations: super::invocations::Invocations,
        mut view: V,
    ) -> Result<Self, RegionError> {
        let code = Arc::clone(programs.code(entry)?);
        if code.invocation == 0
            || !invocations.follows(code.invocation)
            || limits.entry_balance > limits.memory
            || limits.entry_charge > limits.charge
            || limits.entry_depth > limits.depth
            || code.instructions.get(code.entry as usize).is_none()
            || code.recovery.get(code.entry_prefix as usize).is_none()
        {
            return Err(invalid(Refused::Facts));
        }
        let mut registers = Registers::new(programs.registers, 1)?;
        for (reg, value) in &code.initial {
            registers.write(
                &mut view,
                0,
                code.invocation,
                *reg,
                Operand::Immediate(*value),
            )?;
        }
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(1)
            .map_err(|_| invalid(Refused::Scratch))?;
        frames.push(Frame {
            program: entry,
            invocation: code.invocation,
            pc: code.entry,
            prefix: code.entry_prefix,
            return_pc: 0,
            activation: None,
        });
        let mut source = super::source_counters::SourceCounters::bind(&code.source_initial)?;
        let occurrence = source
            .prepare(&code.recovery[code.entry_prefix as usize].counters)?
            .accept();
        let reached = ReachedPrefix {
            program: entry,
            invocation: code.invocation,
            ordinal: 0,
            occurrence,
            prefix: code.entry_prefix,
            debt: 0,
        };
        let counters = Counters {
            charge: limits.entry_charge,
            progress: limits.entry_progress,
            balance: limits.entry_balance,
            peak: limits.entry_balance,
            depth: limits.entry_depth,
            debt: 0,
        };
        let mut arguments = Vec::new();
        arguments
            .try_reserve_exact(programs.arguments)
            .map_err(|_| invalid(Refused::Scratch))?;
        Ok(Self {
            view,
            arguments,
            code,
            programs,
            registers,
            source,
            frames,
            reached,
            counters,
            limits,
            attempted: None,
            stop: None,
            payload: None,
            faulted: false,
            lease: Rc::new(()),
            returning: None,
            entering: None,
            invocations,
            #[cfg(test)]
            executed: 0,
        })
    }

    pub(super) fn read(&self, reg: Register) -> Result<Operand<V::Value>, RegionError> {
        let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        self.registers
            .read(self.frames.len() - 1, frame.invocation, reg)
    }

    pub(super) fn value(&mut self, reg: Register) -> Result<V::Value, RegionError> {
        self.read(reg)?
            .bind(&mut self.view)
            .map_err(RegionError::Heap)
    }

    pub(super) fn fee(&self, fee: Fee) -> Result<u64, RegionError> {
        match fee {
            Fee::Storage { .. } => Err(invalid(Refused::Facts)),
            Fee::Fixed(units) => Ok(units),
            Fee::WordSize {
                value,
                zero,
                nonzero,
            } => match self.read(value)?.scalar(&self.view)? {
                Value::Int(0) => Ok(zero),
                Value::Int(_) => Ok(nonzero),
                _ => Err(invalid(Refused::Facts)),
            },
        }
    }

    pub(super) fn affordable(&self, cost: Cost) -> Result<Option<Counters>, RegionError> {
        let fee = if self.limits.charge_enabled {
            self.fee(cost.fee)?
        } else {
            0
        };
        Ok(Self::budget(self.counters, self.limits, cost, fee))
    }

    pub(super) fn budget(
        counters: Counters,
        limits: Limits,
        cost: Cost,
        fee: u64,
    ) -> Option<Counters> {
        let charge = counters.charge.checked_add(fee)?;
        let increment = if limits.charge_enabled {
            fee
        } else {
            cost.pace
        };
        let progress = counters.progress.checked_add(increment)?;
        let balance = i128::from(counters.balance)
            .checked_add(cost.delta)
            .and_then(|n| u64::try_from(n).ok())?;
        let temporary = counters.balance.checked_add(cost.peak)?;
        let depth = i64::from(counters.depth)
            .checked_add(i64::from(cost.depth))
            .and_then(|n| u32::try_from(n).ok())?;
        let debt = counters.debt.checked_add(1)?;
        let peak = counters.peak.max(temporary).max(balance);
        (charge <= limits.charge && peak <= limits.memory && depth <= limits.depth).then_some(
            Counters {
                charge,
                progress,
                balance,
                peak,
                depth,
                debt,
            },
        )
    }

    pub(super) fn refuse(&mut self, prefix: u32) -> Result<(), RegionError> {
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

    pub(super) fn reached(&mut self, prefix: u32, counters: Counters) -> Result<(), RegionError> {
        let map = self
            .code
            .recovery
            .get(prefix as usize)
            .ok_or_else(|| invalid(Refused::Exit))?;
        let ordinal = self
            .reached
            .ordinal
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let frame = self
            .frames
            .last_mut()
            .ok_or_else(|| invalid(Refused::Facts))?;
        let ready = self.source.prepare(&map.counters)?;
        let occurrence = ready.accept();
        frame.prefix = prefix;
        self.reached = ReachedPrefix {
            program: frame.program,
            invocation: frame.invocation,
            ordinal,
            occurrence,
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
            if self.counters.progress >= self.limits.slice {
                self.stop = Some(RegionStop::Slice);
                return Ok(RegionStop::Slice);
            }
            if self.returning.is_some() {
                match self.advance_return() {
                    Ok(()) => (),
                    Err(_) if self.stop == Some(RegionStop::Refusal) => {
                        return Ok(RegionStop::Refusal);
                    }
                    Err(error) => return Err(error),
                }
                if matches!(request, StopAt::NextLegalPrefix) {
                    self.stop = Some(RegionStop::Handoff);
                    return Ok(RegionStop::Handoff);
                }
                continue;
            }
            if let Some(site) = self.entering {
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
                if matches!(request, StopAt::NextLegalPrefix) {
                    self.stop = Some(RegionStop::Handoff);
                    return Ok(RegionStop::Handoff);
                }
                continue;
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
            if let Op::Heap { site } = op {
                match self.heap_step(site, prefix, next, cost) {
                    Ok(()) => (),
                    Err(_) if self.stop == Some(RegionStop::Refusal) => {
                        return Ok(RegionStop::Refusal);
                    }
                    Err(error) => return Err(error),
                }
                if let Some(stop) = self.stop {
                    return Ok(stop);
                }
            } else if let Op::Call { site } = op {
                match self.begin_enter(site) {
                    Ok(()) => (),
                    Err(_) if self.stop == Some(RegionStop::Refusal) => {
                        return Ok(RegionStop::Refusal);
                    }
                    Err(error) => return Err(error),
                }
            } else if let Op::Return { value } = op {
                if self.frames.len() == 1 {
                    let answer = self.value(value)?;
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
                    .programs
                    .code(activation.caller_program)?
                    .calls
                    .get(activation.plan as usize)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                if plan.result != value {
                    return Err(invalid(Refused::Facts));
                }
                match self.begin_return(activation) {
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
                    Op::Constant { dst, value } => Some((dst, Operand::Immediate(value))),
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
                        Some((dst, Operand::Immediate(answer)))
                    }
                    Op::Float {
                        kernel,
                        lhs,
                        rhs,
                        dst,
                    } => {
                        let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
                        let index = self.frames.len() - 1;
                        let lhs = self.registers.float(index, frame.invocation, lhs)?;
                        let rhs = self.registers.float(index, frame.invocation, rhs)?;
                        #[cfg(test)]
                        self.code
                            .semantic_operations
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let answer = if kernel.compares() {
                            lash_kernel_doc::scalar::float_truth(kernel, lhs, rhs).map(Value::Bool)
                        } else {
                            lash_kernel_doc::scalar::float_number(kernel, lhs, rhs)
                                .map(|n| Value::Float(lash_kernel_doc::Float::new(n)))
                        };
                        let Some(answer) = answer else {
                            self.stop = Some(RegionStop::Boundary);
                            return Ok(RegionStop::Boundary);
                        };
                        Some((dst, Operand::Immediate(answer)))
                    }
                    Op::Branch { test, yes, no } => {
                        jump = match self.read(Register::Bool(test))?.scalar(&self.view)? {
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
                        let actual = self.value(Register::Value(base))?;
                        let selector = match member {
                            ReadSelector::List(index) => super::storage::Selector::List(index),
                            ReadSelector::Table(key) => {
                                super::storage::Selector::Table(self.value(key)?)
                            }
                            ReadSelector::Field(index) => {
                                super::storage::Selector::Field(Arc::clone(
                                    self.code
                                        .fields
                                        .get(index as usize)
                                        .ok_or_else(|| invalid(Refused::Facts))?,
                                ))
                            }
                            ReadSelector::Variable => super::storage::Selector::Variable,
                        };
                        let value = match self.view.read(actual, &selector) {
                            Ok(value) => value,
                            Err(lash_kernel_state::kernel_internals::logical::LeaseFault::BoundaryRequired) => {
                                self.stop = Some(RegionStop::Boundary);
                                return Ok(RegionStop::Boundary);
                            }
                            Err(error) => return Err(storage_fault(error)),
                        };
                        Some((dst, Operand::Dynamic(value)))
                    }

                    Op::Throw { value } => {
                        thrown = Some(self.value(value)?);
                        None
                    }
                    Op::Meter | Op::Exit => None,
                    Op::Heap { .. } | Op::Call { .. } | Op::Return { .. } | Op::Boundary => {
                        unreachable!("handled before scalar dispatch")
                    }
                };
                if let Some((dst, value)) = write {
                    let frame = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
                    self.registers.write(
                        &mut self.view,
                        self.frames.len() - 1,
                        frame.invocation,
                        dst,
                        value,
                    )?;
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

    fn begin_enter(&mut self, site: u32) -> Result<(), RegionError> {
        if self.entering.is_some() || self.returning.is_some() {
            return Err(invalid(Refused::Facts));
        }
        let caller = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        let instruction = self
            .code
            .instructions
            .get(caller.pc as usize)
            .ok_or_else(|| invalid(Refused::Path))?;
        if !matches!(instruction.op, Op::Call { site: actual } if actual == site) {
            return Err(invalid(Refused::Facts));
        }
        let plan = self
            .code
            .calls
            .get(site as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        for (src, _) in &plan.arguments {
            self.read(*src)?;
        }
        let prefix = plan.before_prefix;
        let Some(counters) = self.affordable(plan.before)? else {
            self.refuse(prefix)?;
            return Err(invalid(Refused::Peak));
        };
        self.reached(prefix, counters)?;
        self.entering = Some(site);
        Ok(())
    }

    pub(super) fn enter_call(&mut self, site: u32) -> Result<CallActivation, RegionError> {
        if self.entering.is_none() {
            self.begin_enter(site)?;
        }
        if self.entering != Some(site) {
            return Err(invalid(Refused::Facts));
        }
        let caller = self.frames.last().ok_or_else(|| invalid(Refused::Facts))?;
        let caller_program = caller.program;
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
        for (src, _) in &plan.arguments {
            self.read(*src)?;
        }
        let Some(counters) = self.affordable(cost)? else {
            self.refuse(prefix)?;
            return Err(invalid(Refused::Peak));
        };
        let reserved = self.invocations.prepare()?;
        let invocation = reserved.id;
        let depth = self.frames.len();
        self.frames
            .try_reserve(1)
            .map_err(|_| invalid(Refused::Scratch))?;
        self.registers.reserve_frame(depth)?;
        self.registers.clear(depth)?;
        for (src, dst) in &plan.arguments {
            let value = self.read(*src)?;
            self.registers
                .write(&mut self.view, depth, invocation, *dst, value)?;
        }
        let (entry, entry_prefix) = (plan.entry, plan.prefix);
        let target_program = plan.target;
        let target_code = Arc::clone(self.programs.code(target_program)?);
        self.frames
            .last_mut()
            .ok_or_else(|| invalid(Refused::Facts))?
            .prefix = prefix;
        self.frames.push(Frame {
            program: target_program,
            invocation,
            pc: entry,
            prefix: entry_prefix,
            return_pc: next,
            activation: None,
        });
        self.invocations.accept(reserved);
        self.code = target_code;
        self.entering = None;
        self.reached(entry_prefix, counters)?;
        Ok(CallActivation {
            lease: Rc::clone(&self.lease),
            invocation,
            depth,
            plan: site,
            caller_program,
        })
    }
}
