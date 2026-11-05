//! Calls borrow the current lease and reuse storage by active guest depth.
use super::super::{operands::Operand, *};
use super::{Code, Running, charge};
use crate::compile::region_isa::{CheckedKind, Pc, PrefixId, RegionOp};
use crate::compile::room::charges::region_isa::ChargeMoment;
use lash_kernel_state::kernel_internals::logical::LeaseOp;

pub(super) struct Parked {
    pub(super) code: Arc<Code>,
    pub(super) pc: Pc,
    pub(super) prefix: PrefixId,
    pub(super) reached: ReachedPrefix,
    pub(super) activation: Option<CallActivation>,
    pub(super) site: u32,
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
        let plan = call
            .plan
            .static_target()
            .ok_or_else(|| invalid(Refused::Facts))?;
        let target = self
            .programs
            .target(caller.program.code(), site, plan.code())?;
        // Shared parameters require A's distinct cell phases. Captures and
        // callable dispatch use the same bank after their binding is installed.
        if target.parameters.iter().any(|parameter| parameter.shared)
            || !target.captures.is_empty()
            || plan.debt().before_entry() != 0
        {
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
            || entry.peak.fixed != i128::from(plan.frame_price())
            || entry.delta.fixed != i128::from(plan.frame_price())
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
        let payment = charge::bill(
            self.counters,
            self.limits,
            &step.charges,
            ChargeMoment::Before,
            None,
        )?;
        self.counters = payment.counters;
        if let Some(crossing) = payment.crossing {
            self.crossing = Some(crossing);
            self.attempted = Some(step.at.clone());
            self.stop = Some(RegionStop::Refusal);
            return Ok(None);
        }
        let required_depth = self.counters.depth.checked_add(step.depth.max(1));
        if required_depth.is_none_or(|depth| depth > self.limits.depth) {
            self.attempted = Some(step.at.clone());
            self.stop = Some(RegionStop::Refusal);
            return Ok(None);
        }
        let depth = self.frame_index();
        target.inputs(
            &self.registers,
            depth,
            self.reached.invocation,
            &mut self.view,
            &mut self.call_values,
        )?;
        let reserved = self.invocations.prepare()?;
        // This checks the actual D reservation before reusing a frame bank.
        self.invocations.next_identity(reserved)?;
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
            self.registers.write(
                &mut self.view,
                frame,
                reserved.id,
                parameter.output,
                Operand::Dynamic(*value),
            )?;
        }
        let holds = &mut self.scratch.holds;
        let releases = &mut self.scratch.releases;
        holds.clear();
        holds.extend(self.call_values.iter().map(|value| (*value, false)));
        releases.clear();
        for &argument in &call.arguments {
            let value = self
                .registers
                .read(depth, self.reached.invocation, argument)?
                .bind(&mut self.view)
                .map_err(RegionError::Heap)?;
            releases.push((value, false));
        }
        let phase = self
            .view
            .stage(LeaseOp::Transfer { holds, releases })
            .map_err(storage_fault)?;
        let delta = phase
            .facts()
            .delta()
            .checked_add(i128::from(plan.frame_price()))
            .ok_or_else(|| invalid(Refused::Facts))?;
        let balance = i128::from(self.counters.balance)
            .checked_add(delta)
            .and_then(|balance| u64::try_from(balance).ok())
            .ok_or_else(|| invalid(Refused::Facts))?;
        let peak = self
            .counters
            .balance
            .checked_add(plan.frame_price())
            .and_then(|peak| peak.checked_add(phase.facts().peak()))
            .ok_or_else(|| invalid(Refused::Peak))?
            .max(balance)
            .max(self.counters.peak);
        if peak > self.limits.memory {
            drop(phase);
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
        let parked = Parked {
            code: Arc::clone(&caller),
            pc: self.pc,
            prefix: self.prefix,
            reached: self.reached,
            activation: self.activation.take(),
            site,
        };
        // All bank writes, allocation and checks precede this affine accept.
        phase.reserve().map_err(RegionError::Heap)?.accept();
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
        self.counters = execute::Counters {
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
