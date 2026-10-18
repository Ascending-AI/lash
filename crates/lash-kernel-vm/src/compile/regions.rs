//! Region formation at lowering (`docs/design/kernel-regions.md` §1): the
//! cold side table of the spans of guest code whose fast path holds no
//! effect, park, task switch, projection point or slice or bound crossing.
//!
//! A [`LoopPlan`] states, for a `while` or a `for` over a list or a tuple
//! whose body is region statements, its head (the `while`'s test, or the
//! `for`'s scheduled units and its protocol's `item` hook) and its body as
//! a tree: each statement's fixed units, its formulas' maximum, its static
//! charge program, the slots it reads and writes, the `if` arms it enters,
//! and the `continue`s that end an iteration early. It also states the most
//! one iteration charges. [`LoopPlan::verify`] checks the plan against the
//! lowered code, its stream's `RegionTable` events and the statement units
//! before lowering admits it. Plans belong to the executable: deterministic,
//! immutable and never saved. Their compiled form is built on first entry
//! and cached in the plan. A layout bit turns formation off
//! ([`super::Layout::regions`]), which is how the regions law compares the
//! two plans.
//!
//! Region ops in this version: reads of unshared slots, resident scalar
//! constants, prepared operators (`Op::Operator`, or `Op::Nested` of an
//! operator) with a scalar charge table, writes and declarations of
//! unshared slots, an `if`'s branch, and `continue`. A `break` or a
//! `return` is a statement the region leaves to the reference: it stops
//! before it. Everything else is a barrier, which no plan contains.

use std::sync::OnceLock;

use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, OpRange, RegRange, Weight};

use super::formula::ScalarTable;
use super::{
    Block, BlockId, Code, Constant, Entry, Executable, Form, Lib, LibId, NestedKind, Source, Stmt,
    StmtId, Stream,
};
use crate::machine::SlotState;

/// The deepest `if` nesting a loop body's plan holds.
pub(crate) const NESTING: usize = 8;

/// One event of a region statement's static charge program, in the order
/// the reference charges it (`docs/design/kernel-end-state.md` §B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A fixed charge: a statement or loop-test unit, or one event of a
    /// charge region.
    Fixed(Weight),
    /// The next operator's formula, charged whole at its completion.
    Formula,
}

/// What a region statement does with the value its ops compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// A `while`'s test.
    Test,
    /// An assignment of an unshared slot.
    Write,
    /// A declaration of an unshared slot.
    Bind,
    /// An `if`'s branch.
    Branch,
    /// A `continue`: no value.
    Continue,
}

/// A region statement on its one path: a loop's test, the `for`'s head, a
/// body statement, or an `if`'s test.
#[derive(Clone)]
pub(crate) struct Summary {
    /// Its ops, up to the op that ends it.
    ops: OpRange,
    /// The fixed units it charges: its statement or test units and the
    /// totals its charge regions' heads state.
    pub(crate) fixed: u64,
    /// The most its formulas charge: each operator site's table maximum.
    pub(crate) formula_max: u64,
    /// What it charges, in order.
    pub(crate) program: Box<[Event]>,
    /// The frame positions it reads.
    pub(crate) reads: Box<[u32]>,
    /// The frame position it writes or declares.
    pub(crate) write: Option<u32>,
    /// Invoke occurrences committed only when this statement succeeds.
    calls: Box<[usize]>,
    /// A statement-root invoke keeps its original action and resume form.
    action: Option<StmtId>,
}

impl Summary {
    /// The most the statement charges.
    fn max(&self) -> Option<u64> {
        self.fixed.checked_add(self.formula_max)
    }
}

/// A statement of a loop body's plan.
#[derive(Clone)]
pub(crate) enum Step {
    /// `set` of an unshared slot.
    Write(Summary),
    /// `let` of an unshared slot its block declares.
    Bind(Summary),
    /// An `if`: its test, which ends in its branch, and its two arms.
    If {
        test: Summary,
        then: Body,
        otherwise: Body,
    },
    /// `continue`: its unit, then the next iteration.
    Continue(Summary),
    /// A `break` or a `return`, which the reference runs: the region stops
    /// before it.
    Leave,
}

/// A block of a loop body's plan: the loop's body, or an `if` arm.
#[derive(Clone)]
pub(crate) struct Body {
    pub(crate) block: BlockId,
    pub(crate) steps: Box<[Step]>,
    /// The variables it declares, which end with it.
    pub(crate) declares: Box<[u32]>,
}

impl Body {
    fn calls(&self, calls: &mut Vec<usize>) {
        for step in &self.steps {
            match step {
                Step::Write(summary) | Step::Bind(summary) | Step::Continue(summary) => {
                    calls.extend_from_slice(&summary.calls);
                }
                Step::If {
                    test,
                    then,
                    otherwise,
                } => {
                    calls.extend_from_slice(&test.calls);
                    then.calls(calls);
                    otherwise.calls(calls);
                }
                Step::Leave => {}
            }
        }
    }

