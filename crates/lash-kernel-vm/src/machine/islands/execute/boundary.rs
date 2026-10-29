//! Where a register frame meets its source owner: resuming a validated
//! source frame into a physical window, and projecting stopped windows back
//! into source frames at their semantic coordinates.

use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(in crate::machine::islands) fn resume(
        &mut self,
        callee: Callee<'e>,
        state: &crate::machine::Frame,
        suffix: Option<&super::ActionSuffix>,
    ) -> Option<Frame<'e>> {
        let mut window = self.scratch.windows.pop().unwrap_or_default();
        let control = &mut window.control;
        for item in &state.control {
            match item {
                Control::Block { block, next } => {
                    let index = callee
                        .compiled
                        .blocks
                        .iter()
                        .position(|entry| entry.id == *block)?;
                    control.push(Position::Block(index, *next));
                }
                Control::While { stmt, started } => {
                    let header = callee.compiled.header(*stmt)?;
                    control.push(Position::Loop {
                        id: *stmt,
                        header,
                        started: *started,
                        cursor: None,
                        guard: None,
                    });
                }
                Control::For {
                    stmt,
                    started,
                    cursor,
                    header: Header::Body,
                    guard,
                } => {
                    let header = callee.compiled.header(*stmt)?;
                    control.push(Position::Loop {
                        id: *stmt,
                        header,
                        started: *started,
                        cursor: Some(cursor.clone()),
                        guard: guard.clone(),
                    });
                }
                Control::Try { stmt, phase } => {
                    let cleanup = callee.compiled.cleanup(*stmt)?;
                    control.push(Position::Try {
                        cleanup,
                        phase: Phase::of(phase).0,
                    });
                }
                _ => return None,
            }
        }
        // A loop on top stopped after its body's block ended, before its
        // header ran the next iteration.
        let pc = match control.last() {
            Some(Position::Block(block, next)) => {
                let block = &callee.compiled.blocks[*block];
                if let Some(suffix) = suffix {
                    suffix.after
                } else {
                    block.starts.get(*next).copied().unwrap_or(block.end)
                }
            }
            Some(Position::Loop { header, .. }) if suffix.is_none() => *header,
            _ => return None,
        };
        // The source producer already carries values with their cell
        // identities. Validate that identity class, then copy it intact:
        // every cell stands in a shared variable and is live.
        let slots = state.slot_states();
        let mut cells = slots
            .iter()
            .filter(|slot| matches!(slot, SlotState::Cell(_)))
            .count();
        for at in &callee.compiled.layout.shared {
            if let Some(SlotState::Cell(cell)) = slots.get(*at as usize) {
                self.arena.cell(*cell).ok()?;
                cells -= 1;
            }
        }
        if cells != 0 {
            return None;
        }
        let may_clean = control
            .iter()
            .any(|position| matches!(position, Position::Try { .. }));
        let recovery = control.iter().any(
            |position| matches!(position, Position::Try { phase, .. } if phase.needs_source()),
        );
        self.register_bytes += callee.compiled.register_bytes();
        let ranges = self.scratch.slab.open(
            callee.plan.slots,
            callee.compiled.registers,
            callee.compiled.pending_slots(),
        );
        let mut frame = Frame::new(callee, ranges, window);
        self.scratch
            .slab
            .view(&mut frame)
            .slots
            .copy_from_slice(state.slot_states());
        for source in &state.control {
            if let Control::Try { stmt, phase } = source {
                let cleanup = callee.compiled.cleanup(*stmt)?;
                self.scratch.slab.view(&mut frame).pending[cleanup.pending] = Phase::of(phase).1;
            }
        }
        frame.may_clean = may_clean;
        frame.recovery = recovery;
        frame.pc = pc;
        frame.resident = true;
        Some(frame)
    }

    pub(in crate::machine::islands) fn action_suffix(
        &mut self,
        frame: &mut Frame<'e>,
        suffix: &super::ActionSuffix,
        values: &crate::machine::registers::Registers,
    ) {
        frame.pc = suffix.after;
        frame.statement = Some(suffix.stmt);
        let live = &frame.callee.compiled.code.recovery[suffix.after].live;
        let mut frame = self.scratch.slab.view(frame);
        for reg in live {
            frame.regs[*reg as usize] = values[*reg as usize];
        }
    }

    pub(in crate::machine::islands) fn project_handoff(
        &mut self,
        active: &mut Frame<'e>,
        state: &mut crate::machine::Frame,
        values: &mut crate::machine::registers::Registers,
    ) -> Option<RunsOn> {
        let mut slab = std::mem::take(&mut self.scratch.slab);
        let handoff = self.handoff_values(&mut slab.view(active), state, values);
        if handoff.is_some() {
            slab.release(active);
        }
        self.scratch.slab = slab;
        handoff
    }

    /// A returned resident root keeps its source bindings until its caller
    /// closes the completion formula, including a refusal at that close.
    pub(in crate::machine::islands) fn project_return(
        &mut self,
        active: &mut Frame<'e>,
        state: &mut crate::machine::Frame,
    ) {
        let mut slab = std::mem::take(&mut self.scratch.slab);
        let mut values = slab.view(active);
        if let Some(returned) = values.state.returned {
            self.restore_expression_scopes(values.slots, returned.statement, returned.pc);
        }
        for (at, slot) in values.slots.iter().enumerate() {
            state.set_slot(at as u32, *slot);
        }
        self.recycle(&mut values);
        slab.release(active);
        self.scratch.slab = slab;
    }

    /// A resident root already has a semantic owner. Project directly into
    /// that owner before releasing the borrow and running the scheduler.
    fn handoff_values(
        &mut self,
        active: &mut Values<'_, 'e>,
        state: &mut crate::machine::Frame,
        values: &mut crate::machine::registers::Registers,
    ) -> Option<RunsOn> {
        if !active.state.handoff {
            return None;
        }
        state.link = active.state.callee.link;
        state.control.replace(
            active
                .state
                .control
                .iter()
                .map(|position| {
                    let mut control = Control::Block {
                        block: active.state.callee.compiled.blocks[0].id,
                        next: 0,
                    };
                    position.project(active.state.callee.compiled, &active.pending, &mut control);
                    control
                })
                .collect(),
        );
        let Some(stmt) = active.state.statement else {
            unreachable!("a scheduler handoff has a statement");
        };
        let Some(pc) = self.scratch.exit.at else {
            unreachable!("a scheduler handoff has a source PC");
        };
        self.restore_expression_scopes(active.slots, stmt, pc);
        for (at, slot) in active.slots.iter().enumerate() {
            state.set_slot(at as u32, *slot);
        }
        if active.state.callee.captured != super::RESIDENT {
            for (index, at) in active.state.callee.plan.captures.iter().enumerate() {
                let (_, cell) =
                    self.scratch.captured[active.state.callee.captured as usize + index];
                state.set_slot(*at, SlotState::Cell(cell));
            }
        }
        // The disjoint source register owner receives the ordered live prefix
        // before the physical window dies. No callback occurs before dispatch.
        values.statement(self.exe.stmt(stmt).regs);
        for reg in active.state.handoff_live {
            if !self.scratch.exit.regs.iter().any(|(at, _)| at == reg) {
                values.put(*reg as usize, active.regs[*reg as usize]);
            }
        }
        for (reg, value) in &self.scratch.exit.regs {
            values.put(*reg as usize, *value);
        }
        self.scratch.exit = Exit::default();
        self.recycle(active);
        Some(RunsOn::Scheduling(stmt, pc, Vec::new()))
    }

    /// Project the one loop's stopped windows into their source owners.
    #[cold]
    #[inline(never)]
    pub(super) fn slow_exit(
        &mut self,
        active: &mut Frame<'e>,
        callers: &mut Vec<Frame<'e>>,
        slab: &mut Slab,
        recovery: &'e Recovery,
        stop: Stop,
    ) -> Answer {
        self.give_back();
        if let Some(sampling) = self.sampling {
            sampling.statement(active.callee.plan.code, active.statement);
        }
        #[cfg(any(test, feature = "perf-witness"))]
        if !self.scratch.exit.scheduling {
            let reason = if let Some(reason) = self.scratch.exit.reason.take() {
                reason
            } else if self.scratch.exit.halt.is_some() {
                Reason::OccurrenceOrHalt
            } else if self.spent() {
                Reason::Slice
            } else if !self.fits(1) {
                Reason::Charge
            } else {
                recovery.reason
            };
            execution::slow_exit(
                &self.scratch.census,
                Owner::RegisterProgram,
                reason,
                self.scratch.counting,
            );
            let lib = recovery.library;
            if let Some(lib) = lib {
                execution::operation_exit(
                    &self.scratch.census,
                    reason,
                    self.exe.lib(lib).header.name.as_str(),
                    self.scratch.counting,
                );
            }
        }
        let mapped = std::mem::take(&mut self.scratch.exit.mapped);
        if self.scratch.exit.halt.is_none() {
            if active.resident && callers.is_empty() && self.scratch.exit.scheduling {
                active.handoff = true;
                active.handoff_live = &recovery.live;
                self.depth = 0;
                return Err(stop);
            }
            let library = callers
                .last_mut()
                .and_then(|caller| self.awaited_library(&slab.view(caller)));
            let mut current = slab.view(active);
            if !mapped {
                self.spill(&current, &recovery.live);
            }
            self.materialize(&current, stop, library);
            self.recycle(&mut current);
        } else {
            self.recycle(&mut slab.view(active));
        }
        slab.release(active);
        while let Some(mut caller) = callers.pop() {
            let Some(continuation) = caller.continuation else {
                return Err(self.halt(fault("a caller has no call continuation")));
            };
            let site = continuation.site;
            if self.scratch.exit.halt.is_none() {
                if let Some(act) = site.act {
                    self.scratch.exit.suspended = Some(act);
                } else if let Some(stmt) = caller.statement {
                    self.scratch.exit.stands = Some(Stands::Action { stmt });
                }
                let library = callers
                    .last_mut()
                    .and_then(|caller| self.awaited_library(&slab.view(caller)));
                let current = slab.view(&mut caller);
                self.spill(&current, continuation.live);
                self.materialize(&current, Stop, library);
            }
            self.recycle(&mut slab.view(&mut caller));
            slab.release(&caller);
        }
        self.depth = 0;
        Err(Stop)
    }

    /// Keep values that a semantic handler already saved (a fused helper
    /// may have committed its first operation), then add the live outer
    /// operands. Neither scratch positions nor this physical cursor persist.
    pub(super) fn spill(&mut self, frame: &Values<'_, '_>, live: &[u32]) {
        if frame.state.continuation.is_none() {
            for restart in &frame.state.callee.compiled.restarts {
                if frame.state.statement == Some(restart.statement)
                    && self.scratch.exit.at == Some(restart.head)
                    && matches!(frame.regs[restart.saved as usize + 1], Value::Bool(true))
                {
                    let value = frame.regs[restart.saved as usize];
                    if let Some((_, held)) = self
                        .scratch
                        .exit
                        .regs
                        .iter_mut()
                        .find(|(reg, _)| *reg == restart.reg)
                    {
                        *held = value;
                    } else {
                        self.scratch.exit.regs.push((restart.reg, value));
                    }
                }
            }
        }
        for reg in live {
            if !self.scratch.exit.regs.iter().any(|(at, _)| at == reg) {
                self.scratch
                    .exit
                    .regs
                    .push((*reg, frame.regs[*reg as usize]));
            }
        }
        if let Some(continuation) = frame.state.continuation
            && let Some(inputs) = &continuation.site.inputs
        {
            for read in inputs.reads() {
                let reg = read.reg();
                if !self.scratch.exit.regs.iter().any(|(at, _)| *at == reg) {
                    self.scratch.exit.regs.push((reg, frame.regs[reg as usize]));
                }
            }
        }
    }

    pub(super) fn awaited_library(&self, frame: &Values<'_, 'e>) -> Option<LibraryCall> {
        let CallContinuation { site, args, .. } = frame.state.continuation?;
        Some(LibraryCall {
            lib: site.library?,
            args: if self.exe.charging() && frame.state.callee.plan.charged {
                args.iter()
                    .map(|reg| frame.regs[*reg as usize])
                    .chain(std::iter::repeat(Value::Absent))
                    .take(self.exe.lib(site.library?).arity)
                    .collect()
            } else {
                Box::default()
            },
        })
    }

    /// A paid library body completes under its caller's original formula.
    /// Refusal leaves the return in its source owner, before the fee commits.
    #[inline(always)]
    pub(super) fn complete_library(
        &mut self,
        frame: &Values<'_, 'e>,
        caller: Option<&Values<'_, 'e>>,
        value: Value,
    ) -> Result<(), Stop> {
        if let Some(lib) = frame.state.boundary
            && !crate::compile::import_result_fits(
                &self.exe.lib(lib).header.signature.result,
                value,
            )
        {
            return self.library_return(frame, value);
        }
        if frame.state.callee.plan.charged {
            return Ok(());
        }
        let Some(caller) =
            caller.filter(|caller| caller.state.callee.plan.charged && self.exe.charging())
        else {
            return Ok(());
        };
        let Some(CallContinuation { site, args, .. }) = caller.state.continuation else {
            return Ok(());
        };
        let Some(lib) = site.library else {
            return Ok(());
        };
        let args: Vec<_> = args
            .iter()
            .map(|reg| caller.regs[*reg as usize])
            .chain(std::iter::repeat(Value::Absent))
            .take(self.exe.lib(lib).arity)
            .collect();
        let units = measured_formula(self.arena, &self.exe.lib(lib).charge, &args, Some(&value));
        if let Some(units) = units.filter(|units| self.root_fits(0, *units, site.root.is_some())) {
            self.charged += units;
            if units != 0
                && let Some(tape) = self.recorder
            {
                tape.push(Weight::Atomic(units));
            }
            return Ok(());
        }
        #[cfg(any(test, feature = "perf-witness"))]
        {
            self.scratch.exit.reason = Some(Reason::Charge);
        }
        self.library_return(frame, value)
    }

    fn library_return(&mut self, frame: &Values<'_, 'e>, value: Value) -> Result<(), Stop> {
        if let Some(id) = frame.state.statement {
            let Some(ReturnOperand { statement, pc, reg }) = frame.state.returned else {
                return Err(self.halt(fault("a library completion has no return operand")));
            };
            if statement != id {
                return Err(self.halt(fault("a library completion has a different return owner")));
            }
            Err(self.at_held(pc, reg, value))
        } else {
            self.scratch.exit.stands = Some(Stands::Boundary);
            Err(Stop)
        }
    }

    pub(super) fn materialize(
        &mut self,
        frame: &Values<'_, 'e>,
        stop: Stop,
        library: Option<LibraryCall>,
    ) {
        self.link = frame.state.callee.link;
        self.stream = frame.state.callee.stream;
        let source = frame.state.statement.and_then(|stmt| {
            frame
                .state
                .continuation
                .map(|continuation| continuation.site.pc)
                .or(self.scratch.exit.at)
                .map(|pc| (stmt, pc))
        });
        if let Some(stmt) = frame.state.statement {
            self.stand(stmt, stop);
        }
        self.scratch.exit.control.extend(
            frame
                .state
                .control
                .iter()
                .rev()
                .map(|position| position.projected(frame.state.callee.compiled, &frame.pending)),
        );
        self.left(
            frame.state.callee.plan,
            frame.state.callee.captured,
            frame.slots,
            library.filter(|_| !frame.state.callee.plan.charged),
            frame.state.boundary,
            source,
        );
    }
}
