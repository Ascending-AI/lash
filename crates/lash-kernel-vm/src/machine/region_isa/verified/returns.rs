//! Return, entered-scope Leaves, frame Leave and caller Restore stay ordered.
use super::super::{operands::Operand, *};
use super::{Running, charge};
use crate::compile::region_isa::RegionOp;
use lash_kernel_state::kernel_internals::logical::LeaseValue;

pub(super) struct Departure {
    pub(super) origin: Point,
    pub(super) value: LeaseValue,
    pub(super) completion: u64,
}
pub(super) struct Incoming {
    pub(super) value: LeaseValue,
    pub(super) completion: u64,
    pub(super) paid: bool,
    pub(super) site: u32,
}
impl Running<'_> {
    fn authenticate_activation(&self) -> Result<(), RegionError> {
        match (self.parked.last(), self.activation.as_ref()) {
            (None, None) => Ok(()),
            (Some(parked), Some(activation))
                if Rc::ptr_eq(&activation.lease, &self.identity)
                    && activation.depth == self.parked.len()
                    && activation.invocation == self.reached.invocation
                    && activation.caller_program == parked.code.program.code()
                    && activation.plan == parked.site
                    && self.code.program.code() == parked.target =>
            {
                Ok(())
            }
            _ => Err(invalid(Refused::Facts)),
        }
    }
    pub(super) fn export_return(&mut self, value: LeaseValue) -> Result<bool, RegionError> {
        use lash_kernel_state::kernel_internals::logical::Owner;
        if !self.parked.is_empty() || !self.limits.charge_enabled {
            return Ok(true);
        }
        let Owner::Frame { task, .. } = self
            .canonical
            .logical
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?
            .invocation()
            .frame()
        else {
            return Err(invalid(Refused::Facts));
        };
        if task != lash_kernel_doc::TaskId::MAIN
            || matches!(
                self.canonical.convention,
                Some(
                    lash_kernel_doc::StartConvention::TsDirect { .. }
                        | lash_kernel_doc::StartConvention::TsArgsList { .. }
                )
            )
        {
            return Ok(true);
        }
        let size = self
            .view
            .measure(value, lash_kernel_doc::Measure::DeepSize)
            .map_err(storage_fault)?;
        let counters = self.counters;
        let payment = charge::billed(self.limiters, &mut self.limits, |limits| {
            Ok(charge::export(counters, limits, size))
        })?;
        self.counters = payment.counters;
        if let Some(crossing) = payment.crossing {
            self.refusal = Some(crate::Bound::Charge);
            self.crossing = Some(crossing);
            self.attempted = Some(
                self.code
                    .program
                    .step(self.pc)
                    .ok_or_else(|| invalid(Refused::Path))?
                    .at
                    .clone(),
            );
            self.stop = Some(RegionStop::Refusal);
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) fn quote_return(&mut self, value: LeaseValue) -> Result<Departure, RegionError> {
        self.authenticate_activation()?;
        let completion = if let Some(parked) = self.parked.last() {
            let call = parked
                .code
                .calls
                .get(parked.site as usize)
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let plan = call
                .plan
                .targets()
                .iter()
                .find(|plan| plan.code() == parked.target)
                .ok_or_else(|| invalid(Refused::Facts))?;
            self.call_values.clear();
            if plan.debt().needs_arguments() {
                if let Some(frame) = self
                    .canonical_callees
                    .get(self.parked.len() - 1)
                    .filter(|frame| frame.invocation == self.reached.invocation)
                {
                    let library = frame
                        .library
                        .as_ref()
                        .ok_or_else(|| invalid(Refused::Facts))?;
                    for value in library.args.iter().take(call.arguments.len()) {
                        self.call_values
                            .push(self.view.import(value).map_err(RegionError::Heap)?);
                    }
                } else {
                    for register in &call.arguments {
                        self.call_values.push(
                            self.registers
                                .read(self.parked.len() - 1, parked.reached.invocation, *register)?
                                .bind(&mut self.view)
                                .map_err(RegionError::Heap)?,
                        );
                    }
                }
            }
            plan.debt()
                .completion(&mut self.view, &self.call_values, value)
                .map_err(storage_fault)?
                .ok_or_else(|| invalid(Refused::Facts))?
        } else if let Some(library) = self
            .canonical
            .library
            .as_ref()
            .filter(|_| self.limits.charge_enabled)
        {
            // K-CHG-003: a root library frame, a spawned task's, bills its
            // own formula over its arguments and result when it leaves.
            let code = Arc::clone(&self.code);
            let function = code.exe.lib(library.lib);
            self.call_values.clear();
            for argument in &library.args {
                let argument = self.view.import(argument).map_err(RegionError::Heap)?;
                self.call_values.push(argument);
            }
            function
                .charge
                .lease(&mut self.view, &self.call_values, value)
                .map_err(storage_fault)?
        } else {
            0
        };
        Ok(Departure {
            origin: self
                .code
                .program
                .step(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?
                .at
                .clone(),
            value,
            completion,
        })
    }
    pub(super) fn finish_frame(&mut self) -> Result<(), RegionError> {
        self.authenticate_activation()?;
        if self.parked.is_empty()
            && (self.canonical.cleaning.is_some()
                || self
                    .canonical
                    .public_result
                    .as_ref()
                    .is_some_and(|ty| !matches!(ty, lash_kernel_value::types::Type::Any))
                || !matches!(
                    self.canonical.convention,
                    None | Some(
                        lash_kernel_doc::StartConvention::Direct
                            | lash_kernel_doc::StartConvention::PyArgsList { .. }
                    )
                )
                || (self.canonical.convention.is_none()
                    && self
                        .code
                        .exe
                        .root_cleanup(self.code.program.code())
                        .is_some()))
        {
            return Err(invalid(Refused::Facts));
        }
        self.activation.take();
        let departing = self
            .departing
            .take()
            .ok_or_else(|| invalid(Refused::Facts))?;
        let step = self
            .code
            .program
            .step(self.pc)
            .ok_or_else(|| invalid(Refused::Path))?;
        if step.at.site != self.code.program.source().site {
            return Err(invalid(Refused::Path));
        }
        if self.parked.is_empty() {
            self.closed_sessions = Some(self.session_values()?);
        }
        self.registers.clear(self.frame_index())?;
        self.counters.depth = self
            .counters
            .depth
            .checked_sub(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let Some(parked) = self.parked.pop() else {
            self.returning = Some(Incoming {
                value: departing.value,
                completion: 0,
                paid: true,
                site: 0,
            });
            self.complete = true;
            self.stop = Some(RegionStop::Complete);
            return Ok(());
        };
        self.returning = Some(Incoming {
            value: departing.value,
            completion: departing.completion,
            paid: false,
            site: parked.site,
        });
        self.code = parked.code;
        self.pc = parked.pc;
        self.prefix = self
            .code
            .program
            .step(self.pc)
            .ok_or_else(|| invalid(Refused::Path))?
            .pending;
        self.reached = ReachedPrefix {
            ordinal: self.reached.ordinal,
            debt: self.counters.debt,
            prefix: self.prefix.index() as u32,
            ..parked.reached
        };
        self.activation = parked.activation;
        Ok(())
    }
    pub(super) fn restore_call(&mut self) -> Result<bool, RegionError> {
        let incoming = self
            .returning
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?;
        let code = Arc::clone(&self.code);
        let call = code
            .calls
            .get(incoming.site as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid(Refused::Facts))?;
        if !incoming.paid {
            let step = code
                .program
                .step(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?;
            let counters = self.counters;
            let completion = incoming.completion;
            let payment = charge::billed(self.limiters, &mut self.limits, |limits| {
                charge::bill(
                    counters,
                    limits,
                    &step.charges,
                    crate::compile::room::charges::region_isa::ChargeMoment::After,
                    Some(completion),
                )
            })?;
            self.counters = payment.counters;
            if let Some(crossing) = payment.crossing {
                self.refusal = Some(crate::Bound::Charge);
                self.crossing = Some(crossing);
                self.attempted = Some(call.plan.resume().clone());
                self.stop = Some(RegionStop::Refusal);
                return Ok(false);
            }
            self.returning
                .as_mut()
                .ok_or_else(|| invalid(Refused::Facts))?
                .paid = true;
        }
        let value = self
            .returning
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?
            .value;
        let write = self.registers.prepare_write(
            &mut self.view,
            self.parked.len(),
            self.reached.invocation,
            super::bind::physical(
                &code.program,
                code.program
                    .call_incoming(match code.program.op(self.pc) {
                        Some(RegionOp::Call { site, .. }) => *site,
                        _ => return Err(invalid(Refused::Path)),
                    })
                    .ok_or_else(|| invalid(Refused::Facts))?,
            )?,
            Operand::Dynamic(value),
        )?;
        self.registers.publish_write(write);
        let step = code
            .program
            .step(self.pc)
            .ok_or_else(|| invalid(Refused::Path))?;
        self.prefix = step.after;
        self.pc = call.next;
        self.reached.ordinal = self
            .reached
            .ordinal
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        self.counters.debt = self
            .counters
            .debt
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        self.reached.prefix = self.prefix.index() as u32;
        self.reached.debt = self.counters.debt;
        self.returning = None;
        Ok(true)
    }
    pub(in crate::machine::region_isa) fn leave_call(
        &mut self,
        activation: CallActivation,
    ) -> Result<(), RegionError> {
        let parked = self.parked.last().ok_or_else(|| invalid(Refused::Facts))?;
        if self.faulted
            || self.departing.is_some()
            || self.returning.is_some()
            || !Rc::ptr_eq(&activation.lease, &self.identity)
            || activation.depth != self.parked.len()
            || activation.invocation != self.reached.invocation
            || activation.caller_program != parked.code.program.code()
            || activation.plan != parked.site
            || self.code.program.code() != parked.target
            || !matches!(self.code.program.op(self.pc), Some(RegionOp::Return { .. }))
        {
            self.faulted = true;
            return Err(invalid(Refused::Facts));
        }
        let depth = self.parked.len();
        self.activation = Some(activation);
        loop {
            let stop = self.execute(StopAt::NextLegalPrefix)?;
            if self.parked.len() < depth && self.returning.is_none() {
                return Ok(());
            }
            if stop != RegionStop::Handoff {
                return Ok(());
            }
        }
    }
}
