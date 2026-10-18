//! Region formation at lowering (`docs/design/kernel-regions.md` §1): the
//! cold side table of the spans of guest code whose fast path holds no
//! effect, park, task switch, projection point or slice or bound crossing.
//!
//! A [`LoopPlan`] states, for a `while` whose test and body are region
//! statements, each statement's fixed units, its formulas' maximum, its
//! static charge program, the slots it reads and writes, and the most one
//! iteration charges. [`LoopPlan::verify`] checks the plan against the
//! stream's `RegionTable` events and the statement units before lowering
//! admits it. Plans belong to the executable: deterministic, immutable and
//! never saved. Their compiled form is built on first entry and cached in
//! the plan. A layout bit turns formation off ([`super::Layout::regions`]),
//! which is how the regions law compares the two plans.
//!
//! Region ops in this version: reads of unshared slots, resident scalar
//! constants, prepared operators (`Op::Operator`, or `Op::Nested` of an
//! operator) with a scalar charge table, and writes of unshared slots.
//! Everything else is a barrier, which no plan contains.

use std::sync::OnceLock;

use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, OpRange, RegRange, Weight};

use super::{
    BlockId, Code, Constant, Executable, Form, Lib, LibId, NestedKind, Source, Stmt, StmtId, Stream,
};
use crate::machine::SlotState;

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

/// A region statement on its one path: a loop's test, or a statement of
/// its body.
#[derive(Clone)]
pub(crate) struct Summary {
    /// Its ops, up to the `Test` or `WriteLocal` that ends it.
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
    /// The frame position it writes; none for a test.
    pub(crate) write: Option<u32>,
}

impl Summary {
    /// The most the statement charges.
    fn max(&self) -> Option<u64> {
        self.fixed.checked_add(self.formula_max)
    }
}

/// A `while` loop that runs as a batch of whole iterations.
pub(crate) struct LoopPlan {
    pub(crate) stmt: StmtId,
    pub(crate) body: BlockId,
    pub(crate) test: Summary,
    pub(crate) statements: Box<[Summary]>,
    /// The loop-test unit an iteration charges last, for the next test:
    /// a region is entered from a test whose unit is charged.
    pub(crate) loop_test: u64,
    /// The most one iteration charges: the test's and every body
    /// statement's fixed units and formula maxima, and the next test's
    /// unit.
    pub(crate) iteration_max: u64,
    /// The call depth its sites need beyond the frame's own, when one
    /// checks it (`K-BND-002`): an operation's admission, or a helper
    /// template's inline depth.
    pub(crate) depth: Option<u32>,
    /// Its call sites' occurrence indexes (`K-EFF-008`): a loop region of
    /// this version makes no call.
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
    /// The iteration maximum is not the statements' sum.
    Iteration,
    /// A summary's reads or write are not the slots its ops read and
    /// write.
    Slots { statement: usize },
    /// The call sites are not the occurrences of the calls its ops make.
    Calls,
    /// The loop-test unit is not the cost table's.
    LoopTest,
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

/// A loop plan's compiled form: its test and each body statement's value.
pub(crate) struct CompiledLoop {
    pub(crate) test: Node,
    pub(crate) statements: Box<[Node]>,
}

/// The values a region holds in registers and slots.
#[inline(always)]
fn scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
    )
}

/// The libraries and the document code a plan is formed against.
pub(super) struct Formation<'a> {
    pub(super) stream: &'a Stream,
    pub(super) libs: &'a [Lib],
    /// The cost table's loop test.
    pub(super) loop_test: u64,
}