    /// The most one pass through the block charges: every statement's
    /// most, and an `if`'s test and its dearer arm.
    fn max(&self) -> Option<u64> {
        self.steps.iter().try_fold(0_u64, |sum, step| {
            let most = match step {
                Step::Write(summary) | Step::Bind(summary) | Step::Continue(summary) => {
                    summary.max()?
                }
                Step::If {
                    test,
                    then,
                    otherwise,
                } => test.max()?.checked_add(then.max()?.max(otherwise.max()?))?,
                Step::Leave => 0,
            };
            sum.checked_add(most)
        })
    }

    /// Whether a statement of the block or an arm in it declares.
    fn binds(&self) -> bool {
        self.steps.iter().any(|step| match step {
            Step::Bind(_) => true,
            Step::If {
                then, otherwise, ..
            } => then.binds() || otherwise.binds(),
            _ => false,
        })
    }
}

/// How a loop region starts each iteration.
#[derive(Clone)]
pub(crate) enum Head {
    /// A `while`'s test, which an iteration runs when it holds.
    While(Summary),
    /// A `for` over a list or a tuple, stepped by the loop's own cursor:
    /// its scheduled units and its `item` hook's formula, in `charge`
    /// (no ops), the variable each element is bound to, and the hook.
    For {
        charge: Summary,
        binding: u32,
        item: Option<LibId>,
    },
}

/// A `while` or a `for` loop that runs as a batch of whole iterations.
pub(crate) struct LoopPlan {
    pub(crate) stmt: StmtId,
    pub(crate) head: Head,
    pub(crate) body: Body,
    /// The loop-test unit an iteration charges last, for the next test:
    /// a region is entered from a test whose unit is charged.
    pub(crate) loop_test: u64,
    /// The most one iteration charges: the head's, the body's dearest
    /// path's, and the next test's unit.
    pub(crate) iteration_max: u64,
    /// Whether an iteration declares a variable: a `for`'s element, or a
    /// `let` of its body. Its memory is held as the reference holds it.
    pub(crate) binds: bool,
    /// The call depth its sites need beyond the frame's own, when one
    /// checks it (`K-BND-002`): an operation's admission, or a helper
    /// template's inline depth.
    pub(crate) depth: Option<u32>,
    /// Its invoke sites' occurrence indexes (`K-EFF-008`), in source order.
    pub(crate) calls: Box<[usize]>,
    compiled: OnceLock<Option<CompiledLoop>>,
}

/// Why lowering does not admit a formed plan.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// A summary's fixed units are not its program's fixed events.
    Fixed { statement: usize },
    /// A charge region's head states a total its events do not sum to.
    Region { region: u32 },
    /// A program's events are not the stream's, in order.
    Program { statement: usize },
    /// The iteration maximum is not the head's, the dearest path's and the
    /// loop-test unit's sum.
    Iteration,
    /// A summary's reads or write are not the slots its ops read and
    /// write.
    Slots { statement: usize },
    /// The call sites are not the occurrences of the calls its ops make.
    Calls,
    /// The loop-test unit is not the cost table's.
    LoopTest,
    /// The plan's statements, arms or declarations are not the lowered
    /// code's, or its head is not its loop's.
    Shape { statement: usize },
}

/// The lowered code a plan is formed and checked against: its stream, and
/// its blocks and statements by id.
pub(crate) struct Lowered<'a> {
    pub(crate) stream: &'a Stream,
    pub(crate) block: &'a dyn Fn(BlockId) -> &'a Block,
    pub(crate) stmt: &'a dyn Fn(StmtId) -> &'a Stmt,
}

/// What a compiled expression charges as it runs.
pub(crate) struct Meter {
    /// Its formulas' units.
    pub(crate) units: u64,
    /// Each formula's units, in order, when a charge tape records them.
    pub(crate) formulas: Vec<u64>,
    record: bool,
}

impl Meter {
    pub(crate) fn new(record: bool) -> Self {
        Self {
            units: 0,
            formulas: Vec::new(),
            record,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.units = 0;
        self.formulas.clear();
    }

    #[inline(always)]
    fn formula(&mut self, units: u64) {
        self.units += units;
        if self.record {
            self.formulas.push(units);
        }
    }
}

/// A compiled region expression: it reads the frame's slots, meters its
/// formulas and answers its value, or `None` on a guard miss, having
/// changed nothing a run sees.
pub(crate) type Node = Box<dyn Fn(&[SlotState], &mut Meter) -> Option<Value> + Send + Sync>;

/// A compiled `item` hook: what the loop walks and the stepped item to the
/// value bound, metered, or `None` on a guard miss.
pub(crate) type Hook = Box<dyn Fn(Value, Value, &mut Meter) -> Option<Value> + Send + Sync>;

/// A loop plan's compiled form: its head and its body as trees that hold
/// what each statement charges, so that an iteration reads nothing else.
pub(crate) struct CompiledLoop {
    pub(crate) head: CompiledHead,
    pub(crate) body: CompiledBody,
}

/// What a compiled statement commits: its fixed units and its static
/// charge program (§3.1).
pub(crate) struct Commit {
    pub(crate) fixed: u64,
    pub(crate) program: Box<[Event]>,
    pub(crate) calls: Box<[usize]>,
}

impl From<&Summary> for Commit {
    fn from(summary: &Summary) -> Self {
        Self {
            fixed: summary.fixed,
            program: summary.program.clone(),
            calls: summary.calls.clone(),
        }
    }
}

pub(crate) enum CompiledHead {
    While {
        test: Node,
        commit: Commit,
    },
    For {
        hook: Option<Hook>,
        commit: Commit,
        binding: u32,
    },
}

/// A block's compiled statements, and the variables that end with it.
pub(crate) struct CompiledBody {
    pub(crate) block: BlockId,
    pub(crate) declares: Box<[u32]>,
    pub(crate) steps: Box<[CompiledStep]>,
}

pub(crate) enum CompiledStep {
    Write {
        value: Node,
        at: u32,
        commit: Commit,
    },
    Bind {
        value: Node,
        at: u32,
        commit: Commit,
    },
    If {
        condition: Node,
        commit: Commit,
        then: CompiledBody,
        otherwise: CompiledBody,
    },
    Continue(Commit),
    Leave,
}

/// The values a region holds in registers and slots.
#[inline(always)]
pub(crate) fn scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
    )
}

