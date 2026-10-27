//! One active register frame. Calls move it onto an explicit stack and
//! returns recover it; Rust stack depth is independent of guest call depth.
//! Every legal step polls, including straight-line statements and frame entry.

#[cfg(any(test, feature = "perf-witness"))]
use crate::perf_witness::execution::{self, Owner, Reason};

use super::compiled::{Instruction, Op};
use super::*;
use crate::machine::loops::{Cursor, Guard, Header};
use crate::machine::{Completion, LibraryCall, TryPhase};

mod access;
mod accumulate;
mod boundary;
mod call;
mod merge;
mod numeric;

enum Position {
    Block(usize, usize),
    Loop {
        id: StmtId,
        header: usize,
        started: u64,
        cursor: Option<Cursor>,
        /// A protocol walk's check, which its header hooks keep.
        guard: Option<Box<Guard>>,
    },
    Try {
        instruction: usize,
        phase: TryPhase,
    },
}

impl Position {
    #[inline(always)]
    fn project(&self, compiled: &Compiled, target: &mut Control) {
        match self {
            Position::Block(block, next) => {
                let id = compiled.blocks[*block].id;
                if let Control::Block { block, next: at } = target {
                    *block = id;
                    *at = *next;
                } else {
                    *target = Control::Block {
                        block: id,
                        next: *next,
                    };
                }
            }
            Position::Loop {
                id,
                started,
                cursor: None,
                ..
            } => {
                if let Control::While { stmt, started: at } = target {
                    *stmt = *id;
                    *at = *started;
                } else {
                    *target = Control::While {
                        stmt: *id,
                        started: *started,
                    };
                }
            }
            Position::Loop {
                id,
                started,
                cursor: Some(cursor),
                guard: kept,
                ..
            } => {
                if let Control::For {
                    stmt,
                    started: at,
                    cursor: saved,
                    header,
                    guard,
                } = target
                {
                    *stmt = *id;
                    *at = *started;
                    saved.clone_from(cursor);
                    *header = Header::Body;
                    guard.clone_from(kept);
                } else {
                    *target = Control::For {
                        stmt: *id,
                        started: *started,
                        cursor: cursor.clone(),
                        header: Header::Body,
                        guard: kept.clone(),
                    };
                }
            }
            Position::Try { instruction, phase } => {
                let Op::Try { id, .. } = compiled.ops[*instruction].op else {
                    unreachable!("a try position names its instruction");
                };
                if let Control::Try { stmt, phase: saved } = target {
                    *stmt = id;
                    saved.clone_from(phase);
                } else {
                    *target = Control::Try {
                        stmt: id,
                        phase: phase.clone(),
                    };
                }
            }
        }
    }

    fn projected(&self, compiled: &Compiled) -> Control {
        let mut control = Control::Block {
            block: compiled.blocks[0].id,
            next: 0,
        };
        self.project(compiled, &mut control);
        control
    }
}

/// Reusable physical windows own no logical value between entries.
#[derive(Default)]
pub(super) struct Window {
    slots: Vec<SlotState>,
    regs: Vec<Value>,
    control: Vec<Position>,
    texts: Vec<(usize, ArenaHandle)>,
}

pub(super) struct Frame<'e> {
    callee: Callee<'e>,
    slots: Vec<SlotState>,
    regs: Vec<Value>,
    pc: usize,
    /// Compiled block identity and next source statement, outermost first.
    control: Vec<Position>,
    /// Each accumulator slot whose long text its own statement made in its
    /// loop's running activation, which nothing else holds (FIG-6795).
    texts: Vec<(usize, ArenaHandle)>,
    statement: Option<StmtId>,
    waiting: Option<usize>,
    may_clean: bool,
    admitted: bool,
    swift: bool,
    covered: bool,
    transient: u64,
    resident: bool,
    handoff: bool,
    recovery: bool,
}

impl crate::compile::Slots for Frame<'_> {
    #[inline(always)]
    fn value(&self, at: u32) -> Option<Value> {
        if self.callee.plan.captures.contains(&at) {
            return None;
        }
        match self.slots.get(at as usize)? {
            SlotState::Value(value) => Some(*value),
            SlotState::Empty | SlotState::Cell(_) => None,
        }
    }
    #[inline(always)]
    fn assign(&mut self, at: u32, value: Value) {
        self.slots[at as usize] = SlotState::Value(value);
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
        window.slots.resize(callee.plan.slots, SlotState::Empty);
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
            texts: window.texts,
            statement: None,
            waiting: None,
            may_clean: false,
            admitted: false,
            swift: false,
            covered: false,
            transient: 0,
            resident: false,
            handoff: false,
            recovery: false,
        }
    }
}

