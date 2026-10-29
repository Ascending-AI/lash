//! Typed blocks (`docs/design/kernel-regions.md` §2.4, FIG-6946): a
//! representation of admitted register instructions whose operands are all
//! floats or all word integers at the run's entry, compiled to one run over
//! unboxed registers. An integer control prefix and a float tail may repeat
//! together when their frame slots are disjoint; both guard before entry.
//!
//! A run is a block's statements from one of them on: writes and
//! declarations of frame variables whose values are operators with typed
//! kernels ([`lash_kernel_doc::scalar`]) over variables and constants, and
//! `if`s whose test is such a comparison and whose arms are such runs
//! whole. A `while`'s test heads its body's first run. The run reads each
//! variable it reads before it assigns it once, at its entry, where each
//! must hold the run's domain and each variable it writes must hold a
//! value, and writes back what it assigned once, at its end, its floats
//! made canonical there. Operators and branches are flat instructions with
//! explicit operands; temporaries never escape a statement.
//!
//! Its charge is batched: a float run's statements charge a constant sum on
//! each path through its arms, an integer run's the sum of its operators'
//! scalar table entries at the sizes their values have, as the fused nodes
//! charge them one by one. A run answers all or nothing. Where a kernel
//! leaves its operands to the general path, a guard misses or a `while`'s
//! test fails, it has written and charged nothing, and the region runs the
//! same statements one by one from the run's entry: what the reference's
//! ops would do there, they do. A scalar plan admits whole iterations
//! against the bound and the slice's end, so no bound falls inside a run
//! that completes: an iteration it would fall in is never entered, and the
//! reference runs that iteration op by op, stopping at its op with its
//! charge. With a charge tape attached, a run records its statements'
//! static programs with its formulas' units as it completes, which is what
//! the statements' own commits record.

#[cfg(any(test, feature = "perf-witness"))]
use crate::perf_witness::execution;

use lash_kernel_doc::scalar::{self, Pair, ScalarOp};
use lash_kernel_doc::{Float, Value};

use crate::machine::islands::compiled::{
    Instruction as RegisterInstruction, Op as RegisterOp, OperatorSite, Read as RegisterRead,
};
use lash_kernel_exec::Weight;

use super::super::expr::{Expr, Kind};
use super::super::{Executable, LibId};
use super::{Body, Event, RegionSlot, Role, Step, Summary};

mod code;
mod recurrence;
use code::Instruction;
use recurrence::Recurrence;

/// The most variables a run holds in registers.
const REGISTERS: usize = 16;

/// The variables a run reads and writes back: its frame's.
pub(crate) trait Slots {
    fn value(&self, at: u32) -> Option<Value>;
    fn assign(&mut self, at: u32, value: Value);
}

/// A run of a block's statements, compiled for each domain whose kernels
/// answer every operator of it.
pub(crate) struct Typed {
    floats: Option<Program<f64>>,
    ints: Option<Program<i64>>,
    mixed: Option<Mixed>,
    /// It is a `while`'s test and its whole body:
    /// it runs whole iterations ([`Typed::repeat`]).
    pub(crate) whole: bool,
    #[allow(
        dead_code,
        reason = "phase-one source projection; consumed by ledger admission later"
    )]
    pub(crate) source_room: std::sync::Arc<super::super::room::Source>,
    #[allow(
        dead_code,
        reason = "phase-one prefix selector; ledger admission follows"
    )]
    pub(crate) source_site: lash_kernel_value::sites::Site,
}

/// What admits each iteration a whole loop's typed run repeats: the charge
/// so far, the bound and the slice's end, an iteration's most and the next
/// test's unit an iteration charges last, as a scalar plan's batch admits
/// them.
#[derive(Clone, Copy)]
pub(crate) struct Headroom {
    pub(crate) charged: u64,
    pub(crate) bound: u64,
    pub(crate) slice_end: u64,
    pub(crate) iteration_max: u64,
    pub(crate) loop_test: u64,
}

impl Headroom {
    #[inline(always)]
    fn budget(self) -> Option<crate::admission::RepeatBudget> {
        crate::admission::Window::new(self.charged, self.bound, self.slice_end)
            .repeating(self.iteration_max)
    }
}

/// How a program's run, or one iteration of its repeat, ended.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ran {
    /// It ran, charging these units.
    Done(u64),
    /// Its `while`'s test did not hold: the loop ends, as the reference's
    /// test ends it.
    Ended,
    /// A guard or a kernel missed.
    Missed,
}

impl Typed {
    /// Repeats a whole loop's iterations on `frame`'s variables while each
    /// is admitted by `headroom` and the test holds: the iterations it ran
    /// and what they charged, each with the next test's unit, having
    /// written back their outer variables (body locals stay unboxed),
    /// and with `pending` holding their
    /// charge program when `RECORD`; and how the next iteration ended. An
    /// iteration that misses, or whose test fails, has written and charged
    /// nothing: the region runs it.
    #[inline]
    pub(crate) fn repeat<const RECORD: bool, const COUNT: bool>(
        &self,
        exe: &Executable,
        #[cfg(any(test, feature = "perf-witness"))] census: &execution::Recorder,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
        headroom: Headroom,
    ) -> (u64, u64, Ran) {
        if let Some(mixed) = &self.mixed
            && let Some(repeated) = mixed.repeat::<RECORD, COUNT>(
                exe,
                #[cfg(any(test, feature = "perf-witness"))]
                census,
                frame,
                pending,
                headroom,
            )
        {
            return repeated;
        }
        let mut repeated = None;
        if let Some(floats) = &self.floats {
            repeated = floats.repeat::<RECORD, COUNT>(
                exe,
                #[cfg(any(test, feature = "perf-witness"))]
                census,
                frame,
                pending,
                headroom,
            );
        }
        if let (None, Some(ints)) = (&repeated, &self.ints) {
            repeated = ints.repeat::<RECORD, COUNT>(
                exe,
                #[cfg(any(test, feature = "perf-witness"))]
                census,
                frame,
                pending,
                headroom,
            );
        }
        repeated.unwrap_or((0, 0, Ran::Missed))
    }
}

