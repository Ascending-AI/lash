//! A block head's trace (`bytecode::trace`), run on the prepaid path.
//!
//! Each micro-op does what the dispatcher does at its op on a prepaid,
//! unobserved block, where the exact step is skipped. A micro-op that
//! cannot gives the dispatcher its op before any effect, and the
//! dispatcher runs it under every rule. Only a head's micro-op enters a
//! block; a call ends its block, so the op after an inline call is a
//! head, and every other micro-op runs on the reservation its trace's
//! last head made.
//!
//! Each kind of micro-op is its own handler function, called from a
//! table by the step's kind, so that each is register-allocated alone:
//! the runner keeps only the machine, the line and the cursor live
//! between them. A handler gives the next micro-op's index, the op the
//! dispatcher runs next ([`EXIT`]), or [`LEAVE`] with the trace's result
//! in [`Line::out`]. A path a handler takes
//! rarely (a miss, an error, a cold read) is its own function, called
//! last with the handler's own arguments.

use lash_kernel_doc::Value;

use super::{Flow, Run, Vm, fused};
use crate::bytecode::program::{Fused, Program, Then, Unit};
use crate::bytecode::trace::{
    BRANCH, GENERIC_BUNDLE, KINDS, Kind, NO_PASS, NONE, OPERATIONS, PLAIN_KINDS, POP, PUSH,
    SET_LOCAL, SMALL_BUNDLE, SMALL_PREFIXES, Step, WORD_PREFIXES, word_kind,
};
use crate::heap::Heap;

#[cfg(test)]
thread_local! {
    /// The micro-ops run on this thread, for the laws: proof that a law's
    /// run took a trace.
    static RAN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The micro-ops run on this thread so far.
#[cfg(test)]
pub(crate) fn ran() -> u64 {
    RAN.with(std::cell::Cell::get)
}

/// What a trace's handlers share besides the machine: the running
/// frame's program, unit and index (`top`), and where the trace's result
/// goes when it leaves.
pub(super) struct Line<'a> {
    program: &'a Program,
    unit: &'a Unit,
    top: usize,
    out: Run<Option<usize>>,
}

/// A handler's answer where the trace leaves with its result in
/// [`Line::out`]: an error, or a frame entered.
const LEAVE: usize = usize::MAX;

/// Set in a handler's answer where the trace leaves at the dispatcher's
/// op that the rest of the answer names.
const EXIT: usize = 1 << 63;

/// Leaves the trace with `out`. Out of line, so that a handler that
/// leaves only at an op keeps its registers.
#[cold]
#[inline(never)]
fn leave(line: &mut Line<'_>, out: Run<Option<usize>>) -> usize {
    line.out = out;
    LEAVE
}

/// Leaves the trace at the dispatcher's op `pc`.
#[inline(always)]
fn exit_at(pc: usize) -> usize {
    EXIT | pc
}

/// Leaves the trace with an internal fault.
#[cold]
#[inline(never)]
fn fault_out(line: &mut Line<'_>, message: &'static str) -> usize {
    leave(line, Err(super::fault(message)))
}

