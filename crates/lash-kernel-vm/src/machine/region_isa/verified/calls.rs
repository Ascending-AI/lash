//! Calls borrow the current lease and reuse storage by active guest depth.
use super::super::{operands::Operand, *};
use super::{Code, Running, charge};
use crate::compile::region_isa::{CheckedKind, Pc, PrefixId, RegionOp};
use crate::compile::room::charges::region_isa::ChargeMoment;
use lash_kernel_state::kernel_internals::logical::{LeaseAllocation, LeaseMember, LeaseOp};

pub(super) struct Parked {
    pub(super) callee: lash_kernel_state::kernel_internals::logical::Invocation,
    pub(super) code: Arc<Code>,
    pub(super) pc: Pc,
    pub(super) prefix: PrefixId,
    pub(super) reached: ReachedPrefix,
    pub(super) activation: Option<CallActivation>,
    pub(super) site: u32,
    pub(super) target: CodeId,
}

impl Running<'_> {
    pub(in crate::machine::region_isa) fn enter_call(
        &mut self,
        site: u32,
    ) -> Result<CallActivation, RegionError> {
        match self.prepare_call(site) {
            Ok(result) => result.ok_or_else(|| invalid(Refused::Peak)),
            Err(error) => {
                self.faulted = true;
                Err(error)
            }
        }
    }

    /// None retains the exact affordable caller prefix. No callee identity,
    /// root move or depth increment is accepted on an admission miss.
    pub(super) fn prepare_call(
        &mut self,
        site: u32,
    ) -> Result<Option<CallActivation>, RegionError> {
        if self.faulted {
            return Err(invalid(Refused::Facts));
        }
        let caller = Arc::clone(&self.code);
        let Some(RegionOp::Call { site: actual, .. }) = caller.program.op(self.pc) else {
            return Err(invalid(Refused::Path));
        };
        if actual.index() != site {
            return Err(invalid(Refused::Path));
        }
        let call = caller.call(*actual)?;
        // A statement call loads its atoms, which the caller holds until the
        // call's Commit. A refused entry leaves them outside its prefix.
        for load in &call.atoms {
            let operand = self.atom(&load.src)?;
            self.write(load.dst, operand)?;
        }
        self.call_captures.clear();
        // A callee read from a captured variable's own register reads its
        // cell; the call takes the function the cell holds.
        let mut current = |register| -> Result<_, RegionError> {
            let value = self
                .registers
                .read(self.frame_index(), self.reached.invocation, register)?
                .bind(&mut self.view)
                .map_err(RegionError::Heap)?;
            if self.view.kind(value).map_err(RegionError::Heap)? != lash_kernel_doc::ValueKind::Ref
            {
                return Ok(value);
            }
            let object = self.view.object(value).map_err(RegionError::Heap)?;
            self.view
                .read(object, &LeaseMember::Variable)
                .map_err(storage_fault)
        };
        let callable = call.callable.map(&mut current).transpose()?;
        let plan = if let Some(callable) = callable {
            let origin = current(
                call.callable_origin
                    .ok_or_else(|| invalid(Refused::Facts))?,
            )?;
            let same = match (self.view.inline(origin), self.view.inline(callable)) {
                (Ok(Some(a)), Ok(Some(b))) => a.same(&b),
                (Ok(None), Ok(None)) => {
                    self.view.object(origin).map_err(RegionError::Heap)?
                        == self.view.object(callable).map_err(RegionError::Heap)?
                }
                (Err(error), _) | (_, Err(error)) => return Err(RegionError::Heap(error)),
                _ => false,
            };
            if !same {
                return Err(invalid(Refused::Facts));
            }
            let Some(binding) = call
                .plan
                .bind_callable(&caller.exe, &mut self.view, callable)
                .map_err(storage_fault)?
            else {
                self.stop = Some(RegionStop::Boundary);
                return Ok(None);
            };
            self.call_captures
                .extend(binding.capture_roles().map(|(_, value)| value));
            let plan = Arc::clone(binding.plan());
            // A dynamic target this lease has not bound stops before the
            // call, unbilled: the machine binds it beside the lease's
            // others and runs on from here.
            if !self
                .programs
                .binds(caller.program.code(), site, plan.code())
            {
                if !caller
                    .exe
                    .reach_dynamic_target(caller.program.code(), site, plan.code())
                {
                    return Err(invalid(Refused::Facts));
                }
                self.call_captures.clear();
                self.stop = Some(RegionStop::Bind);
                return Ok(None);
            }
            plan
        } else {
            Arc::clone(
                call.plan
                    .static_target()
                    .ok_or_else(|| invalid(Refused::Facts))?,
            )
        };
        let programs = Arc::clone(&self.programs);
        let target = programs.target(caller.program.code(), site, plan.code())?;
        if target.captures.len() != self.call_captures.len() {
            return Err(invalid(Refused::Facts));
        }
        let callee = Arc::clone(self.programs.code(plan.code())?);
        let entry_pc = callee.program.entry();
        let entry = callee
            .program
            .step(entry_pc)
            .ok_or_else(|| invalid(Refused::Path))?;
        let Some(RegionOp::Checked { kernel, next, .. }) = callee.program.op(entry_pc) else {
            return Err(invalid(Refused::Source));
        };
        if !matches!(kernel.kind(), CheckedKind::Phase)
            || &entry.at != plan.enter()
            || entry.peak.fixed != i128::from(plan.entry_price())
            || entry.delta.fixed != i128::from(plan.entry_price())
            || !entry.peak.terms.is_empty()
            || !entry.delta.terms.is_empty()
            || !entry.charges.is_empty()
        {
            return Err(invalid(Refused::Source));
        }
        let (next, prefix) = (*next, entry.after);
        let step = caller
            .program
            .step(self.pc)
            .ok_or_else(|| invalid(Refused::Path))?;
        let depth = self.frame_index();
        let guard = self.hook_guard(depth, &caller, call)?;
        target.inputs(
            &self.registers,
            depth,
            self.reached.invocation,
            &mut self.view,
            &mut self.call_values,
            guard,
        )?;
        // A checked helper's arguments fit its signature before entry. The
        // region shows a scalar fit itself; anything else stands unbilled
        // for the machine, which raises or serves the check.
        if let Some(lib) = call.plan.checked() {
            let served = match self.served.take() {
                Some((at, None)) if at == step.at => true,
                Some(_) => return Err(invalid(Refused::Facts)),
                None => false,
            };
            if !served
                && !self.arguments_fit(
                    &caller.exe.lib(lib).header.signature,
                    call.plan.arguments().len(),
                )?
            {
                self.call_captures.clear();
                self.stop = Some(RegionStop::Check);
                return Ok(None);
            }
        }
        let counters = self.counters;
        let payment = charge::billed(self.limiters, &mut self.limits, |limits| {
            charge::bill(counters, limits, &step.charges, ChargeMoment::Before, None)
        })?;
        self.counters = payment.counters;
        if let Some(crossing) = payment.crossing {
            self.refusal = Some(crate::Bound::Charge);
            self.crossing = Some(crossing);
            self.attempted = Some(step.at.clone());
            self.stop = Some(RegionStop::Refusal);
            return Ok(None);
        }
        // The caller's depth counts its own frame: a guest call is admitted
        // while that depth, with any prepared operation depth, is in bound.
        let required_depth = self.counters.depth.checked_add(step.depth);
        if required_depth.is_none_or(|depth| depth > self.limits.depth) {
            self.refusal = Some(crate::Bound::CallDepth);
            self.attempted = Some(step.at.clone());
            self.stop = Some(RegionStop::Refusal);
            return Ok(None);
        }
        let reserved = self.invocations.prepare()?;
        // This checks the actual D reservation before reusing a frame bank.
        let identity = self.invocations.next_identity(reserved)?;
        let frame = depth
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        self.parked
            .try_reserve(1)
            .map_err(|_| invalid(Refused::Scratch))?;
        self.registers.reserve_frame(frame)?;
        self.registers.clear(frame)?;
        let required_loops = frame
            .checked_add(1)
            .and_then(|frames| frames.checked_mul(self.programs.loops))
            .ok_or_else(|| invalid(Refused::Scratch))?;
        if required_loops > self.loops.len() {
            let capacity = required_loops.max(self.loops.len().saturating_mul(2));
            self.loops
                .try_reserve(capacity - self.loops.len())
                .map_err(|_| invalid(Refused::Scratch))?;
            self.loops
                .resize(capacity, super::control::LoopState::default());
        }
        let loop_start = frame * self.programs.loops;
        self.loops[loop_start..loop_start + self.programs.loops]
            .fill(super::control::LoopState::default());
        for (parameter, value) in target.parameters.iter().zip(&self.call_values) {
            // A shared parameter's register takes its cell once the frame
            // is entered; it must be a value register.
            if parameter.shared {
                if !matches!(parameter.output, code::Register::Value(_)) {
                    return Err(invalid(Refused::Facts));
                }
                continue;
            }
            self.registers.write(
                &mut self.view,
                frame,
                reserved.id,
                parameter.output,
                Operand::Dynamic(*value),
            )?;
        }
        for ((_, output), value) in target.captures.iter().zip(&self.call_captures) {
            self.registers.write(
                &mut self.view,
                frame,
                reserved.id,
                *output,
                Operand::Dynamic(*value),
            )?;
        }
        let holds = &mut self.scratch.holds;
        let releases = &mut self.scratch.releases;
        holds.clear();
        holds.extend(self.call_values.iter().map(|value| (*value, false)));
        holds.extend(self.call_captures.iter().map(|value| (*value, true)));
        releases.clear();
        // A nested call moves its callable and arguments into the callee;
        // a statement call's caller holds its atoms beside the callee's.
        let rooted = call.plan.rooted();
        if let Some(callable) = callable
            && !rooted
        {
            releases.push((callable, false));
        }
        for &argument in &call.arguments {
            let value = self
                .registers
                .read(depth, self.reached.invocation, argument)?
                .bind(&mut self.view)
                .map_err(RegionError::Heap)?;
            // `next`, `check` and `item` read what the loop keeps: only the
            // callee's parameter holds it besides.
            if rooted && call.plan.hook().is_some() {
                continue;
            }
            if rooted {
                holds.push((value, false));
            } else {
                releases.push((value, false));
            }
        }
        let phase = self
            .view
            .stage(LeaseOp::FrameTransfer {
                holds,
                releases,
                entering: true,
            })
            .map_err(storage_fault)?;
        if plan.frame_price() != lash_kernel_doc::LogicalHeldV1::FRAME {
            return Err(invalid(Refused::Facts));
        }
        // The shared parameters' cells, allocated right after the frame.
        let cells = plan
            .entry_price()
            .checked_sub(plan.frame_price())
            .ok_or_else(|| invalid(Refused::Facts))?;
        let delta = phase.facts().delta();
        let balance = i128::from(self.counters.balance)
            .checked_add(delta)
            .and_then(|balance| u64::try_from(balance).ok())
            .ok_or_else(|| invalid(Refused::Facts))?;
        let peak = self
            .counters
            .balance
            .checked_add(phase.facts().peak())
            .ok_or_else(|| invalid(Refused::Peak))?
            .max(balance)
            .max(self.counters.peak);
        let entered = balance
            .checked_add(cells)
            .ok_or_else(|| invalid(Refused::Peak))?;
        if !charge::fits_memory(self.limiters, &mut self.limits, peak.max(entered)) {
            drop(phase);
            self.refusal = Some(crate::Bound::Memory);
            self.attempted = Some(step.at.clone());
            self.stop = Some(RegionStop::Refusal);
            return Ok(None);
        }
        let debt = self
            .counters
            .debt
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let ordinal = self
            .reached
            .ordinal
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let activation = CallActivation {
            lease: Rc::clone(&self.identity),
            invocation: reserved.id,
            depth: frame,
            plan: site,
            caller_program: caller.program.code(),
        };
        let counter = self.source.prepare(&call.counter)?;
        let mut reached = self.reached;
        // The caller's suspended Pending carries the occurrence issued by
        // this entry, while the callee starts its own source coordinates.
        let ready = phase.reserve().map_err(RegionError::Heap)?;
        reached.occurrence = counter.accept();
        reached.prefix = step.after.index() as u32;
        let parked = Parked {
            callee: identity,
            code: Arc::clone(&caller),
            pc: self.pc,
            prefix: step.after,
            reached,
            activation: self.activation.take(),
            site,
            target: plan.code(),
        };
        // All bank writes, allocation and checks precede this affine accept.
        ready.accept();
        let (balance, peak) = self.enter_cells(target, frame, reserved.id, balance, peak)?;
        self.release_atoms()?;
        self.parked.push(parked);
        self.invocations.accept(reserved);
        self.code = callee;
        self.pc = next;
        self.prefix = prefix;
        self.reached = ReachedPrefix {
            program: self.code.program.code(),
            invocation: reserved.id,
            ordinal,
            occurrence: None,
            prefix: prefix.index() as u32,
            debt,
        };
        self.counters = Counters {
            balance,
            peak,
            depth: self.counters.depth + 1,
            debt,
            ..self.counters
        };
        self.stop = None;
        Ok(Some(activation))
    }
}