/// The libraries and the lowered code a plan is formed against.
pub(super) struct Formation<'a> {
    pub(super) lowered: Lowered<'a>,
    pub(super) libs: &'a [Lib],
    /// The cost table's loop test.
    pub(super) loop_test: u64,
}

impl Formation<'_> {
    /// The plan of loop statement `id` of `code`; `None` when it is no
    /// loop region.
    pub(super) fn form(&self, code: &Code, id: StmtId) -> Option<LoopPlan> {
        if !code.charging {
            return None;
        }
        let statement = (self.lowered.stmt)(id);
        let mut depth = None;
        let (head, block) = match &statement.form {
            Form::While {
                metered,
                test,
                body,
                ..
            } => {
                let units = vec![Event::Fixed(Weight::Atomic(*metered))];
                let test = self.summarize(code, *test, End::Test, *metered, units, &mut depth)?;
                (Head::While(test), *body)
            }
            Form::For {
                metered,
                binding,
                via,
                body,
                ..
            } => {
                if binding.shared || code.slots.get(binding.slot as usize)?.shared {
                    return None;
                }
                let item = via.and_then(|via| via.item);
                let mut program = vec![Event::Fixed(Weight::Atomic(*metered))];
                let mut formula_max = 0;
                if let Some(item) = item {
                    let mut defined = vec![true, true];
                    self.site(item, 0, 2, &mut defined, &mut formula_max, &mut depth)?;
                    program.push(Event::Formula);
                }
                let charge = Summary {
                    ops: OpRange { start: 0, len: 0 },
                    fixed: *metered,
                    formula_max,
                    program: lean(program),
                    reads: Box::default(),
                    write: Some(binding.at),
                    calls: Box::default(),
                    action: None,
                };
                let head = Head::For {
                    charge,
                    binding: binding.at,
                    item,
                };
                (head, *body)
            }
            _ => return None,
        };
        let body = self.body(code, block, &mut depth, 0)?;
        if body.steps.is_empty() {
            return None;
        }
        let head_max = match &head {
            Head::While(test) => test.max()?,
            Head::For { charge, .. } => charge.max()?,
        };
        let iteration_max = head_max
            .checked_add(body.max()?)?
            .checked_add(self.loop_test)?;
        let binds = matches!(head, Head::For { .. }) || body.binds();
        let mut calls = match &head {
            Head::While(test) => test.calls.to_vec(),
            Head::For { .. } => Vec::new(),
        };
        body.calls(&mut calls);
        Some(LoopPlan {
            stmt: id,
            head,
            body,
            loop_test: self.loop_test,
            iteration_max,
            binds,
            depth,
            calls: calls.into(),
            compiled: OnceLock::new(),
        })
    }

    /// The plan of block `block`, `nesting` `if`s deep in a loop's body.
    fn body(
        &self,
        code: &Code,
        block: BlockId,
        depth: &mut Option<u32>,
        nesting: usize,
    ) -> Option<Body> {
        if nesting > NESTING {
            return None;
        }
        let lowered = (self.lowered.block)(block);
        let steps = lowered
            .stmts
            .iter()
            .map(|id| {
                let statement = (self.lowered.stmt)(*id);
                let unit = || vec![Event::Fixed(Weight::Atomic(1))];
                match &statement.form {
                    Form::Plain => {
                        let last = self.stream_op(statement.ops.end().checked_sub(1)?)?;
                        let end = match last {
                            Op::WriteLocal { .. } => End::Write,
                            Op::Bind { .. } => End::Bind,
                            Op::Continue => End::Continue,
                            Op::Break | Op::Return { .. } => return Some(Step::Leave),
                            _ => return None,
                        };
                        let summary = self.summarize(code, statement.ops, end, 1, unit(), depth)?;
                        Some(match end {
                            End::Write => Step::Write(summary),
                            End::Bind => Step::Bind(summary),
                            _ => Step::Continue(summary),
                        })
                    }
                    Form::Action(_) => self.action_summary(code, *id, depth),
                    Form::If {
                        then_block,
                        else_block,
                    } => Some(Step::If {
                        test: self.summarize(code, statement.ops, End::Branch, 1, unit(), depth)?,
                        then: self.body(code, *then_block, depth, nesting + 1)?,
                        otherwise: self.body(code, *else_block, depth, nesting + 1)?,
                    }),
                    _ => None,
                }
            })
            .collect::<Option<Box<[Step]>>>()?;
        Some(Body {
            block,
            steps,
            declares: lowered.declares.clone().into(),
        })
    }

    /// A root invoke has atom charge events rather than expression ops.
    /// Keep its action form so a decline resumes the original continuation.
    fn action_summary(&self, code: &Code, id: StmtId, depth: &mut Option<u32>) -> Option<Step> {
        let statement = (self.lowered.stmt)(id);
        let (action, lib, args) = library_invoke(&statement.form)?;
        let ops = self
            .lowered
            .stream
            .ops
            .get(statement.ops.start as usize..statement.ops.end() as usize)?;
        let [Op::Act, end] = ops else {
            return None;
        };
        let (local, bind) = match end {
            Op::WriteLocal { local, .. } => (local, false),
            Op::Bind { local, .. } => (local, true),
            _ => return None,
        };
        if code.slots.get(local.slot as usize)?.shared {
            return None;
        }
        let mut reads = Vec::new();
        for arg in args {
            match arg {
                super::Atom::Var(super::Var::Local(local)) if !local.shared => reads.push(local.at),
                super::Atom::Literal(Constant::Resident(value)) if scalar(value) => {}
                _ => return None,
            }
        }
        let mut defined = vec![true; args.len()];
        let mut formula_max = 0;
        self.site(lib, 0, args.len(), &mut defined, &mut formula_max, depth)?;
        let mut program = vec![Event::Fixed(Weight::Atomic(1))];
        program.extend(args.iter().map(|_| Event::Fixed(Weight::Atomic(1))));
        program.push(Event::Formula);
        let calls = if matches!(action.site.unit, lash_kernel_doc::Unit::Library(_)) {
            Box::default()
        } else {
            vec![action.occurrence].into_boxed_slice()
        };
        let summary = Summary {
            ops: statement.ops,
            fixed: 1 + args.len() as u64,
            formula_max,
            program: program.into(),
            reads: reads.into(),
            write: Some(local.at),
            calls,
            action: Some(id),
        };
        Some(if bind {
            Step::Bind(summary)
        } else {
            Step::Write(summary)
        })
    }

    fn stream_op(&self, at: u32) -> Option<Op> {
        self.lowered.stream.ops.get(at as usize).copied()
    }

    /// The summary of the region statement in `range`, which ends as
    /// `end`, charged `fixed` units and `program` before its ops.
    fn summarize(
        &self,
        code: &Code,
        range: OpRange,
        end: End,
        mut fixed: u64,
        mut program: Vec<Event>,
        depth: &mut Option<u32>,
    ) -> Option<Summary> {
        let stream = self.lowered.stream;
        let ops = stream.ops.get(range.start as usize..range.end() as usize)?;
        let (last, body) = ops.split_last()?;
        // Which registers hold a value the statement has not yet used.
        let mut defined: Vec<bool> = Vec::new();
        let mut formula_max = 0_u64;
        let mut reads = Vec::new();
        let mut calls = Vec::new();
        for op in body {
            match *op {
                Op::Region { region, total } => {
                    if total == u32::MAX {
                        return None;
                    }
                    fixed = fixed.checked_add(u64::from(total))?;
                    let events = stream.regions.events(region.0);
                    program.extend(events.iter().map(|event| Event::Fixed(event.weight)));
                }
                Op::Local { dst, local } => {
                    if code.slots.get(local.slot as usize)?.shared
                        || !define(&mut defined, dst.ix())
                    {
                        return None;
                    }
                    reads.push(local.at);
                }
                Op::Const { dst, c } => match stream.consts.get(c.ix())? {
                    Constant::Resident(value)
                        if scalar(value) && define(&mut defined, dst.ix()) => {}
                    _ => return None,
                },
                Op::Operator { lib, args } => {
                    let (start, count) = (args.start.ix(), args.len as usize);
                    self.site(
                        LibId(lib.0),
                        start,
                        count,
                        &mut defined,
                        &mut formula_max,
                        depth,
                    )?;
                    program.push(Event::Formula);
                }
                Op::Nested { act, args } => {
                    let nested = stream.nested.get(act.ix())?;
                    let lib = nested.kind.operator_lib()?;
                    if !nested.operator {
                        return None;
                    }
                    let (start, count) = (args.start.ix(), args.len as usize);
                    self.site(lib, start, count, &mut defined, &mut formula_max, depth)?;
                    if matches!(nested.kind, NestedKind::Call(_))
                        && !matches!(nested.site.unit, lash_kernel_doc::Unit::Library(_))
                    {
                        calls.push(nested.occurrence);
                    }
                    program.push(Event::Formula);
                }
                _ => return None,
            }
        }
        let unshared = |slot: u32| code.slots.get(slot as usize).is_some_and(|s| !s.shared);
        let write = match (*last, end) {
            (Op::Test { cond }, End::Test) | (Op::Branch { cond, .. }, End::Branch)
                if take(&mut defined, cond.ix()) =>
            {
                None
            }
            (Op::WriteLocal { local, src }, End::Write) | (Op::Bind { local, src }, End::Bind)
                if unshared(local.slot) && take(&mut defined, src.ix()) =>
            {
                Some(local.at)
            }
            (Op::Continue, End::Continue) if reads.is_empty() && formula_max == 0 => None,
            _ => return None,
        };
        // Every value the statement computes reaches its end.
        if defined.iter().any(|held| *held) {
            return None;
        }
        Some(Summary {
            ops: range,
            fixed,
            formula_max,
            program: lean(program),
            reads: reads.into(),
            write,
            calls: calls.into(),
            action: None,
        })
    }

    /// Operator `lib` called on the `count` registers from `start`, which a
    /// region runs when its prepared entry has a scalar charge table and
    /// its operands are the values the statement computed last: its result
    /// replaces them, and its table's maximum adds to `formula_max`.
    fn site(
        &self,
        lib: LibId,
        start: usize,
        count: usize,
        defined: &mut Vec<bool>,
        formula_max: &mut u64,
        depth: &mut Option<u32>,
    ) -> Option<()> {
        let function = self.libs.get(lib.0 as usize)?;
        let (_, fast) = function.prepared.as_ref()?;
        if function.arity != count || !(1..=2).contains(&count) {
            return None;
        }
        // An operation whose body shares a parameter makes its cell at
        // entry: an allocation, which no region makes.
        if function.operation() && function.shares_params {
            return None;
        }
        if !(start..start + count).all(|at| take(defined, at)) || !define(defined, start) {
            return None;
        }
        *formula_max = formula_max.checked_add(fast.scalar()?.max())?;
        let (entry, _) = function.prepared.as_ref()?;
        if function.operation() || entry.depth() > 0 {
            *depth = Some(depth.unwrap_or(0).max(entry.depth()));
        }
        Some(())
    }
}

