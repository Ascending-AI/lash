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
//! made canonical there. Each operator is a closure specialized to its
//! operands' kinds, as the fused nodes are, over the registers.
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
    pub(crate) fn repeat<const RECORD: bool>(
        &self,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
        headroom: Headroom,
    ) -> (u64, u64, Ran) {
        let mut repeated = None;
        if let Some(floats) = &self.floats {
            repeated = floats.repeat::<RECORD>(frame, pending, headroom);
        }
        if let (None, Some(ints)) = (&repeated, &self.ints) {
            repeated = ints.repeat::<RECORD>(frame, pending, headroom);
        }
        repeated.unwrap_or((0, 0, Ran::Missed))
    }

    /// Runs the block on `frame`'s variables: `Done` with its charge,
    /// having written back what it assigned, and with `pending` holding its
    /// charge program when `RECORD`; otherwise having written and charged
    /// nothing.
    #[inline]
    pub(crate) fn run<const RECORD: bool>(
        &self,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
    ) -> Ran {
        let mut ran = Ran::Missed;
        if let Some(floats) = &self.floats {
            ran = floats.run::<RECORD>(frame, pending);
        }
        if let (Ran::Missed, Some(ints)) = (ran, &self.ints) {
            ran = ints.run::<RECORD>(frame, pending);
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
    const MEASURED: bool = true;
}

/// One run of a program: its registers and what it has charged, assigned
/// and, recording, held of its charge program.
struct State<T> {
    regs: [T; REGISTERS],
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

impl<T: Domain> State<T> {
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

/// A compiled typed value: `None` where its kernel leaves its operands to
/// the general path.
type Eval<T> = Box<dyn Fn(&mut State<T>) -> Option<T> + Send + Sync>;

/// A compiled typed comparison.
type Holds<T> = Box<dyn Fn(&mut State<T>) -> Option<bool> + Send + Sync>;

/// An operand of an operator, read in place.
trait Get<T>: Send + Sync + 'static {
    fn get(&self, state: &mut State<T>) -> Option<T>;
}

/// A variable's register.
struct Reg(u8);
/// A constant.
struct Const<T>(T);
/// Another operator's value.
struct Node<T>(Eval<T>);

impl<T: Domain> Get<T> for Reg {
    #[inline(always)]
    fn get(&self, state: &mut State<T>) -> Option<T> {
        Some(state.regs[usize::from(self.0) % REGISTERS])
    }
}

impl<T: Domain> Get<T> for Const<T> {
    #[inline(always)]
    fn get(&self, _: &mut State<T>) -> Option<T> {
        Some(self.0)
    }
}

impl<T: Domain> Get<T> for Node<T> {
    #[inline(always)]
    fn get(&self, state: &mut State<T>) -> Option<T> {
        (self.0)(state)
    }
}

/// An operand as compilation gives it.
enum Arg<T> {
    Reg(u8),
    Const(T),
    Node(Eval<T>),
}

/// What an operator compiles to over its two operands, specialized to
/// their kinds.
trait Over<T: Domain> {
    type Out;
    fn over<A: Get<T>, B: Get<T>>(self, a: A, b: B) -> Self::Out;
}

/// `over`, specialized to the kinds of `a` and `b`.
fn specialize<T: Domain, O: Over<T>>(over: O, a: Arg<T>, b: Arg<T>) -> O::Out {
    match (a, b) {
        (Arg::Reg(a), Arg::Reg(b)) => over.over(Reg(a), Reg(b)),
        (Arg::Reg(a), Arg::Const(b)) => over.over(Reg(a), Const(b)),
        (Arg::Reg(a), Arg::Node(b)) => over.over(Reg(a), Node(b)),
        (Arg::Const(a), Arg::Reg(b)) => over.over(Const(a), Reg(b)),
        (Arg::Const(a), Arg::Const(b)) => over.over(Const(a), Const(b)),
        (Arg::Const(a), Arg::Node(b)) => over.over(Const(a), Node(b)),
        (Arg::Node(a), Arg::Reg(b)) => over.over(Node(a), Reg(b)),
        (Arg::Node(a), Arg::Const(b)) => over.over(Node(a), Const(b)),
        (Arg::Node(a), Arg::Node(b)) => over.over(Node(a), Node(b)),
    }
}

/// An arithmetic operator and its scalar table.
struct Arith {
    op: ScalarOp,
    table: [u64; 16],
}

impl<T: Domain> Over<T> for Arith {
    type Out = Eval<T>;
    fn over<A: Get<T>, B: Get<T>>(self, a: A, b: B) -> Eval<T> {
        let Self { op, table } = self;
        Box::new(move |state| {
            let x = a.get(state)?;
            let y = b.get(state)?;
            let r = T::number(op, x, y)?;
            state.charge(&table, x, y, r.sized());
            Some(r)
        })
    }
}

/// A comparison and its scalar table: a truth's size bit is clear.
struct Compare {
    op: ScalarOp,
    table: [u64; 16],
}

impl<T: Domain> Over<T> for Compare {
    type Out = Holds<T>;
    fn over<A: Get<T>, B: Get<T>>(self, a: A, b: B) -> Holds<T> {
        let Self { op, table } = self;
        Box::new(move |state| {
            let x = a.get(state)?;
            let y = b.get(state)?;
            let holds = T::truth(op, x, y)?;
            state.charge(&table, x, y, 0);
            Some(holds)
        })
    }
}

/// The value of a statement whose value is a variable's or a constant's.
fn leaf<T: Domain>(arg: Arg<T>) -> Eval<T> {
    match arg {
        Arg::Reg(reg) => Box::new(move |state| Reg(reg).get(state)),
        Arg::Const(value) => Box::new(move |_| Some(value)),
        Arg::Node(node) => node,
    }
}

/// A comparison an `if` or a `while` tests, and its statement's charge.
struct Test<T> {
    holds: Holds<T>,
    /// Its statement's fixed units and, in a charged run of floats, its
    /// formulas' units.
    units: u64,
    /// Its formulas, which its program places.
    formulas: usize,
    program: Box<[Event]>,
}

enum Stmt<T> {
    /// A write or a declaration of the variable whose register is `dst`.
    Set {
        value: Eval<T>,
        dst: u8,
        formulas: usize,
        program: Box<[Event]>,
    },
    If {
        test: Test<T>,
        then: Block<T>,
        otherwise: Block<T>,
    },
}

/// Statements on one path: a run, or an arm of an `if` in it.
struct Block<T> {
    stmts: Box<[Stmt<T>]>,
    /// What its statements charge outside their `if`s that no value
    /// decides: their fixed units and, in a charged run of floats, their
    /// formulas'.
    units: u64,
    /// The registers its statements assign outside their `if`s.
    writes: u32,
}

/// A run compiled for domain `T`.
struct Program<T> {
    /// Each variable it reads before it assigns it, and its register.
    loads: Box<[(u32, u8)]>,
    /// The variables it writes that it does not read first: each holds a
    /// value at its entry.
    held: Box<[u32]>,
    /// Each register's variable's frame position.
    slots: Box<[u32]>,
    /// A `while`'s test, which must hold.
    test: Option<Test<T>>,
    block: Block<T>,
    /// Whether its formulas are charged.
    charged: bool,
}

impl<T: Domain> Program<T> {
    /// The run's state before its entry.
    #[inline(always)]
    fn state<const RECORD: bool>(&self) -> State<T> {
        State {
            regs: [T::default(); REGISTERS],
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
        state: &mut State<T>,
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
    fn leave(&self, frame: &mut impl Slots, state: &State<T>, mut written: u32) {
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
    fn repeat<const RECORD: bool>(
        &self,
        frame: &mut impl Slots,
        pending: &mut Vec<Weight>,
        headroom: Headroom,
    ) -> Option<(u64, u64, Ran)> {
        let test = self.test.as_ref()?;
        let mut state = self.state::<RECORD>();
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
            let ran = match test.run(&mut state) {
                Some(true) if self.block.run(&mut state).is_some() => Ran::Done(0),
                Some(false) => Ran::Ended,
                _ => Ran::Missed,
            };
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
    fn run<const RECORD: bool>(&self, frame: &mut impl Slots, pending: &mut Vec<Weight>) -> Ran {
        let mut state = self.state::<RECORD>();
        if self.enter::<RECORD>(&mut state, frame, pending).is_none() {
            return Ran::Missed;
        }
        let ran = match &self.test {
            Some(test) => match test.run(&mut state) {
                Some(true) => Ran::Done(0),
                Some(false) => Ran::Ended,
                None => Ran::Missed,
            },
            None => Ran::Done(0),
        };
        let ran = match ran {
            Ran::Done(_) if self.block.run(&mut state).is_some() => Ran::Done(state.units),
            Ran::Done(_) => Ran::Missed,
            ran => ran,
        };
        if RECORD {
            *pending = std::mem::take(&mut state.pending);
        }
        if let Ran::Done(_) = ran {
            self.leave(frame, &state, state.written);
        }
        ran
    }
}

impl<T: Domain> Test<T> {
    /// Whether the test holds, its statement charged.
    #[inline(always)]
    fn run(&self, state: &mut State<T>) -> Option<bool> {
        state.units += self.units;
        let holds = (self.holds)(state)?;
        if state.record {
            state.recorded(&self.program, self.formulas);
        }
        Some(holds)
    }
}

impl<T: Domain> Block<T> {
    #[inline]
    fn run(&self, state: &mut State<T>) -> Option<()> {
        state.units += self.units;
        state.written |= self.writes;
        for stmt in &*self.stmts {
            match stmt {
                Stmt::Set {
                    value,
                    dst,
                    formulas,
                    program,
                } => {
                    let value = value(state)?;
                    state.regs[usize::from(*dst) % REGISTERS] = value;
                    if state.record {
                        state.recorded(program, *formulas);
                    }
                }
                Stmt::If {
                    test,
                    then,
                    otherwise,
                } => {
                    if test.run(state)? {
                        then.run(state)?;
                    } else {
                        otherwise.run(state)?;
                    }
                }
            }
        }
        Some(())
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
    domain: std::marker::PhantomData<T>,
}

impl<'e, T: Domain> Builder<'e, T> {
    fn new(exe: &'e Executable, charged: bool) -> Self {
        Self {
            exe,
            charged,
            state: Allotted::default(),
            domain: std::marker::PhantomData,
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
        let test = match test {
            Some(test) => Some(self.test(test)?),
            None => None,
        };
        let mut stmts = Vec::new();
        let mut units = 0_u64;
        let mut writes = 0_u32;
        for step in body.steps.get(from..)? {
            // A statement it cannot run ends the run before it, leaving
            // nothing it began to compile.
            let saved = self.state.clone();
            let Some((stmt, charge, dst)) = self.step(step) else {
                self.state = saved;
                break;
            };
            units = units.checked_add(charge)?;
            writes |= dst.map_or(0, |dst| 1 << dst);
            stmts.push(stmt);
        }
        let covers = stmts.len();
        if self.state.operators < OPERATORS {
            return None;
        }
        let Allotted {
            slots, loads, held, ..
        } = self.state;
        Some((
            Program {
                loads: loads.into(),
                held: held.into(),
                slots: slots.into(),
                test,
                block: Block {
                    stmts: stmts.into(),
                    units,
                    writes,
                },
                charged: self.charged,
            },
            covers,
        ))
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

    /// The statement of `step`, a body statement: with what it charges
    /// whatever its values, outside an `if`'s arms, and the register it
    /// assigns there.
    fn step(&mut self, step: &Step) -> Option<(Stmt<T>, u64, Option<u8>)> {
        match step {
            Step::Write(summary) | Step::Bind(summary) => {
                let Some(RegionSlot::Frame(at)) = summary.write else {
                    return None;
                };
                let root = root(summary)?;
                let program = program(summary)?;
                let mut formulas = Formulas::default();
                let value = leaf(self.operand(root, &mut formulas)?);
                let dst = self.assign(at, matches!(step, Step::Write(_)))?;
                let units = self.units(&program, &formulas)?;
                let stmt = Stmt::Set {
                    value,
                    dst,
                    formulas: formulas.count,
                    program,
                };
                Some((stmt, units, Some(dst)))
            }
            // An arm that declares ends its variables: its own run's.
            Step::If {
                test,
                then,
                otherwise,
            } if then.declares.is_empty() && otherwise.declares.is_empty() => {
                let test = self.test(test)?;
                let before = self.state.assigned;
                let then = self.arm(then)?;
                let assigned = self.state.assigned;
                self.state.assigned = before;
                let otherwise = self.arm(otherwise)?;
                self.state.assigned &= assigned;
                let stmt = Stmt::If {
                    test,
                    then,
                    otherwise,
                };
                Some((stmt, 0, None))
            }
            _ => None,
        }
    }

    /// An `if`'s arm, which runs whole.
    fn arm(&mut self, body: &Body) -> Option<Block<T>> {
        let mut stmts = Vec::new();
        let mut units = 0_u64;
        let mut writes = 0_u32;
        for step in &body.steps {
            let (stmt, charge, dst) = self.step(step)?;
            units = units.checked_add(charge)?;
            writes |= dst.map_or(0, |dst| 1 << dst);
            stmts.push(stmt);
        }
        Some(Block {
            stmts: stmts.into(),
            units,
            writes,
        })
    }

    /// The comparison `summary`, a test's, makes.
    fn test(&mut self, summary: &Summary) -> Option<Test<T>> {
        let root = root(summary)?;
        let program = program(summary)?;
        let Kind::Operator {
            lib, args, invoke, ..
        } = &root.kind
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
        Some(Test {
            holds: specialize(Compare { op, table }, a, b),
            units: self.units(&program, &formulas)?,
            formulas: formulas.count,
            program,
        })
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
                Some(Arg::Node(specialize(Arith { op, table }, a, b)))
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