/// What a typed run computes in: floats or word integers, through the
/// typed kernels the operators' prepared entries answer by.
trait Domain: Copy + Default + Send + Sync + 'static {
    /// Its value held by `value`, a variable's or a constant's.
    fn of(value: &Value) -> Option<Self>;
    /// The value the run writes back.
    fn value(self) -> Value;
    /// The kernel `pair` answers this domain's operands by.
    fn kernel(pair: Pair) -> Option<ScalarOp>;
    /// Whether the kernel of `op` answers this domain's operands at all.
    fn answers(op: ScalarOp) -> bool;
    fn number(op: ScalarOp, a: Self, b: Self) -> Option<Self>;
    fn truth(op: ScalarOp, a: Self, b: Self) -> Option<bool>;
    /// Its size bit in a scalar table's mask: set for a nonzero word
    /// integer.
    fn sized(self) -> u8;
    /// Identity of constants, including signed zero and canonical NaN.
    fn same(self, other: Self) -> bool;
    /// Whether an operator's units depend on its operands' values: a float
    /// operator's never do.
    const MEASURED: bool;
}

impl Domain for f64 {
    #[inline(always)]
    fn of(value: &Value) -> Option<Self> {
        match value {
            Value::Float(float) => Some(float.get()),
            _ => None,
        }
    }
    #[inline(always)]
    fn value(self) -> Value {
        Value::Float(Float::new(self))
    }
    fn kernel(pair: Pair) -> Option<ScalarOp> {
        pair.floats
    }
    fn answers(op: ScalarOp) -> bool {
        !matches!(op, ScalarOp::RemFloor | ScalarOp::FloorDiv)
    }
    #[inline(always)]
    fn number(op: ScalarOp, a: Self, b: Self) -> Option<Self> {
        scalar::float_number(op, a, b)
    }
    #[inline(always)]
    fn truth(op: ScalarOp, a: Self, b: Self) -> Option<bool> {
        scalar::float_truth(op, a, b)
    }
    #[inline(always)]
    fn sized(self) -> u8 {
        scalar::float_size_bit(self)
    }
    fn same(self, other: Self) -> bool {
        self.to_bits() == other.to_bits()
    }
    const MEASURED: bool = false;
}

impl Domain for i64 {
    #[inline(always)]
    fn of(value: &Value) -> Option<Self> {
        match value {
            Value::Int(word) => Some(*word),
            _ => None,
        }
    }
    #[inline(always)]
    fn value(self) -> Value {
        Value::Int(self)
    }
    fn kernel(pair: Pair) -> Option<ScalarOp> {
        pair.ints
    }
    // A word integer division's answer is a float: the general path's.
    fn answers(op: ScalarOp) -> bool {
        op != ScalarOp::Div
    }
    #[inline(always)]
    fn number(op: ScalarOp, a: Self, b: Self) -> Option<Self> {
        scalar::int_number(op, a, b)
    }
    #[inline(always)]
    fn truth(op: ScalarOp, a: Self, b: Self) -> Option<bool> {
        scalar::int_truth(op, a, b)
    }
    #[inline(always)]
    fn sized(self) -> u8 {
        scalar::word_size_bit(self)
    }
    fn same(self, other: Self) -> bool {
        self == other
    }
    const MEASURED: bool = true;
}

/// Ephemeral operator results use the register-sized stack window on hot
/// scalar blocks. Larger admitted programs retain their full temporary slice.
struct Scratch<T> {
    stack: [T; REGISTERS],
    heap: Vec<T>,
}

impl<T: Domain> Scratch<T> {
    #[inline(always)]
    fn new() -> Self {
        Self {
            stack: [T::default(); REGISTERS],
            heap: Vec::new(),
        }
    }

    #[inline(always)]
    fn temps(&mut self, count: usize) -> &mut [T] {
        if count <= REGISTERS {
            &mut self.stack[..count]
        } else {
            self.heap.resize(count, T::default());
            &mut self.heap
        }
    }
}

/// One run of a program: its registers and what it has charged, assigned
/// and, recording, held of its charge program.
struct State<'t, T> {
    regs: [T; REGISTERS],
    temps: &'t mut [T],
    holds: bool,
    units: u64,
    /// The registers it assigned.
    written: u32,
    /// Whether its formulas are charged.
    charged: bool,
    /// Whether a charge tape records it.
    record: bool,
    /// Its charge program, recording.
    pending: super::super::TapedCharges,
}

