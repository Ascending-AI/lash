//! One active register frame. Calls move it onto an explicit stack and
//! returns recover it; Rust stack depth is independent of guest call depth.
//! Every legal step polls, including straight-line statements and frame entry.

#[cfg(any(test, feature = "perf-witness"))]
use crate::perf_witness::execution::{self, Owner, Reason};

use super::compiled::{Instruction, Op};
use super::*;

/// Reusable physical windows own no logical value between entries.
#[derive(Default)]
pub(super) struct Window {
    slots: Vec<Option<Value>>,
    regs: Vec<Value>,
    control: Vec<(usize, usize)>,
}

pub(super) struct Frame<'e> {
    callee: Callee<'e>,
    slots: Vec<Option<Value>>,
    regs: Vec<Value>,
    pc: usize,
    /// Compiled block identity and next source statement, outermost first.
    control: Vec<(usize, usize)>,
    statement: Option<StmtId>,
    waiting: Option<usize>,
    admitted: bool,
    swift: bool,
    covered: bool,
    transient: u64,
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
        args: &[Value],
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
            admitted: false,
            swift: false,
            covered: false,
            transient: 0,
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

impl<'e> Island<'e> {
    pub(super) fn enter(&mut self, callee: Callee<'e>, args: &[Value]) -> Answer {
        if let Some(value) = self.returned_literal(callee) {
            return Ok(value);
        }
        let mut active = self.new_frame(callee, args);
        self.record_entry();
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
        let mut control = Vec::new();
        for item in &state.control {
            let Control::Block { block, next } = item else {
                return None;
            };
            let index = callee
                .compiled
                .blocks
                .iter()
                .position(|entry| entry.id == *block)?;
            control.push((index, *next));
        }
        let (block, next) = *control.last()?;
        let block = &callee.compiled.blocks[block];
        let pc = match self.exe.block(block.id).stmts.get(next) {
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
        frame.control = control;
        frame.pc = pc;
        Some(frame)
    }

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
                    let mut values = [Value::Absent; MAX_ARGS];
                    for (to, reg) in values.iter_mut().zip(args.iter()) {
                        *to = active.regs[*reg as usize];
                    }
                    let given = args.len();
                    active.waiting = Some(active.pc - 1);
                    (active.admitted, active.swift, active.covered) =
                        (self.admitted, self.swift, self.covered);
                    let inner = self.new_frame(callee, &values[..given]);
                    slots += inner.slots.len();
                    self.record_entry();
                    callers.push(std::mem::replace(active, inner));
                    self.depth += 1;
                    self.activate(active);
                }
                Ok(Next::Return(value)) => {
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
                                Op::Cond { .. } | Op::Branch { .. } => Reason::Condition,
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
    fn record_entry(&self) {
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
            .extend(
                frame
                    .control
                    .iter()
                    .rev()
                    .map(|(block, next)| Control::Block {
                        block: frame.callee.compiled.blocks[*block].id,
                        next: *next,
                    }),
            );
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
        frame.statement = None;
        if self.admit(ahead) {
            self.charged += 1;
        } else if !self.unit() {
            return Err(Stop);
        }
        frame
            .control
            .last_mut()
            .ok_or_else(|| self.halt(fault("a statement has no enclosing block")))?
            .1 = index + 1;
        frame.statement = Some(id);
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
            Op::Open(block) => {
                frame.control.push((*block, 0));
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
                    frame.control.push((*block, 0));
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
                return Ok(Next::Return(*value));
            }
            Op::Constant { dst, value } => regs[*dst as usize] = *value,
            Op::Local { dst, at, miss } => {
                regs[*dst as usize] = frame.slots[*at].ok_or_else(|| self.at(*miss))?;
            }
            Op::Session { dst, name, miss } => {
                regs[*dst as usize] = self.session(*name).ok_or_else(|| self.at(*miss))?;
            }
            Op::Operator {
                site,
                args,
                covered,
            } => {
                let mut values = [Value::Absent; MAX_ARGS];
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
                let answer = match typed {
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
                let mut values = [Value::Absent; MAX_ARGS];
                for (to, reg) in values.iter_mut().zip(args.iter()) {
                    *to = regs[*reg as usize];
                }
                let given = args.len();
                let args = &values[..given];
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
            Op::Write { at, reg, pc, bind } => {
                let value = regs[*reg as usize];
                if !*bind && frame.slots[*at].is_none() {
                    return Err(self.at(*pc));
                }
                frame.slots[*at] = Some(value);
            }
            Op::Return(reg) => return Ok(Next::Return(regs[*reg as usize])),
            Op::Done(leaves) => {
                self.transient = self
                    .transient
                    .saturating_sub(std::mem::take(&mut frame.transient));
                let due = self.arena.boundary_due();
                regs.fill(Value::Null);
                frame.statement = None;
                for _ in 0..*leaves {
                    let (block, _) = frame.control.pop().ok_or_else(|| {
                        self.halt(fault("a step has no enclosing block to leave"))
                    })?;
                    let block = &frame.callee.compiled.blocks[block];
                    for at in &block.declares {
                        frame.slots[*at as usize] = None;
                    }
                    frame.pc = block.after;
                }
                if frame.control.is_empty() {
                    return Ok(Next::Return(Value::Null));
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
}