impl Running<'_> {
    /// Whether the call's `count` arguments, read into the call values,
    /// visibly fit `signature`: each typed parameter a scalar of its kind.
    fn arguments_fit(
        &mut self,
        signature: &lash_kernel_value::Signature,
        count: usize,
    ) -> Result<bool, RegionError> {
        use lash_kernel_doc::ValueKind;
        use lash_kernel_value::Type;
        if !(signature.required()..=signature.params.len()).contains(&count) {
            return Ok(false);
        }
        for (param, value) in signature.params.iter().zip(&self.call_values).take(count) {
            if matches!(param.ty, Type::Any) {
                continue;
            }
            let kind = self.view.kind(*value).map_err(RegionError::Heap)?;
            let fits = match param.ty {
                Type::Bool => kind == ValueKind::Bool,
                Type::Int => kind == ValueKind::Int,
                Type::Float => kind == ValueKind::Float,
                Type::Number => matches!(kind, ValueKind::Int | ValueKind::Float),
                Type::Text => kind == ValueKind::Text,
                _ => false,
            };
            if !fits {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// Each shared parameter's argument moves into a fresh cell in one
    /// atomic phase, and the cell becomes the parameter's register. The
    /// call's memory check already covered these cells.
    fn enter_cells(
        &mut self,
        target: &super::call_bind::Target,
        frame: usize,
        invocation: u64,
        mut balance: u64,
        mut peak: u64,
    ) -> Result<(u64, u64), RegionError> {
        for (parameter, value) in target.parameters.iter().zip(&self.call_values) {
            if !parameter.shared {
                continue;
            }
            let code::Register::Value(index) = parameter.output else {
                return Err(invalid(Refused::Facts));
            };
            let releases = std::slice::from_ref(value);
            let phase = self
                .view
                .stage(LeaseOp::AllocateMove {
                    allocation: LeaseAllocation::cell(value),
                    releases,
                })
                .map_err(storage_fault)?;
            let after = i128::from(balance)
                .checked_add(phase.facts().delta())
                .and_then(|balance| u64::try_from(balance).ok())
                .ok_or_else(|| invalid(Refused::Facts))?;
            peak = balance
                .checked_add(phase.facts().peak())
                .ok_or_else(|| invalid(Refused::Peak))?
                .max(after)
                .max(peak);
            if !charge::fits_memory(self.limiters, &mut self.limits, peak) {
                return Err(invalid(Refused::Peak));
            }
            let write =
                self.registers
                    .prepare_dynamic(frame, invocation, code::Register::Value(index))?;
            let ready = phase.reserve().map_err(RegionError::Heap)?;
            let cell = ready.accept();
            if cell.is_none() {
                return Err(invalid(Refused::Facts));
            }
            write.publish(cell);
            balance = after;
        }
        Ok((balance, peak))
    }
}

impl Running<'_> {
    /// The counts a protocol `for`'s `check` call takes after what the loop
    /// walks (`K-ITER-005`), from its guard in the frame at `frame`; `None`
    /// for any other call.
    pub(super) fn hook_guard(
        &self,
        frame: usize,
        code: &Code,
        call: &super::bind::Call,
    ) -> Result<Option<super::call_bind::GuardArguments>, RegionError> {
        if call.plan.hook() != Some(crate::compile::room::Phase::Check) {
            return Ok(None);
        }
        let index = code
            .program
            .loop_at(&call.plan.point().site)
            .ok_or_else(|| invalid(Refused::Facts))?;
        frame
            .checked_mul(self.programs.loops)
            .and_then(|base| base.checked_add(index as usize))
            .and_then(|at| self.loops.get(at))
            .and_then(|state| state.guard)
            .map(Some)
            .ok_or_else(|| invalid(Refused::Facts))
    }
}

impl super::call_bind::GuardArguments {
    /// The counts as `check` takes them: size, remaining, more.
    pub(super) fn values(self) -> [Value; 3] {
        [
            crate::machine::loops::counted(self.size),
            crate::machine::loops::counted(self.remaining),
            Value::Bool(self.more),
        ]
    }
}
