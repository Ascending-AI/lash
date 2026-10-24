//! One active register frame. Calls move it onto an explicit stack and
//! returns recover it; Rust stack depth is independent of guest call depth.
//! Every legal step polls, including straight-line statements and frame entry.

#[cfg(any(test, feature = "perf-witness"))]
use crate::perf_witness::execution::{self, Owner, Reason};

use super::compiled::{Instruction, Op};
use super::*;
use crate::machine::loops::{Cursor, Header};
use crate::machine::{Completion, TryPhase};

enum Position {
    Block(usize, usize),
    Loop {
        id: StmtId,
        header: usize,
        started: u64,
        cursor: Option<Cursor>,
    },
    Try {
        instruction: usize,
        phase: TryPhase,
    },
}

/// Reusable physical windows own no logical value between entries.
#[derive(Default)]
pub(super) struct Window {
    slots: Vec<Option<Value>>,
    regs: Vec<Value>,
    control: Vec<Position>,
}

pub(super) struct Frame<'e> {
    callee: Callee<'e>,
    slots: Vec<Option<Value>>,
    regs: Vec<Value>,
    pc: usize,
    /// Compiled block identity and next source statement, outermost first.
    control: Vec<Position>,
    statement: Option<StmtId>,
    waiting: Option<usize>,
    may_clean: bool,
    admitted: bool,
    swift: bool,
    covered: bool,
    transient: u64,
    resident: bool,
}

impl crate::compile::Slots for Frame<'_> {
    #[inline(always)]
    fn value(&self, at: u32) -> Option<Value> {
        if self.callee.plan.captures.contains(&at) {
            return None;
        }
        self.slots.get(at as usize).copied().flatten()
    }
    #[inline(always)]
    fn assign(&mut self, at: u32, value: Value) {
        self.slots[at as usize] = Some(value);
    }
}

impl<'e> Frame<'e> {
    /// An explicit return retains its source statement; falling out of the
    /// last block clears it. Only the former copies a result out of the run.
    pub(super) fn returned_explicitly(&self) -> bool {
        self.statement.is_some()
    }

    #[inline(always)]
    fn new(
        callee: Callee<'e>,
        args: impl ExactSizeIterator<Item = Value>,
        captured: &[(Value, ArenaHandle)],
        mut window: Window,
    ) -> Self {
        window.slots.resize(callee.plan.slots, None);
        callee
            .compiled
            .layout
            .fill(&mut window.slots, args, captured);
        window.regs.resize(callee.compiled.registers, Value::Null);
        Self {
            callee,
            slots: window.slots,
            regs: window.regs,
            pc: 0,
            control: window.control,
            statement: None,
            waiting: None,
            may_clean: false,
            admitted: false,
            swift: false,
            covered: false,
            transient: 0,
            resident: false,
        }
    }
}