/// `program` without the zero-unit events the reference never records.
fn lean(program: Vec<Event>) -> Box<[Event]> {
    program
        .into_iter()
        .filter(|event| *event != Event::Fixed(Weight::Atomic(0)))
        .collect()
}

/// Register `at` gets a value; `false` when it holds one still unused.
fn define(defined: &mut Vec<bool>, at: usize) -> bool {
    if defined.len() <= at {
        defined.resize(at + 1, false);
    }
    !std::mem::replace(&mut defined[at], true)
}

/// Register `at`'s value is used; `false` when it holds none.
fn take(defined: &mut [bool], at: usize) -> bool {
    defined
        .get_mut(at)
        .is_some_and(|held| std::mem::replace(held, false))
}

/// The plan's check against the lowered code: the summaries in the order
/// [`LoopPlan::verify`] numbers them, and the calls their ops make.
struct Check<'a, 'l> {
    lowered: &'a Lowered<'l>,
    statement: usize,
    calls: Vec<usize>,
}

impl Check<'_, '_> {
    /// Checks `summary`, the next statement, charged `head` before its ops
    /// and ending as `end`.
    fn summary(&mut self, summary: &Summary, head: Weight, end: End) -> Result<(), Refused> {
        let index = self.statement;
        self.statement += 1;
        let stream = self.lowered.stream;
        let mut expected = vec![Event::Fixed(head)];
        let mut calls = Vec::new();
        let (mut reads, mut write) = (Vec::new(), None);
        let ops = stream
            .ops
            .get(summary.ops.start as usize..summary.ops.end() as usize)
            .ok_or(Refused::Program { statement: index })?;
        let ended = matches!(
            (ops.last(), end),
            (Some(Op::Test { .. }), End::Test)
                | (Some(Op::Branch { .. }), End::Branch)
                | (Some(Op::WriteLocal { .. }), End::Write)
                | (Some(Op::Bind { .. }), End::Bind)
                | (Some(Op::Continue), End::Continue)
        );
        if !ended {
            return Err(Refused::Shape { statement: index });
        }
        for op in ops {
            match *op {
                Op::Region { region, total } => {
                    let stated = stream
                        .regions
                        .summary(region.0)
                        .map_err(|_| Refused::Region { region: region.0 })?;
                    if u64::from(total) != stated.total() {
                        return Err(Refused::Region { region: region.0 });
                    }
                    expected.extend(
                        stream
                            .regions
                            .events(region.0)
                            .iter()
                            .map(|event| Event::Fixed(event.weight)),
                    );
                }
                Op::Act => {
                    let id = summary.action.ok_or(Refused::Shape { statement: index })?;
                    let (action, _, args) = library_invoke(&(self.lowered.stmt)(id).form)
                        .ok_or(Refused::Shape { statement: index })?;
                    for arg in args {
                        if let super::Atom::Var(super::Var::Local(local)) = arg {
                            reads.push(local.at);
                        }
                        expected.push(Event::Fixed(Weight::Atomic(1)));
                    }
                    if !matches!(action.site.unit, lash_kernel_doc::Unit::Library(_)) {
                        calls.push(action.occurrence);
                    }
                    expected.push(Event::Formula);
                }
                Op::Operator { .. } => expected.push(Event::Formula),
                Op::Nested { act, .. } => {
                    if let Some(nested) = stream.nested.get(act.ix())
                        && matches!(nested.kind, NestedKind::Call(_))
                        && !matches!(nested.site.unit, lash_kernel_doc::Unit::Library(_))
                    {
                        calls.push(nested.occurrence);
                    }
                    expected.push(Event::Formula);
                }
                Op::Local { local, .. } => reads.push(local.at),
                Op::WriteLocal { local, .. } | Op::Bind { local, .. } => write = Some(local.at),
                _ => {}
            }
        }
        if *summary.reads != *reads || summary.write != write {
            return Err(Refused::Slots { statement: index });
        }
        if *summary.calls != *calls {
            return Err(Refused::Calls);
        }
        self.calls.extend(calls);
        self.program(summary, expected, index)
    }

