//! Indexed dispatch advances only after its actual answer and local phase agree.
use super::super::*;
use super::{Running, charge};
use crate::compile::region_isa::RegionOp;
use crate::compile::room::charges::region_isa::ChargeMoment;

impl Running<'_> {
    pub(super) fn execute(&mut self, request: StopAt) -> Result<RegionStop, RegionError> {
        if self.faulted {
            return Err(invalid(Refused::Facts));
        }
        if let Some(stop) = &self.stop
            && *stop != RegionStop::Handoff
        {
            return Ok(stop.clone());
        }
        self.stop = None;
        let outcome = self.dispatch(request);
        if outcome.is_err() {
            self.faulted = true;
        }
        outcome
    }

    /// The charge for copying the value in `value` out of the run.
    fn copy_charge<T>(
        &mut self,
        counters: Counters,
        value: crate::compile::region_isa::Reg<T>,
    ) -> Result<charge::Payment, RegionError> {
        if !self.limits.charge_enabled {
            return Ok(charge::Payment {
                counters,
                crossing: None,
            });
        }
        let value = self.value(value)?;
        let size = self
            .view
            .measure(value, lash_kernel_doc::Measure::DeepSize)
            .map_err(storage_fault)?;
        charge::billed(self.limiters, &mut self.limits, |limits| {
            Ok(charge::export(counters, limits, size))
        })
    }

    fn halt(&mut self, reason: RegionStop) -> Result<RegionStop, RegionError> {
        self.release_atoms()?;
        self.stop = Some(reason.clone());
        Ok(reason)
    }

    fn prefix_stop(&mut self, request: StopAt) -> Option<RegionStop> {
        let stop = if self.counters.progress >= self.limits.slice {
            RegionStop::Slice
        } else if matches!(request, StopAt::NextLegalPrefix) {
            RegionStop::Handoff
        } else {
            return None;
        };
        self.stop = Some(stop.clone());
        Some(stop)
    }

    fn dispatch(&mut self, request: StopAt) -> Result<RegionStop, RegionError> {
        loop {
            if self.counters.progress >= self.limits.slice {
                return self.halt(RegionStop::Slice);
            }
            if self.returning.is_some() {
                if self.complete {
                    return self.halt(RegionStop::Complete);
                }
                if !self.restore_call()? {
                    return self.halt(RegionStop::Refusal);
                }
                if let Some(stop) = self.prefix_stop(request) {
                    return Ok(stop);
                }
                continue;
            }
            let code = Arc::clone(&self.code);
            let step = code
                .program
                .step(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?;
            let op = code
                .program
                .op(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?;
            if let RegionOp::Call { site, .. } = op {
                let Some(activation) = self.prepare_call(site.index())? else {
                    let stop = self.stop.take().unwrap_or(RegionStop::Refusal);
                    return self.halt(stop);
                };
                self.activation = Some(activation);
                if let Some(stop) = self.prefix_stop(request) {
                    return Ok(stop);
                }
                continue;
            }
            if let RegionOp::Boundary { spec } = op
                && spec.action().is_none()
            {
                return self.halt(RegionStop::Boundary);
            }
            // A root frame the machine completes stands before its Exit,
            // its value in its departure role.
            if matches!(op, RegionOp::Exit { .. })
                && self.parked.is_empty()
                && self.machine_completes_root()
            {
                return self.halt(RegionStop::Leave);
            }
            // A step the machine serves stands unbilled for it; the step
            // then takes what was served through the same acceptance. A
            // native its checked kernel cannot answer in place is answered;
            // an effect's value is copied out of the roles holding it, once
            // its statement charge is known to fit.
            let served = match self.served.take() {
                Some((at, answer)) if at == step.at => Some(answer),
                Some(_) => return Err(invalid(Refused::Facts)),
                None => None,
            };
            // A plain `for` given what it cannot walk refuses before its
            // Enter takes anything, as the source does.
            if self.unwalkable(op)? {
                if served.is_some() {
                    return Err(invalid(Refused::Facts));
                }
                return self.halt(RegionStop::Refused {
                    kind: "type_error",
                    message: crate::machine::loops::NOT_ITERABLE.into(),
                });
            }
            let mut answered = None;
            let mut taken = None;
            let mut staged = None;
            let mut finally = None;
            let mut guarded = false;
            match op {
                // A plain `for` takes its next item as its Test event. An
                // exhausted one takes no event: what stands before its Test
                // stands at the event after the loop.
                RegionOp::Checked {
                    kernel,
                    args,
                    dst,
                    next,
                } if matches!(
                    kernel.kind(),
                    crate::compile::region_isa::CheckedKind::Cursor { .. }
                ) =>
                {
                    if served.is_some() {
                        return Err(invalid(Refused::Facts));
                    }
                    let crate::compile::region_isa::CheckedKind::Cursor { at, exhausted } =
                        kernel.kind()
                    else {
                        return Err(invalid(Refused::Facts));
                    };
                    let iterable = *args.first().ok_or_else(|| invalid(Refused::Facts))?;
                    let Some((answer, position)) =
                        self.cursor_answer(at, iterable, *dst, *next, *exhausted)?
                    else {
                        return self.halt(RegionStop::Boundary);
                    };
                    if answer.next == *exhausted {
                        let edit = self.prepare_control(op, &answer, None, None, None, None)?;
                        self.accept_control(edit);
                        self.pc = *exhausted;
                        continue;
                    }
                    taken = position;
                    answered = Some(answer);
                }
                // A protocol `for`'s header row steps, takes what a hook
                // returned, or goes on unaccepted to `next` or past the loop.
                RegionOp::Checked {
                    kernel,
                    args,
                    dst,
                    next,
                } if matches!(
                    kernel.kind(),
                    crate::compile::region_isa::CheckedKind::Header { .. }
                        | crate::compile::region_isa::CheckedKind::LoopItem { .. }
                ) =>
                {
                    if served.is_some() {
                        return Err(invalid(Refused::Facts));
                    }
                    match self.header_answer(kernel.kind(), args, *dst, *next)? {
                        super::header::Stood::Taken(answer, info, state) => {
                            staged = Some((info, state));
                            answered = Some(answer);
                        }
                        super::header::Stood::Passed(next, info, state) => {
                            let index = self.loop_index(info)?;
                            self.loops[index] = state;
                            self.pc = next;
                            continue;
                        }
                        super::header::Stood::Refused(message) => {
                            return self.halt(RegionStop::Refused {
                                kind: "type_error",
                                message: message.into(),
                            });
                        }
                        super::header::Stood::Boundary => {
                            return self.halt(RegionStop::Boundary);
                        }
                    }
                }
                // A finally row suspends or restores the frame's exit, or
                // completes a cleanup to the Restore of what it suspended.
                RegionOp::Checked {
                    kernel,
                    args,
                    dst,
                    next,
                } if kernel.kind().finally() => {
                    if served.is_some() {
                        return Err(invalid(Refused::Facts));
                    }
                    // A cleanup entered normally has nothing to supersede.
                    if self.passes(kernel.kind()) {
                        self.pc = *next;
                        continue;
                    }
                    let (answer, change) = self.finally_answer(kernel.kind(), args, *dst, *next)?;
                    finally = change;
                    answered = Some(answer);
                }
                RegionOp::Checked { kernel, .. }
                    if matches!(
                        kernel.kind(),
                        crate::compile::region_isa::CheckedKind::Native { .. }
                    ) =>
                {
                    answered = match served {
                        Some(Some(value)) => Some(self.delivered_answer(op, value)?),
                        Some(None) => return Err(invalid(Refused::Facts)),
                        None => match self.scalar_answer(op)? {
                            Some(answer) => Some(answer),
                            None => return self.halt(RegionStop::Native),
                        },
                    };
                }
                // A scalar kernel specialized from a native whose operands
                // its guard cannot unpack stands before that native, unbilled;
                // the machine answers it and the step takes the answer.
                _ if code.program.native_operands(self.pc).is_some() => match served {
                    Some(Some(value)) => answered = Some(self.delivered_answer(op, value)?),
                    Some(None) => return Err(invalid(Refused::Facts)),
                    None => {
                        if let Some(guard) = &step.guard
                            && !self.guard(guard)?
                        {
                            return self.halt(RegionStop::Native);
                        }
                        guarded = true;
                        // What the kernel has no answer for (a zero divisor,
                        // a result past the word) is the native's to answer.
                        match self.scalar_answer(op)? {
                            Some(answer) => answered = Some(answer),
                            None => return self.halt(RegionStop::Native),
                        }
                    }
                },
                RegionOp::Effect { kind, value, .. } => match served {
                    Some(None) => {}
                    Some(Some(_)) => return Err(invalid(Refused::Facts)),
                    // A charge that would not fit refuses below, unserved.
                    None => {
                        let counters = self.counters;
                        let before = charge::billed(self.limiters, &mut self.limits, |limits| {
                            charge::bill(
                                counters,
                                limits,
                                &step.charges,
                                ChargeMoment::Before,
                                None,
                            )
                        })?;
                        // The limiter, asked once at a crossing here, is
                        // not asked again by the step's own billing.
                        let paid = match before.crossing {
                            Some(_) => before,
                            None => self.copy_charge(before.counters, *value)?,
                        };
                        let Some(crossing) = paid.crossing else {
                            return self.halt(RegionStop::Effect(*kind));
                        };
                        self.counters = paid.counters;
                        self.refusal = Some(crate::Bound::Charge);
                        self.crossing = Some(crossing);
                        self.attempted = Some(step.at.clone());
                        return self.halt(RegionStop::Refusal);
                    }
                },
                _ if served.is_some() => return Err(invalid(Refused::Facts)),
                _ => {}
            }
            if !guarded
                && answered.is_none()
                && let Some(guard) = &step.guard
                && !self.guard(guard)?
            {
                // A branch or loop test whose condition is not a bool raises
                // its source error before the test is billed, as a served
                // step does; any other failed guard has no meaning.
                if matches!(
                    guard.kind(),
                    crate::compile::region_isa::CheckedKind::Truth { .. }
                ) && let Some(message) = code.program.condition_error(&step.at.site)
                {
                    return self.halt(RegionStop::Refused {
                        kind: "type_error",
                        message: message.into(),
                    });
                }
                return Err(invalid(Refused::Facts));
            }
            // An integer kernel on registers no native could take raises a
            // zero divisor itself, before it is billed, as the native does.
            if answered.is_none()
                && let RegionOp::IntBinary { kernel, rhs, .. } = op
                && let Some((kind, message)) = kernel.refusal(self.registers.integer(
                    self.frame_index(),
                    self.reached.invocation,
                    rhs.index(),
                )?)
            {
                return self.halt(RegionStop::Refused {
                    kind,
                    message: message.into(),
                });
            }
            // A read the state refuses with a typed error (a missing key, an
            // index past the end) raises it before the read is billed, as a
            // refused heap step does.
            let read = match op {
                RegionOp::Read {
                    spec,
                    base,
                    dst,
                    next,
                } => match self.read_answer(spec, *base, *dst, *next) {
                    Ok(answer) => Some(answer),
                    Err(
                        lash_kernel_state::kernel_internals::logical::LeaseFault::BoundaryRequired,
                    ) => return self.halt(RegionStop::Boundary),
                    Err(lash_kernel_state::kernel_internals::logical::LeaseFault::Native(
                        lash_kernel_doc::NativeError::Refused { kind, message },
                    )) => {
                        return self.halt(RegionStop::Refused {
                            kind,
                            message: message.into(),
                        });
                    }
                    Err(fault) => return Err(storage_fault(fault)),
                },
                _ => None,
            };
            let counters = self.counters;
            let before = charge::billed(self.limiters, &mut self.limits, |limits| {
                charge::bill(counters, limits, &step.charges, ChargeMoment::Before, None)
            })?;
            self.counters = before.counters;
            if let Some(crossing) = before.crossing {
                self.refusal = Some(crate::Bound::Charge);
                self.crossing = Some(crossing);
                self.attempted = Some(step.at.clone());
                return self.halt(RegionStop::Refusal);
            }
            // K-CHG-005: an effect's value is copied out at one deep-size fee.
            if let RegionOp::Effect { value, .. } = op {
                let copy = self.copy_charge(self.counters, *value)?;
                self.counters = copy.counters;
                if let Some(crossing) = copy.crossing {
                    self.refusal = Some(crate::Bound::Charge);
                    self.crossing = Some(crossing);
                    self.attempted = Some(step.at.clone());
                    return self.halt(RegionStop::Refusal);
                }
            }
            if self
                .counters
                .depth
                .checked_add(step.depth)
                .is_none_or(|depth| depth > self.limits.depth)
            {
                self.refusal = Some(crate::Bound::CallDepth);
                self.attempted = Some(step.at.clone());
                return self.halt(RegionStop::Refusal);
            }
            if let RegionOp::Heap {
                kernel,
                args,
                dst,
                next,
            } = op
            {
                if !self.heap_step(kernel, args, *dst, *next)? {
                    let stop = self.stop.take().unwrap_or(RegionStop::Refusal);
                    return self.halt(stop);
                }
                if let Some(stop) = self.prefix_stop(request) {
                    return Ok(stop);
                }
                continue;
            }
            let answer = match op {
                RegionOp::Read { .. } => read.ok_or_else(|| invalid(Refused::Facts))?,
                _ => match answered {
                    Some(answer) => answer,
                    None => self
                        .scalar_answer(op)?
                        .ok_or_else(|| invalid(Refused::Facts))?,
                },
            };
            let departure = if matches!(op, RegionOp::Exit { .. })
                && self.departing.is_none()
                && step.at.phase == crate::compile::room::Phase::Restore
            {
                // A `return via` returns what its policy gave.
                let value = answer
                    .write
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .1
                    .bind(&mut self.view)
                    .map_err(RegionError::Heap)?;
                if !self.export_return(value)? {
                    return self.halt(RegionStop::Refusal);
                }
                Some(self.quote_return(value)?)
            } else if matches!(op, RegionOp::Exit { .. }) && self.departing.is_none() {
                let map = code
                    .program
                    .recovery(step.before)
                    .ok_or_else(|| invalid(Refused::Exit))?;
                let value = map
                    .roles
                    .iter()
                    .find_map(|(slot, reg)| {
                        matches!(
                            slot,
                            lash_kernel_state::kernel_internals::logical::Slot::Departure(_)
                        )
                        .then_some(*reg)
                    })
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let value = self.value(value)?;
                Some(self.quote_return(value)?)
            } else if matches!(op, RegionOp::Return { .. }) {
                let value = answer
                    .write
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .1
                    .bind(&mut self.view)
                    .map_err(RegionError::Heap)?;
                if !self.export_return(value)? {
                    return self.halt(RegionStop::Refusal);
                }
                Some(self.quote_return(value)?)
            } else {
                None
            };
            let counters = self.counters;
            let formula = answer.formula;
            let after = charge::billed(self.limiters, &mut self.limits, |limits| {
                charge::bill(
                    counters,
                    limits,
                    &step.charges,
                    ChargeMoment::After,
                    formula,
                )
            })?;
            self.counters = after.counters;
            // A root frame's own completion is billed as it leaves; a
            // callee's is billed at its caller's Restore.
            let after = match op {
                RegionOp::Exit { .. } if self.parked.is_empty() && after.crossing.is_none() => {
                    let completion = departure
                        .as_ref()
                        .or(self.departing.as_ref())
                        .map_or(0, |departure| departure.completion);
                    let counters = self.counters;
                    let payment = charge::billed(self.limiters, &mut self.limits, |limits| {
                        Ok(charge::export(counters, limits, completion))
                    })?;
                    self.counters = payment.counters;
                    payment
                }
                _ => after,
            };
            if let Some(crossing) = after.crossing {
                self.refusal = Some(crate::Bound::Charge);
                self.crossing = Some(crossing);
                self.attempted = Some(step.at.clone());
                let map = code
                    .program
                    .recovery(step.pending)
                    .ok_or_else(|| invalid(Refused::Exit))?;
                if let Some(pending) = map.pending {
                    let value = answer.write.ok_or_else(|| invalid(Refused::Facts))?.1;
                    // A result pending at its output role is held there; an
                    // adopted answer is held wherever it is pending.
                    if answer.native.is_some() || map.roles.iter().any(|(_, reg)| *reg == pending) {
                        let result = value.bind(&mut self.view).map_err(RegionError::Heap)?;
                        let Some(counters) = self.hold_pending(result, answer.native)? else {
                            self.refusal = Some(crate::Bound::Memory);
                            self.crossing = None;
                            return self.halt(RegionStop::Refusal);
                        };
                        self.counters = counters;
                    }
                    self.write(pending, value)?;
                }
                self.reached.ordinal = self
                    .reached
                    .ordinal
                    .checked_add(1)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                self.prefix = step.pending;
                self.reached.prefix = step.pending.index() as u32;
                self.reached.debt = self.counters.debt;
                return self.halt(RegionStop::Refusal);
            }
            let flow = code
                .flows
                .get(self.pc.index())
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let looping = code.program.loop_departure(self.pc);
            let control = self.prepare_control(
                op,
                &answer,
                flow.loop_branch,
                looping.as_ref(),
                taken,
                staged,
            )?;
            let ordinal = self
                .reached
                .ordinal
                .checked_add(1)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let write = answer
                .write
                .map(|(destination, value)| {
                    self.registers.prepare_write(
                        &mut self.view,
                        self.parked.len(),
                        self.reached.invocation,
                        destination,
                        value,
                    )
                })
                .transpose()?;
            let Some(resources) = self.resources(flow, &answer)? else {
                self.refusal = Some(crate::Bound::Memory);
                self.attempted = Some(step.at.clone());
                return self.halt(RegionStop::Refusal);
            };
            let counters = resources.accept()?;
            self.release_atoms()?;
            self.accept_control(control);
            if let Some(write) = write {
                self.registers.publish_write(write);
            }
            self.counters = counters;
            self.reached.ordinal = ordinal;
            self.prefix = step.after;
            self.reached.prefix = step.after.index() as u32;
            self.reached.debt = counters.debt;
            if let Some(departure) = departure {
                self.departing = Some(departure);
            }
            if let Some(change) = finally {
                self.accept_finally(change)?;
            }
            match looping {
                Some(crate::compile::region_isa::LoopDeparture::Origin(origin)) => {
                    self.looping = Some(origin);
                }
                Some(crate::compile::region_isa::LoopDeparture::End { .. }) => {
                    self.looping = None;
                }
                None => {}
            }
            // An accepted uncaught raise leaves the region: the machine
            // carries it out of the frame from its departure role.
            if matches!(op, RegionOp::Raise { .. }) {
                return self.halt(RegionStop::Throw);
            }
            // An accepted Escape restored a raise no try of the frame takes:
            // the machine carries it out of the frame.
            if let RegionOp::Checked { kernel, .. } = op
                && let crate::compile::region_isa::CheckedKind::Escape { exit, .. } = kernel.kind()
            {
                return match exit {
                    crate::compile::room::ExitKind::Raise => self.halt(RegionStop::Throw),
                    _ => Err(invalid(Refused::Facts)),
                };
            }
            if matches!(op, RegionOp::Exit { .. }) {
                self.finish_frame()?;
                if self.complete {
                    return self.halt(RegionStop::Complete);
                }
            } else {
                self.pc = answer.next;
            }
            // An accepted action Enter stands for the machine to perform it.
            if matches!(op, RegionOp::Boundary { .. }) {
                return self.halt(RegionStop::Action);
            }
            // An accepted finish or fail ends the program.
            if let RegionOp::Effect {
                kind:
                    crate::compile::region_isa::EffectKind::Finish
                    | crate::compile::region_isa::EffectKind::Fail,
                ..
            } = op
            {
                return self.halt(RegionStop::End);
            }
            if let Some(stop) = self.prefix_stop(request) {
                return Ok(stop);
            }
        }
    }
}
