//! Typed blocks (`docs/design/kernel-regions.md` §2.4, FIG-6946): a
//! straight-line run of a scalar plan's statements whose operands are all
//! floats or all word integers at the run's entry, compiled to one run over
//! unboxed registers.
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
use crate::perf_witness::execution::{self, Owner};

use lash_kernel_doc::scalar::{self, Pair, ScalarOp};
use lash_kernel_doc::{Float, Value};

use lash_kernel_exec::Weight;

use super::super::expr::{Expr, Kind};
use super::super::{Executable, LibId};
use super::{Body, Event, RegionSlot, Role, Step, Summary};
use crate::machine::SlotState;

/// The most variables a run holds in registers.
const REGISTERS: usize = 16;

/// The variables a run reads and writes back: its frame's.
pub(crate) trait Slots {
    fn slots(&self) -> &[SlotState];
    fn assign(&mut self, at: u32, value: Value);
}

/// A run of a block's statements, compiled for each domain whose kernels
/// answer every operator of it.
pub(crate) struct Typed {
    floats: Option<Program<f64>>,
    ints: Option<Program<i64>>,
    /// The statements of its block it runs, from its first.
    pub(crate) covers: usize,
    /// It is a `while`'s test and its whole body, which declares nothing:
    /// it runs whole iterations ([`Typed::repeat`]).
    pub(crate) whole: bool,
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
    /// written back what they assigned, and with `pending` holding their
    /// charge program when `RECORD`; and how the next iteration ended. An
    /// iteration that misses, or whose test fails, has written and charged
    /// nothing: the region runs it.
    #[inline]
    pub(crate) fn repeat<const RECORD: bool, const COUNT: bool>(
        &self,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
        headroom: Headroom,
    ) -> (u64, u64, Ran) {
        let mut repeated = None;
        if let Some(floats) = &self.floats {
            repeated = floats.repeat::<RECORD, COUNT>(frame, pending, headroom);
        }
        if let (None, Some(ints)) = (&repeated, &self.ints) {
            repeated = ints.repeat::<RECORD, COUNT>(frame, pending, headroom);
        }
        repeated.unwrap_or((0, 0, Ran::Missed))
    }