/// A micro-op's handler: the machine, the line, the step, its index and
/// the frame's base, giving the next step's index, [`EXIT`] with the op
/// the dispatcher runs next, or [`LEAVE`].
type Handler<H> = fn(&mut Vm<H>, &mut Line<'_>, Step, usize, usize) -> usize;

impl<H: Heap> Vm<H> {
    /// Each kind's handler, by its index; an index no kind has runs
    /// [`Self::step_none`].
    const HANDLERS: [Handler<H>; KINDS] = {
        let mut table: [Handler<H>; KINDS] = [Self::step_none; KINDS];
        table[Kind::Block as usize] = Self::step_block;
        table[Kind::Trip as usize] = Self::step_trip;
        table[Kind::Then as usize] = Self::step_then;
        table[Kind::Pass as usize] = Self::step_pass;
        table[Kind::Local as usize] = Self::step_local;
        table[Kind::Const as usize] = Self::step_konst;
        table[Kind::SetLocal as usize] = Self::step_set_local;
        table[Kind::Copy as usize] = Self::step_copy;
        table[Kind::Pop as usize] = Self::step_pop;
        table[Kind::CaptureGet as usize] = Self::step_capture_get;
        table[Kind::CellGet as usize] = Self::step_cell_get;
        table[Kind::Fused as usize] = Self::step_fused;
        table[Kind::Inline as usize] = Self::step_inline;
        table[Kind::Branch as usize] = Self::step_branch;
        table[Kind::Jump as usize] = Self::step_jump;
        table[Kind::Exit as usize] = Self::step_exit;
        macro_rules! word_prefixes {
            ($op:literal $pushes:literal; $($prefix:literal)*) => {$(
                word_prefixes!(@ $op $pushes $prefix PUSH);
                word_prefixes!(@ $op $pushes $prefix SET_LOCAL);
                word_prefixes!(@ $op $pushes $prefix POP);
                word_prefixes!(@ $op $pushes $prefix BRANCH);
            )*};
            (@ $op:literal $pushes:literal $prefix:literal $then:ident) => {
                table[PLAIN_KINDS * ($prefix + 1) + word_kind($op, $pushes, $then)] =
                    Self::step_bundle::<{word_kind($op, $pushes, $then)}, {WORD_PREFIXES[$prefix]}>;
            };
        }
        macro_rules! words {
            ($($op:literal)*) => {$(
                words!(@ $op 0); words!(@ $op 1); words!(@ $op 2);
            )*};
            (@ $op:literal $pushes:literal) => {
                word_prefixes!($op $pushes; 0 1 2 3 4 5 6 7 8 9 10 11 12);
                table[GENERIC_BUNDLE + word_kind($op, $pushes, PUSH)] = Self::step_bundle::<{word_kind($op, $pushes, PUSH)}, {u64::MAX}>;
                table[GENERIC_BUNDLE + word_kind($op, $pushes, SET_LOCAL)] = Self::step_bundle::<{word_kind($op, $pushes, SET_LOCAL)}, {u64::MAX}>;
                table[GENERIC_BUNDLE + word_kind($op, $pushes, POP)] = Self::step_bundle::<{word_kind($op, $pushes, POP)}, {u64::MAX}>;
                table[GENERIC_BUNDLE + word_kind($op, $pushes, BRANCH)] = Self::step_bundle::<{word_kind($op, $pushes, BRANCH)}, {u64::MAX}>;
                table[word_kind($op, $pushes, PUSH)] = Self::step_words::<$op, $pushes, PUSH>;
                table[word_kind($op, $pushes, SET_LOCAL)] =
                    Self::step_words::<$op, $pushes, SET_LOCAL>;
                table[word_kind($op, $pushes, POP)] = Self::step_words::<$op, $pushes, POP>;
                table[word_kind($op, $pushes, BRANCH)] = Self::step_words::<$op, $pushes, BRANCH>;
            };
        }
        words!(0 1 2 3 4 5 6 7 8 9 10 11);
        macro_rules! bundle { ($($kind:ident)*) => {$(
            table[GENERIC_BUNDLE + Kind::$kind as usize] = Self::step_bundle::<{Kind::$kind as usize}, {u64::MAX}>;
        )*}; }
        bundle!(Block Trip Then Pass Local Const SetLocal Copy Pop CaptureGet CellGet Fused Inline Branch Jump Exit);
        macro_rules! small_prefixes {
            ($($prefix:literal)*) => {$( small_prefixes!(@ $prefix; 11 12 13 14 15); )*};
            (@ $prefix:literal; $($kind:literal)*) => {$(
                table[SMALL_BUNDLE + $prefix * 16 + $kind] = Self::step_bundle::<$kind, {SMALL_PREFIXES[$prefix]}>;
            )*};
        }
        small_prefixes!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57);
        table
    };

    /// Runs trace `n` of `unit`, the running frame's (`top`), whose locals
    /// start at `base`. Gives the op the dispatcher runs next, or `None`
    /// where a frame was entered. An error leaves the frame where the dispatcher's
    /// would.
    #[inline(never)]
    pub(super) fn trace(
        &mut self,
        program: &Program,
        unit: &Unit,
        top: usize,
        n: u32,
        base: usize,
    ) -> Run<Option<usize>> {
        let handlers = &Self::HANDLERS;
        let micros = &unit.micros[..];
        let mut line = Line {
            program,
            unit,
            top,
            out: Ok(None),
        };
        let mut at = unit.traces[n as usize].start as usize;
        loop {
            let step = micros[at];
            debug_assert!(
                [Kind::Block, Kind::Trip, Kind::Exit]
                    .iter()
                    .any(|&kind| step.kind() == kind as usize)
                    || step.kind() >= PLAIN_KINDS
                    || !self.exact,
                "a trace runs on the prepaid path"
            );
            at = handlers[step.kind()](self, &mut line, step, at, base);
            if at >= EXIT {
                return if at == LEAVE {
                    line.out
                } else {
                    Ok(Some(at & !EXIT))
                };
            }
        }
    }

    /// Runs the small leading ops, the primary handler, and its jump
    /// without returning to the runner between them.
    fn step_bundle<const KIND: usize, const CODE: u64>(
        &mut self,
        line: &mut Line<'_>,
        group: Step,
        at: usize,
        base: usize,
    ) -> usize {
        let steps = &line.unit.trace_steps;
        if CODE == u64::MAX {
            let next = self.bundle_prefix(line, group, at, base);
            if next != at + 1 {
                return next;
            }
        } else {
            macro_rules! leading {
                ($($n:literal)*) => {$(
                    if (CODE >> ($n * 4)) & 15 != 0 {
                        let prefix = steps[group.a() as usize + $n];
                        debug_assert_eq!(prefix.kind(), (((CODE >> ($n * 4)) & 15) - 1) as usize);
                        let next = Self::HANDLERS[(((CODE >> ($n * 4)) & 15) - 1) as usize](
                            self, line, prefix, at, base);
                        if next != at + 1 { return next; }
                    }
                )*};
            }
            leading!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15);
        }
        let next = Self::HANDLERS[KIND](self, line, steps[group.b() as usize], at, base);
        if next != at + 1 {
            return next;
        }
        if group.small() != 0 {
            return self.step_jump(line, steps[group.b() as usize + 1], at, base);
        }
        at + 1
    }

    /// Rare leading sequences keep their decoding outside the primary
    /// handler's register allocation.
    #[inline(never)]
    fn bundle_prefix(&mut self, line: &mut Line<'_>, group: Step, at: usize, base: usize) -> usize {
        for prefix in group.a()..group.b() {
            let next = self.small_step(line, line.unit.trace_steps[prefix as usize], at, base);
            if next != at + 1 {
                return next;
            }
        }
        at + 1
    }

    #[inline(always)]
    fn small_step(&mut self, line: &mut Line<'_>, step: Step, at: usize, base: usize) -> usize {
        match step.kind() {
            k if k == Kind::Block as usize => self.step_block(line, step, at, base),
            k if k == Kind::Trip as usize => self.step_trip(line, step, at, base),
            k if k == Kind::Then as usize => self.step_then(line, step, at, base),
            k if k == Kind::Pass as usize => self.step_pass(line, step, at, base),
            k if k == Kind::Local as usize => self.step_local(line, step, at, base),
            k if k == Kind::Const as usize => self.step_konst(line, step, at, base),
            k if k == Kind::SetLocal as usize => self.step_set_local(line, step, at, base),
            k if k == Kind::Copy as usize => self.step_copy(line, step, at, base),
            k if k == Kind::Pop as usize => self.step_pop(line, step, at, base),
            k if k == Kind::CaptureGet as usize => self.step_capture_get(line, step, at, base),
            k if k == Kind::CellGet as usize => self.step_cell_get(line, step, at, base),
            k if k == Kind::Jump as usize => self.step_jump(line, step, at, base),
            _ => fault_out(line, "a bundled prefix of no small kind"),
        }
    }

    /// A block head: the block before ended, what it reserved is spent,
    /// and this one is entered.
    #[inline(always)]
    fn step_block(&mut self, line: &mut Line<'_>, step: Step, at: usize, _: usize) -> usize {
        #[cfg(test)]
        self.witness(line.unit, step);
        self.meter.leave();
        let block = &line.unit.blocks[step.a() as usize];
        self.exact = self.watch.is_some() || !self.meter.enter(block.charge);
        if self.exact {
            return exit_at(step.pc() as usize + 1);
        }
        at + 1
    }

    /// A loop's head, entered with the block its condition falls into.
    #[inline(always)]
    fn step_trip(&mut self, line: &mut Line<'_>, step: Step, at: usize, _: usize) -> usize {
        #[cfg(test)]
        self.witness(line.unit, step);
        self.meter.leave();
        let blocks = &line.unit.blocks;
        let charge = blocks[step.a() as usize].charge;
        let ahead = blocks[step.b() as usize].charge;
        self.exact = self.watch.is_some() || !self.meter.enter_two(charge, ahead);
        if self.exact {
            return exit_at(step.pc() as usize + 1);
        }
        at + 1
    }

    /// The head of the block a trip prepaid ahead, entered with the trip
    /// where the pair fit, else here as a head is; then the loop's pass.
    #[inline(always)]
    fn step_then(&mut self, line: &mut Line<'_>, step: Step, at: usize, base: usize) -> usize {
        if !self.meter.then() {
            self.meter.leave();
            let block = &line.unit.blocks[step.a() as usize];
            self.exact = self.watch.is_some() || !self.meter.enter(block.charge);
            if self.exact {
                #[cfg(test)]
                self.witness(line.unit, step.with(Kind::Block, NO_PASS));
                return exit_at(step.pc() as usize + 1);
            }
        }
        #[cfg(test)]
        self.witness(line.unit, step);
        if step.small() != NO_PASS {
            self.count_pass(base + usize::from(step.small()));
        }
        at + 1
    }

    /// A loop's pass counts one more trip.
    #[inline(always)]
    fn count_pass(&mut self, at: usize) {
        let local = &mut self.cx.stack[at];
        if let Value::Int(count) = *local {
            *local = Value::Int(count.saturating_add(1));
        }
    }

    #[inline(always)]
    fn step_pass(&mut self, _line: &mut Line<'_>, step: Step, at: usize, base: usize) -> usize {
        #[cfg(test)]
        self.witness(_line.unit, step);
        self.count_pass(base + usize::from(step.small()));
        at + 1
    }

    #[inline(always)]
    fn step_local(&mut self, _line: &mut Line<'_>, step: Step, at: usize, base: usize) -> usize {
        #[cfg(test)]
        self.witness(_line.unit, step);
        self.push(self.cx.stack[base + usize::from(step.small())]);
        at + 1
    }

    #[inline(always)]
    fn step_konst(&mut self, _line: &mut Line<'_>, step: Step, at: usize, _: usize) -> usize {
        #[cfg(test)]
        self.witness(_line.unit, step);
        self.push(self.konst(step.a()));
        at + 1
    }

    #[inline(always)]
    fn step_set_local(
        &mut self,
        _line: &mut Line<'_>,
        step: Step,
        at: usize,
        base: usize,
    ) -> usize {
        #[cfg(test)]
        self.witness(_line.unit, step);
        let value = self.pop();
        self.cx.stack[base + usize::from(step.small())] = value;
        at + 1
    }

    /// `Local(from)` then `SetLocal(to)`.
    #[inline(always)]
    fn step_copy(&mut self, _line: &mut Line<'_>, step: Step, at: usize, base: usize) -> usize {
        #[cfg(test)]
        self.witness(_line.unit, step);
        self.cx.stack[base + step.a() as usize] = self.cx.stack[base + usize::from(step.small())];
        at + 1
    }

    #[inline(always)]
    fn step_pop(&mut self, _line: &mut Line<'_>, _step: Step, at: usize, _: usize) -> usize {
        #[cfg(test)]
        self.witness(_line.unit, _step);
        self.pop();
        at + 1
    }

    /// A capture's value, where the arena holds it; else the dispatcher
    /// runs the op.
    #[inline(always)]
    fn step_capture_get(&mut self, _line: &mut Line<'_>, step: Step, at: usize, _: usize) -> usize {
        let Some(value) = self
            .capture(step.small())
            .ok()
            .and_then(|cell| self.heap.cell_get(cell).ok())
        else {
            return exit_at(step.pc() as usize);
        };
        #[cfg(test)]
        self.witness(_line.unit, step);
        self.push(value);
        at + 1
    }

    /// A cell's value, where the arena holds it; else the dispatcher runs
    /// the op.
    #[inline(always)]
    fn step_cell_get(&mut self, _line: &mut Line<'_>, step: Step, at: usize, _: usize) -> usize {
        let Some(value) = self
            .cell_at(step.small())
            .ok()
            .and_then(|cell| self.heap.cell_get(cell).ok())
        else {
            return exit_at(step.pc() as usize);
        };
        #[cfg(test)]
        self.witness(_line.unit, step);
        self.push(value);
        at + 1
    }

    /// Fused run `step.a()` over two word ints, answered in place; any other
    /// pair goes on in [`Self::fused_miss`].
    #[inline(always)]
    fn step_fused(&mut self, line: &mut Line<'_>, step: Step, at: usize, base: usize) -> usize {
        let fused = &line.unit.fused[step.a() as usize];
        debug_assert!(fused.entry.is_none(), "a traced run counts no entry");
        let len = self.cx.stack.len();
        let args = self.fused_args(fused, base, len);
        match self.fused_words(fused, args) {
            Some(value) => self.fused_take(line, fused, step, value, len, at, base),
            None => self.fused_miss(line, step, at, base),
        }
    }

    /// A step of no kind: the builder makes none.
    fn step_none(&mut self, line: &mut Line<'_>, _: Step, _: usize, _: usize) -> usize {
        fault_out(line, "a trace step of no kind")
    }

    /// Fused run `step.a()` of operation `OP` (by [`OPERATIONS`]), which
    /// pushes `PUSHES` operands and whose answer `THEN` takes: answered in
    /// place over two word ints by its words form, or over two numbers by
    /// its numbers form, each fee prepaid; any other pair goes on in
    /// [`Self::fused_miss`].
    #[inline(always)]
    fn step_words<const OP: u8, const PUSHES: u8, const THEN: u8>(
        &mut self,
        line: &mut Line<'_>,
        step: Step,
        at: usize,
        base: usize,
    ) -> usize {
        let operation = OPERATIONS[OP as usize];
        let fused = &line.unit.fused[step.a() as usize];
        debug_assert!(fused.entry.is_none(), "a traced run counts no entry");
        debug_assert_eq!(fused.pushes, PUSHES);
        let len = self.cx.stack.len();
        let args = self.fused_args(fused, base, len);
        if let (Some(form), [Value::Int(a), Value::Int(b)]) = (&fused.words, args)
            && let Some(value) = lash_kernel_doc::scalar::ints(operation, a, b)
        {
            let paid = match &form.fee {
                Some(table) if !self.off => {
                    let large = matches!(value, Value::Int(word) if word != 0);
                    self.meter.prepay(
                        table[usize::from(a != 0)
                            | usize::from(b != 0) << 1
                            | usize::from(large) << 2],
                    )
                }
                _ => true,
            };
            if paid {
                return self.word_take::<PUSHES, THEN>(line, fused, step, value, len, at, base);
            }
        } else if let Some(form) = &fused.numbers
            && let Some((value, fee)) = form.numbers_of(operation, args)
            && (self.off || fee.is_none_or(|units| self.meter.prepay(units)))
        {
            return self.word_take::<PUSHES, THEN>(line, fused, step, value, len, at, base);
        }
        self.fused_miss(line, step, at, base)
    }

    /// The op after fused run `fused`, which pushed `PUSHES` operands,
    /// takes its answer `value` as `THEN`; `len` is the stack's height
    /// before the run.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn word_take<const PUSHES: u8, const THEN: u8>(
        &mut self,
        line: &mut Line<'_>,
        fused: &Fused,
        step: Step,
        value: Value,
        len: usize,
        at: usize,
        base: usize,
    ) -> usize {
        #[cfg(test)]
        self.witness(line.unit, step);
        if PUSHES < 2 {
            self.cx.stack.truncate(len + usize::from(PUSHES) - 2);
        }
        match THEN {
            PUSH => self.push(value),
            SET_LOCAL => self.cx.stack[base + usize::from(step.small())] = value,
            POP => {}
            _ => match value {
                Value::Bool(true) => {}
                Value::Bool(false) => {
                    self.meter.unfold();
                    if step.b() == NONE {
                        let Then::Branch { to, .. } = fused.then else {
                            return fault_out(line, "a fused branch of no target");
                        };
                        return exit_at(to as usize);
                    }
                    return step.b() as usize;
                }
                _ => {
                    let Then::Branch { looping, .. } = fused.then else {
                        return fault_out(line, "a fused branch of no target");
                    };
                    return self.fused_raise(line, fused, looping);
                }
            },
        }
        at + 1
    }

    /// Fused run `step.a()`'s other inline answer; or the native's ordinary
    /// answer, or its call, from the run's call on, as the dispatcher runs
    /// it. A block prepaid ahead is given back first: the dispatcher
    /// enters it as its head.
    #[inline(never)]
    fn fused_miss(&mut self, line: &mut Line<'_>, step: Step, at: usize, base: usize) -> usize {
        let (program, unit, top) = (line.program, line.unit, line.top);
        let n = step.a();
        let fused = &unit.fused[n as usize];
        let len = self.cx.stack.len();
        let args = self.fused_args(fused, base, len);
        match self.fused_other(program, unit, fused, args) {
            Ok(Some(value)) => self.fused_take(line, fused, step, value, len, at, base),
            Ok(None) => {
                #[cfg(test)]
                self.witness(unit, step);
                self.meter.unfold();
                let out = self.fused_slow(program, unit, n, base, args, len, top);
                leave(line, out)
            }
            Err(flow) => {
                #[cfg(test)]
                self.witness(unit, step);
                self.meter.unfold();
                self.cx.frames[top].pc = match flow {
                    Flow::Cold { .. } => fused::rerun_from(fused),
                    _ => fused.call + 1,
                };
                leave(line, Err(flow))
            }
        }
    }

    /// The op after fused run `fused` takes its answer `value`; `len` is
    /// the stack's height before the run.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn fused_take(
        &mut self,
        line: &mut Line<'_>,
        fused: &Fused,
        step: Step,
        value: Value,
        len: usize,
        at: usize,
        base: usize,
    ) -> usize {
        #[cfg(test)]
        self.witness(line.unit, step);
        self.cx.stack.truncate(len + usize::from(fused.pushes) - 2);
        match fused.then {
            Then::Push => self.push(value),
            Then::SetLocal(k) => self.cx.stack[base + usize::from(k)] = value,
            Then::Pop => {}
            Then::Branch { to, looping } => match value {
                Value::Bool(true) => {}
                Value::Bool(false) => {
                    self.meter.unfold();
                    if step.b() == NONE {
                        return exit_at(to as usize);
                    }
                    return step.b() as usize;
                }
                // The answer and its fee are the run's: the branch
                // raises after them, as its op does.
                _ => return self.fused_raise(line, fused, looping),
            },
        }
        at + 1
    }

    /// A fused branch's answer is no bool.
    #[cold]
    #[inline(never)]
    fn fused_raise(&mut self, line: &mut Line<'_>, fused: &Fused, looping: bool) -> usize {
        self.meter.unfold();
        self.cx.frames[line.top].pc = fused.call + 2;
        let error = self.condition_error(looping);
        leave(line, Err(error))
    }

    /// Inline call `step.a()` of the unit at `step.pc()`, in the running
    /// frame, where a fold holds and is billed whole. Any other call the
    /// dispatcher runs whole, from its checks on, so a region or a fold's
    /// general path is entered in its loop alone.
    #[inline(always)]
    fn step_inline(&mut self, line: &mut Line<'_>, step: Step, at: usize, _: usize) -> usize {
        let unit = line.unit;
        let site = &unit.inlines[step.a() as usize];
        let call = &unit.calls[site.call as usize];
        debug_assert!(call.entry.is_none(), "a traced call counts no entry");
        if site.steps && self.meter.spent() {
            return exit_at(step.pc() as usize);
        }
        self.cx.frames[line.top].pc = step.pc() + 1;
        if let Some(checked) = &call.checked
            && let Err(flow) = self.check_arguments(checked, usize::from(call.argc))
        {
            return leave(line, Err(flow));
        }
        let (start, end) = site.folds;
        match self.inline_fast(unit, step.a(), &unit.folds[start as usize..end as usize]) {
            Ok(true) => {
                #[cfg(test)]
                self.witness(unit, step);
                at + 1
            }
            Ok(false) => exit_at(step.pc() as usize),
            Err(flow) => leave(line, Err(flow)),
        }
    }

    /// A `JumpIfFalse` or, when `step.small()`, a `WhileFalse`.
    #[inline(always)]
    fn step_branch(&mut self, line: &mut Line<'_>, step: Step, at: usize, _: usize) -> usize {
        #[cfg(test)]
        self.witness(line.unit, step);
        match self.pop() {
            Value::Bool(true) => at + 1,
            Value::Bool(false) if step.b() == NONE => exit_at(step.a() as usize),
            Value::Bool(false) => step.b() as usize,
            _ => self.branch_raise(line, step),
        }
    }

    /// A branch's condition is no bool.
    #[cold]
    #[inline(never)]
    fn branch_raise(&mut self, line: &mut Line<'_>, step: Step) -> usize {
        self.cx.frames[line.top].pc = step.pc() + 1;
        let error = self.condition_error(step.small() != 0);
        leave(line, Err(error))
    }

    #[inline(always)]
    fn step_jump(&mut self, _line: &mut Line<'_>, step: Step, _: usize, _: usize) -> usize {
        #[cfg(test)]
        self.witness(_line.unit, step);
        if step.b() == NONE {
            return exit_at(step.a() as usize);
        }
        step.b() as usize
    }

    /// The dispatcher runs the op at `step.pc()`.
    #[inline(always)]
    fn step_exit(&mut self, _line: &mut Line<'_>, step: Step, _: usize, _: usize) -> usize {
        exit_at(step.pc() as usize)
    }

    /// Counts a micro-op for the laws, and the steps its ops start as the
    /// dispatcher counts them.
    #[cfg(test)]
    fn witness(&self, unit: &Unit, step: Step) {
        RAN.with(|ran| ran.set(ran.get() + 1));
        let pc = step.pc() as usize;
        let ops = if step.kind() == Kind::Copy as usize
            || step.kind() == Kind::Then as usize && step.small() != NO_PASS
        {
            2
        } else {
            1
        };
        for pc in pc..pc + ops {
            if unit.steps && unit.step[pc] {
                crate::perf_witness::record(|work| work.vm_steps += 1);
            }
        }
    }
}