    /// Checks that `summary`'s program is `expected` and that its fixed
    /// units are that program's.
    fn program(
        &self,
        summary: &Summary,
        expected: Vec<Event>,
        index: usize,
    ) -> Result<(), Refused> {
        if *summary.program != *lean(expected) {
            return Err(Refused::Program { statement: index });
        }
        let fixed = summary
            .program
            .iter()
            .map(|event| match event {
                Event::Fixed(weight) => weight.units(),
                Event::Formula => 0,
            })
            .sum::<u64>();
        if fixed != summary.fixed {
            return Err(Refused::Fixed { statement: index });
        }
        Ok(())
    }

    /// Checks `body` against block `block`'s statements and declarations.
    fn body(&mut self, body: &Body, block: BlockId) -> Result<(), Refused> {
        let lowered = (self.lowered.block)(block);
        let shape = Refused::Shape {
            statement: self.statement,
        };
        if body.block != block
            || *body.declares != *lowered.declares
            || body.steps.len() != lowered.stmts.len()
        {
            return Err(shape);
        }
        for (step, id) in body.steps.iter().zip(&lowered.stmts) {
            let statement = (self.lowered.stmt)(*id);
            let last = statement
                .ops
                .end()
                .checked_sub(1)
                .and_then(|at| self.lowered.stream.ops.get(at as usize));
            // Each statement's summary covers exactly its own ops.
            let own = match step {
                Step::Write(summary) | Step::Bind(summary) | Step::Continue(summary) => {
                    summary.ops == statement.ops
                        && summary.action
                            == matches!(statement.form, Form::Action(_)).then_some(*id)
                }
                Step::If { test, .. } => test.ops == statement.ops,
                Step::Leave => true,
            };
            if !own {
                return Err(Refused::Shape {
                    statement: self.statement,
                });
            }
            let unit = Weight::Atomic(1);
            match (step, &statement.form) {
                (Step::Write(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Write)?
                }
                (Step::Bind(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Bind)?
                }
                (Step::Continue(summary), Form::Plain) => {
                    self.summary(summary, unit, End::Continue)?;
                }
                (Step::Leave, Form::Plain)
                    if matches!(last, Some(Op::Break | Op::Return { .. })) => {}
                (
                    Step::If {
                        test,
                        then,
                        otherwise,
                    },
                    Form::If {
                        then_block,
                        else_block,
                    },
                ) => {
                    self.summary(test, unit, End::Branch)?;
                    self.body(then, *then_block)?;
                    self.body(otherwise, *else_block)?;
                }
                _ => {
                    return Err(Refused::Shape {
                        statement: self.statement,
                    });
                }
            }
        }
        Ok(())
    }
}