/// A single operation either advances the active cursor, calls, returns,
/// or exits with the source map. No operation owns another control loop.
enum Next {
    Continue,
    Return(Value),
}

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(super) fn enter(&mut self, callee: Callee<'e>, args: &[Value]) -> Answer {
        if let Some(value) = self.returned_literal(callee) {
            return Ok(value);
        }
        self.record_entry(callee, None);
        let mut active = self.new_frame(callee, args);
        self.execute(&mut active)
    }

    /// A single literal return needs no physical slot or control window.
    /// Admission still belongs to the call site. When either event could
    /// refuse, the ordinary instruction path preserves its exact prefix.
    #[inline(always)]
    fn returned_literal(&mut self, callee: Callee<'e>) -> Option<Value> {
        let literal = callee.compiled.literal.as_ref()?;
        let units = literal.units;
        if self.spent() || !self.fits(units) {
            return None;
        }
        crate::observe::register_entry::<true>(
            #[cfg(any(test, feature = "perf-witness"))]
            &self.scratch.census,
            self.recorder,
            self.scratch.counting,
        );
        if OBSERVING && let Some(watching) = &self.projection() {
            let Op::ReturnLiteral {
                id, index, close, ..
            } = &callee.compiled.ops[0].op
            else {
                unreachable!("the literal projection came from its instruction");
            };
            watching.push(callee.plan.code, self.charged, self.arena.allocations());
            watching.statement(
                self.exe,
                StatementFact {
                    code: callee.plan.code,
                    stmt: *id,
                    block: callee.plan.body.id,
                    index: *index,
                    charging: self.exe.charging(),
                },
                (self.charged + 1, self.arena.allocations()),
            );
            if let Some(close) = close {
                watching.expressions(callee.stream, close, self.exe.charging());
            }
            watching.pop(self.charged + units, self.arena.allocations());
        }
        self.charged += units;
        if let Some(tape) = self.recorder.filter(|_| self.exe.charging()) {
            let Op::ReturnLiteral { close, .. } = &callee.compiled.ops[0].op else {
                unreachable!("the literal projection came from its instruction");
            };
            tape.push(Weight::Atomic(1));
            if let Some(close) = close {
                super::record(tape, callee.stream, close.region);
            }
        }
        Some(literal.value)
    }

    // One loop and instruction inventory serve both modes. The entry-fixed
    // flag erases passive projection taps when no observer borrowed this run.
    #[inline(always)]
    pub(super) fn execute(&mut self, active: &mut Frame<'e>) -> Answer {
        #[cfg(any(test, feature = "perf-witness"))]
        if self.scratch.counting {
            return self.execute_counted(active);
        }
        self.execute_program(active)
    }

    #[cfg(any(test, feature = "perf-witness"))]
    #[cold]
    #[inline(never)]
    fn execute_counted(&mut self, active: &mut Frame<'e>) -> Answer {
        let _execution =
            execution::Scope::enter(&self.scratch.census, Owner::RegisterProgram, true);
        self.execute_program(active)
    }

    fn execute_program(&mut self, active: &mut Frame<'e>) -> Answer {
        let mut callers = Vec::new();
        // This loop owns both the physical frames and their reservation.
        // It never commits a call that cannot fit all live windows.
        let mut slots = active.slots.len();
        self.depth = 1;
        self.activate(active);
        loop {
            let instruction = &active.callee.compiled.ops[active.pc];
            active.pc += 1;
            if let Some(begin) = &instruction.begin
                && let Err(stop) = self.begin(active, begin.id, begin.index, begin.ahead)
            {
                return self.slow_exit(active, &mut callers, instruction, stop);
            }
            if OBSERVING
                && let Some(watching) = &self.projection()
                && watching.sites()
                && matches!(
                    &instruction.op,
                    Op::Call { .. }
                        | Op::Operator { .. }
                        | Op::DirectOperator { .. }
                        | Op::Unary { .. }
                        | Op::Binary { .. }
                        | Op::Accumulation { .. }
                        | Op::Heap { .. }
                        | Op::MemberAccess { .. }
                        | Op::MemberStore { .. }
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
                            loops.push(lash_kernel_value::sites::LoopIteration {
                                site: site.clone(),
                                iteration: started.saturating_sub(1),
                            });
                        }
                    }
                }
                watching.loops(loops);
            }
            let next = self.instruction(active, instruction, &mut callers, &mut slots);
            match next {
                Ok(Next::Continue) => {}
                Ok(Next::Return(value)) => {
                    if !active.callee.plan.charged
                        && let Err(stop) = self.complete_library(active, callers.last(), value)
                    {
                        return self.slow_exit(active, &mut callers, instruction, stop);
                    }
                    if OBSERVING
                        && !active.resident
                        && let Some(watching) = &self.projection()
                    {
                        watching.pop(self.charged, self.arena.allocations());
                    }
                    if !active.resident
                        && let Some(sampling) = self.sampling
                    {
                        sampling.pop();
                    }
                    slots -= active.slots.len();
                    self.recycle(active);
                    let Some(caller) = callers.pop() else {
                        self.depth = 0;
                        return Ok(value);
                    };
                    *active = caller;
                    self.depth -= 1;
                    self.activate_mode(active);
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
                Err(stop) => return self.slow_exit(active, &mut callers, instruction, stop),
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
        let mut frame = self.frame_window(callee, args);
        if callee.compiled.cell_parameters != 0 {
            for at in &callee.plan.params {
                if callee.plan.shared[*at as usize] {
                    let value = match frame.slots[*at as usize] {
                        SlotState::Value(value) => value,
                        _ => Value::Absent,
                    };
                    // Call admission claimed these bytes before its occurrence.
                    frame.slots[*at as usize] = SlotState::Cell(self.arena.new_cell(value));
                }
            }
        }
        frame
    }

    #[inline(always)]
    fn frame_window(
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

    #[inline(always)]
    fn recycle(&mut self, frame: &mut Frame<'_>) {
        self.end_statement(frame);
        let mut slots = std::mem::take(&mut frame.slots);
        let mut regs = std::mem::take(&mut frame.regs);
        self.register_bytes -= frame.callee.compiled.register_bytes();
        let mut control = std::mem::take(&mut frame.control);
        let mut texts = std::mem::take(&mut frame.texts);
        slots.fill(SlotState::Empty);
        regs.fill(Value::Null);
        control.clear();
        texts.clear();
        self.scratch.windows.push(Window {
            slots,
            regs,
            control,
            texts,
        });
    }

    fn end_statement(&mut self, frame: &mut Frame<'_>) {
        self.transient = self
            .transient
            .saturating_sub(std::mem::take(&mut frame.transient));
    }

    /// An accumulator statement, which cannot stop once its concatenation
    /// answered, wrote `value` to the slot at `at`: the text that
    /// concatenation made is the slot's to extend again. Storage it took
    /// belongs to that value; any other value would leave it with a text
    /// no longer whole.
    #[inline(always)]
    fn accumulated(
        &mut self,
        frame: &mut Frame<'e>,
        at: usize,
        value: Value,
        made: crate::compile::Made,
    ) -> Result<(), Stop> {
        match value {
            Value::LongText(text) if text == made.text => {
                #[cfg(any(test, feature = "perf-witness"))]
                if made.moved.is_some() {
                    execution::accumulated(&self.scratch.census, self.scratch.counting);
                }
                frame.texts.push((at, text));
                Ok(())
            }
            _ if made.moved.is_none() => Ok(()),
            _ => Err(self.halt(fault(
                "an accumulator's concatenation is not its statement's value",
            ))),
        }
    }

    /// A statement that stops after its accumulator's concatenation took
    /// its operand's storage gives the storage back, so its source owner
    /// runs it over the text it read.
    pub(super) fn give_back(&mut self) {
        if let Some(crate::compile::Accumulator {
            left,
            made: Some(made),
            ..
        }) = self.accumulator.take()
            && let Some(len) = made.moved
        {
            self.arena.unextend_text(left, made.text, len);
        }
    }

    fn activate(&mut self, frame: &mut Frame<'e>) {
        self.activate_mode(frame);
        if let Some(sampling) = self.sampling {
            sampling.statement(frame.callee.plan.code, frame.statement);
        }
        self.admitted = frame.callee.compiled.admits(
            self.base + u64::from(self.depth),
            self.call_depth,
            self.observed,
        );
        self.swift = self.admitted && self.plain;
        self.covered = false;
    }

    #[inline(always)]
    fn activate_mode(&mut self, frame: &Frame<'e>) {
        self.link = frame.callee.link;
        self.stream = frame.callee.stream;
        // Entry starts in the charged/pace policy of Ground. Same-mode
        // calls retain that policy; a helper/callback handoff changes it.
        if self.accounting != frame.callee.plan.charged {
            self.accounting = frame.callee.plan.charged;
            self.charging = self.exe.charging() && self.accounting;
            self.tape = self.recorder.filter(|_| self.charging);
            self.plain = self.charging && self.tape.is_none() && !OBSERVING;
        }
    }

    /// Only a fresh virtual call replaces a reference frame push. Resuming
    /// an existing semantic frame after a slice is no new island entry.
    fn record_entry(&self, callee: Callee<'_>, caller: Option<(CodeId, Option<StmtId>)>) {
        if let Some(sampling) = self.sampling {
            if let Some((code, statement)) = caller {
                sampling.statement(code, statement);
            }
            sampling.push(callee.plan.code);
        }
        if OBSERVING && let Some(watching) = &self.projection() {
            watching.push(callee.plan.code, self.charged, self.arena.allocations());
        }
        if !self.spent() {
            crate::observe::register_entry::<false>(
                #[cfg(any(test, feature = "perf-witness"))]
                &self.scratch.census,
                self.recorder,
                self.scratch.counting,
            );
        }
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
        if self.deferred_end.is_some() {
            self.progressed();
        }
        if self.admit(ahead) {
            self.charged += 1;
        } else if self.accounting {
            self.unit()?;
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
        execution::statements(
            &self.scratch.census,
            Owner::RegisterProgram,
            1,
            self.scratch.counting,
        );
        Ok(())
    }

    /// Attribute nursery growth to the open statement even on refusal.
    /// Pure instructions never touch this ledger.
    #[inline(always)]
    fn retaining<T>(&mut self, retained: &mut u64, operation: impl FnOnce(&mut Self) -> T) -> T {
        let before = self.transient;
        let answer = operation(self);
        *retained += self.transient.saturating_sub(before);
        answer
    }

    #[inline(always)]
    fn instruction(
        &mut self,
        frame: &mut Frame<'e>,
        instruction: &Instruction,
        callers: &mut Vec<Frame<'e>>,
        slots: &mut usize,
    ) -> Result<Next, Stop> {
        let mut op = &instruction.op;
        loop {
            let regs = &mut frame.regs;
            match op {
                Op::Collection {
                    step,
                    tail,
                    original,
                } => {
                    if let Some(answer) = self.collection_step(frame, step, *tail) {
                        return answer;
                    }
                    op = original;
                    continue;
                }
                Op::Try { .. } => {
                    frame.may_clean = true;
                    frame.control.push(Position::Try {
                        instruction: frame.pc - 1,
                        phase: TryPhase::Body,
                    });
                    frame.statement = None;
                }
                Op::Loop { id, header, texts } => {
                    frame.texts.retain(|(slot, _)| !texts.contains(slot));
                    frame.control.push(Position::Loop {
                        id: *id,
                        header: *header,
                        started: 0,
                        cursor: None,
                        guard: None,
                    });
                    frame.statement = None;
                }
                Op::For {
                    id,
                    header,
                    reg,
                    pc,
                    via,
                    texts,
                } => {
                    if *via {
                        return Err(self.at(*pc));
                    }
                    frame.texts.retain(|(slot, _)| !texts.contains(slot));
                    let cursor =
                        Cursor::over(regs[*reg as usize], false).map_err(|_| self.at(*pc))?;
                    frame.control.push(Position::Loop {
                        id: *id,
                        header: *header,
                        started: 0,
                        cursor: Some(cursor),
                        guard: None,
                    });
                    frame.statement = None;
                }
                Op::Header {
                    repeat,
                    id,
                    cost,
                    metered,
                    at,
                    hooked,
                    otherwise,
                    ..
                } => {
                    if self.deferred_end.is_some() {
                        self.progressed();
                    }
                    // No header fact is committed until the complete charge
                    // prefix and (for a cursor) the resident next item fit.
                    // A refusal resumes next_iteration in this same step.
                    let next = if let Some(at) = at {
                        if *hooked {
                            self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                            return Err(Stop);
                        }
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
                    if next.as_ref().is_some_and(|(at, _, value)| {
                        value.is_some() && frame.callee.plan.shared[*at] && !self.cell_room(1)
                    }) {
                        self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                        return Err(Stop);
                    }
                    if !self
                        .fits((if self.accounting { *cost } else { 0 }).saturating_add(*metered))
                    {
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
                        let (iterations, units, _) =
                            match (self.tape.is_some(), self.scratch.counting) {
                                (true, true) => site.run.repeat::<true, true>(
                                    self.exe,
                                    #[cfg(any(test, feature = "perf-witness"))]
                                    &self.scratch.census,
                                    frame,
                                    &mut pending,
                                    headroom,
                                ),
                                (true, false) => site.run.repeat::<true, false>(
                                    self.exe,
                                    #[cfg(any(test, feature = "perf-witness"))]
                                    &self.scratch.census,
                                    frame,
                                    &mut pending,
                                    headroom,
                                ),
                                (false, true) => site.run.repeat::<false, true>(
                                    self.exe,
                                    #[cfg(any(test, feature = "perf-witness"))]
                                    &self.scratch.census,
                                    frame,
                                    &mut pending,
                                    headroom,
                                ),
                                (false, false) => site.run.repeat::<false, false>(
                                    self.exe,
                                    #[cfg(any(test, feature = "perf-witness"))]
                                    &self.scratch.census,
                                    frame,
                                    &mut pending,
                                    headroom,
                                ),
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
                    if *metered != 0 {
                        self.scheduled(*metered);
                    }
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
                                frame.slots[at] = if frame.callee.plan.shared[at] {
                                    SlotState::Cell(self.arena.new_cell(value))
                                } else {
                                    SlotState::Value(value)
                                };
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
                            let Some(Position::Loop { started, .. }) = frame.control.last_mut()
                            else {
                                return Err(self.halt(fault("a while test left its loop")));
                            };
                            *started += 1;
                        }
                        Value::Bool(false) => frame.pc = *otherwise,
                        _ => return Err(self.at(*pc)),
                    }
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
                    if frame.recovery || (self.public_return && self.depth == 1 && frame.may_clean)
                    {
                        return Err(self.at_held(self.exe.stmt(*id).ops.end() - 1, *reg, *value));
                    }
                    return self.returning(frame, *value);
                }
                Op::Keep(restart) => {
                    regs[restart.saved as usize] = regs[restart.reg as usize];
                    regs[restart.saved as usize + 1] = Value::Bool(true);
                }
                Op::Constant { dst, value } => regs[*dst as usize] = *value,
                Op::Local {
                    dst,
                    at,
                    shared,
                    miss,
                } => {
                    regs[*dst as usize] = if *shared {
                        let SlotState::Cell(cell) = frame.slots[*at] else {
                            return Err(self.at(*miss));
                        };
                        self.arena.cell(cell).ok()
                    } else {
                        match frame.slots[*at] {
                            SlotState::Value(value) => Some(value),
                            _ => None,
                        }
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
                    regs[site.reg as usize] = self.retaining(&mut frame.transient, |island| {
                        island.operator_value(site, values, *covered)
                    })?;
                }
                Op::DirectOperator {
                    site,
                    reads,
                    covered,
                } => {
                    let values = self.inputs::<3>(&frame.slots, reads)?;
                    regs[site.reg as usize] = self.retaining(&mut frame.transient, |island| {
                        island.operator_value(site, &values[..reads.len()], *covered)
                    })?;
                }
                Op::Unary {
                    site,
                    arg,
                    covered,
                    to,
                } => {
                    return self.numeric_unary(frame, site, arg, *covered, to);
                }
                Op::Binary {
                    kernel,
                    site,
                    left,
                    right,
                    covered,
                    to,
                } => {
                    return self.numeric_binary(frame, *kernel, site, (left, right), *covered, to);
                }
                Op::Native { site, args } => {
                    let mut values = [Value::Absent; MAX_ARGS];
                    for (to, reg) in values.iter_mut().zip(args.iter()) {
                        *to = regs[*reg as usize];
                    }
                    let answer = self.retaining(&mut frame.transient, |island| {
                        island.native(site, &values, args.len())
                    });
                    regs[site.reg as usize] = answer?;
                }
                Op::DirectNative { site, reads } => {
                    let values = self.inputs::<MAX_ARGS>(&frame.slots, reads)?;
                    regs[site.reg as usize] = self.retaining(&mut frame.transient, |island| {
                        island.native(site, &values, reads.len())
                    })?;
                }
                Op::MemberStore {
                    site,
                    args,
                    reads,
                    property,
                    finish,
                } => {
                    let values = match reads {
                        Some(reads) => self.inputs::<3>(&frame.slots, reads.as_slice())?,
                        None => std::array::from_fn(|at| frame.regs[args[at] as usize]),
                    };
                    let answer = self
                        .member_write(frame.statement, property, site.close, &values)
                        .map(|answer| answer.map(|()| values[2]))
                        .unwrap_or_else(|| {
                            self.retaining(&mut frame.transient, |island| {
                                island.heap_call(site, &values)
                            })
                        });
                    frame.regs[site.reg as usize] = answer.inspect_err(|_| {
                        for (source, value) in args.iter().zip(values) {
                            if !self.scratch.exit.regs.iter().any(|(reg, _)| reg == source) {
                                self.scratch.exit.regs.push((*source, value));
                            }
                        }
                    })?;
                    if let Some(leaves) = finish {
                        return self.finish_statement(frame, *leaves, None);
                    }
                }
                Op::MemberAccess {
                    site,
                    args,
                    units,
                    reads,
                } => {
                    let values = match reads {
                        Some(reads) => self.inputs::<2>(&frame.slots, reads.as_slice())?,
                        None => [regs[args[0] as usize], regs[args[1] as usize]],
                    };
                    let answer = self.member_value(site, &values, *units).unwrap_or_else(|| {
                        self.retaining(&mut frame.transient, |island| {
                            island.heap_call(site, &values)
                        })
                    });
                    regs[site.reg as usize] = answer.inspect_err(|_| {
                        for (source, value) in args.iter().zip(values) {
                            if !self.scratch.exit.regs.iter().any(|(reg, _)| reg == source) {
                                self.scratch.exit.regs.push((*source, value));
                            }
                        }
                    })?;
                }
                Op::Heap { site, args } => {
                    let mut values = [Value::Absent; MAX_ARGS];
                    for (to, reg) in values.iter_mut().zip(args.iter()) {
                        *to = regs[*reg as usize];
                    }
                    let answer = self.retaining(&mut frame.transient, |island| {
                        island.heap_call(site, &values[..args.len()])
                    });
                    regs[site.reg as usize] = answer?;
                }
                Op::Reserve {
                    kind,
                    members,
                    miss,
                } => {
                    if !self.retaining(&mut frame.transient, |island| {
                        island.reserve_construction(*kind, *members)
                    }) {
                        return Err(self.at(*miss));
                    }
                }
                Op::Make {
                    ahead,
                    make,
                    items,
                    reg,
                    members,
                    miss,
                } => {
                    // Construction destroys its source operands. Refuse before
                    // that overwrite when the unpaid source region cannot fit;
                    // otherwise a later close would replay it with its result.
                    if !self.fits(*ahead) {
                        return Err(self.at(*miss));
                    }
                    let before = self.transient;
                    let kind = make.kind();
                    let floor = self.construction_floor(kind, *members);
                    self.transient = self.transient.saturating_sub(floor);
                    frame.transient = frame.transient.saturating_sub(floor);
                    let values = items.iter().map(|reg| regs[*reg as usize]);
                    let answer = self.make(*make, values);
                    frame.transient += self.transient.saturating_sub(before);
                    regs[*reg as usize] = answer.ok_or_else(|| self.at(*miss))?;
                }
                Op::Record {
                    ahead,
                    names,
                    values,
                    reg,
                    members,
                    miss,
                } => {
                    if !self.fits(*ahead) {
                        return Err(self.at(*miss));
                    }
                    let before = self.transient;
                    let floor =
                        self.construction_floor(lash_kernel_doc::ValueKind::Record, *members);
                    self.transient = self.transient.saturating_sub(floor);
                    frame.transient = frame.transient.saturating_sub(floor);
                    let values = values.iter().map(|reg| regs[*reg as usize]);
                    let answer = self.record(names, values);
                    frame.transient += self.transient.saturating_sub(before);
                    regs[*reg as usize] = answer.ok_or_else(|| self.at(*miss))?;
                }
                Op::Closure { code, reg, miss } => {
                    let closure = self.retaining(&mut frame.transient, |island| {
                        island.fresh_closure(frame.callee, &frame.slots, *code)
                    });
                    regs[*reg as usize] = Value::Closure(closure.ok_or_else(|| self.at(*miss))?);
                }
                Op::Store {
                    target,
                    index,
                    value,
                    field,
                    pc,
                    inputs,
                    finish,
                } => {
                    let values = match inputs {
                        Some(inputs) => self.inputs::<3>(&frame.slots, &inputs.reads)?,
                        None => [
                            regs[*target as usize],
                            index.map_or(Value::Absent, |reg| regs[reg as usize]),
                            regs[*value as usize],
                        ],
                    };
                    let ordered = inputs
                        .as_ref()
                        .map_or(values, |inputs| inputs.roles.map(|at| values[at]));
                    let stored = self.retaining(&mut frame.transient, |island| {
                        island.store(*field, ordered[0], ordered[1], ordered[2])
                    });
                    if stored.is_none() {
                        let stop = self.at(*pc);
                        if let Some(inputs) = inputs {
                            for (read, value) in inputs.reads.iter().zip(values) {
                                if !self
                                    .scratch
                                    .exit
                                    .regs
                                    .iter()
                                    .any(|(reg, _)| *reg == read.reg())
                                {
                                    self.scratch.exit.regs.push((read.reg(), value));
                                }
                            }
                        }
                        return Err(stop);
                    }
                    if let Some(leaves) = finish {
                        return self.finish_statement(frame, *leaves, None);
                    }
                }
                Op::Unit { site, src } => {
                    regs[site.reg as usize] = self.retaining(&mut frame.transient, |island| {
                        island.unit_test(site, regs[*src as usize])
                    })?
                }
                Op::Kind { site, src } => {
                    regs[site.same.reg as usize] = self
                        .retaining(&mut frame.transient, |island| {
                            island.kind_test(site, regs[*src as usize])
                        })?
                }
                Op::Access(site) => return self.access(frame, site),
                Op::AccessInit {
                    head,
                    original,
                    held,
                } => {
                    return self.access_init(frame, *head, original, held.as_deref());
                }
                Op::DirectKind { site, reads } => {
                    let [value] = self.inputs::<1>(&frame.slots, reads)?;
                    regs[site.same.reg as usize] = self
                        .retaining(&mut frame.transient, |island| island.kind_test(site, value))?;
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
                        .retaining(&mut frame.transient, |island| island.item(*site, x, at))
                        .ok_or_else(|| self.at_operands(*miss, *dst, &[x, at]))?;
                }
                Op::Call { site, read, args } => {
                    return self.call(frame, site, (*read, args), callers, slots);
                }
                Op::Bind { at, src } => {
                    let value = regs[*src as usize];
                    frame.slots[*at] = SlotState::Value(value);
                }
                Op::Unbind(at) => frame.slots[*at] = SlotState::Empty,
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
                Op::Write { site } => {
                    if let Some(begin) = &site.begin {
                        self.begin(frame, begin.id, begin.index, begin.ahead)?;
                    }
                    let value = if let Some(reads) = &site.reads {
                        self.inputs::<1>(&frame.slots, reads)?[0]
                    } else {
                        frame.regs[site.reg as usize]
                    };
                    return self.write_value(frame, site, value);
                }
                Op::Accumulate(at) => self.accumulate(frame, *at),
                Op::Accumulation {
                    site,
                    append,
                    reads,
                    covered,
                    write,
                } => {
                    return self.accumulation(
                        frame,
                        site,
                        append.as_deref(),
                        reads,
                        *covered,
                        write,
                    );
                }
                Op::Publish {
                    name,
                    reg,
                    pc,
                    bind,
                } => {
                    if !self.write_session(*name, regs[*reg as usize], *bind) {
                        return Err(self.at(*pc));
                    }
                }
                Op::Effect(pc) => return Err(self.at(*pc)),
                Op::Return { reg, pc } => {
                    // A public root return copies its value out of the run in
                    // its source statement, which raises a value that is no data
                    // at that statement's coordinate (FIG-7377).
                    if frame.recovery || (self.public_return && self.depth == 1) {
                        return Err(self.at(*pc));
                    }
                    let value = regs[*reg as usize];
                    return self.returning(frame, value);
                }
                Op::Done { leaves, empty } => return self.finish_statement(frame, *leaves, *empty),
                Op::Branch {
                    reg,
                    pc,
                    otherwise,
                    opens,
                } => {
                    let arm = match regs[*reg as usize] {
                        Value::Bool(true) => 0,
                        Value::Bool(false) => {
                            frame.pc = *otherwise;
                            1
                        }
                        _ => return Err(self.at(*pc)),
                    };
                    self.transient = self
                        .transient
                        .saturating_sub(std::mem::take(&mut frame.transient));
                    frame.statement = None;
                    if let Some(block) = opens[arm] {
                        frame.control.push(Position::Block(block, 0));
                        if self.spent() || self.arena.boundary_due() {
                            #[cfg(any(test, feature = "perf-witness"))]
                            if !self.spent() && self.fits(1) {
                                self.scratch.exit.reason = Some(Reason::SourceContinuation);
                            }
                            return Err(Stop);
                        }
                    }
                }
                Op::Spawn(pc) | Op::Yield(pc) | Op::Join(pc) | Op::Wait(pc) | Op::Cancel(pc) => {
                    // The instruction has paid its ordered operand/charge prefix.
                    // Project all windows before a scheduler handler can claim a
                    // task, expose an effect, call the host, or collect the arena.
                    self.scratch.exit.scheduling = true;
                    return Err(self.at(*pc));
                }
                Op::Cold(pc) => return Err(self.at(*pc)),
            }
            return Ok(Next::Continue);
        }
    }

    #[inline(always)]
    fn finish_statement(
        &mut self,
        frame: &mut Frame<'e>,
        leaves: u32,
        empty: Option<crate::compile::BlockId>,
    ) -> Result<Next, Stop> {
        if let Some(block) = empty
            && OBSERVING
            && let Some(watching) = &self.projection()
        {
            watching.empty(self.exe, block);
        }
        self.transient = self
            .transient
            .saturating_sub(std::mem::take(&mut frame.transient));
        let due = self.arena.boundary_due();
        frame.statement = None;
        for _ in 0..leaves {
            let Position::Block(block, _) = frame
                .control
                .pop()
                .ok_or_else(|| self.halt(fault("a step has no enclosing block to leave")))?
            else {
                return Err(self.halt(fault("a block edge leaves a loop")));
            };
            let block = &frame.callee.compiled.blocks[block];
            for at in &block.declares {
                frame.slots[*at as usize] = SlotState::Empty;
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
        if let Some(Position::Try {
            phase:
                TryPhase::Catch | TryPhase::Finally(Completion::Throw(_) | Completion::ReturnVia { .. }),
            ..
        }) = frame.control.last()
        {
            return Err(Stop);
        }
        if let Some(Position::Try { instruction, phase }) = frame.control.last_mut() {
            let Op::Try { finally, after, .. } = frame.callee.compiled.ops[*instruction].op else {
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
        Ok(Next::Continue)
    }

    fn pop_block(frame: &mut Frame<'_>) {
        if let Some(Position::Block(block, _)) = frame.control.pop() {
            frame.pc = frame.callee.compiled.blocks[block].after;
            for at in &frame.callee.compiled.blocks[block].declares {
                frame.slots[*at as usize] = SlotState::Empty;
            }
        }
    }

    #[inline(always)]
    fn returning(&mut self, frame: &mut Frame<'e>, value: Value) -> Result<Next, Stop> {
        // A catch-only scope has no work on a successful return. Keep
        // that return's source coordinate for a library formula refusal.
        if !frame.may_clean || !frame.callee.compiled.cleanups {
            return Ok(Next::Return(value));
        }
        self.departure(frame, Completion::Return(Box::new(value)))
    }

    fn departure(&mut self, frame: &mut Frame<'e>, departure: Completion) -> Result<Next, Stop> {
        // Dead physical operands are never roots or exit roles. Each next
        // read is produced by the same source expression before it is used.
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
                        departure @ Completion::Throw(_) => {
                            self.scratch.exit.stands = Some(Stands::Departure(departure));
                            Err(Stop)
                        }
                        _ => Err(self.halt(fault("a control departure has no owner"))),
                    };
                }
            }
        }
    }
}