impl<T: Domain> State<'_, T> {
    /// Charges an operator whose table is `table` on `a` and `b`, which
    /// answered a value of size bit `r`, at the sizes the fused node's mask
    /// states: an integer run's units, and the formula a tape records.
    #[inline(always)]
    fn charge(&mut self, table: &[u64; 16], a: T, b: T, r: u8) {
        if (T::MEASURED && self.charged) || self.record {
            let units = table[usize::from(a.sized() | b.sized() << 1 | r << 3) % 16];
            if T::MEASURED && self.charged {
                self.units += units;
            }
            if self.record {
                self.formula(units);
            }
        }
    }

    /// Holds a formula's units, which the next [`Self::recorded`] places:
    /// none where formulas are not charged, as a statement's meter takes
    /// none.
    #[cold]
    fn formula(&mut self, units: u64) {
        let units = if self.charged { units } else { 0 };
        self.pending.push(Weight::Atomic(units));
    }

    /// Records the statement whose last `formulas` held formulas are its
    /// own: its `program`'s fixed events, each formula's units in its
    /// place, as the statement's commit records them.
    #[cold]
    #[inline(never)]
    fn recorded(&mut self, program: &[Event], formulas: usize) {
        let held = self.pending.formulas(formulas);
        let mut held = held.into_iter();
        for event in program {
            let weight = match event {
                Event::Fixed(weight) => *weight,
                Event::Formula => held.next().unwrap_or(Weight::Atomic(0)),
                Event::Arm(_) | Event::Call => continue,
            };
            if weight != Weight::Atomic(0) {
                self.pending.push(weight);
            }
        }
    }

    #[inline(always)]
    fn commit(&mut self, units: u64, formulas: usize, program: &[Event]) {
        self.units += units;
        if self.record {
            self.recorded(program, formulas);
        }
    }
}

/// A scalar operand: a frame register, a constant, or a statement-local
/// temporary produced by an earlier instruction.
#[derive(Clone, Copy)]
enum Arg<T> {
    Reg(u8),
    Const(T),
    Temp(usize),
}

impl<T: Domain> Arg<T> {
    #[inline(always)]
    fn get(self, state: &State<'_, T>) -> T {
        match self {
            Self::Reg(reg) => state.regs[usize::from(reg)],
            Self::Const(value) => value,
            Self::Temp(at) => state.temps[at],
        }
    }
}

#[derive(Clone)]
struct Kernel {
    op: ScalarOp,
    table: [u64; 16],
}

#[derive(Clone)]
struct Unary {
    lib: LibId,
    entry: super::super::Entry,
    table: [u64; 16],
}

/// A run compiled for domain `T`.
#[derive(Clone)]
struct Program<T> {
    /// Each variable it reads before it assigns it, and its register.
    loads: Box<[(u32, u8)]>,
    /// The variables it writes that it does not read first: each holds a
    /// value at its entry.
    held: Box<[u32]>,
    /// Each register's variable's frame position.
    slots: Box<[u32]>,
    /// Body-local registers never escape a repeated iteration.
    locals: u32,
    code: Box<[Instruction<T>]>,
    kernels: Box<[Kernel]>,
    unaries: Box<[Unary]>,
    temps: usize,
    tested: bool,
    /// Whether its formulas are charged.
    charged: bool,
    recurrence: Option<Box<Recurrence<T>>>,
}

impl<T: Domain> Program<T> {
    /// The run's state before its entry.
    #[inline(always)]
    fn state<'t, const RECORD: bool>(&self, temps: &'t mut [T]) -> State<'t, T> {
        State {
            regs: [T::default(); REGISTERS],
            temps,
            holds: false,
            units: 0,
            written: 0,
            charged: self.charged,
            record: RECORD,
            pending: super::super::TapedCharges::new(),
        }
    }

    /// Enters the run, its variables loaded into `state`: `None` where a
    /// guard misses.
    #[inline(always)]
    fn enter<const RECORD: bool>(
        &self,
        state: &mut State<'_, T>,
        frame: &impl Slots,
        pending: &mut Vec<Weight>,
    ) -> Option<()> {
        for &(at, reg) in &*self.loads {
            let value = frame.value(at)?;
            state.regs[usize::from(reg) % REGISTERS] = T::of(&value)?;
        }
        for &at in &*self.held {
            frame.value(at)?;
        }
        if RECORD {
            pending.clear();
            state.pending.events = std::mem::take(pending);
        }
        Some(())
    }

    /// Writes back the registers `written` names, each to its variable.
    #[inline(always)]
    fn leave(&self, frame: &mut impl Slots, state: &State<'_, T>, mut written: u32) {
        // Only the variables' registers are assigned, each with its
        // variable.
        while written != 0 {
            let reg = written.trailing_zeros() as usize;
            written &= written - 1;
            if let Some(at) = self.slots.get(reg) {
                frame.assign(*at, state.regs[reg % REGISTERS].value());
            }
        }
    }

    fn repeatable(&self) -> bool {
        !self
            .loads
            .iter()
            .any(|(_, reg)| self.locals & (1 << reg) != 0)
            && !self.held.iter().any(|at| {
                self.slots
                    .iter()
                    .enumerate()
                    .any(|(reg, slot)| slot == at && self.locals & (1 << reg) != 0)
            })
    }

    /// [`Typed::repeat`] in this domain: `None` where a guard misses at
    /// its entry; else the iterations, their charge and how the next
    /// iteration ended.
    #[inline(always)]
    fn repeat<const RECORD: bool, const COUNT: bool>(
        &self,
        exe: &Executable,
        #[cfg(any(test, feature = "perf-witness"))] census: &execution::Recorder,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
        headroom: Headroom,
    ) -> Option<(u64, u64, Ran)> {
        if !self.tested || !self.repeatable() {
            return None;
        }
        let mut scratch = Scratch::new();
        let mut state = self.state::<RECORD>(scratch.temps(self.temps));
        self.enter::<RECORD>(&mut state, frame, pending)?;
        if !RECORD && let Some(recurrence) = &self.recurrence {
            let (iterations, units, ended) = recurrence.run::<COUNT>(
                self,
                &mut state,
                headroom,
                #[cfg(any(test, feature = "perf-witness"))]
                census,
            );
            let written = if iterations == 0 {
                0
            } else {
                recurrence.written
            };
            self.leave(frame, &state, written & !self.locals);
            return Some((iterations, units, ended));
        }
        let mut iterations = 0_u64;
        let mut units = 0_u64;
        let mut written = 0_u32;
        let budget = headroom.budget();
        let ended = loop {
            // The charge headroom guard, as the batch's.
            let Some(_admission) = budget.as_ref().and_then(|budget| budget.admit(units)) else {
                break Ran::Done(0);
            };
            let regs = state.regs;
            let recorded = state.pending.mark();
            state.units = 0;
            state.written = 0;
            let ran = self.execute::<COUNT>(
                exe,
                #[cfg(any(test, feature = "perf-witness"))]
                census,
                &mut state,
            );
            if !matches!(ran, Ran::Done(_)) {
                // The iteration that did not complete leaves nothing.
                state.regs = regs;
                if RECORD {
                    state.pending.rewind(recorded);
                }
                break ran;
            }
            iterations += 1;
            units += state.units + headroom.loop_test;
            written |= state.written;
            if RECORD && headroom.loop_test != 0 {
                state.pending.push(Weight::Atomic(headroom.loop_test));
            }
        };
        if RECORD {
            *pending = std::mem::take(&mut state.pending.events);
        }
        self.leave(frame, &state, written & !self.locals);
        Some((iterations, units, ended))
    }
}