    /// Runs the block on `frame`'s variables: `Done` with its charge,
    /// having written back what it assigned, and with `pending` holding its
    /// charge program when `RECORD`; otherwise having written and charged
    /// nothing.
    #[inline]
    pub(crate) fn run<const RECORD: bool, const COUNT: bool>(
        &self,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
    ) -> Ran {
        let mut ran = Ran::Missed;
        if let Some(floats) = &self.floats {
            ran = floats.run::<RECORD, COUNT>(frame, pending);
        }
        if let (Ran::Missed, Some(ints)) = (ran, &self.ints) {
            ran = ints.run::<RECORD, COUNT>(frame, pending);
        }
        ran
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
        0
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
        u8::from(self != 0)
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
    pending: Vec<Weight>,
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
        let start = self.pending.len().saturating_sub(formulas);
        let held: Vec<Weight> = self.pending.drain(start..).collect();
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

/// Private execution positions, rebuilt with the executable, never durable PCs.
/// All branches go forward. A comparison's result is consumed by its branch.
#[derive(Clone)]
enum Instruction<T> {
    Number {
        kernel: usize,
        a: Arg<T>,
        b: Arg<T>,
        dst: usize,
    },
    Compare {
        kernel: usize,
        a: Arg<T>,
        b: Arg<T>,
    },
    Set {
        value: Arg<T>,
        dst: u8,
    },
    Commit {
        at: u32,
        units: u64,
        formulas: usize,
        program: Box<[Event]>,
    },
    Branch {
        otherwise: usize,
    },
    Jump {
        to: usize,
    },
    Exit {
        ended: bool,
    },
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
    code: Box<[Instruction<T>]>,
    kernels: Box<[Kernel]>,
    temps: usize,
    tested: bool,
    /// Whether its formulas are charged.
    charged: bool,
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
            pending: Vec::new(),
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
        let slots = frame.slots();
        for &(at, reg) in &*self.loads {
            let SlotState::Value(value) = slots.get(at as usize)? else {
                return None;
            };
            state.regs[usize::from(reg) % REGISTERS] = T::of(value)?;
        }
        for &at in &*self.held {
            if !matches!(slots.get(at as usize)?, SlotState::Value(_)) {
                return None;
            }
        }
        if RECORD {
            pending.clear();
            state.pending = std::mem::take(pending);
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

    /// [`Typed::repeat`] in this domain: `None` where a guard misses at
    /// its entry; else the iterations, their charge and how the next
    /// iteration ended.
    #[inline(always)]
    fn repeat<const RECORD: bool, const COUNT: bool>(
        &self,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
        headroom: Headroom,
    ) -> Option<(u64, u64, Ran)> {
        if !self.tested {
            return None;
        }
        let mut scratch = Scratch::new();
        let mut state = self.state::<RECORD>(scratch.temps(self.temps));
        self.enter::<RECORD>(&mut state, frame, pending)?;
        let mut iterations = 0_u64;
        let mut units = 0_u64;
        let mut written = 0_u32;
        let ended = loop {
            // The charge headroom guard, as the batch's.
            match headroom
                .charged
                .checked_add(units)
                .and_then(|charged| charged.checked_add(headroom.iteration_max))
            {
                Some(end) if end <= headroom.bound && end < headroom.slice_end => {}
                _ => break Ran::Done(0),
            }
            let regs = state.regs;
            let recorded = state.pending.len();
            state.units = 0;
            state.written = 0;
            let ran = self.execute::<COUNT>(&mut state);
            if !matches!(ran, Ran::Done(_)) {
                // The iteration that did not complete leaves nothing.
                state.regs = regs;
                if RECORD {
                    state.pending.truncate(recorded);
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
            *pending = std::mem::take(&mut state.pending);
        }
        self.leave(frame, &state, written);
        Some((iterations, units, ended))
    }

    #[inline(always)]
    fn run<const RECORD: bool, const COUNT: bool>(
        &self,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
    ) -> Ran {
        let mut scratch = Scratch::new();
        let mut state = self.state::<RECORD>(scratch.temps(self.temps));
        if self.enter::<RECORD>(&mut state, frame, pending).is_none() {
            return Ran::Missed;
        }
        let ran = self.execute::<COUNT>(&mut state);
        if RECORD {
            *pending = std::mem::take(&mut state.pending);
        }
        if let Ran::Done(_) = ran {
            self.leave(frame, &state, state.written);
        }
        ran
    }
}

impl<T: Domain> Program<T> {
    #[inline]
    fn execute<const COUNT: bool>(&self, state: &mut State<'_, T>) -> Ran {
        let mut statements = 0;
        let ran = self
            .execute_checked::<COUNT>(state, &mut statements)
            .unwrap_or(Ran::Missed);
        #[cfg(any(test, feature = "perf-witness"))]
        if COUNT && matches!(ran, Ran::Done(_)) {
            execution::statements(
                Owner::ScalarRegion,
                statements.saturating_sub(u64::from(self.tested)),
                COUNT,
            );
        }
        ran
    }

    #[inline]
    fn execute_checked<const COUNT: bool>(
        &self,
        state: &mut State<'_, T>,
        _statements: &mut u64,
    ) -> Option<Ran> {
        let mut pc = 0;
        loop {
            match &self.code[pc] {
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
                    #[cfg(any(test, feature = "perf-witness"))]
                    if COUNT {
                        *_statements += 1;
                    }
                    state.units += units;
                    if state.record {
                        state.recorded(program, *formulas);
                    }
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
    charged: bool,
    test: Option<&Summary>,
    body: &Body,
    from: usize,
) -> Option<Typed> {
    let floats = Builder::<f64>::new(exe, charged).program(test, body, from);
    let ints = Builder::<i64>::new(exe, charged).program(test, body, from);
    let covers = match (&floats, &ints) {
        (Some((_, a)), Some((_, b))) => (*a).max(*b),
        (Some((_, covers)), None) | (None, Some((_, covers))) => *covers,
        (None, None) => return None,
    };
    // Both domains run the same statements, or the wider one alone.
    Some(Typed {
        floats: floats.and_then(|(program, covered)| (covered == covers).then_some(program)),
        ints: ints.and_then(|(program, covered)| (covered == covers).then_some(program)),
        covers,
        whole: test.is_some() && covers == body.steps.len() && body.declares.is_empty(),
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

/// Compiles a run for domain `T`.
struct Builder<'e, T> {
    exe: &'e Executable,
    charged: bool,
    state: Allotted,
    code: Vec<Instruction<T>>,
    kernels: Vec<Kernel>,
    temps: usize,
    next_temp: usize,
}

impl<'e, T: Domain> Builder<'e, T> {
    fn new(exe: &'e Executable, charged: bool) -> Self {
        Self {
            exe,
            charged,
            state: Allotted::default(),
            code: Vec::new(),
            kernels: Vec::new(),
            temps: 0,
            next_temp: 0,
        }
    }

    /// The program of `body`'s run from `from`, and how many statements it
    /// runs.
    fn program(
        mut self,
        test: Option<&Summary>,
        body: &Body,
        from: usize,
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
        for step in body.steps.get(from..)? {
            // Failed compilation leaves neither registers nor instructions.
            let saved = self.state.clone();
            let code = self.code.len();
            let kernels = self.kernels.len();
            let temps = self.temps;
            let charge = self.step(step);
            if charge.is_none() {
                self.state = saved;
                self.code.truncate(code);
                self.kernels.truncate(kernels);
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
            loads: self.state.loads.into(),
            held: self.state.held.into(),
            slots: self.state.slots.into(),
            code: self.code.into(),
            kernels: self.kernels.into(),
            temps: self.temps,
            tested,
            charged: self.charged,
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

    /// Appends one whole statement, including its explicit branch targets.
    fn step(&mut self, step: &Step) -> Option<u64> {
        match step {
            Step::Write(summary) | Step::Bind(summary) => {
                let Some(RegionSlot::Frame(at)) = summary.write else {
                    return None;
                };
                let mut formulas = Formulas::default();
                let value = self.operand(root(summary)?, &mut formulas)?;
                let dst = self.assign(at, matches!(step, Step::Write(_)))?;
                self.code.push(Instruction::Set { value, dst });
                self.commit(summary, formulas)
            }
            Step::If {
                test,
                then,
                otherwise,
            } if then.declares.is_empty() && otherwise.declares.is_empty() => {
                self.test(test)?;
                let branch = self.branch();
                let before = self.state.assigned;
                self.arm(then)?;
                let assigned = self.state.assigned;
                self.state.assigned = before;
                let jump = self.code.len();
                self.code.push(Instruction::Jump { to: usize::MAX });
                self.code[branch] = Instruction::Branch {
                    otherwise: self.code.len(),
                };
                self.arm(otherwise)?;
                self.state.assigned &= assigned;
                self.code[jump] = Instruction::Jump {
                    to: self.code.len(),
                };
                Some(0)
            }
            _ => None,
        }
    }

    fn arm(&mut self, body: &Body) -> Option<()> {
        let mut units = 0_u64;
        for step in &body.steps {
            units = units.checked_add(self.step(step)?)?;
        }
        Some(())
    }

    fn test(&mut self, summary: &Summary) -> Option<()> {
        let Kind::Operator {
            lib, args, invoke, ..
        } = &root(summary)?.kind
        else {
            return None;
        };
        let [a, b] = &**args else {
            return None;
        };
        let (op, table) = self.kernel(*lib, invoke.is_some_and(|invoke| invoke.checked))?;
        if !op.compares() {
            return None;
        }
        let mut formulas = Formulas::default();
        let a = self.operand(a, &mut formulas)?;
        let b = self.operand(b, &mut formulas)?;
        formulas.count += 1;
        formulas.units = formulas.units.checked_add(table[0])?;
        self.state.operators += 1;
        let kernel = self.kernels.len();
        self.kernels.push(Kernel { op, table });
        self.code.push(Instruction::Compare { kernel, a, b });
        self.commit(summary, formulas).map(|_| ())
    }

    /// The typed kernel of operator `lib` in this domain and its scalar
    /// table: the fused node's, which the entry's general path agrees with.
    fn kernel(&self, lib: LibId, checked: bool) -> Option<(ScalarOp, [u64; 16])> {
        let function = self.exe.lib(lib);
        let (entry, fast) = function.prepared.as_ref()?;
        let op = T::kernel(entry.pair()?)?;
        // A kernel that never answers this domain leaves every run to the
        // general path.
        if !T::answers(op) {
            return None;
        }
        if checked {
            let signature = &function.header.signature;
            let value = T::value(T::default());
            let result = if op.compares() {
                Value::Bool(false)
            } else {
                value
            };
            if !signature
                .params
                .iter()
                .all(|param| super::super::import_result_fits(&param.ty, value))
                || !super::super::import_result_fits(&signature.result, result)
            {
                return None;
            }
        }
        let table = fast.scalar()?;
        let mut units = [0; 16];
        for (mask, units) in units.iter_mut().enumerate() {
            *units = table.units(u8::try_from(mask).ok()?)?;
        }
        Some((op, units))
    }

    /// The operand `expr` gives: an arithmetic operator's value, its
    /// operands first, as the fused nodes run them, or a variable's or a
    /// constant's.
    fn operand(&mut self, expr: &Expr, formulas: &mut Formulas) -> Option<Arg<T>> {
        match &expr.kind {
            Kind::Operator {
                lib, args, invoke, ..
            } => {
                let [a, b] = &**args else {
                    return None;
                };
                let (op, table) = self.kernel(*lib, invoke.is_some_and(|invoke| invoke.checked))?;
                if op.compares() {
                    return None;
                }
                let a = self.operand(a, formulas)?;
                let b = self.operand(b, formulas)?;
                formulas.count += 1;
                formulas.units = formulas.units.checked_add(table[0])?;
                self.state.operators += 1;
                let dst = self.next_temp;
                self.next_temp += 1;
                let kernel = self.kernels.len();
                self.kernels.push(Kernel { op, table });
                self.code.push(Instruction::Number { kernel, a, b, dst });
                Some(Arg::Temp(dst))
            }
            Kind::Local {
                local,
                scoped: false,
                ..
            } => Some(Arg::Reg(self.read(local.at)?)),
            Kind::Const(value) => Some(Arg::Const(T::of(value)?)),
            _ => None,
        }
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

/// Checks the private program against the already verified semantic summaries.
/// The cursor consumes every instruction once, including both branch arms;
/// operands must name initialized registers or earlier statement temporaries.
struct Verify<'a, T> {
    program: &'a Program<T>,
    exe: &'a Executable,
    pc: usize,
    initialized: u32,
    temps: usize,
    formulas: Formulas,
}

impl<T: Domain> Program<T> {
    fn verify(
        &self,
        exe: &Executable,
        test: Option<&Summary>,
        body: &Body,
        from: usize,
        covers: usize,
    ) -> bool {
        if self.slots.len() > REGISTERS
            || self.tested != test.is_some()
            || self
                .slots
                .iter()
                .enumerate()
                .any(|(i, slot)| self.slots[..i].contains(slot))
        {
            return false;
        }
        let mut initialized = 0;
        for &(slot, reg) in &*self.loads {
            if self.slots.get(usize::from(reg)) != Some(&slot) || initialized & (1 << reg) != 0 {
                return false;
            }
            initialized |= 1 << reg;
        }
        if self
            .held
            .iter()
            .any(|slot| !self.slots.contains(slot) || self.loads.iter().any(|(at, _)| at == slot))
        {
            return false;
        }
        let Some(steps) = body.steps.get(from..).and_then(|steps| steps.get(..covers)) else {
            return false;
        };
        let mut check = Verify {
            program: self,
            exe,
            pc: 0,
            initialized,
            temps: 0,
            formulas: Formulas::default(),
        };
        let valid = (|| {
            let branch = if let Some(test) = test {
                check.test(test)?;
                Some(check.branch()?)
            } else {
                None
            };
            for step in steps {
                check.step(step)?;
            }
            check.exit(false)?;
            if let Some(to) = branch {
                if to != check.pc {
                    return None;
                }
                check.exit(true)?;
            }
            (check.pc == self.code.len()).then_some(())
        })();
        valid.is_some()
    }
}

impl<T: Domain> Verify<'_, T> {
    fn next(&mut self) -> Option<&Instruction<T>> {
        let instruction = self.program.code.get(self.pc)?;
        self.pc += 1;
        Some(instruction)
    }

    fn same(&self, actual: Arg<T>, expected: Arg<T>) -> Option<()> {
        let valid = match (actual, expected) {
            (Arg::Reg(a), Arg::Reg(b)) => {
                a == b
                    && usize::from(a) < self.program.slots.len()
                    && self.initialized & (1 << a) != 0
            }
            (Arg::Const(a), Arg::Const(b)) => a.same(b),
            (Arg::Temp(a), Arg::Temp(b)) => a == b && a < self.temps,
            _ => false,
        };
        valid.then_some(())
    }

    fn kernel(&mut self, index: usize, lib: LibId, checked: bool, comparison: bool) -> Option<()> {
        let actual = self.program.kernels.get(index)?;
        let (op, table) = Builder::<T>::new(self.exe, self.program.charged).kernel(lib, checked)?;
        if actual.op != op || actual.table != table || op.compares() != comparison {
            return None;
        }
        self.formulas.count += 1;
        self.formulas.units = self.formulas.units.checked_add(table[0])?;
        Some(())
    }

    fn operand(&mut self, expr: &Expr) -> Option<Arg<T>> {
        match &expr.kind {
            Kind::Local {
                local,
                scoped: false,
                ..
            } => Some(Arg::Reg(
                u8::try_from(self.program.slots.iter().position(|at| *at == local.at)?).ok()?,
            )),
            Kind::Const(value) => Some(Arg::Const(T::of(value)?)),
            Kind::Operator {
                lib, args, invoke, ..
            } => {
                let [a, b] = &**args else {
                    return None;
                };
                let a = self.operand(a)?;
                let b = self.operand(b)?;
                let Instruction::Number {
                    kernel,
                    a: x,
                    b: y,
                    dst,
                } = *self.next()?
                else {
                    return None;
                };
                self.same(x, a)?;
                self.same(y, b)?;
                self.kernel(
                    kernel,
                    *lib,
                    invoke.is_some_and(|invoke| invoke.checked),
                    false,
                )?;
                if dst != self.temps || dst >= self.program.temps {
                    return None;
                }
                self.temps += 1;
                Some(Arg::Temp(dst))
            }
            _ => None,
        }
    }

    fn commit(&mut self, summary: &Summary) -> Option<()> {
        let builder = Builder::<T>::new(self.exe, self.program.charged);
        let expected = program(summary)?;
        let units = builder.units(&expected, &self.formulas)?;
        let formulas = self.formulas.count;
        let Instruction::Commit {
            at,
            units: actual,
            formulas: count,
            program,
        } = self.next()?
        else {
            return None;
        };
        if *at != summary.ops.start
            || *actual != units
            || *count != formulas
            || **program != *expected
        {
            return None;
        }
        self.formulas = Formulas::default();
        self.temps = 0;
        Some(())
    }

    fn test(&mut self, summary: &Summary) -> Option<()> {
        let Kind::Operator {
            lib, args, invoke, ..
        } = &root(summary)?.kind
        else {
            return None;
        };
        let [a, b] = &**args else {
            return None;
        };
        let a = self.operand(a)?;
        let b = self.operand(b)?;
        let Instruction::Compare { kernel, a: x, b: y } = *self.next()? else {
            return None;
        };
        self.same(x, a)?;
        self.same(y, b)?;
        self.kernel(
            kernel,
            *lib,
            invoke.is_some_and(|invoke| invoke.checked),
            true,
        )?;
        self.commit(summary)
    }

    fn branch(&mut self) -> Option<usize> {
        let Instruction::Branch { otherwise } = *self.next()? else {
            return None;
        };
        (otherwise > self.pc - 1 && otherwise < self.program.code.len()).then_some(otherwise)
    }

    fn exit(&mut self, expected: bool) -> Option<()> {
        let Instruction::Exit { ended } = self.next()? else {
            return None;
        };
        (*ended == expected).then_some(())
    }

    fn step(&mut self, step: &Step) -> Option<()> {
        match step {
            Step::Write(summary) | Step::Bind(summary) => {
                let value = self.operand(root(summary)?)?;
                let Instruction::Set { value: actual, dst } = *self.next()? else {
                    return None;
                };
                self.same(actual, value)?;
                let Some(RegionSlot::Frame(at)) = summary.write else {
                    return None;
                };
                if self.program.slots.get(usize::from(dst)) != Some(&at) {
                    return None;
                }
                if matches!(step, Step::Write(_))
                    && !self.program.loads.iter().any(|(slot, _)| *slot == at)
                    && !self.program.held.contains(&at)
                {
                    return None;
                }
                self.initialized |= 1 << dst;
                self.commit(summary)
            }
            Step::If {
                test,
                then,
                otherwise,
            } if then.declares.is_empty() && otherwise.declares.is_empty() => {
                self.test(test)?;
                let target = self.branch()?;
                let before = self.initialized;
                for step in &then.steps {
                    self.step(step)?;
                }
                let assigned = self.initialized;
                let Instruction::Jump { to } = *self.next()? else {
                    return None;
                };
                if target != self.pc || to < self.pc || to >= self.program.code.len() {
                    return None;
                }
                self.initialized = before;
                for step in &otherwise.steps {
                    self.step(step)?;
                }
                self.initialized &= assigned;
                (to == self.pc).then_some(())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
impl super::LoopPlan {
    /// Semantic adversaries for the private verifier, called by the body-tree law.
    pub(crate) fn assert_flat_verified(&self, exe: &Executable) {
        let super::Head::While(test) = &self.head else {
            panic!("a while plan")
        };
        let (program, covers) = Builder::<i64>::new(exe, true)
            .program(Some(test), &self.body, 0)
            .expect("the scalar plan forms a typed program");
        assert_eq!(covers, self.body.steps.len());
        let verify =
            |program: &Program<i64>| program.verify(exe, Some(test), &self.body, 0, covers);
        assert!(verify(&program));
        for at in 0..program.code.len() {
            let mut bad = program.clone();
            match &mut bad.code[at] {
                Instruction::Number { a, .. } => *a = Arg::Temp(program.temps),
                Instruction::Compare { b, .. } => *b = Arg::Reg(REGISTERS as u8),
                Instruction::Set { dst, .. } => *dst = REGISTERS as u8,
                Instruction::Branch { otherwise } => *otherwise = at,
                Instruction::Jump { to } => *to = program.code.len(),
                Instruction::Commit { at, .. } => *at = at.saturating_add(1),
                Instruction::Exit { ended } => *ended = !*ended,
            }
            assert!(!verify(&bad), "malformed instruction {at}");
        }
        let mut bad = program.clone();
        bad.loads = Box::default();
        assert!(!verify(&bad), "reads must be initialized");
        let mut bad = program.clone();
        let commit = bad
            .code
            .iter_mut()
            .find_map(|op| match op {
                Instruction::Commit { program, .. } if program.len() > 1 => Some(program),
                _ => None,
            })
            .unwrap();
        commit.swap(0, commit.len() - 1);
        assert!(
            !verify(&bad),
            "event order must match the lowered statement"
        );
        assert!(!program.verify(exe, Some(test), &self.body, 0, covers - 1));
    }
}

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