impl LoopPlan {
    /// Checks the plan against `lowered` and the cost table's `loop_test`:
    /// its head is its loop's, its body's statements, arms and
    /// declarations are the blocks', each summary's fixed units are its
    /// program's fixed events, every region's head states the total its
    /// `RegionSummary` gives, the programs are the stream's events in
    /// order, and the iteration maximum is the head's, the dearest path's
    /// and the loop-test unit's sum. Summaries are numbered in order: the
    /// head 0, then the body's statements depth first.
    pub(crate) fn verify(&self, lowered: &Lowered<'_>, loop_test: u64) -> Result<(), Refused> {
        if self.loop_test != loop_test {
            return Err(Refused::LoopTest);
        }
        let mut check = Check {
            lowered,
            statement: 0,
            calls: Vec::new(),
        };
        let statement = (lowered.stmt)(self.stmt);
        let head = Refused::Shape { statement: 0 };
        let (block, binds) = match (&self.head, &statement.form) {
            (
                Head::While(test),
                Form::While {
                    metered,
                    test: ops,
                    body,
                    ..
                },
            ) => {
                if test.ops != *ops {
                    return Err(head);
                }
                check.summary(test, Weight::Atomic(*metered), End::Test)?;
                (*body, false)
            }
            (
                Head::For {
                    charge,
                    binding,
                    item,
                },
                Form::For {
                    metered,
                    binding: bound,
                    via,
                    body,
                    ..
                },
            ) => {
                if *binding != bound.at
                    || bound.shared
                    || *item != via.and_then(|via| via.item)
                    || charge.write != Some(bound.at)
                    || !charge.reads.is_empty()
                {
                    return Err(head);
                }
                let mut expected = vec![Event::Fixed(Weight::Atomic(*metered))];
                expected.extend(item.map(|_| Event::Formula));
                check.statement += 1;
                check.program(charge, expected, 0)?;
                (*body, true)
            }
            _ => return Err(head),
        };
        check.body(&self.body, block)?;
        if self.binds != (binds || self.body.binds()) {
            return Err(Refused::Shape { statement: 0 });
        }
        let head_max = match &self.head {
            Head::While(test) => test.max(),
            Head::For { charge, .. } => charge.max(),
        };
        let iteration = head_max
            .zip(self.body.max())
            .and_then(|(head, body)| head.checked_add(body)?.checked_add(loop_test));
        if iteration != Some(self.iteration_max) {
            return Err(Refused::Iteration);
        }
        if *self.calls != *check.calls {
            return Err(Refused::Calls);
        }
        Ok(())
    }