impl Formation<'_> {
    /// The plan of `while` statement `id`, `statement`, of `code`, whose
    /// body is `body` with `statements`; `None` when it is no loop region.
    pub(super) fn form(
        &self,
        code: &Code,
        id: StmtId,
        statement: &Stmt,
        body: (BlockId, &[u32], &[&Stmt]),
    ) -> Option<LoopPlan> {
        let Form::While {
            metered,
            test,
            else_block: None,
            ..
        } = &statement.form
        else {
            return None;
        };
        let (block, declares, statements) = body;
        if !code.charging || !declares.is_empty() || statements.is_empty() {
            return None;
        }
        let mut depth = None;
        let units = vec![Event::Fixed(Weight::Atomic(*metered))];
        let test = self.summarize(code, *test, true, *metered, units, &mut depth)?;
        let statements = statements
            .iter()
            .map(|statement| match statement.form {
                Form::Plain => {
                    let unit = vec![Event::Fixed(Weight::Atomic(1))];
                    self.summarize(code, statement.ops, false, 1, unit, &mut depth)
                }
                _ => None,
            })
            .collect::<Option<Box<[Summary]>>>()?;
        let first = test.max()?.checked_add(self.loop_test)?;
        let iteration_max = statements
            .iter()
            .try_fold(first, |sum, statement| sum.checked_add(statement.max()?))?;
        Some(LoopPlan {
            stmt: id,
            body: block,
            test,
            statements,
            loop_test: self.loop_test,
            iteration_max,
            depth,
            calls: Box::default(),
            compiled: OnceLock::new(),
        })
    }

    /// The summary of the region statement in `range`, a test when `test`,
    /// charged `fixed` units and `program` before its ops.
    fn summarize(
        &self,
        code: &Code,
        range: OpRange,
        test: bool,
        mut fixed: u64,
        mut program: Vec<Event>,
        depth: &mut Option<u32>,
    ) -> Option<Summary> {
        let ops = self
            .stream
            .ops
            .get(range.start as usize..range.end() as usize)?;
        let (last, body) = ops.split_last()?;
        // Which registers hold a value the statement has not yet used.
        let mut defined: Vec<bool> = Vec::new();
        let mut formula_max = 0_u64;
        let mut reads = Vec::new();
        for op in body {
            match *op {
                Op::Region { region, total } => {
                    if total == u32::MAX {
                        return None;
                    }
                    fixed = fixed.checked_add(u64::from(total))?;
                    let events = self.stream.regions.events(region.0);
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
                Op::Const { dst, c } => match self.stream.consts.get(c.ix())? {
                    Constant::Resident(value)
                        if scalar(value) && define(&mut defined, dst.ix()) => {}
                    _ => return None,
                },
                Op::Operator { lib, args } => {
                    self.site(LibId(lib.0), args, &mut defined, &mut formula_max, depth)?;
                    program.push(Event::Formula);
                }
                Op::Nested { act, args } => {
                    let nested = self.stream.nested.get(act.ix())?;
                    let NestedKind::Body(lib) = nested.kind else {
                        return None;
                    };
                    if !nested.operator {
                        return None;
                    }
                    self.site(lib, args, &mut defined, &mut formula_max, depth)?;
                    program.push(Event::Formula);
                }
                _ => return None,
            }
        }
        let write = match (*last, test) {
            (Op::Test { cond }, true) if take(&mut defined, cond.ix()) => None,
            (Op::WriteLocal { local, src }, false)
                if !code.slots.get(local.slot as usize)?.shared && take(&mut defined, src.ix()) =>
            {
                Some(local.at)
            }
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
            program: program
                .into_iter()
                .filter(|event| *event != Event::Fixed(Weight::Atomic(0)))
                .collect(),
            reads: reads.into(),
            write,
        })
    }

    /// Operator `lib` called on `args`, which a region runs when its
    /// prepared entry has a scalar charge table and its operands are the
    /// values the statement computed last: its result replaces them, and
    /// its table's maximum adds to `formula_max`.
    fn site(
        &self,
        lib: LibId,
        args: RegRange,
        defined: &mut Vec<bool>,
        formula_max: &mut u64,
        depth: &mut Option<u32>,
    ) -> Option<()> {
        let function = self.libs.get(lib.0 as usize)?;
        let (_, fast) = function.prepared.as_ref()?;
        let count = args.len as usize;
        if function.arity != count || !(1..=2).contains(&count) {
            return None;
        }
        // An operation whose body shares a parameter makes its cell at
        // entry: an allocation, which no region makes.
        if function.operation() && function.shares_params {
            return None;
        }
        let start = args.start.ix();
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

impl LoopPlan {
    /// Checks the plan against `stream`'s charge regions and the statement
    /// units, `loop_test` and the loop's `metered` units: each summary's
    /// fixed units are its program's fixed events, every region's head
    /// states the total its `RegionSummary` gives, the programs are the
    /// stream's events in order, and the iteration maximum is their sum
    /// and the loop-test unit.
    pub(crate) fn verify(
        &self,
        stream: &Stream,
        statement: &Stmt,
        loop_test: u64,
    ) -> Result<(), Refused> {
        if self.loop_test != loop_test {
            return Err(Refused::LoopTest);
        }
        let summaries = std::iter::once(&self.test).chain(self.statements.iter());
        let mut iteration = Some(loop_test);
        let mut calls = Vec::new();
        for (index, summary) in summaries.enumerate() {
            let mut expected = Vec::new();
            let (mut reads, mut write) = (Vec::new(), None);
            let head = if index == 0 {
                match &statement.form {
                    Form::While { metered, .. } => vec![Weight::Atomic(*metered)],
                    _ => return Err(Refused::Program { statement: index }),
                }
            } else {
                vec![Weight::Atomic(1)]
            };
            expected.extend(head.into_iter().map(Event::Fixed));
            let ops = stream
                .ops
                .get(summary.ops.start as usize..summary.ops.end() as usize)
                .ok_or(Refused::Program { statement: index })?;
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
                    Op::Operator { .. } => expected.push(Event::Formula),
                    Op::Nested { act, .. } => match stream.nested.get(act.ix()) {
                        Some(nested) if matches!(nested.kind, NestedKind::Call(_)) => {
                            calls.push(nested.occurrence);
                        }
                        _ => expected.push(Event::Formula),
                    },
                    Op::Local { local, .. } => reads.push(local.at),
                    Op::WriteLocal { local, .. } => write = Some(local.at),
                    _ => {}
                }
            }
            if *summary.reads != *reads || summary.write != write {
                return Err(Refused::Slots { statement: index });
            }
            expected.retain(|event| *event != Event::Fixed(Weight::Atomic(0)));
            if *summary.program != *expected {
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
            iteration = iteration.and_then(|sum| sum.checked_add(summary.max()?));
        }
        if iteration != Some(self.iteration_max) {
            return Err(Refused::Iteration);
        }
        if *self.calls != *calls {
            return Err(Refused::Calls);
        }
        Ok(())
    }

    /// The plan as stated, uncompiled: what a law misstates.
    #[cfg(test)]
    pub(crate) fn copied(&self) -> Self {
        Self {
            stmt: self.stmt,
            body: self.body,
            test: self.test.clone(),
            statements: self.statements.clone(),
            loop_test: self.loop_test,
            iteration_max: self.iteration_max,
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
                Some(CompiledLoop {
                    test: compile(stream, &lib, self.test.ops)?,
                    statements: self
                        .statements
                        .iter()
                        .map(|summary| compile(stream, &lib, summary.ops))
                        .collect::<Option<_>>()?,
                })
            })
            .as_ref()
    }
}