/// An integer control prefix and an independent float tail retain their
/// own unboxed registers across complete iterations. Disjoint frame slots
/// prove that neither part can observe a delayed write-back from the other.
struct Mixed {
    head: Program<i64>,
    tail: Program<f64>,
}

impl Mixed {
    fn compile(
        exe: &Executable,
        registers: &[RegisterInstruction],
        charged: bool,
        test: &Summary,
        body: &Body,
        from: usize,
    ) -> Option<Self> {
        let (_, covered) =
            Builder::<i64>::new(exe, registers, charged).program(Some(test), body, from)?;
        // Limit speculative suffix formation to the typed register budget.
        for split in (from + 1..=from + covered).rev().take(REGISTERS) {
            let Some((tail, rest)) =
                Builder::<f64>::new(exe, registers, charged).program(None, body, split)
            else {
                continue;
            };
            if split + rest != body.steps.len() {
                continue;
            }
            let (head, prefix) = Builder::<i64>::new(exe, registers, charged).program_until(
                Some(test),
                body,
                from,
                split,
            )?;
            if prefix != split - from
                || head.slots.iter().any(|slot| tail.slots.contains(slot))
                || !head.repeatable()
                || !tail.repeatable()
            {
                continue;
            }
            return Some(Self {
                head: head.fused(),
                tail: tail.fused(),
            });
        }
        None
    }

    #[inline]
    fn repeat<const RECORD: bool, const COUNT: bool>(
        &self,
        exe: &Executable,
        #[cfg(any(test, feature = "perf-witness"))] census: &execution::Recorder,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
        headroom: Headroom,
    ) -> Option<(u64, u64, Ran)> {
        let mut integer_scratch = Scratch::new();
        let mut float_scratch = Scratch::new();
        let mut head = self
            .head
            .state::<RECORD>(integer_scratch.temps(self.head.temps));
        let mut tail = self
            .tail
            .state::<RECORD>(float_scratch.temps(self.tail.temps));
        self.tail
            .enter::<RECORD>(&mut tail, frame, &mut Vec::new())?;
        self.head.enter::<RECORD>(&mut head, frame, pending)?;
        let mut iterations = 0_u64;
        let mut units = 0_u64;
        let mut head_written = 0_u32;
        let mut tail_written = 0_u32;
        let budget = headroom.budget();
        let ended = loop {
            let Some(_admission) = budget.as_ref().and_then(|budget| budget.admit(units)) else {
                break Ran::Done(0);
            };
            let integers = head.regs;
            let floats = tail.regs;
            let recorded = head.pending.mark();
            head.units = 0;
            tail.units = 0;
            head.written = 0;
            tail.written = 0;
            let mut statements = 0_u64;
            let mut ran = self.head.execute_program(exe, &mut head, || {
                if COUNT {
                    statements += 1;
                }
            });
            if matches!(ran, Ran::Done(_)) {
                ran = self.tail.execute_program(exe, &mut tail, || {
                    if COUNT {
                        statements += 1;
                    }
                });
            }
            if !matches!(ran, Ran::Done(_)) {
                head.regs = integers;
                tail.regs = floats;
                if RECORD {
                    head.pending.rewind(recorded);
                    tail.pending.clear();
                }
                break ran;
            }
            iterations += 1;
            units += head.units + tail.units + headroom.loop_test;
            head_written |= head.written;
            tail_written |= tail.written;
            if RECORD {
                head.pending.append(&mut tail.pending);
                if headroom.loop_test != 0 {
                    head.pending.push(Weight::Atomic(headroom.loop_test));
                }
            }
            #[cfg(any(test, feature = "perf-witness"))]
            if COUNT {
                execution::statements_here(census, statements.saturating_sub(1), true);
            }
        };
        if RECORD {
            *pending = std::mem::take(&mut head.pending.events);
        }
        self.head
            .leave(frame, &head, head_written & !self.head.locals);
        self.tail
            .leave(frame, &tail, tail_written & !self.tail.locals);
        Some((iterations, units, ended))
    }
}

impl<T: Domain> Program<T> {
    #[inline(always)]
    fn execute<const COUNT: bool>(
        &self,
        exe: &Executable,
        #[cfg(any(test, feature = "perf-witness"))] census: &execution::Recorder,
        state: &mut State<'_, T>,
    ) -> Ran {
        #[cfg(any(test, feature = "perf-witness"))]
        if COUNT {
            return self.execute_counted(exe, census, state);
        }
        self.execute_program(exe, state, || {})
    }