/// A single operation either advances the active cursor, calls, returns,
/// or exits with the source map. No operation owns another control loop.
enum Next<'e> {
    Continue,
    Call(Callee<'e>),
    Return(Value),
}

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(super) fn enter(&mut self, callee: Callee<'e>, args: &[Value]) -> Answer {
        if let Some(value) = self.returned_literal(callee) {
            return Ok(value);
        }
        let mut active = self.new_frame(callee, args);
        self.record_entry(callee);
        self.execute(&mut active)
    }

    /// A single literal return needs no physical slot or control window.
    /// Admission still belongs to the call site. When either event could
    /// refuse, the ordinary instruction path preserves its exact prefix.
    #[inline(always)]
    fn returned_literal(&mut self, callee: Callee<'e>) -> Option<Value> {
        let Op::ReturnLiteral {
            entry: Some(_),
            value,
            close,
            id,
            index,
            ..
        } = &callee.compiled.ops.first()?.op
        else {
            return None;
        };
        let units = 1 + close.map_or(0, |close| u64::from(close.total));
        if self.spent() || !self.fits(units) {
            return None;
        }
        crate::observe::register_entry::<true>(self.recorder, self.scratch.counting);
        if OBSERVING && let Some(watching) = &self.projection() {
            watching.push(callee.plan.code, self.charged, self.arena.allocations());
            watching.statement(
                self.exe,
                StatementFact {
                    code: callee.plan.code,
                    stmt: *id,
                    block: callee.plan.body.id,
                    index: *index,
                    charging: self.charging,
                },
                (self.charged + 1, self.arena.allocations()),
            );
            if let Some(close) = close {
                watching.expressions(self.stream, close, self.charging);
            }
            watching.pop(self.charged + units, self.arena.allocations());
        }
        self.charged += units;
        if let Some(tape) = self.tape {
            tape.push(Weight::Atomic(1));
            if let Some(close) = close {
                super::record(tape, self.stream, close.region);
            }
        }
        Some(*value)
    }

    pub(super) fn resume(
        &mut self,
        callee: Callee<'e>,
        state: &super::super::Frame,
    ) -> Option<Frame<'e>> {
        // A source return has already crossed its public boundary before
        // entering finally. Its semantic owner must finish that departure,
        // including any replacement return or failure, without crossing twice.
        if self.public_return
            && state.control.iter().any(|control| {
                matches!(
                    control,
                    Control::Try {
                        phase: TryPhase::Finally(Completion::Return(_)),
                        ..
                    }
                )
            })
        {
            return None;
        }
        let mut control = Vec::new();
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
                    let header = callee.compiled.ops.iter().position(|instruction| matches!(instruction.op, Op::Header { id, .. } if id == *stmt))?;
                    control.push(Position::Loop {
                        id: *stmt,
                        header,
                        started: *started,
                        cursor: None,
                    });
                }
                Control::For {
                    stmt,
                    started,
                    cursor,
                    header: Header::Body,
                    guard: None,
                } => {
                    let header = callee.compiled.ops.iter().position(|instruction| matches!(instruction.op, Op::Header { id, .. } if id == *stmt))?;
                    control.push(Position::Loop {
                        id: *stmt,
                        header,
                        started: *started,
                        cursor: Some(cursor.clone()),
                    });
                }
                Control::Try {
                    phase:
                        TryPhase::Catch
                        | TryPhase::Finally(Completion::Throw(_) | Completion::ReturnVia { .. }),
                    ..
                } => return None,
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
        let Position::Block(block, next) = control.last()? else {
            return None;
        };
        let block = &callee.compiled.blocks[*block];
        let pc = match self.exe.block(block.id).stmts.get(*next) {
            Some(stmt) => callee.compiled.ops.iter().position(
                |instruction| matches!(instruction.op, Op::Begin { id, .. } | Op::ReturnLiteral { id, .. } if id == *stmt),
            )?,
            None => block.end,
        };
        let mut slots = vec![None; callee.plan.slots];
        for (at, state) in state.slot_states().iter().enumerate() {
            slots[at] = match state {
                SlotState::Empty => None,
                SlotState::Value(value) => Some(*value),
                // This admission represents only declared captures as cells.
                // A cold branch can have created other shared locals: their
                // source frame must retain ownership, rather than flatten
                // their identities into copied values on materialization.
                SlotState::Cell(cell) if callee.plan.captures.contains(&(at as u32)) => {
                    Some(self.arena.cell(*cell).ok()?)
                }
                _ => return None,
            };
        }
        let mut frame = self.new_frame(callee, &[]);
        frame.slots.copy_from_slice(&slots);
        frame.may_clean = control
            .iter()
            .any(|position| matches!(position, Position::Try { .. }));
        frame.control = control;
        frame.pc = pc;
        frame.resident = true;
        Some(frame)
    }

    // One loop and instruction inventory serve both modes. The entry-fixed
    // flag erases passive projection taps when no observer borrowed this run.
    pub(super) fn execute(&mut self, active: &mut Frame<'e>) -> Answer {
        #[cfg(any(test, feature = "perf-witness"))]
        let _execution = execution::Scope::enter(Owner::RegisterProgram, self.scratch.counting);
        let mut callers = Vec::new();
        // This loop owns both the physical frames and their reservation.
        // It never commits a call that cannot fit all live windows.
        let mut slots = active.slots.len();
        self.depth = 1;
        self.activate(active);
        loop {
            let instruction = &active.callee.compiled.ops[active.pc];
            active.pc += 1;
            if OBSERVING
                && let Some(watching) = &self.projection()
                && watching.sites()
                && matches!(
                    &instruction.op,
                    Op::Call { .. } | Op::Operator { .. } | Op::Heap { .. }
                )
            {
                let mut loops = Vec::new();
                for frame in callers.iter().chain(std::iter::once(&*active)) {
                    for position in &frame.control {
                        if let Position::Loop { id, started, .. } = position {
                            let (crate::compile::Form::For { site, .. }
                            | crate::compile::Form::While { site, .. }) = &self.exe.stmt(*id).form
                            else {
                                unreachable!("a loop position names its source loop");
                            };
                            loops.push(lash_vocabulary::sites::LoopIteration {
                                site: site.clone(),
                                iteration: started.saturating_sub(1),
                            });
                        }
                    }
                }
                watching.loops(loops);
            }
            let before = self.transient;
            let next = self.instruction(active, instruction, slots);
            active.transient += self.transient.saturating_sub(before);
            match next {
                Ok(Next::Continue) => {}
                Ok(Next::Call(callee)) => {
                    if let Some(value) = self.returned_literal(callee) {
                        let Op::Call { site, .. } = &instruction.op else {
                            unreachable!("a call transition has a call instruction")
                        };
                        active.regs[site.reg as usize] = value;
                        if site.after != 0 {
                            self.admit(site.after);
                        }
                        continue;
                    }
                    let Op::Call { args, .. } = &instruction.op else {
                        return Err(self.halt(fault("a call transition has no call instruction")));
                    };
                    active.waiting = Some(active.pc - 1);
                    (active.admitted, active.swift, active.covered) =
                        (self.admitted, self.swift, self.covered);
                    let inner =
                        self.frame_from(callee, args.iter().map(|reg| active.regs[*reg as usize]));
                    slots += inner.slots.len();
                    self.record_entry(callee);
                    callers.push(std::mem::replace(active, inner));
                    self.depth += 1;
                    self.activate(active);
                }
                Ok(Next::Return(value)) => {
                    if OBSERVING
                        && !active.resident
                        && let Some(watching) = &self.projection()
                    {
                        watching.pop(self.charged, self.arena.allocations());
                    }
                    slots -= active.slots.len();
                    self.recycle(active);
                    let Some(caller) = callers.pop() else {
                        self.depth = 0;
                        return Ok(value);
                    };
                    *active = caller;
                    self.depth -= 1;
                    self.link = active.callee.link;
                    (self.admitted, self.swift, self.covered) =
                        (active.admitted, active.swift, active.covered);
                    let call = active
                        .waiting
                        .take()
                        .ok_or_else(|| self.halt(fault("a caller has no call continuation")))?;
                    let Op::Call { site, .. } = &active.callee.compiled.ops[call].op else {
                        unreachable!("a caller's continuation is a call")
                    };
                    active.regs[site.reg as usize] = value;
                    if site.after != 0 {
                        self.admit(site.after);
                    }
                }
                Err(stop) => {
                    #[cfg(any(test, feature = "perf-witness"))]
                    {
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
                                Op::Close { .. } | Op::Begin { .. } | Op::ReturnLiteral { .. } => {
                                    Reason::Charge
                                }
                                Op::Local { .. }
                                | Op::Session { .. }
                                | Op::Field { .. }
                                | Op::Index { .. } => Reason::ResidentRead,
                                Op::Operator { .. } | Op::Unit { .. } | Op::Kind { .. } => {
                                    Reason::Operator
                                }
                                Op::Native { .. } | Op::Heap { .. } => Reason::Native,
                                Op::Call { .. } => Reason::Call,
                                Op::Bind { .. } | Op::Write { .. } | Op::Store { .. } => {
                                    Reason::Binding
                                }
                                Op::Cond { .. } | Op::Branch { .. } | Op::LoopBranch { .. } => {
                                    Reason::Condition
                                }
                                Op::Cold(_) => Reason::ColdBranch,
                                _ => Reason::SourceContinuation,
                            }
                        };
                        execution::slow_exit(Owner::RegisterProgram, reason, self.scratch.counting);
                    }
                    if self.scratch.exit.halt.is_none() {
                        self.spill(active, instruction);
                        self.materialize(active, stop);
                    }
                    self.recycle(active);
                    while let Some(mut caller) = callers.pop() {
                        let at = caller
                            .waiting
                            .ok_or_else(|| self.halt(fault("a caller has no call continuation")))?;
                        let instruction = &caller.callee.compiled.ops[at];
                        let Op::Call { site, .. } = &instruction.op else {
                            unreachable!("a caller's continuation is a call")
                        };
                        if self.scratch.exit.halt.is_none() {
                            self.scratch.exit.suspended = Some(site.act);
                            self.spill(&caller, instruction);
                            self.materialize(&caller, Stop);
                        }
                        self.recycle(&mut caller);
                    }
                    self.depth = 0;
                    return Err(Stop);
                }
            }
        }
    }

    #[inline(always)]
    fn new_frame(&mut self, callee: Callee<'e>, args: &[Value]) -> Frame<'e> {
        self.frame_from(callee, args.iter().copied())
    }

    #[inline(always)]
    fn frame_from(
        &mut self,
        callee: Callee<'e>,
        args: impl ExactSizeIterator<Item = Value>,
    ) -> Frame<'e> {
        self.register_bytes += callee.compiled.register_bytes();
        let window = self.scratch.windows.pop().unwrap_or_default();
        Frame::new(
            callee,
            args,
            &self.scratch.captured[callee.captured as usize..],
            window,
        )
    }

    fn recycle(&mut self, frame: &mut Frame<'_>) {
        self.end_statement(frame);
        let mut slots = std::mem::take(&mut frame.slots);
        let mut regs = std::mem::take(&mut frame.regs);
        self.register_bytes -= frame.callee.compiled.register_bytes();
        let mut control = std::mem::take(&mut frame.control);
        slots.fill(None);
        regs.fill(Value::Null);
        control.clear();
        self.scratch.windows.push(Window {
            slots,
            regs,
            control,
        });
    }

    fn end_statement(&mut self, frame: &mut Frame<'_>) {
        self.transient = self
            .transient
            .saturating_sub(std::mem::take(&mut frame.transient));
    }

    fn activate(&mut self, frame: &mut Frame<'_>) {
        self.link = frame.callee.link;
        self.admitted = frame.callee.compiled.admits(
            self.base + u64::from(self.depth),
            self.call_depth,
            self.observed,
        );
        self.swift = self.admitted && self.plain;
        self.covered = false;
    }

    /// Only a fresh virtual call replaces a reference frame push. Resuming
    /// an existing semantic frame after a slice is no new island entry.
    fn record_entry(&self, callee: Callee<'_>) {
        if OBSERVING && let Some(watching) = &self.projection() {
            watching.push(callee.plan.code, self.charged, self.arena.allocations());
        }
        if !self.spent() {
            crate::observe::register_entry::<false>(self.recorder, self.scratch.counting);
        }
    }

    /// Keep values that a semantic handler already saved (a fused helper
    /// may have committed its first operation), then add the live outer
    /// operands. Neither scratch positions nor this physical cursor persist.
    fn spill(&mut self, frame: &Frame<'_>, instruction: &Instruction) {
        for reg in &instruction.live {
            if !self.scratch.exit.regs.iter().any(|(at, _)| at == reg) {
                self.scratch
                    .exit
                    .regs
                    .push((*reg, frame.regs[*reg as usize]));
            }
        }
    }

    fn materialize(&mut self, frame: &Frame<'_>, stop: Stop) {
        self.link = frame.callee.link;
        if let Some(stmt) = frame.statement {
            self.stand(stmt, stop);
        }
        self.scratch
            .exit
            .control
            .extend(frame.control.iter().rev().map(|position| match position {
                Position::Block(block, next) => Control::Block {
                    block: frame.callee.compiled.blocks[*block].id,
                    next: *next,
                },
                Position::Loop {
                    id,
                    started,
                    cursor: None,
                    ..
                } => Control::While {
                    stmt: *id,
                    started: *started,
                },
                Position::Loop {
                    id,
                    started,
                    cursor: Some(cursor),
                    ..
                } => Control::For {
                    stmt: *id,
                    started: *started,
                    cursor: cursor.clone(),
                    header: Header::Body,
                    guard: None,
                },
                Position::Try { instruction, phase } => {
                    let Op::Try { id, .. } = frame.callee.compiled.ops[*instruction].op else {
                        unreachable!("a try position names its instruction");
                    };
                    Control::Try {
                        stmt: id,
                        phase: phase.clone(),
                    }
                }
            }));
        self.left(frame.callee.plan, frame.callee.captured, &frame.slots);
    }

    #[inline(always)]
    fn begin(
        &mut self,
        frame: &mut Frame<'_>,
        id: StmtId,
        index: usize,
        ahead: u64,
    ) -> Result<(), Stop> {
        self.end_statement(frame);
        frame.statement = None;
        if self.admit(ahead) {
            self.charged += 1;
        } else if !self.unit() {
            return Err(Stop);
        }
        let Position::Block(_, next) = frame
            .control
            .last_mut()
            .ok_or_else(|| self.halt(fault("a statement has no enclosing block")))?
        else {
            return Err(self.halt(fault("a statement's enclosing control is not a block")));
        };
        *next = index + 1;
        frame.statement = Some(id);
        if OBSERVING && let Some(watching) = &self.projection() {
            let Some(Position::Block(block, _)) = frame.control.last() else {
                unreachable!("begin validated its block");
            };
            let block = frame.callee.compiled.blocks[*block].id;
            watching.statement(
                self.exe,
                StatementFact {
                    code: frame.callee.plan.code,
                    stmt: id,
                    block,
                    index,
                    charging: self.charging,
                },
                (self.charged, self.arena.allocations()),
            );
        }
        #[cfg(any(test, feature = "perf-witness"))]
        execution::statements(Owner::RegisterProgram, 1, self.scratch.counting);
        Ok(())
    }

    fn instruction(
        &mut self,
        frame: &mut Frame<'e>,
        instruction: &Instruction,
        slots: usize,
    ) -> Result<Next<'e>, Stop> {
        let regs = &mut frame.regs;
        match &instruction.op {
            Op::Try { .. } => {
                frame.may_clean = true;
                frame.control.push(Position::Try {
                    instruction: frame.pc - 1,
                    phase: TryPhase::Body,
                });
                frame.statement = None;
            }
            Op::Loop { id, header } => {
                frame.control.push(Position::Loop {
                    id: *id,
                    header: *header,
                    started: 0,
                    cursor: None,
                });
                frame.statement = None;
            }
            Op::For {
                id,
                header,
                reg,
                pc,
            } => {
                let cursor = Cursor::over(regs[*reg as usize], false).map_err(|_| self.at(*pc))?;
                frame.control.push(Position::Loop {
                    id: *id,
                    header: *header,
                    started: 0,
                    cursor: Some(cursor),
                });
                regs.fill(Value::Null);
                frame.statement = None;
            }
            Op::Header {
                repeat,
                id,
                cost,
                metered,
                at,
                otherwise,
                ..
            } => {
                // No header fact is committed until the complete charge
                // prefix and (for a cursor) the resident next item fit.
                // A refusal resumes next_iteration in this same step.
                let next = if let Some(at) = at {
                    let Some(Position::Loop {
                        cursor: Some(cursor),
                        ..
                    }) = frame.control.last()
                    else {
                        return Err(self.halt(fault("a for header has no cursor")));
                    };
                    let mut cursor = cursor.clone();
                    match cursor.step(self.arena) {
                        Ok(value) => Some((*at, cursor, value)),
                        _ => {
                            self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                            #[cfg(any(test, feature = "perf-witness"))]
                            {
                                self.scratch.exit.reason = Some(Reason::ResidentRead);
                            }
                            return Err(Stop);
                        }
                    }
                } else {
                    None
                };
                if !self.fits(cost.saturating_add(*metered)) {
                    self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                    #[cfg(any(test, feature = "perf-witness"))]
                    {
                        self.scratch.exit.reason = Some(Reason::Charge);
                    }
                    return Err(Stop);
                }
                if at.is_none()
                    && OBSERVING
                    && let Some(watching) = &self.projection()
                {
                    watching.condition(self.exe, *id);
                }
                // The typed kernel executes every iteration and charges
                // the next test last. Reserve the metered tail so a paid
                // test never has to materialize without a source PC.
                let mut repeated = 0;
                if self.admitted
                    && !OBSERVING
                    && self.exe.charging()
                    && let Some(site) = repeat
                {
                    let headroom = crate::compile::Headroom {
                        charged: self.charged + cost,
                        bound: self.bound.saturating_sub(*metered),
                        slice_end: self.slice_end.saturating_sub(*metered),
                        iteration_max: site.iteration_max,
                        loop_test: *cost,
                    };
                    let mut pending = Vec::new();
                    let (iterations, units, _) = match (self.tape.is_some(), self.scratch.counting)
                    {
                        (true, true) => {
                            site.run
                                .repeat::<true, true>(self.exe, frame, &mut pending, headroom)
                        }
                        (true, false) => {
                            site.run
                                .repeat::<true, false>(self.exe, frame, &mut pending, headroom)
                        }
                        (false, true) => {
                            site.run
                                .repeat::<false, true>(self.exe, frame, &mut pending, headroom)
                        }
                        (false, false) => {
                            site.run
                                .repeat::<false, false>(self.exe, frame, &mut pending, headroom)
                        }
                    };
                    if iterations != 0 {
                        self.formula(*cost);
                        self.charged += units;
                        if let Some(tape) = self.tape {
                            for weight in pending {
                                tape.push(weight);
                            }
                        }
                        if let Some(recorder) = self.recorder {
                            recorder.typed(iterations);
                        }
                        repeated = iterations;
                        #[cfg(test)]
                        super::TYPED_REPEATED.with(|n| n.set(n.get() + iterations));
                    }
                }
                if repeated == 0 {
                    self.formula(*cost);
                }
                self.formula(*metered);
                if repeated != 0
                    && let Some(Position::Loop { started, .. }) = frame.control.last_mut()
                {
                    *started += repeated;
                }
                self.covered = false;
                if let Some((at, next_cursor, value)) = next {
                    let Some(Position::Loop {
                        cursor, started, ..
                    }) = frame.control.last_mut()
                    else {
                        return Err(self.halt(fault("a for header left its loop")));
                    };
                    *cursor = Some(next_cursor);
                    match value {
                        Some(value) => {
                            *started += 1;
                            frame.slots[at] = Some(value);
                        }
                        None => frame.pc = *otherwise,
                    }
                } else {
                    frame.statement = Some(*id);
                }
            }
            Op::LoopBranch { reg, pc, otherwise } => {
                self.transient = self
                    .transient
                    .saturating_sub(std::mem::take(&mut frame.transient));
                match regs[*reg as usize] {
                    Value::Bool(true) => {
                        let Some(Position::Loop { started, .. }) = frame.control.last_mut() else {
                            return Err(self.halt(fault("a while test left its loop")));
                        };
                        *started += 1;
                    }
                    Value::Bool(false) => frame.pc = *otherwise,
                    _ => return Err(self.at(*pc)),
                }
                regs.fill(Value::Null);
                frame.statement = None;
            }
            Op::EndLoop => {
                if !matches!(frame.control.pop(), Some(Position::Loop { .. })) {
                    return Err(self.halt(fault("an exhausted loop has no control")));
                }
            }
            Op::LoopDone(leaves) => {
                let Some(Position::Loop { started, .. }) = frame.control.pop() else {
                    return Err(self.halt(fault("an exhausted loop has no control")));
                };
                // Only the loop statement's own initial step takes its
                // statement edge. A backedge completes its header step.
                if started == 0 {
                    for _ in 0..*leaves {
                        Self::pop_block(frame);
                    }
                }
                if self.spent() {
                    return Err(Stop);
                }
            }
            Op::Depart { continuing } => {
                return self.departure(
                    frame,
                    if *continuing {
                        Completion::Continue
                    } else {
                        Completion::Break
                    },
                );
            }
            Op::Open(block) => {
                frame.control.push(Position::Block(*block, 0));
                frame.statement = None;
                if self.spent() || self.arena.boundary_due() {
                    return Err(Stop);
                }
            }
            Op::Begin { id, index, ahead } => self.begin(frame, *id, *index, *ahead)?,
            Op::ReturnLiteral {
                id,
                index,
                ahead,
                value,
                close,
                covered,
                reg,
                entry,
            } => {
                if let Some(block) = entry
                    && frame.control.is_empty()
                {
                    frame.control.push(Position::Block(*block, 0));
                    frame.statement = None;
                    if self.spent() {
                        return Err(Stop);
                    }
                }
                self.begin(frame, *id, *index, *ahead)?;
                if let Some(close) = close
                    && !self.close_in(close, *covered)
                {
                    return Err(self.at_held(close.head, *reg, *value));
                }
                if self.public_return && self.depth == 1 && frame.may_clean {
                    return Err(self.at_held(self.exe.stmt(*id).ops.end() - 1, *reg, *value));
                }
                return self.returning(frame, *value);
            }
            Op::Constant { dst, value } => regs[*dst as usize] = *value,
            Op::Local {
                dst,
                at,
                cell,
                miss,
            } => {
                regs[*dst as usize] = match cell {
                    Some(index) => {
                        let (_, cell) =
                            self.scratch.captured[frame.callee.captured as usize + index];
                        self.arena.cell(cell).ok()
                    }
                    None => frame.slots[*at],
                }
                .ok_or_else(|| self.at(*miss))?;
            }
            Op::Session { dst, name, miss } => {
                regs[*dst as usize] = self.session(*name).ok_or_else(|| self.at(*miss))?;
            }
            Op::Operator {
                site,
                args,
                covered,
            } => {
                let mut values = [Value::Absent; 3];
                for (to, reg) in values.iter_mut().zip(args.iter()) {
                    *to = regs[*reg as usize];
                }
                let values = &values[..args.len()];
                let typed = site.entry.pair().and_then(|pair| {
                    let [x, y] = values else {
                        return None;
                    };
                    compiled::typed(site, &pair, *x, *y, self.exe)
                });
                let answer = if OBSERVING {
                    self.covered = false;
                    self.operate(site, values)
                } else {
                    match typed {
                        Some((value, Some(units))) if *covered && self.covered => {
                            self.charged += site.fixed;
                            if let Some(occurrence) = site.counted {
                                self.count(occurrence)?;
                            }
                            self.charged += units;
                            Ok(value)
                        }
                        Some((value, units)) => match self.typed(site, value, units) {
                            Some(answer) => answer,
                            None => {
                                self.covered = false;
                                self.operate(site, values)
                            }
                        },
                        None => {
                            self.covered = false;
                            self.operate(site, values)
                        }
                    }
                };
                regs[site.reg as usize] = answer?;
            }
            Op::Native { site, args } => {
                let mut values = [Value::Absent; MAX_ARGS];
                for (to, reg) in values.iter_mut().zip(args.iter()) {
                    *to = regs[*reg as usize];
                }
                regs[site.reg as usize] = self.native(site, &values, args.len())?;
            }
            Op::Heap { site, args } => {
                let mut values = [Value::Absent; MAX_ARGS];
                for (to, reg) in values.iter_mut().zip(args.iter()) {
                    *to = regs[*reg as usize];
                }
                regs[site.reg as usize] = self.heap_call(site, &values[..args.len()])?;
            }
            Op::Reserve {
                kind,
                members,
                miss,
            } => {
                if !self.reserve_construction(*kind, *members) {
                    return Err(self.at(*miss));
                }
            }
            Op::Make {
                make,
                items,
                reg,
                members,
                miss,
            } => {
                let kind = make.kind();
                let floor = self.construction_floor(kind, *members);
                self.transient = self.transient.saturating_sub(floor);
                frame.transient = frame.transient.saturating_sub(floor);
                let values = items.iter().map(|reg| regs[*reg as usize]).collect();
                regs[*reg as usize] = self.make(*make, values).ok_or_else(|| self.at(*miss))?;
            }
            Op::Record {
                names,
                values,
                reg,
                members,
                miss,
            } => {
                let floor = self.construction_floor(lash_kernel_doc::ValueKind::Record, *members);
                self.transient = self.transient.saturating_sub(floor);
                frame.transient = frame.transient.saturating_sub(floor);
                let values = values.iter().map(|reg| regs[*reg as usize]).collect();
                regs[*reg as usize] = self.record(names, values).ok_or_else(|| self.at(*miss))?;
            }
            Op::Store {
                target,
                index,
                value,
                field,
                pc,
            } => {
                self.store(
                    *field,
                    regs[*target as usize],
                    index.map_or(Value::Absent, |reg| regs[reg as usize]),
                    regs[*value as usize],
                )
                .ok_or_else(|| self.at(*pc))?;
            }
            Op::Unit { site, src } => {
                regs[site.reg as usize] = self.unit_test(site, regs[*src as usize])?
            }
            Op::Kind { site, src } => {
                regs[site.same.reg as usize] = self.kind_test(site, regs[*src as usize])?
            }
            Op::Field { dst, field, miss } => {
                let x = regs[*dst as usize];
                regs[*dst as usize] = self
                    .field(x, *field)
                    .ok_or_else(|| self.at_held(*miss, *dst, x))?;
            }
            Op::Index { dst, site, miss } => {
                let (x, at) = (regs[*dst as usize], regs[*dst as usize + 1]);
                regs[*dst as usize] = self
                    .item(*site, x, at)
                    .ok_or_else(|| self.at_operands(*miss, *dst, &[x, at]))?;
            }
            Op::Call { site, read, args } => {
                let callee = read.then(|| regs[site.reg as usize]);
                if let Some(close) = &site.close
                    && !self.close(close)
                {
                    return Err(self.at(close.head));
                }
                let target = match (site.callee, callee) {
                    (Some(code), _) => self.declared(code),
                    (None, Some(callee)) => self.resolve(site.site, callee),
                    _ => None,
                }
                .filter(|target| {
                    args.len() <= target.compiled.layout.arity
                        && self.depth < self.frames
                        && slots + target.plan.slots <= ISLAND_SLOTS
                        && self.register_room(target.compiled)
                });
                let Some(target) = target else {
                    #[cfg(any(test, feature = "perf-witness"))]
                    {
                        self.scratch.exit.reason = Some(if self.depth >= self.frames {
                            Reason::Depth
                        } else {
                            Reason::Call
                        });
                    }
                    return Err(self.at(site.pc));
                };
                if OBSERVING
                    && self
                        .watching
                        .as_ref()
                        .is_some_and(|watching| !watching.can_call())
                {
                    return Err(self.at(site.pc));
                }
                self.count(site.occurrence)?;
                return Ok(Next::Call(target));
            }
            Op::Bind { at, src } => {
                let value = regs[*src as usize];
                frame.slots[*at] = Some(value);
            }
            Op::Unbind(at) => frame.slots[*at] = None,
            Op::Cond {
                reg,
                close,
                pc,
                otherwise,
            } => {
                if let Some(close) = close
                    && !self.close(close)
                {
                    return Err(self.at(close.head));
                }
                match regs[*reg as usize] {
                    Value::Bool(true) => {}
                    Value::Bool(false) => frame.pc = *otherwise,
                    _ => return Err(self.at(*pc)),
                }
            }
            Op::Jump(pc) => frame.pc = *pc,
            Op::Close { close, covered, .. } => {
                if !self.close_in(close, *covered) {
                    return Err(self.at(close.head));
                }
            }
            Op::Write {
                at,
                cell,
                reg,
                pc,
                bind,
            } => {
                let value = regs[*reg as usize];
                if !*bind && frame.slots[*at].is_none() {
                    return Err(self.at(*pc));
                }
                if let Some(index) = cell {
                    let (_, cell) = self.scratch.captured[frame.callee.captured as usize + index];
                    self.arena.cell_set(cell, value).map_err(|_| self.at(*pc))?;
                } else {
                    frame.slots[*at] = Some(value);
                }
            }
            Op::Return { reg, pc } => {
                if self.public_return && self.depth == 1 && frame.may_clean {
                    return Err(self.at(*pc));
                }
                let value = regs[*reg as usize];
                return self.returning(frame, value);
            }
            Op::Done { leaves, empty } => {
                if let Some(block) = empty
                    && OBSERVING
                    && let Some(watching) = &self.projection()
                {
                    watching.empty(self.exe, *block);
                }
                self.transient = self
                    .transient
                    .saturating_sub(std::mem::take(&mut frame.transient));
                let due = self.arena.boundary_due();
                regs.fill(Value::Null);
                frame.statement = None;
                for _ in 0..*leaves {
                    let Position::Block(block, _) = frame.control.pop().ok_or_else(|| {
                        self.halt(fault("a step has no enclosing block to leave"))
                    })?
                    else {
                        return Err(self.halt(fault("a block edge leaves a loop")));
                    };
                    let block = &frame.callee.compiled.blocks[block];
                    for at in &block.declares {
                        frame.slots[*at as usize] = None;
                    }
                    frame.pc = block.after;
                }
                if frame.control.is_empty() {
                    return Ok(Next::Return(Value::Null));
                }
                if let Some(Position::Loop { header, .. }) = frame.control.last() {
                    frame.pc = *header;
                    if due || self.spent() {
                        return Err(Stop);
                    }
                    return Ok(Next::Continue);
                }
                if let Some(Position::Try { instruction, phase }) = frame.control.last_mut() {
                    let Op::Try { finally, after, .. } = frame.callee.compiled.ops[*instruction].op
                    else {
                        unreachable!("a try position names its instruction");
                    };
                    match std::mem::replace(phase, TryPhase::Body) {
                        TryPhase::Finally(departure) => {
                            frame.control.pop();
                            if !matches!(departure, Completion::Normal) {
                                return self.departure(frame, departure);
                            }
                            frame.pc = after;
                        }
                        TryPhase::Body | TryPhase::Catch => match finally {
                            Some(cleanup) => {
                                *phase = TryPhase::Finally(Completion::Normal);
                                frame.pc = cleanup;
                                return Ok(Next::Continue);
                            }
                            None => {
                                frame.control.pop();
                                frame.pc = after;
                            }
                        },
                    }
                }
                if due || self.spent() {
                    return Err(Stop);
                }
            }
            Op::Branch { reg, pc, otherwise } => {
                match regs[*reg as usize] {
                    Value::Bool(true) => {}
                    Value::Bool(false) => frame.pc = *otherwise,
                    _ => return Err(self.at(*pc)),
                }
                self.transient = self
                    .transient
                    .saturating_sub(std::mem::take(&mut frame.transient));
                regs.fill(Value::Null);
                frame.statement = None;
            }
            Op::Cold(pc) => return Err(self.at(*pc)),
        }
        Ok(Next::Continue)
    }

    fn pop_block(frame: &mut Frame<'_>) {
        if let Some(Position::Block(block, _)) = frame.control.pop() {
            frame.pc = frame.callee.compiled.blocks[block].after;
            for at in &frame.callee.compiled.blocks[block].declares {
                frame.slots[*at as usize] = None;
            }
        }
    }

    #[inline(always)]
    fn returning(&mut self, frame: &mut Frame<'e>, value: Value) -> Result<Next<'e>, Stop> {
        if !frame.may_clean {
            return Ok(Next::Return(value));
        }
        self.departure(frame, Completion::Return(Box::new(value)))
    }

    fn departure(
        &mut self,
        frame: &mut Frame<'e>,
        departure: Completion,
    ) -> Result<Next<'e>, Stop> {
        frame.regs.fill(Value::Null);
        frame.statement = None;
        loop {
            match frame.control.last_mut() {
                Some(Position::Block(..)) => Self::pop_block(frame),
                Some(Position::Loop { header, .. }) => {
                    let header = *header;
                    match departure {
                        Completion::Continue => {
                            frame.pc = header;
                            return Ok(Next::Continue);
                        }
                        Completion::Break => {
                            let Op::Header { after, .. } = frame.callee.compiled.ops[header].op
                            else {
                                unreachable!("a loop position names its header");
                            };
                            frame.pc = after;
                            frame.control.pop();
                            if self.spent() {
                                return Err(Stop);
                            }
                            return Ok(Next::Continue);
                        }
                        _ => {
                            frame.control.pop();
                        }
                    }
                }
                Some(Position::Try { instruction, phase }) => {
                    let Op::Try { finally, .. } = frame.callee.compiled.ops[*instruction].op else {
                        unreachable!("a try position names its instruction");
                    };
                    if !matches!(phase, TryPhase::Finally(_))
                        && let Some(cleanup) = finally
                    {
                        *phase = TryPhase::Finally(departure);
                        frame.pc = cleanup;
                        return Ok(Next::Continue);
                    }
                    frame.control.pop();
                }
                None => {
                    return match departure {
                        Completion::Return(value) => Ok(Next::Return(*value)),
                        _ => Err(self.halt(fault("a control departure has no owner"))),
                    };
                }
            }
        }
    }
}
