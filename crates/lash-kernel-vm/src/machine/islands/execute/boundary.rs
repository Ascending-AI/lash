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
                    let instruction = callee.compiled.ops.iter().position(
                        |instruction| matches!(instruction.op, Op::Try { id, .. } if id == *stmt),
                    )?;
                    control.push(Position::Try {
                        instruction,
                        phase: phase.clone(),
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
        let recovery = control.iter().any(|position| {
            matches!(
                position,
                Position::Try {
                    phase: TryPhase::Catch
                        | TryPhase::Finally(Completion::Throw(_) | Completion::ReturnVia { .. }),
                    ..
                }
            )
        });
        self.register_bytes += callee.compiled.register_bytes();
        let mut frame = Frame::new(callee, std::iter::empty(), &[], window);
        frame.slots.copy_from_slice(state.slot_states());
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
        for reg in &frame.callee.compiled.ops[suffix.after].live {
            frame.regs[*reg as usize] = values[*reg as usize];
        }
    }

    /// A resident root already has a semantic owner. Project directly into
    /// that owner before releasing the borrow and running the scheduler.
    pub(in crate::machine::islands) fn project_handoff(
        &mut self,
        active: &mut Frame<'e>,
        state: &mut crate::machine::Frame,
        values: &mut crate::machine::registers::Registers,
    ) -> Option<RunsOn> {
        if !active.handoff {
            return None;
        }
        state.link = active.callee.link;
        state
            .control
            .resize_with(active.control.len(), || Control::Block {
                block: active.callee.compiled.blocks[0].id,
                next: 0,
            });
        for (position, target) in active.control.iter().zip(&mut state.control) {
            position.project(active.callee.compiled, target);
        }
        for (at, slot) in active.slots.iter().enumerate() {
            state.set_slot(at as u32, *slot);
        }
        if active.callee.captured != super::RESIDENT {
            for (index, at) in active.callee.plan.captures.iter().enumerate() {
                let (_, cell) = self.scratch.captured[active.callee.captured as usize + index];
                state.set_slot(*at, SlotState::Cell(cell));
            }
        }
        let Some(stmt) = active.statement else {
            unreachable!("a scheduler handoff has a statement");
        };
        let Some(pc) = self.scratch.exit.at else {
            unreachable!("a scheduler handoff has a source PC");
        };
        // The disjoint source register owner receives the ordered live prefix
        // before the physical window dies. No callback occurs before dispatch.
        values.statement(self.exe.stmt(stmt).regs);
        for reg in &active.callee.compiled.ops[active.pc - 1].live {
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
        instruction: &Instruction,
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
                match instruction.op {
                    Op::Close { .. } | Op::ReturnLiteral { .. } => Reason::Charge,
                    Op::Local { .. } | Op::Session { .. } | Op::Field { .. } | Op::Index { .. } => {
                        Reason::ResidentRead
                    }
                    Op::Operator { .. }
                    | Op::DirectOperator { .. }
                    | Op::Unary { .. }
                    | Op::Binary { .. }
                    | Op::Accumulation { .. }
                    | Op::Unit { .. }
                    | Op::Kind { .. }
                    | Op::DirectKind { .. }
                    | Op::Access(_) => Reason::Operator,
                    Op::Native { .. }
                    | Op::DirectNative { .. }
                    | Op::Heap { .. }
                    | Op::MemberStore { .. }
                    | Op::MemberAccess { .. } => Reason::Native,
                    Op::Call { .. } => Reason::Call,
                    Op::Bind { .. } | Op::Write { .. } | Op::Store { .. } | Op::Publish { .. } => {
                        Reason::Binding
                    }
                    Op::Cond { .. } | Op::Branch { .. } | Op::LoopBranch { .. } => {
                        Reason::Condition
                    }
                    Op::Cold(Deferred::Source(_)) => Reason::ColdBranch,
                    Op::Cold(Deferred::Text { .. }) => Reason::Native,
                    _ => Reason::SourceContinuation,
                }
            };
            execution::slow_exit(
                &self.scratch.census,
                Owner::RegisterProgram,
                reason,
                self.scratch.counting,
            );
            let lib = match &instruction.op {
                Op::Native { site, .. } | Op::DirectNative { site, .. } => Some(site.lib),
                Op::Operator { site, .. }
                | Op::DirectOperator { site, .. }
                | Op::Unary { site, .. }
                | Op::Binary { site, .. } => Some(site.lib),
                Op::Heap { site, .. }
                | Op::MemberStore { site, .. }
                | Op::MemberAccess { site, .. } => Some(site.lib),
                Op::Call { site, .. } => site.library,
                _ => None,
            };
            if let Some(lib) = lib {
                execution::operation_exit(
                    &self.scratch.census,
                    reason,
                    self.exe.lib(lib).header.name.as_str(),
                    self.scratch.counting,
                );
            }
        }
        if self.scratch.exit.halt.is_none() {
            if active.resident && callers.is_empty() && self.scratch.exit.scheduling {
                active.handoff = true;
                self.depth = 0;
                return Err(stop);
            }
            self.spill(active, instruction);
            self.materialize(
                active,
                stop,
                callers
                    .last()
                    .and_then(|caller| self.awaited_library(caller)),
            );
        }
        self.recycle(active);
        while let Some(mut caller) = callers.pop() {
            if !caller.waiting {
                return Err(self.halt(fault("a caller has no call continuation")));
            }
            let instruction = &caller.callee.compiled.ops[caller.pc - 1];
            let Op::Call { site, .. } = &instruction.op else {
                unreachable!("a caller's continuation is a call")
            };
            if self.scratch.exit.halt.is_none() {
                if let Some(act) = site.act {
                    self.scratch.exit.suspended = Some(act);
                } else if let Some(stmt) = caller.statement {
                    self.scratch.exit.stands = Some(Stands::Action { stmt });
                }
                self.spill(&caller, instruction);
                self.materialize(
                    &caller,
                    Stop,
                    callers
                        .last()
                        .and_then(|caller| self.awaited_library(caller)),
                );
            }
            self.recycle(&mut caller);
        }
        self.depth = 0;
        Err(Stop)
    }

    /// Keep values that a semantic handler already saved (a fused helper
    /// may have committed its first operation), then add the live outer
    /// operands. Neither scratch positions nor this physical cursor persist.
    pub(super) fn spill(&mut self, frame: &Frame<'_>, instruction: &Instruction) {
        if !frame.waiting && frame.callee.compiled.has_restarts {
            for restart in
                frame
                    .callee
                    .compiled
                    .ops
                    .iter()
                    .filter_map(|instruction| match &instruction.op {
                        Op::Keep(restart) => Some(restart),
                        _ => None,
                    })
            {
                if frame.statement == Some(restart.statement)
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
        for reg in &instruction.live {
            if !self.scratch.exit.regs.iter().any(|(at, _)| at == reg) {
                self.scratch
                    .exit
                    .regs
                    .push((*reg, frame.regs[*reg as usize]));
            }
        }
        if frame.waiting
            && let Op::Call { site, .. } = &instruction.op
            && let Some(inputs) = &site.inputs
        {
            for read in inputs.reads() {
                let reg = read.reg();
                if !self.scratch.exit.regs.iter().any(|(at, _)| *at == reg) {
                    self.scratch.exit.regs.push((reg, frame.regs[reg as usize]));
                }
            }
        }
    }

    pub(super) fn awaited_library(&self, frame: &Frame<'e>) -> Option<LibraryCall> {
        if !frame.waiting {
            return None;
        }
        let Op::Call { site, args, .. } = &frame.callee.compiled.ops[frame.pc - 1].op else {
            return None;
        };
        Some(LibraryCall {
            lib: site.library?,
            args: if self.exe.charging() && frame.callee.plan.charged {
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
        frame: &Frame<'e>,
        caller: Option<&Frame<'e>>,
        value: Value,
    ) -> Result<(), Stop> {
        if let Some(lib) = frame.boundary
            && !crate::compile::import_result_fits(
                &self.exe.lib(lib).header.signature.result,
                value,
            )
        {
            return self.library_return(frame, value);
        }
        if frame.callee.plan.charged {
            return Ok(());
        }
        let Some(caller) =
            caller.filter(|caller| caller.callee.plan.charged && self.exe.charging())
        else {
            return Ok(());
        };
        if !caller.waiting {
            return Ok(());
        }
        let Op::Call { site, args, .. } = &caller.callee.compiled.ops[caller.pc - 1].op else {
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

    fn library_return(&mut self, frame: &Frame<'e>, value: Value) -> Result<(), Stop> {
        if let Some(id) = frame.statement {
            let pc = self.exe.stmt(id).ops.end() - 1;
            let reg = match &frame.callee.compiled.ops[frame.pc - 1].op {
                Op::Return { reg, .. } | Op::ReturnLiteral { reg, .. } => *reg,
                _ => return Err(self.halt(fault("a library completion has no return operand"))),
            };
            Err(self.at_held(pc, reg, value))
        } else {
            self.scratch.exit.stands = Some(Stands::Boundary);
            Err(Stop)
        }
    }

    pub(super) fn materialize(
        &mut self,
        frame: &Frame<'e>,
        stop: Stop,
        library: Option<LibraryCall>,
    ) {
        self.link = frame.callee.link;
        self.stream = frame.callee.stream;
        if let Some(stmt) = frame.statement {
            self.stand(stmt, stop);
        }
        self.scratch.exit.control.extend(
            frame
                .control
                .iter()
                .rev()
                .map(|position| position.projected(frame.callee.compiled)),
        );
        self.left(
            frame.callee.plan,
            frame.callee.captured,
            &frame.slots,
            library.filter(|_| !frame.callee.plan.charged),
            frame.boundary,
        );
    }
}