    /// The plan as stated, uncompiled: what a law misstates.
    #[cfg(test)]
    pub(crate) fn copied(&self) -> Self {
        Self {
            stmt: self.stmt,
            head: self.head.clone(),
            body: self.body.clone(),
            loop_test: self.loop_test,
            iteration_max: self.iteration_max,
            binds: self.binds,
            depth: self.depth,
            calls: self.calls.clone(),
            compiled: OnceLock::new(),
        }
    }

    /// The plan's compiled form, built on its first entry.
    pub(crate) fn compiled(&self, exe: &Executable) -> Option<&CompiledLoop> {
        self.compiled
            .get_or_init(|| {
                let (stream, _, _) = exe.statement(self.stmt);
                let lib = |id: LibId| exe.lib(id);
                let head = match &self.head {
                    Head::While(test) => CompiledHead::While {
                        test: compile(exe, stream, &lib, test)?,
                        commit: test.into(),
                    },
                    Head::For {
                        charge,
                        binding,
                        item,
                    } => CompiledHead::For {
                        hook: match item {
                            Some(item) => Some(hook(lib(*item))?),
                            None => None,
                        },
                        commit: charge.into(),
                        binding: *binding,
                    },
                };
                Some(CompiledLoop {
                    head,
                    body: compile_body(exe, stream, &lib, &self.body)?,
                })
            })
            .as_ref()
    }
}

/// `body`'s statements, compiled.
fn compile_body<'e>(
    exe: &Executable,
    stream: &Stream,
    lib: &impl Fn(LibId) -> &'e Lib,
    body: &Body,
) -> Option<CompiledBody> {
    let steps = body
        .steps
        .iter()
        .map(|step| {
            Some(match step {
                Step::Write(summary) => CompiledStep::Write {
                    value: compile(exe, stream, lib, summary)?,
                    at: summary.write?,
                    commit: summary.into(),
                },
                Step::Bind(summary) => CompiledStep::Bind {
                    value: compile(exe, stream, lib, summary)?,
                    at: summary.write?,
                    commit: summary.into(),
                },
                Step::If {
                    test,
                    then,
                    otherwise,
                } => CompiledStep::If {
                    condition: compile(exe, stream, lib, test)?,
                    commit: test.into(),
                    then: compile_body(exe, stream, lib, then)?,
                    otherwise: compile_body(exe, stream, lib, otherwise)?,
                },
                Step::Continue(summary) => CompiledStep::Continue(summary.into()),
                Step::Leave => CompiledStep::Leave,
            })
        })
        .collect::<Option<_>>()?;
    Some(CompiledBody {
        block: body.block,
        declares: body.declares.clone(),
        steps,
    })
}

/// The statically named library invoke of a statement action.
fn library_invoke(form: &Form) -> Option<(&super::Action, LibId, &[super::Atom])> {
    let Form::Action(action) = form else {
        return None;
    };
    let super::ActionKind::Call {
        callee: super::Callee::Library(lib),
        args,
    } = &action.kind
    else {
        return None;
    };
    Some((action, *lib, args))
}