/// The value of the region statement in `range` as one expression tree.
fn compile<'e>(stream: &Stream, lib: &impl Fn(LibId) -> &'e Lib, range: OpRange) -> Option<Node> {
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
                let operands = (args.start.ix()..args.end() as usize)
                    .map(|at| regs.get_mut(at)?.take())
                    .collect::<Option<Vec<_>>>()?;
                set(
                    &mut regs,
                    args.start.ix(),
                    operator(lib(LibId(id.0)), operands)?,
                );
            }
            Op::Nested { act, args } => {
                let NestedKind::Body(id) = stream.nested.get(act.ix())?.kind else {
                    return None;
                };
                let operands = (args.start.ix()..args.end() as usize)
                    .map(|at| regs.get_mut(at)?.take())
                    .collect::<Option<Vec<_>>>()?;
                set(&mut regs, args.start.ix(), operator(lib(id), operands)?);
            }
            Op::Test { cond: src } | Op::WriteLocal { src, .. } => {
                return regs.get_mut(src.ix())?.take();
            }
            _ => return None,
        }
    }
    None
}

/// The compiled operator `function` over `operands`: its prepared entry on
/// scalar values, charged its formula from its scalar table at the sizes
/// its guard certifies or, where it certifies none, the values show.
fn operator(function: &Lib, operands: Vec<Node>) -> Option<Node> {
    let (prepared, fast) = function.prepared.as_ref()?;
    let table = fast.scalar()?.clone();
    let prepared = prepared.clone();
    let answer = move |args: &[Value]| -> Option<(Value, u64)> {
        let answer = prepared.answer(args, true)?;
        let result = answer.value;
        if !scalar(&result) {
            return None;
        }
        let mask = match answer.scalar_sizes {
            Some(mask) => mask,
            None => table.classify(|source| match *source {
                Source::Arg(index) => args.get(index),
                Source::Result => Some(&result),
                Source::Nothing => None,
            })?,
        };
        Some((result, table.units(mask)?))
    };
    let mut operands = operands.into_iter();
    let a = operands.next()?;
    Some(match operands.next() {
        None => Box::new(move |slots, meter| {
            let x = a(slots, meter)?;
            if !scalar(&x) {
                return None;
            }
            let (result, units) = answer(&[x])?;
            meter.formula(units);
            Some(result)
        }),
        Some(b) => Box::new(move |slots, meter| {
            let x = a(slots, meter)?;
            let y = b(slots, meter)?;
            if !scalar(&x) || !scalar(&y) {
                return None;
            }
            let (result, units) = answer(&[x, y])?;
            meter.formula(units);
            Some(result)
        }),
    })
}
