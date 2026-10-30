//! One active register frame. Calls move it onto an explicit stack and
//! returns recover it; Rust stack depth is independent of guest call depth.
//! Every legal step polls, including straight-line statements and frame entry.

#[cfg(any(test, feature = "perf-witness"))]
use crate::perf_witness::execution::{self, Owner, Reason};

use super::code::{Opcode, Packet, Recovery, sites};
use super::compiled::{Cleanup, Destination};
use super::*;
use crate::machine::loops::{Cursor, Guard, Header};
use crate::machine::{Completion, LibraryCall, TryPhase};

mod access;
mod accumulate;
mod boundary;
mod call;
mod dispatch;
mod merge;
mod numeric;
mod pending;
use pending::{Departing, Departure, Phase};
mod format;
mod repeat;
pub(super) mod source;
mod storage;
mod update;
#[cfg(test)]
pub(super) use storage::StackPlacement;
use storage::Values;
pub(super) use storage::{Operands, Slab};

#[derive(Clone, Copy)]
struct CallContinuation<'e> {
    site: &'e compiled::CallSite,
    args: &'e [u32],
    live: &'e [u32],
}

#[derive(Clone, Copy)]
struct ReturnOperand {
    statement: StmtId,
    pc: u32,
    reg: u32,
}

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
        cleanup: Cleanup,
        phase: Phase,
    },
}

impl Position {
    #[inline(always)]
    fn project(&self, compiled: &Compiled, pending: &Operands<'_>, target: &mut Control) {
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
            Position::Try { cleanup, phase } => {
                if let Control::Try { stmt, phase: saved } = target {
                    *stmt = cleanup.id;
                    *saved = phase.project(pending[cleanup.pending]);
                } else {
                    *target = Control::Try {
                        stmt: cleanup.id,
                        phase: phase.project(pending[cleanup.pending]),
                    };
                }
            }
        }
    }

    fn projected(&self, compiled: &Compiled, pending: &Operands<'_>) -> Control {
        let mut control = Control::Block {
            block: compiled.blocks[0].id,
            next: 0,
        };
        self.project(compiled, pending, &mut control);
        control
    }
}

/// Reused control buffers carry no value holdings between frames.
#[derive(Default)]
pub(super) struct Window {
    control: Vec<Position>,
    texts: Vec<(usize, ArenaHandle)>,
}

pub(super) struct Frame<'e> {
    source: Option<super::source::Activation>,
    callee: Callee<'e>,
    bindings: std::ops::Range<usize>,
    operands: std::ops::Range<usize>,
    pending: std::ops::Range<usize>,
    pending_call: Option<(Callee<'e>, Option<crate::compile::LibId>)>,
    pc: usize,
    /// Compiled block identity and next source statement, outermost first.
    control: Vec<Position>,
    /// Each accumulator slot whose long text its own statement made in its
    /// loop's running activation, which nothing else holds (FIG-6795).
    texts: Vec<(usize, ArenaHandle)>,
    statement: Option<StmtId>,
    continuation: Option<CallContinuation<'e>>,
    returned: Option<ReturnOperand>,
    may_clean: bool,
    admitted: bool,
    swift: bool,
    covered: bool,
    transient: u64,
    resident: bool,
    handoff: bool,
    handoff_live: &'e [u32],
    recovery: bool,
    boundary: Option<crate::compile::LibId>,
}

impl<'e> Frame<'e> {
    pub(super) fn returned_explicitly(&self) -> bool {
        self.statement.is_some()
    }

    fn new(callee: Callee<'e>, ranges: storage::Ranges, window: Window) -> Self {
        Self {
            callee,
            source: None,
            bindings: ranges.bindings,
            operands: ranges.operands,
            pending: ranges.pending,
            pending_call: None,
            pc: 0,
            control: window.control,
            texts: window.texts,
            statement: None,
            continuation: None,
            returned: None,
            may_clean: false,
            admitted: false,
            swift: false,
            covered: false,
            transient: 0,
            resident: false,
            handoff: false,
            handoff_live: &[],
            recovery: false,
            boundary: None,
        }
    }
}