/// The value of the region statement in `range` as one expression tree.
fn compile<'e>(
    exe: &Executable,
    stream: &Stream,
    lib: &impl Fn(LibId) -> &'e Lib,
    summary: &Summary,
) -> Option<Node> {
    if let Some(id) = summary.action {
        let (_, target, args) = library_invoke(&exe.stmt(id).form)?;
        let operands = args
            .iter()
            .map(|arg| -> Option<Node> {
                match arg {
                    super::Atom::Var(super::Var::Local(local)) => {
                        let at = local.at as usize;
                        Some(Box::new(move |slots, _| match slots.get(at) {
                            Some(SlotState::Value(value)) => Some(*value),
                            _ => None,
                        }))
                    }
                    super::Atom::Literal(Constant::Resident(value)) => {
                        let value = *value;
                        Some(Box::new(move |_, _| Some(value)))
                    }
                    _ => None,
                }
            })
            .collect::<Option<Vec<_>>>()?;
        return operator(lib(target), operands);
    }
    let range = summary.ops;
    let mut regs: Vec<Option<Node>> = Vec::new();
    let set = |regs: &mut Vec<Option<Node>>, at: usize, node: Node| {
        if regs.len() <= at {
            regs.resize_with(at + 1, || None);
        }
        regs[at] = Some(node);
    };
    for op in stream.ops.get(range.start as usize..range.end() as usize)? {
        match *op {
            Op::Region { .. } => {}
            Op::Local { dst, local } => {
                let at = local.at as usize;
                set(
                    &mut regs,
                    dst.ix(),
                    Box::new(move |slots, _| match slots.get(at) {
                        Some(SlotState::Value(value)) => Some(*value),
                        _ => None,
                    }),
                );
            }
            Op::Const { dst, c } => match stream.consts.get(c.ix())? {
                Constant::Resident(value) => {
                    let value = *value;
                    set(&mut regs, dst.ix(), Box::new(move |_, _| Some(value)));
                }
                Constant::Deferred(_) => return None,
            },
            Op::Operator { lib: id, args } => {
                let operands = operands(&mut regs, args)?;
                set(
                    &mut regs,
                    args.start.ix(),
                    operator(lib(LibId(id.0)), operands)?,
                );
            }
            Op::Nested { act, args } => {
                let id = stream.nested.get(act.ix())?.kind.operator_lib()?;
                let operands = operands(&mut regs, args)?;
                set(&mut regs, args.start.ix(), operator(lib(id), operands)?);
            }
            Op::Test { cond: src }
            | Op::Branch { cond: src, .. }
            | Op::WriteLocal { src, .. }
            | Op::Bind { src, .. } => {
                return regs.get_mut(src.ix())?.take();
            }
            _ => return None,
        }
    }
    None
}

/// The nodes of the registers `args`, taken.
fn operands(regs: &mut [Option<Node>], args: RegRange) -> Option<Vec<Node>> {
    (args.start.ix()..args.end() as usize)
        .map(|at| regs.get_mut(at)?.take())
        .collect()
}

/// A prepared entry and its scalar charge table: an operator site's
/// answer.
struct Answer {
    prepared: Entry,
    table: ScalarTable,
}

impl Answer {
    /// `function`'s, when it has a prepared entry with a scalar table.
    fn of(function: &Lib) -> Option<Self> {
        let (prepared, fast) = function.prepared.as_ref()?;
        Some(Self {
            prepared: prepared.clone(),
            table: fast.scalar()?.clone(),
        })
    }

    /// The entry on `args`: its scalar value and its formula from the
    /// table, at the sizes its guard certifies or, where it certifies none,
    /// the values show.
    #[inline(always)]
    fn call(&self, args: &[Value]) -> Option<(Value, u64)> {
        let answer = self.prepared.answer(args, true)?;
        let result = answer.value;
        if !scalar(&result) {
            return None;
        }
        let mask = match answer.scalar_sizes {
            Some(mask) => mask,
            None => self.table.classify(|source| match *source {
                Source::Arg(index) => args.get(index),
                Source::Result => Some(&result),
                Source::Nothing => None,
            })?,
        };
        Some((result, self.table.units(mask)?))
    }
}

/// The compiled operator `function` over `operands`, which must be
/// scalars.
fn operator(function: &Lib, operands: Vec<Node>) -> Option<Node> {
    let answer = Answer::of(function)?;
    let mut operands = operands.into_iter();
    let a = operands.next()?;
    Some(match operands.next() {
        None => Box::new(move |slots, meter| {
            let x = a(slots, meter)?;
            if !scalar(&x) {
                return None;
            }
            let (result, units) = answer.call(&[x])?;
            meter.formula(units);
            Some(result)
        }),
        Some(b) => Box::new(move |slots, meter| {
            let x = a(slots, meter)?;
            let y = b(slots, meter)?;
            if !scalar(&x) || !scalar(&y) {
                return None;
            }
            let (result, units) = answer.call(&[x, y])?;
            meter.formula(units);
            Some(result)
        }),
    })
}

/// The compiled `item` hook `function`: what the loop walks and a scalar
/// item it stepped to, as the reference's handoff passes them.
fn hook(function: &Lib) -> Option<Hook> {
    let answer = Answer::of(function)?;
    Some(Box::new(move |walked, raw, meter| {
        if !scalar(&raw) {
            return None;
        }
        let (result, units) = answer.call(&[walked, raw])?;
        meter.formula(units);
        Some(result)
    }))
}