    #[cfg(any(test, feature = "perf-witness"))]
    #[inline(never)]
    fn execute_counted(
        &self,
        exe: &Executable,
        census: &execution::Recorder,
        state: &mut State<'_, T>,
    ) -> Ran {
        let mut statements = 0_u64;
        let ran = self.execute_program(exe, state, || statements += 1);
        if matches!(ran, Ran::Done(_)) {
            execution::statements_here(
                census,
                statements.saturating_sub(u64::from(self.tested)),
                true,
            );
        }
        ran
    }

    // The silent callback is zero-sized: an uncounted iteration carries no
    // collector argument. Keep the checked loop and its refusal mapping at
    // this one boundary rather than decoding Option<Ran> in every caller.
    #[inline(always)]
    fn execute_program(
        &self,
        _exe: &Executable,
        state: &mut State<'_, T>,
        committed: impl FnMut(),
    ) -> Ran {
        self.execute_checked(state, committed)
            .unwrap_or(Ran::Missed)
    }

    #[inline(always)]
    fn execute_checked(
        &self,
        state: &mut State<'_, T>,
        mut committed: impl FnMut(),
    ) -> Option<Ran> {
        let mut pc = 0;
        loop {
            match &self.code[pc] {
                Instruction::Assigned(assignment) => {
                    let Kernel { op, table } = &self.kernels[assignment.kernel];
                    let (a, b) = (assignment.a.get(state), assignment.b.get(state));
                    let value = T::number(*op, a, b)?;
                    state.charge(table, a, b, value.sized());
                    state.regs[usize::from(assignment.dst)] = value;
                    state.written |= 1 << assignment.dst;
                    committed();
                    let commit = &assignment.commit;
                    state.commit(commit.units, commit.formulas, &commit.program);
                }
                Instruction::Test(test) => {
                    let Kernel { op, table } = &self.kernels[test.kernel];
                    let (a, b) = (test.a.get(state), test.b.get(state));
                    state.holds = T::truth(*op, a, b)?;
                    state.charge(table, a, b, 0);
                    committed();
                    let commit = &test.commit;
                    state.commit(commit.units, commit.formulas, &commit.program);
                    pc = if state.holds { pc + 1 } else { test.otherwise };
                    continue;
                }
                Instruction::Unary { kernel, a, dst } => {
                    let Unary { entry, table, .. } = &self.unaries[*kernel];
                    let a = a.get(state);
                    let value = T::of(&entry.call(&[a.value()])?)?;
                    state.charge(table, a, T::default(), value.sized());
                    state.temps[*dst] = value;
                }
                Instruction::Number { kernel, a, b, dst } => {
                    let Kernel { op, table } = &self.kernels[*kernel];
                    let (a, b) = (a.get(state), b.get(state));
                    let value = T::number(*op, a, b)?;
                    state.charge(table, a, b, value.sized());
                    state.temps[*dst] = value;
                }
                Instruction::Compare { kernel, a, b } => {
                    let Kernel { op, table } = &self.kernels[*kernel];
                    let (a, b) = (a.get(state), b.get(state));
                    state.holds = T::truth(*op, a, b)?;
                    state.charge(table, a, b, 0);
                }
                Instruction::Set { value, dst } => {
                    state.regs[usize::from(*dst)] = value.get(state);
                    state.written |= 1 << dst;
                }
                Instruction::Commit {
                    units,
                    formulas,
                    program,
                    ..
                } => {
                    committed();
                    state.commit(*units, *formulas, program);
                }
                Instruction::Branch { otherwise } if !state.holds => {
                    pc = *otherwise;
                    continue;
                }
                Instruction::Jump { to } => {
                    pc = *to;
                    continue;
                }
                Instruction::Exit { ended } => {
                    return Some(if *ended {
                        Ran::Ended
                    } else {
                        Ran::Done(state.units)
                    });
                }
                Instruction::Branch { .. } => {}
            }
            pc += 1;
        }
    }
}

/// The typed run of `body`'s statements from `from` on, headed by `test`,
/// a `while`'s, when given; `None` where it holds fewer than
/// [`OPERATORS`] operators, or no domain answers it.
pub(super) fn compile(
    exe: &Executable,
    registers: &[RegisterInstruction],
    charged: bool,
    test: Option<&Summary>,
    body: &Body,
    from: usize,
    source: super::super::room::Prefix<'_>,
) -> Option<Typed> {
    let mixed = test.and_then(|test| Mixed::compile(exe, registers, charged, test, body, from));
    let floats = Builder::<f64>::new(exe, registers, charged).program(test, body, from);
    let ints = Builder::<i64>::new(exe, registers, charged).program(test, body, from);
    let covers = match (&floats, &ints) {
        (Some((_, a)), Some((_, b))) => (*a).max(*b),
        (Some((_, covers)), None) | (None, Some((_, covers))) => *covers,
        (None, None) => return None,
    };
    // A mixed run can fall back only to a homogeneous run of the whole
    // body. A shorter prefix must return to the ordinary register program.
    let homogeneous = mixed.is_none() || covers == body.steps.len() - from;
    Some(Typed {
        source_room: std::sync::Arc::clone(source.source),
        source_site: source.at.clone(),
        floats: floats.and_then(|(program, covered)| {
            (homogeneous && covered == covers).then(|| program.fused())
        }),
        ints: ints.and_then(|(program, covered)| {
            (homogeneous && covered == covers).then(|| program.fused())
        }),
        whole: mixed.is_some() || test.is_some() && covers == body.steps.len(),
        mixed,
    })
}