/// A single operation either advances the active cursor, calls, returns,
/// or exits with the source map. No operation owns another control loop.
enum Next {
    Continue,
    Call,
    Return(Value),
}

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(super) fn enter(
        &mut self,
        callee: Callee<'e>,
        args: &[Value],
        inputs: &[lash_kernel_state::kernel_internals::logical::Owner],
    ) -> Answer {
        self.record_entry(callee, None);
        let mut active = self.new_frame(callee, args);
        let mut slab = std::mem::take(&mut self.scratch.slab);
        let entered = self.source_enter(&slab.view(&mut active), inputs);
        self.scratch.slab = slab;
        active.source = Some(entered.map_err(|error| self.halt(error.into()))?);
        self.execute(&mut active)
    }

    fn record_literal_body(&self, callee: Callee<'e>, started: u64) {
        let Some(literal) = &callee.compiled.literal else {
            return;
        };
        if OBSERVING && let Some(watching) = &self.projection() {
            watching.statement(
                self.exe,
                StatementFact {
                    code: callee.plan.code,
                    stmt: literal.statement,
                    block: callee.plan.body.id,
                    index: literal.index,
                    charging: self.exe.charging(),
                },
                (started + 1, self.arena.allocations()),
            );
            if let Some(close) = &literal.close {
                watching.expressions(callee.stream, close, self.exe.charging());
            }
        }
        if let Some(tape) = self.recorder.filter(|_| self.exe.charging()) {
            tape.push(Weight::Atomic(1));
            if let Some(close) = &literal.close {
                super::record(tape, callee.stream, close.region);
            }
        }
    }

    // One loop and instruction inventory serve both modes. The entry-fixed
    // flag erases passive projection taps when no observer borrowed this run.
    #[inline(always)]
    pub(super) fn execute(&mut self, active: &mut Frame<'e>) -> Answer {
        let mut slab = std::mem::take(&mut self.scratch.slab);
        #[cfg(any(test, feature = "perf-witness"))]
        let answer = if self.scratch.counting {
            self.execute_counted(active, &mut slab)
        } else {
            self.execute_program(active, &mut slab)
        };
        #[cfg(not(any(test, feature = "perf-witness")))]
        let answer = self.execute_program(active, &mut slab);
        if !active.handoff && !(active.resident && answer.is_ok()) {
            slab.clear();
        }
        self.scratch.slab = slab;
        answer
    }

    #[cfg(any(test, feature = "perf-witness"))]
    #[cold]
    #[inline(never)]
    fn execute_counted(&mut self, active: &mut Frame<'e>, slab: &mut Slab) -> Answer {
        let _execution =
            execution::Scope::enter(&self.scratch.census, Owner::RegisterProgram, true);
        self.execute_program(active, slab)
    }

    fn execute_program(&mut self, active: &mut Frame<'e>, slab: &mut Slab) -> Answer {
        let mut callers = Vec::new();
        // This loop owns both the physical frames and their reservation.
        // It never commits a call that cannot fit all live windows.
        let mut slots = active.bindings.len();
        self.depth = 1;
        self.activate(&mut slab.view(active));
        loop {
            #[cfg(test)]
            slab.record_placement(&callers, active, self.arena);
            let code = &active.callee.compiled.code;
            let packet = code.packets[active.pc];
            let recovery = &code.recovery[active.pc];
            active.pc += 1;
            #[cfg(any(test, feature = "testing"))]
            slab.dispatched(packet, code);
            if OBSERVING
                && let Some(watching) = &self.projection()
                && watching.sites()
                && matches!(
                    packet.opcode,
                    Opcode::Call | Opcode::Operator | Opcode::Heap
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
            // Ordinary placement and admission stay in the dispatch loop.
            // They have no operation result or continuation to route.
            let mut literal_return = None;
            if active.pc == 1
                && active.control.is_empty()
                && !active.resident
                && active.boundary.is_none()
                && !self.spent()
                && let Some(mut activation) = active.source.take()
            {
                let started = self.charged;
                let published = self
                    .publish_literal_body(active.callee, &mut activation)
                    .map_err(|error| self.halt(error.into()))?;
                active.source = Some(activation);
                if published {
                    let literal =
                        active.callee.compiled.literal.as_ref().ok_or_else(|| {
                            self.halt(fault("literal publication lost its source"))
                        })?;
                    active.statement = Some(literal.statement);
                    active.returned = Some(ReturnOperand {
                        statement: literal.statement,
                        pc: self.exe.stmt(literal.statement).ops.end() - 1,
                        reg: literal.reg,
                    });
                    self.record_literal_body(active.callee, started);
                    literal_return = Some(literal.value);
                }
            }
            let next = if let Some(value) = literal_return {
                Ok(Next::Return(value))
            } else {
                match packet.opcode {
                    Opcode::Begin => {
                        let begin = &code.sites.begin[packet.x as usize];
                        if let Err(stop) =
                            self.begin(&mut slab.view(active), begin.id, begin.index, begin.ahead)
                        {
                            return self.slow_exit(active, &mut callers, slab, recovery, stop);
                        }
                        continue;
                    }
                    Opcode::Constant => {
                        slab.view(active).regs[packet.dst as usize] = code.constant(packet);
                        continue;
                    }
                    Opcode::Local => {
                        let value = match slab.view(active).slots[packet.x as usize] {
                            SlotState::Cell(cell) if packet.mode != 0 => self.arena.cell(cell).ok(),
                            SlotState::Value(value) if packet.mode == 0 => Some(value),
                            _ => None,
                        };
                        let Some(value) = value else {
                            let stop = self.at(packet.y);
                            return self.slow_exit(active, &mut callers, slab, recovery, stop);
                        };
                        slab.view(active).regs[packet.dst as usize] = value;
                        continue;
                    }
                    Opcode::Session => {
                        let Some(value) = self.session(packet.x) else {
                            let stop = self.at(packet.y);
                            return self.slow_exit(active, &mut callers, slab, recovery, stop);
                        };
                        slab.view(active).regs[packet.dst as usize] = value;
                        continue;
                    }
                    Opcode::Bind => {
                        let values = slab.view(active);
                        values.slots[packet.dst as usize] =
                            SlotState::Value(values.regs[packet.x as usize]);
                        continue;
                    }
                    Opcode::Unbind => {
                        slab.view(active).slots[packet.dst as usize] = SlotState::Empty;
                        continue;
                    }
                    Opcode::Jump => {
                        active.pc = packet.x as usize;
                        continue;
                    }
                    _ => {}
                }
                self.packet(&mut slab.view(active), packet, recovery, slots)
            };
            match next {
                Ok(Next::Continue) => {}
                Ok(Next::Call) => {
                    let Some((target, boundary)) = active.pending_call.take() else {
                        return Err(self.halt(fault("a call has no admitted target")));
                    };
                    let Some(continuation) = active.continuation else {
                        return Err(self.halt(fault("a call has no semantic continuation")));
                    };
                    let inputs = self
                        .source_call_inputs(
                            &slab.view(active),
                            continuation.site,
                            continuation.args,
                        )
                        .map_err(|error| self.halt(error.into()))?;
                    let saved = self
                        .source_continuation(&slab.view(active), continuation.site)
                        .map_err(|error| self.halt(error.into()))?;
                    let ranges = slab.call(
                        active,
                        target,
                        continuation.args,
                        &self.scratch.captured[target.captured as usize..],
                    );
                    let window = self.scratch.windows.pop().unwrap_or_default();
                    let mut inner = Frame::new(target, ranges, window);
                    Self::cell_parameters(self.arena, target, slab.view(&mut inner).slots);
                    inner.source = Some(
                        self.source_enter(&slab.view(&mut inner), &inputs)
                            .map_err(|error| self.halt(error.into()))?,
                    );
                    self.source_called(&slab.view(active), &saved)
                        .map_err(|error| self.halt(error.into()))?;
                    inner.boundary = boundary;
                    self.register_bytes += target.compiled.register_bytes();
                    slots += inner.bindings.len();
                    callers.push(std::mem::replace(active, inner));
                    self.depth += 1;
                    self.activate(&mut slab.view(active));
                }
                Ok(Next::Return(value)) => {
                    if !active.callee.plan.charged || active.boundary.is_some() {
                        let completion = if let Some(caller) = callers.last_mut() {
                            let (caller, current) = slab.pair(caller, active);
                            self.complete_library(&current, Some(&caller), value)
                        } else {
                            self.complete_library(&slab.view(active), None, value)
                        };
                        if let Err(stop) = completion {
                            return self.slow_exit(active, &mut callers, slab, recovery, stop);
                        }
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
                    if active.resident && callers.is_empty() {
                        // Its source frame still owns the bindings through
                        // completion charging. Transfer them before recycling.
                        self.depth = 0;
                        return Ok(value);
                    }
                    match self
                        .publish_return_suffix(&mut slab.view(active), value)
                        .map_err(|error| self.halt(error.into()))?
                    {
                        source::ValuePublication::Declined => {
                            let Some(returned) = active.returned else {
                                return Err(
                                    self.halt(fault("a declined return lost its source operand"))
                                );
                            };
                            let stop = self.at_held(returned.pc, returned.reg, value);
                            return self.slow_exit(active, &mut callers, slab, recovery, stop);
                        }
                        source::ValuePublication::Published(_)
                        | source::ValuePublication::Unavailable => {}
                    }
                    if let Some(activation) = &active.source
                        && !activation
                            .frontier
                            .as_ref()
                            .is_some_and(|frontier| frontier.position().unpaid.is_none())
                    {
                        self.source_leave(activation, value)
                            .map_err(|error| self.halt(error.into()))?;
                    }
                    slots -= active.bindings.len();
                    self.recycle(&mut slab.view(active));
                    slab.release(active);
                    let Some(caller) = callers.pop() else {
                        self.depth = 0;
                        return Ok(value);
                    };
                    *active = caller;
                    slab.restore(active);
                    self.depth -= 1;
                    self.activate_mode(&slab.view(active));
                    (self.admitted, self.swift, self.covered) =
                        (active.admitted, active.swift, active.covered);
                    let Some(continuation) = active.continuation.take() else {
                        return Err(self.halt(fault("a caller has no call continuation")));
                    };
                    let site = continuation.site;
                    slab.view(active).regs[site.reg as usize] = value;
                    self.source_returned(&slab.view(active), site, value)
                        .map_err(|error| self.halt(error.into()))?;
                    if site.after != 0 {
                        self.admit(site.after);
                    }
                }
                Err(stop) => return self.slow_exit(active, &mut callers, slab, recovery, stop),
            }
        }
    }

    #[inline(always)]
    fn new_frame(&mut self, callee: Callee<'e>, args: &[Value]) -> Frame<'e> {
        #[cfg(test)]
        self.scratch.slab.entered_call();
        self.register_bytes += callee.compiled.register_bytes();
        let ranges = self.scratch.slab.open(
            callee.plan.slots,
            callee.compiled.registers,
            callee.compiled.pending_slots(),
        );
        let window = self.scratch.windows.pop().unwrap_or_default();
        let mut frame = Frame::new(callee, ranges, window);
        let values = self.scratch.slab.view(&mut frame);
        callee.compiled.layout.fill(
            values.slots,
            args.iter().copied(),
            &self.scratch.captured[callee.captured as usize..],
        );
        Self::cell_parameters(self.arena, callee, values.slots);
        frame
    }

    fn cell_parameters(
        arena: &mut lash_kernel_state::arena::Arena,
        callee: Callee<'_>,
        slots: &mut [SlotState],
    ) {
        if callee.compiled.cell_parameters != 0 {
            for at in &callee.plan.params {
                if callee.plan.shared[*at as usize] {
                    let value = match slots[*at as usize] {
                        SlotState::Value(value) => value,
                        _ => Value::Absent,
                    };
                    slots[*at as usize] = SlotState::Cell(arena.new_cell(value));
                }
            }
        }
    }

    #[inline(always)]
    fn recycle(&mut self, frame: &mut Values<'_, '_>) {
        self.end_statement(frame);
        self.register_bytes -= frame.state.callee.compiled.register_bytes();
        let mut control = std::mem::take(&mut frame.state.control);
        let mut texts = std::mem::take(&mut frame.state.texts);
        control.clear();
        texts.clear();
        self.scratch.windows.push(Window { control, texts });
    }

    fn end_statement(&mut self, frame: &mut Values<'_, '_>) {
        self.transient = self
            .transient
            .saturating_sub(std::mem::take(&mut frame.state.transient));
    }

    /// An accumulator statement, which cannot stop once its concatenation
    /// answered, wrote `value` to the slot at `at`: the text that
    /// concatenation made is the slot's to extend again. Storage it took
    /// belongs to that value; any other value would leave it with a text
    /// no longer whole.
    #[inline(always)]
    fn accumulated(
        &mut self,
        frame: &mut Values<'_, 'e>,
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
                frame.state.texts.push((at, text));
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

    fn activate(&mut self, frame: &mut Values<'_, 'e>) {
        self.activate_mode(frame);
        if let Some(sampling) = self.sampling {
            sampling.statement(frame.state.callee.plan.code, frame.state.statement);
        }
        self.admitted = frame.state.callee.compiled.admits(
            self.base + u64::from(self.depth),
            self.call_depth,
            self.observed,
        );
        self.swift = self.admitted && self.plain;
        self.covered = false;
    }

    #[inline(always)]
    fn activate_mode(&mut self, frame: &Values<'_, 'e>) {
        self.link = frame.state.callee.link;
        self.stream = frame.state.callee.stream;
        // Entry starts in the charged/pace policy of Ground. Same-mode
        // calls retain that policy; a helper/callback handoff changes it.
        if self.accounting != frame.state.callee.plan.charged {
            self.accounting = frame.state.callee.plan.charged;
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
        frame: &mut Values<'_, '_>,
        id: StmtId,
        index: usize,
        ahead: u64,
    ) -> Result<(), Stop> {
        self.end_statement(frame);
        frame.state.statement = None;
        if self.deferred_end.is_some() {
            self.progressed();
        }
        if self.admit(ahead) {
            self.charged += 1;
        } else if self.accounting {
            self.unit().inspect_err(|_| {
                self.scratch.exit.stands = Some(Stands::Statement);
            })?;
        }
        let Position::Block(_, next) = frame
            .state
            .control
            .last_mut()
            .ok_or_else(|| self.halt(fault("a statement has no enclosing block")))?
        else {
            return Err(self.halt(fault("a statement's enclosing control is not a block")));
        };
        *next = index + 1;
        frame.state.statement = Some(id);
        if OBSERVING && let Some(watching) = &self.projection() {
            let Some(Position::Block(block, _)) = frame.state.control.last() else {
                unreachable!("begin validated its block");
            };
            let block = frame.state.callee.compiled.blocks[*block].id;
            watching.statement(
                self.exe,
                StatementFact {
                    code: frame.state.callee.plan.code,
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
    fn finish_statement(
        &mut self,
        frame: &mut Values<'_, 'e>,
        leaves: u32,
        empty: Option<crate::compile::BlockId>,
    ) -> Result<Next, Stop> {
        self.source_clear_operands(frame)
            .and_then(|()| self.source_bindings(frame))
            .and_then(|()| self.logical.flush(self.arena, self.heap))
            .map_err(|error| self.halt(error.into()))?;
        if let Some(block) = empty
            && OBSERVING
            && let Some(watching) = &self.projection()
        {
            watching.empty(self.exe, block);
        }
        self.transient = self
            .transient
            .saturating_sub(std::mem::take(&mut frame.state.transient));
        let due = self.arena.boundary_due();
        frame.state.statement = None;
        for _ in 0..leaves {
            let Some(Position::Block(block, _)) = frame.state.control.last() else {
                return Err(self.halt(fault("a block edge leaves a loop")));
            };
            let block = *block;
            let published = self
                .publish_nested_leave(frame, block)
                .map_err(|error| self.halt(error.into()))?;
            frame.state.control.pop();
            let block = &frame.state.callee.compiled.blocks[block];
            if !published {
                for at in &block.declares {
                    frame.slots[*at as usize] = SlotState::Empty;
                }
            }
            frame.state.pc = block.after;
        }
        if frame.state.control.is_empty() {
            return Ok(Next::Return(Value::Null));
        }
        if let Some(Position::Loop { header, .. }) = frame.state.control.last() {
            frame.state.pc = *header;
            if due || self.spent() {
                return Err(Stop);
            }
            return Ok(Next::Continue);
        }
        if matches!(frame.state.control.last(), Some(Position::Try { phase, .. }) if phase.needs_source())
        {
            return Err(Stop);
        }
        if let Some(Position::Try { cleanup, phase }) = frame.state.control.last_mut() {
            let Cleanup {
                finally,
                after,
                pending,
                ..
            } = *cleanup;
            match std::mem::replace(phase, Phase::Body) {
                Phase::Finally(how) => {
                    let value = std::mem::replace(&mut frame.pending[pending], Value::Null);
                    frame.state.control.pop();
                    if !matches!(how, Departure::Normal) {
                        return self.departure(frame, Departing { how, value });
                    }
                    frame.state.pc = after;
                }
                Phase::Body | Phase::Catch => match finally {
                    Some(cleanup) => {
                        *phase = Phase::Finally(Departure::Normal);
                        frame.state.pc = cleanup;
                        return Ok(Next::Continue);
                    }
                    None => {
                        frame.state.control.pop();
                        frame.state.pc = after;
                    }
                },
            }
        }
        if due || self.spent() {
            return Err(Stop);
        }
        Ok(Next::Continue)
    }

    fn pop_block(frame: &mut Values<'_, '_>) {
        if let Some(Position::Block(block, _)) = frame.state.control.pop() {
            frame.state.pc = frame.state.callee.compiled.blocks[block].after;
            for at in &frame.state.callee.compiled.blocks[block].declares {
                frame.slots[*at as usize] = SlotState::Empty;
            }
        }
    }

    #[inline(always)]
    fn returning(&mut self, frame: &mut Values<'_, 'e>, value: Value) -> Result<Next, Stop> {
        // A catch-only scope has no work on a successful return. Keep
        // that return's source coordinate for a library formula refusal.
        if !frame.state.may_clean || !frame.state.callee.compiled.cleanups {
            return Ok(Next::Return(value));
        }
        self.departure(
            frame,
            Departing {
                how: Departure::Return,
                value,
            },
        )
    }

    fn departure(
        &mut self,
        frame: &mut Values<'_, 'e>,
        departure: Departing,
    ) -> Result<Next, Stop> {
        // Dead physical operands are never roots or exit roles. Each next
        // read is produced by the same source expression before it is used.
        frame.state.statement = None;
        loop {
            match frame.state.control.last_mut() {
                Some(Position::Block(..)) => Self::pop_block(frame),
                Some(Position::Loop { header, .. }) => {
                    let header = *header;
                    match departure.how {
                        Departure::Continue => {
                            frame.state.pc = header;
                            return Ok(Next::Continue);
                        }
                        Departure::Break => {
                            frame.state.pc = frame.state.callee.compiled.code.loop_exit(header);
                            frame.state.control.pop();
                            if self.spent() {
                                return Err(Stop);
                            }
                            return Ok(Next::Continue);
                        }
                        _ => {
                            frame.state.control.pop();
                        }
                    }
                }
                Some(Position::Try { cleanup, phase }) => {
                    let finally = cleanup.finally;
                    if !matches!(phase, Phase::Finally(_))
                        && let Some(target) = finally
                    {
                        frame.pending[cleanup.pending] = departure.value;
                        *phase = Phase::Finally(departure.how);
                        frame.state.pc = target;
                        return Ok(Next::Continue);
                    }
                    // Supersession ends this pending owner exactly once.
                    frame.pending[cleanup.pending] = Value::Null;
                    frame.state.control.pop();
                }
                None => {
                    return match departure.how {
                        Departure::Return => Ok(Next::Return(departure.value)),
                        Departure::Throw => {
                            self.scratch.exit.stands = Some(Stands::Departure(departure.project()));
                            Err(Stop)
                        }
                        _ => Err(self.halt(fault("a control departure has no owner"))),
                    };
                }
            }
        }
    }
}