/// What a run's compilation has allotted so far.
#[derive(Clone, Default)]
struct Allotted {
    /// Each register's variable's frame position.
    slots: Vec<u32>,
    loads: Vec<(u32, u8)>,
    held: Vec<u32>,
    /// The registers assigned on every path so far.
    assigned: u32,
    /// The operators it runs on any path.
    operators: usize,
}

/// The fewest operators a run holds: a run of one costs its entry and its
/// write-back more than the fused node it replaces saves.
const OPERATORS: usize = 2;

/// What a statement's operators charge whatever their values: in a charged
/// run of floats, each one's table's first entry.
#[derive(Default)]
struct Formulas {
    count: usize,
    units: u64,
}

enum SourceEnd {
    Write(usize),
    Test,
    Branch { next: usize, otherwise: usize },
}

fn summary(step: &Step) -> Option<&Summary> {
    match step {
        Step::Write(summary)
        | Step::Bind(summary)
        | Step::Store(summary)
        | Step::Discard(summary)
        | Step::Continue(summary) => Some(summary),
        Step::If { test, .. } => Some(test),
        Step::Leave => None,
    }
}

fn authorities<'a>(body: &'a Body, entries: &mut Vec<&'a Summary>) {
    for step in &body.steps {
        entries.extend(summary(step));
        if let Step::If {
            then, otherwise, ..
        } = step
        {
            authorities(then, entries);
            authorities(otherwise, entries);
        }
    }
}

/// Compiles a run for domain `T`.
struct Builder<'e, T> {
    exe: &'e Executable,
    registers: &'e [RegisterInstruction],
    charged: bool,
    state: Allotted,
    code: Vec<Instruction<T>>,
    kernels: Vec<Kernel>,
    unaries: Vec<Unary>,
    temps: usize,
    next_temp: usize,
}

impl<'e, T: Domain> Builder<'e, T> {
    fn new(exe: &'e Executable, registers: &'e [RegisterInstruction], charged: bool) -> Self {
        Self {
            exe,
            registers,
            charged,
            state: Allotted::default(),
            code: Vec::new(),
            kernels: Vec::new(),
            unaries: Vec::new(),
            temps: 0,
            next_temp: 0,
        }
    }

    /// The program of `body`'s run from `from`, and how many statements it
    /// runs.
    fn program(
        self,
        test: Option<&Summary>,
        body: &Body,
        from: usize,
    ) -> Option<(Program<T>, usize)> {
        self.program_until(test, body, from, body.steps.len())
    }

    fn program_until(
        mut self,
        test: Option<&Summary>,
        body: &Body,
        from: usize,
        until: usize,
    ) -> Option<(Program<T>, usize)> {
        let tested = test.is_some();
        let test_branch = if let Some(test) = test {
            self.test(test)?;
            Some(self.branch())
        } else {
            None
        };
        let mut covers = 0;
        let mut units = 0_u64;
        let mut authority = Vec::new();
        authorities(body, &mut authority);
        for step in body.steps.get(from..until)? {
            // Failed compilation leaves neither registers nor instructions.
            let saved = self.state.clone();
            let code = self.code.len();
            let kernels = self.kernels.len();
            let unaries = self.unaries.len();
            let temps = self.temps;
            let charge = summary(step)
                .and_then(|summary| self.source_start(summary))
                .and_then(|at| self.step(at, &authority).map(|(units, _)| units));
            if charge.is_none() {
                self.state = saved;
                self.code.truncate(code);
                self.kernels.truncate(kernels);
                self.unaries.truncate(unaries);
                self.temps = temps;
                break;
            }
            units = units.checked_add(charge?)?;
            covers += 1;
        }
        if self.state.operators < OPERATORS {
            return None;
        }
        self.code.push(Instruction::Exit { ended: false });
        if let Some(branch) = test_branch {
            self.code[branch] = Instruction::Branch {
                otherwise: self.code.len(),
            };
            self.code.push(Instruction::Exit { ended: true });
        }
        let program = Program {
            locals: self
                .state
                .slots
                .iter()
                .enumerate()
                .filter(|(_, at)| body.declares.contains(at))
                .fold(0, |mask, (reg, _)| mask | (1 << reg)),
            loads: self.state.loads.into(),
            held: self.state.held.into(),
            slots: self.state.slots.into(),
            code: self.code.into(),
            kernels: self.kernels.into(),
            unaries: self.unaries.into(),
            temps: self.temps,
            tested,
            charged: self.charged,
            recurrence: None,
        };
        program
            .verify(self.exe, test, body, from, covers)
            .then_some((program, covers))
    }

    fn branch(&mut self) -> usize {
        let at = self.code.len();
        self.code.push(Instruction::Branch {
            otherwise: usize::MAX,
        });
        at
    }

    fn commit(&mut self, summary: &Summary, formulas: Formulas) -> Option<u64> {
        let program = program(summary)?;
        let units = self.units(&program, &formulas)?;
        self.code.push(Instruction::Commit {
            at: summary.ops.start,
            units,
            formulas: formulas.count,
            program,
        });
        self.temps = self.temps.max(self.next_temp);
        self.next_temp = 0;
        Some(units)
    }

    /// The units a statement of `program` whose operators charge
    /// `formulas` charges whatever its values: its fixed units and, in a
    /// charged run of floats, its formulas'.
    fn units(&self, program: &[Event], formulas: &Formulas) -> Option<u64> {
        let fixed = program.iter().try_fold(0_u64, |sum, event| match event {
            Event::Fixed(weight) => sum.checked_add(weight.units()),
            _ => Some(sum),
        })?;
        if T::MEASURED || !self.charged {
            return Some(fixed);
        }
        fixed.checked_add(formulas.units)
    }

    /// Source summaries index canonical events only. Physical writes and
    /// control edges determine the changed representation's instructions.
    fn step(&mut self, at: usize, authority: &[&Summary]) -> Option<(u64, usize)> {
        let source = authority
            .iter()
            .copied()
            .find(|summary| self.source_start(summary) == Some(at))?;
        let (formulas, end) = self.project(at)?;
        let units = self.commit(source, formulas)?;
        match end {
            SourceEnd::Write(after) => Some((units, after)),
            SourceEnd::Branch { next, otherwise } => {
                let origin = match self.registers.first()?.op {
                    RegisterOp::Loop { header, .. } => header.checked_sub(1)?,
                    _ => 0,
                };
                let otherwise = otherwise.checked_sub(origin)?;
                let jump_at = otherwise.checked_sub(1)?;
                let RegisterOp::Jump(after) = self.registers.get(jump_at)?.op else {
                    return None;
                };
                let after = after.checked_sub(origin)?;
                if next > jump_at || otherwise > after || after > self.registers.len() {
                    return None;
                }
                let branch = self.branch();
                let before = self.state.assigned;
                self.arm(next, jump_at, authority)?;
                let assigned = self.state.assigned;
                self.state.assigned = before;
                let jump = self.code.len();
                self.code.push(Instruction::Jump { to: usize::MAX });
                self.code[branch] = Instruction::Branch {
                    otherwise: self.code.len(),
                };
                self.arm(otherwise, after, authority)?;
                self.state.assigned &= assigned;
                self.code[jump] = Instruction::Jump {
                    to: self.code.len(),
                };
                Some((units, after))
            }
            SourceEnd::Test => None,
        }
    }

    fn arm(&mut self, mut at: usize, end: usize, authority: &[&Summary]) -> Option<()> {
        while at < end {
            if matches!(self.registers.get(at)?.op, RegisterOp::Done { .. }) {
                at += 1;
            } else {
                let (_, next) = self.step(at, authority)?;
                if next <= at || next > end {
                    return None;
                }
                at = next;
            }
        }
        Some(())
    }

    fn test(&mut self, summary: &Summary) -> Option<()> {
        let at = self.source_start(summary)?;
        let (formulas, SourceEnd::Test) = self.project(at)? else {
            return None;
        };
        self.commit(summary, formulas).map(|_| ())
    }

    fn read_register(&mut self, read: &RegisterRead) -> Option<Arg<T>> {
        match read {
            RegisterRead::Literal { value, .. } => Some(Arg::Const(T::of(value)?)),
            RegisterRead::Local {
                at, shared: false, ..
            } => Some(Arg::Reg(self.read(u32::try_from(*at).ok()?)?)),
            _ => None,
        }
    }

    fn operation(
        &mut self,
        site: &OperatorSite,
        args: &[Arg<T>],
        formulas: &mut Formulas,
        comparison: bool,
    ) -> Option<Arg<T>> {
        if site.counted.is_some() || site.act.is_some() || site.root.is_some() {
            return None;
        }
        let dst = self.next_temp;
        self.next_temp += 1;
        let units = match args {
            [a] if !comparison => {
                let unary = self.unary(site.lib, site.checked)?;
                let units = unary.table[0];
                let kernel = self.unaries.len();
                self.unaries.push(unary);
                self.code.push(Instruction::Unary { kernel, a: *a, dst });
                units
            }
            [a, b] => {
                let op = T::kernel(site.pair?)?;
                if !T::answers(op) || op.compares() != comparison {
                    return None;
                }
                let table = site.table.entries();
                let units = table[0];
                let kernel = self.kernels.len();
                self.kernels.push(Kernel { op, table });
                self.code.push(if comparison {
                    Instruction::Compare {
                        kernel,
                        a: *a,
                        b: *b,
                    }
                } else {
                    Instruction::Number {
                        kernel,
                        a: *a,
                        b: *b,
                        dst,
                    }
                });
                units
            }
            _ => return None,
        };
        formulas.count += 1;
        formulas.units = formulas.units.checked_add(units)?;
        self.state.operators += 1;
        Some(Arg::Temp(dst))
    }

    fn source_start(&self, summary: &Summary) -> Option<usize> {
        self.registers.iter().position(|instruction| {
            let entry = instruction.begin.as_ref().or(match &instruction.op {
                RegisterOp::Write { site } => site.begin.as_ref(),
                _ => None,
            });
            entry.is_some_and(|entry| {
                let statement = self.exe.stmt(entry.id);
                let range = match statement.form {
                    super::super::Form::While { test, .. } => test,
                    _ => statement.ops,
                };
                range.start <= summary.ops.start && summary.ops.start < range.end()
            })
        })
    }

    fn project(&mut self, start: usize) -> Option<(Formulas, SourceEnd)> {
        let mut values = std::collections::BTreeMap::new();
        let mut formulas = Formulas::default();
        for (offset, instruction) in self.registers[start..].iter().enumerate() {
            let after = start + offset + 1;
            match &instruction.op {
                RegisterOp::Loop { .. }
                | RegisterOp::Header { .. }
                | RegisterOp::Close { .. }
                | RegisterOp::Accumulate(_) => {}
                RegisterOp::Constant { dst, value } => {
                    values.insert(*dst, Arg::Const(T::of(value)?));
                }
                RegisterOp::Local {
                    dst,
                    at,
                    shared: false,
                    ..
                } => {
                    values.insert(*dst, Arg::Reg(self.read(u32::try_from(*at).ok()?)?));
                }
                RegisterOp::DirectOperator { site, reads, .. } => {
                    let args = reads
                        .iter()
                        .map(|read| self.read_register(read))
                        .collect::<Option<Vec<_>>>()?;
                    let comparison = site
                        .pair
                        .and_then(T::kernel)
                        .is_some_and(ScalarOp::compares);
                    let value = self.operation(site, &args, &mut formulas, comparison)?;
                    values.insert(site.reg, value);
                }
                RegisterOp::Operator { site, args, .. } => {
                    let args = args
                        .iter()
                        .map(|reg| values.get(reg).copied())
                        .collect::<Option<Vec<_>>>()?;
                    let comparison = site
                        .pair
                        .and_then(T::kernel)
                        .is_some_and(ScalarOp::compares);
                    let value = self.operation(site, &args, &mut formulas, comparison)?;
                    values.insert(site.reg, value);
                }
                RegisterOp::Accumulation {
                    site, reads, write, ..
                } if !write.shared => {
                    let args = reads
                        .iter()
                        .map(|read| self.read_register(read))
                        .collect::<Option<Vec<_>>>()?;
                    let value = self.operation(site, &args, &mut formulas, false)?;
                    let dst = self.assign(u32::try_from(write.at).ok()?, !write.bind)?;
                    self.code.push(Instruction::Set { value, dst });
                    return Some((formulas, SourceEnd::Write(after)));
                }
                RegisterOp::Write { site } if !site.shared => {
                    let value = match &site.reads {
                        Some(reads) => {
                            let [read] = &**reads else {
                                return None;
                            };
                            self.read_register(read)?
                        }
                        None => *values.get(&site.reg)?,
                    };
                    let dst = self.assign(u32::try_from(site.at).ok()?, !site.bind)?;
                    self.code.push(Instruction::Set { value, dst });
                    return Some((formulas, SourceEnd::Write(after)));
                }
                RegisterOp::LoopBranch { .. } => return Some((formulas, SourceEnd::Test)),
                RegisterOp::Branch { otherwise, .. } => {
                    return Some((
                        formulas,
                        SourceEnd::Branch {
                            next: after,
                            otherwise: *otherwise,
                        },
                    ));
                }
                _ => return None,
            }
        }
        None
    }

    // The independent summary verifier resolves the installed entry too.
    fn kernel(&self, lib: LibId, checked: bool) -> Option<(ScalarOp, [u64; 16])> {
        let function = self.exe.lib(lib);
        let (_, fast) = function.specialization.prepared.as_ref()?;
        let op = T::kernel(super::super::numeric_pair(function, checked)?)?;
        if !T::answers(op) {
            return None;
        }
        let table = fast.scalar()?;
        Some((op, table.entries()))
    }

    fn unary(&self, lib: LibId, checked: bool) -> Option<Unary> {
        let function = self.exe.lib(lib);
        let (entry, fast) = function.specialization.prepared.as_ref()?;
        if function.arity != 1 {
            return None;
        }
        if checked {
            let signature = &function.header.signature;
            let value = T::default().value();
            if !signature
                .params
                .iter()
                .all(|param| super::super::import_result_fits(&param.ty, value))
                || !super::super::import_result_fits(&signature.result, value)
            {
                return None;
            }
        }
        let table = fast.scalar()?;
        Some(Unary {
            lib,
            entry: entry.clone(),
            table: table.entries(),
        })
    }

    /// The register of the variable at `at`, which a read loads at the
    /// run's entry unless every path to it has assigned it.
    fn read(&mut self, at: u32) -> Option<u8> {
        let reg = self.register(at)?;
        let state = &mut self.state;
        if state.assigned & 1 << reg == 0 && !state.loads.iter().any(|(slot, _)| *slot == at) {
            state.loads.push((at, reg));
            state.held.retain(|slot| *slot != at);
        }
        Some(reg)
    }

    /// The register of the variable at `at`, assigned: a write's (not a
    /// declaration's) holds a value at the run's entry.
    fn assign(&mut self, at: u32, write: bool) -> Option<u8> {
        let reg = self.register(at)?;
        let state = &mut self.state;
        if write && !state.loads.iter().any(|(slot, _)| *slot == at) && !state.held.contains(&at) {
            state.held.push(at);
        }
        state.assigned |= 1 << reg;
        Some(reg)
    }

    fn register(&mut self, at: u32) -> Option<u8> {
        let slots = &mut self.state.slots;
        let reg = match slots.iter().position(|slot| *slot == at) {
            Some(reg) => reg,
            None => {
                slots.push(at);
                slots.len() - 1
            }
        };
        u8::try_from(reg)
            .ok()
            .filter(|reg| usize::from(*reg) < REGISTERS)
    }
}

mod verify;

/// The value a statement's last op takes, where the statement's program
/// charges it alone: no guest call, no counted invoke, no conditional
/// expression and no value but scalars.
fn root(summary: &Summary) -> Option<&Expr> {
    if summary.action.is_some()
        || summary.guest.is_some()
        || !summary.calls.is_empty()
        || !summary.arms.is_empty()
        || summary.general
    {
        return None;
    }
    match &*summary.roots {
        [(Role::Value, root)] => Some(root),
        _ => None,
    }
}

/// `summary`'s charge program, when it holds fixed events and formulas
/// alone.
fn program(summary: &Summary) -> Option<Box<[Event]>> {
    summary
        .program
        .iter()
        .all(|event| matches!(event, Event::Fixed(_) | Event::Formula))
        .then(|| summary.program.clone())
}
