//! Region formation at lowering (`docs/design/kernel-regions.md` §1): the
//! cold side table of the spans of guest code whose fast path holds no
//! effect, park, task switch, projection point or slice or bound crossing.
//!
//! A [`LoopPlan`] states, for a `while` or a `for` over a list or a tuple
//! whose body begins with a region statement, its head (the `while`'s
//! test, or the `for`'s scheduled units and its protocol's `item` hook)
//! and its body as a tree: each statement's fixed units, its formulas'
//! maximum, its static charge program, the slots it reads and writes, the
//! `if` arms it enters, the `continue`s that end an iteration early, and
//! the statements it leaves to the reference. [`LoopPlan::verify`] checks
//! the plan against the lowered code, its stream's `RegionTable` events
//! and the statement units before lowering admits it. Plans belong to the
//! executable: deterministic, immutable and never saved. Their compiled
//! form ([`nodes`]) is built on first entry and cached in the plan. A
//! layout bit turns formation off ([`super::Layout::regions`]), which is
//! how the regions law compares the two plans.
//!
//! Region ops: reads of unshared slots, resident constants, prepared
//! operators (`Op::Operator`, or `Op::Nested` of an operator), writes and
//! declarations of unshared slots, an `if`'s branch and `continue`; and,
//! in an allocating plan (R5), constructions of tuples, lists, sets, maps
//! and records, general calls of natives, field and index reads, member
//! writes and discarded values. A scalar plan's statements hold scalars
//! and charge each operator its scalar table, so one iteration's most is
//! static and admitted whole. An allocating plan's formulas measure what
//! they are given, so each of its statements is admitted on its own. A
//! statement with any other op is a barrier: the region stops before it
//! and the reference runs it, as it runs a `break` or a `return`.

use std::sync::OnceLock;
use std::sync::atomic::AtomicU32;

use lash_kernel_exec::{Op, OpRange, Weight};

mod nodes;

pub(crate) use nodes::{
    Commit, CompiledBody, CompiledHead, CompiledLoop, CompiledStep, Cx, Meter, Place, Role, scalar,
    store,
};

use super::{
    Block, BlockId, Code, Constant, Executable, Form, Lib, LibId, LibRun, NestedKind, Stmt, StmtId,
    Stream,
};
use nodes::Libs;

/// The consecutive unproductive entries that retire a loop plan.
pub(crate) const UNPRODUCTIVE: u32 = 16;

/// The deepest `if` nesting a loop body's plan holds.
pub(crate) const NESTING: usize = 8;

/// One event of a region statement's static charge program, in the order
/// the reference charges it (`docs/design/kernel-end-state.md` §B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A fixed charge: a statement or loop-test unit, or one event of a
    /// charge region.
    Fixed(Weight),
    /// The next operator's or native's formula, charged whole at its
    /// completion.
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
    /// A member write.
    Store,
    /// A `do`: the value is discarded.
    Discard,
}

impl End {
    /// How a plain statement whose last op is `op` ends, if it is one a
    /// region runs.
    fn of(op: Op) -> Option<Self> {
        Some(match op {
            Op::WriteLocal { .. } => Self::Write,
            Op::Bind { .. } => Self::Bind,
            Op::Continue => Self::Continue,
            Op::SetField { .. } | Op::SetIndex { .. } => Self::Store,
            Op::End => Self::Discard,
            _ => return None,
        })
    }

    /// Whether `op` ends a statement as this.
    fn ends(self, op: Option<&Op>) -> bool {
        matches!(
            (op, self),
            (Some(Op::Test { .. }), Self::Test)
                | (Some(Op::Branch { .. }), Self::Branch)
                | (Some(Op::WriteLocal { .. }), Self::Write)
                | (Some(Op::Bind { .. }), Self::Bind)
                | (Some(Op::Continue), Self::Continue)
                | (Some(Op::SetField { .. } | Op::SetIndex { .. }), Self::Store)
                | (Some(Op::End), Self::Discard)
        )
    }
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
    /// `None` when a formula has no scalar table.
    pub(crate) formula_max: Option<u64>,
    /// What it charges, in order.
    pub(crate) program: Box<[Event]>,
    /// The frame positions it reads.
    pub(crate) reads: Box<[u32]>,
    /// The frame position it writes or declares.
    pub(crate) write: Option<u32>,
    /// Whether it allocates, calls a native, reads a member, writes the
    /// heap or holds a value other than a scalar: its plan allocates.
    pub(crate) general: bool,
    /// Invoke occurrences committed only when this statement succeeds.
    calls: Box<[usize]>,
    /// A statement-root invoke keeps its original action and resume form.
    action: Option<StmtId>,
}

impl Summary {
    /// The most the statement charges.
    fn max(&self) -> Option<u64> {
        self.fixed.checked_add(self.formula_max?)
    }

    /// Whether its plan must allocate: it does, or a formula of it has no
    /// static most.
    fn allocates(&self) -> bool {
        self.general || self.formula_max.is_none()
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
    /// A member write.
    Store(Summary),
    /// `do`: a discarded value.
    Discard(Summary),
    /// A statement the reference runs: a barrier, a `break` or a
    /// `return`. The region stops before it.
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
                Step::Write(summary)
                | Step::Bind(summary)
                | Step::Continue(summary)
                | Step::Store(summary)
                | Step::Discard(summary) => calls.extend_from_slice(&summary.calls),
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
                Step::Write(summary)
                | Step::Bind(summary)
                | Step::Continue(summary)
                | Step::Store(summary)
                | Step::Discard(summary) => summary.max()?,
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

    /// Whether a statement of the block or an arm in it allocates.
    fn allocates(&self) -> bool {
        self.steps.iter().any(|step| match step {
            Step::Write(summary)
            | Step::Bind(summary)
            | Step::Continue(summary)
            | Step::Store(summary)
            | Step::Discard(summary) => summary.allocates(),
            Step::If {
                test,
                then,
                otherwise,
            } => test.allocates() || then.allocates() || otherwise.allocates(),
            Step::Leave => false,
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

impl Head {
    fn summary(&self) -> &Summary {
        match self {
            Self::While(test) => test,
            Self::For { charge, .. } => charge,
        }
    }
}

/// A `while` or a `for` loop that runs as a batch of whole iterations.
pub(crate) struct LoopPlan {
    pub(crate) stmt: StmtId,
    pub(crate) head: Head,
    pub(crate) body: Body,
    /// The loop-test unit an iteration charges last, for the next test:
    /// a region is entered from a test whose unit is charged.
    pub(crate) loop_test: u64,
    /// The most one iteration of a scalar plan charges: the head's, the
    /// body's dearest path's, and the next test's unit. An allocating
    /// plan states 0: it admits each statement on its own.
    pub(crate) iteration_max: u64,
    /// Whether the plan allocates (`docs/design/kernel-regions.md` §5, R5):
    /// its values are any values and its formulas measure them.
    pub(crate) allocates: bool,
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
    /// The consecutive entries that missed a guard before an iteration
    /// ended, which [`UNPRODUCTIVE`] of retire the plan: a site whose
    /// prepared entry declines what this run gives it costs the region's
    /// work and the reference's. It changes which code runs a statement,
    /// never what the statement does.
    pub(crate) unproductive: AtomicU32,
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
    /// loop-test unit's sum, or not 0 for an allocating plan.
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
    /// A summary says it allocates where its ops do not, or the other way
    /// round, or the plan does where none of its summaries does.
    Allocates { statement: usize },
}

/// The lowered code a plan is formed and checked against: its stream, its
/// blocks and statements by id, and the libraries it calls.
pub(crate) struct Lowered<'a> {
    pub(crate) stream: &'a Stream,
    pub(crate) block: &'a dyn Fn(BlockId) -> &'a Block,
    pub(crate) stmt: &'a dyn Fn(StmtId) -> &'a Stmt,
    pub(crate) lib: &'a dyn Fn(LibId) -> &'a Lib,
}

/// Whether library `function` is an operator a region calls with `count`
/// arguments: it has a prepared entry, and an operation's body makes no
/// cell for a shared parameter at entry, an allocation.
fn operator_site(function: &Lib, count: usize) -> bool {
    function.prepared.is_some()
        && function.arity == count
        && !(function.operation() && function.shares_params)
}

/// Whether library `function` is a native a region calls with `count`
/// arguments through its general call: no operation, whose admission is
/// the reference's, and no plan or body.
fn native_site(function: &Lib, count: usize) -> bool {
    matches!(function.run, LibRun::Native { .. })
        && !function.operation()
        && count <= function.arity
}

/// Whether `statement` is a barrier: an op of it is one no region runs,
/// or it is no plain statement or `if`. The region stops before it.
fn barrier(lowered: &Lowered<'_>, statement: &Stmt) -> bool {
    if !matches!(statement.form, Form::Plain | Form::If { .. }) {
        return true;
    }
    let stream = lowered.stream;
    let Some(ops) = stream
        .ops
        .get(statement.ops.start as usize..statement.ops.end() as usize)
    else {
        return true;
    };
    ops.iter().any(|op| match *op {
        Op::Region { .. }
        | Op::Local { .. }
        | Op::MakeTuple { .. }
        | Op::MakeList { .. }
        | Op::MakeSet { .. }
        | Op::MakeMap { .. }
        | Op::MakeRecord { .. }
        | Op::Field { .. }
        | Op::Index { .. }
        | Op::Pin { .. }
        | Op::Test { .. }
        | Op::Branch { .. }
        | Op::WriteLocal { .. }
        | Op::Bind { .. }
        | Op::Continue
        | Op::SetField { .. }
        | Op::SetIndex { .. }
        | Op::End => false,
        Op::Const { c, .. } => !matches!(stream.consts.get(c.ix()), Some(Constant::Resident(_))),
        Op::Operator { lib, args } => {
            !operator_site((lowered.lib)(LibId(lib.0)), args.len as usize)
        }
        Op::Nested { act, args } => match stream.nested.get(act.ix()) {
            Some(nested) if nested.operator => match nested.kind.operator_lib() {
                Some(lib) => !operator_site((lowered.lib)(lib), args.len as usize),
                None => true,
            },
            _ => true,
        },
        Op::Call { lib, args } => !native_site((lowered.lib)(LibId(lib.0)), args.len as usize),
        _ => true,
    })
}

/// Whether `op` allocates, calls a native, reads a member or holds a value
/// other than a scalar: what makes its statement's plan allocate.
fn general<'a>(stream: &Stream, lib: &dyn Fn(LibId) -> &'a Lib, op: Op) -> bool {
    match op {
        Op::Call { .. }
        | Op::MakeTuple { .. }
        | Op::MakeList { .. }
        | Op::MakeSet { .. }
        | Op::MakeMap { .. }
        | Op::MakeRecord { .. }
        | Op::Field { .. }
        | Op::Index { .. }
        | Op::Pin { .. }
        | Op::SetField { .. }
        | Op::SetIndex { .. }
        | Op::End => true,
        Op::Const { c, .. } => !matches!(
            stream.consts.get(c.ix()),
            Some(Constant::Resident(value)) if scalar(value)
        ),
        Op::Operator { lib: id, args } => !scalar_site(lib(LibId(id.0)), args.len as usize),
        Op::Nested { act, args } => match stream
            .nested
            .get(act.ix())
            .and_then(|nested| nested.kind.operator_lib())
        {
            Some(id) => !scalar_site(lib(id), args.len as usize),
            None => true,
        },
        _ => false,
    }
}

/// Whether operator `function` with `count` arguments has the scalar table
/// a scalar plan charges it, over one or two operands.
fn scalar_site(function: &Lib, count: usize) -> bool {
    (1..=2).contains(&count)
        && function
            .prepared
            .as_ref()
            .is_some_and(|(_, fast)| fast.scalar().is_some())
}

/// The libraries and the lowered code a plan is formed against.
pub(super) struct Formation<'a> {
    pub(super) lowered: Lowered<'a>,
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
                let mut formula_max = Some(0);
                let mut general = false;
                if let Some(item) = item {
                    let mut defined = vec![true, true];
                    general = !scalar_site((self.lowered.lib)(item), 2);
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
                    general,
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
        // A plan runs from its head into its body's first statement: one
        // that leaves at once would run nothing.
        if matches!(body.steps.first(), None | Some(Step::Leave)) {
            return None;
        }
        let allocates = head.summary().allocates() || body.allocates();
        let iteration_max = if allocates {
            0
        } else {
            head.summary()
                .max()?
                .checked_add(body.max()?)?
                .checked_add(self.loop_test)?
        };
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
            allocates,
            binds,
            depth,
            calls: calls.into(),
            compiled: OnceLock::new(),
            unproductive: AtomicU32::new(0),
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
                // A statement-root invoke of a prepared operator runs in
                // the region; any other action is a barrier.
                if let Form::Action(_) = statement.form {
                    return Some(self.action_summary(code, *id, depth).unwrap_or(Step::Leave));
                }
                if barrier(&self.lowered, statement) {
                    return Some(Step::Leave);
                }
                let unit = || vec![Event::Fixed(Weight::Atomic(1))];
                match &statement.form {
                    Form::Plain => {
                        let last = self.stream_op(statement.ops.end().checked_sub(1)?)?;
                        let end = End::of(last)?;
                        let summary = self.summarize(code, statement.ops, end, 1, unit(), depth)?;
                        Some(match end {
                            End::Write => Step::Write(summary),
                            End::Bind => Step::Bind(summary),
                            End::Store => Step::Store(summary),
                            End::Discard => Step::Discard(summary),
                            _ => Step::Continue(summary),
                        })
                    }
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
        let mut formula_max = Some(0);
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
            general: !scalar_site((self.lowered.lib)(lib), args.len()),
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
        let mut formula_max = Some(0_u64);
        let mut reads = Vec::new();
        let mut calls = Vec::new();
        let mut allocates = general(stream, self.lowered.lib, *last);
        // Takes the registers `start..start + count` and defines `start`.
        let gather = |defined: &mut Vec<bool>, start: usize, count: usize| {
            (start..start + count).all(|at| take(defined, at)) && define(defined, start)
        };
        for op in body {
            allocates |= general(stream, self.lowered.lib, *op);
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
                    Constant::Resident(_) if define(&mut defined, dst.ix()) => {}
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
                Op::Call { args, .. } => {
                    if !gather(&mut defined, args.start.ix(), args.len as usize) {
                        return None;
                    }
                    formula_max = None;
                    program.push(Event::Formula);
                }
                Op::MakeTuple { items } | Op::MakeList { items } | Op::MakeSet { items } => {
                    if !gather(&mut defined, items.start.ix(), items.len as usize) {
                        return None;
                    }
                }
                Op::MakeMap { entries } => {
                    if !gather(&mut defined, entries.start.ix(), entries.len as usize) {
                        return None;
                    }
                }
                Op::MakeRecord { fields, values } => {
                    // A record's names are short texts the region writes
                    // as they are.
                    let short = stream
                        .field_lists
                        .get(fields.ix())?
                        .iter()
                        .all(|name| lash_kernel_doc::arena::ShortText::new(name).is_some());
                    if !short || !gather(&mut defined, values.start.ix(), values.len as usize) {
                        return None;
                    }
                }
                Op::Field { at, .. } => {
                    if !gather(&mut defined, at.ix(), 1) {
                        return None;
                    }
                }
                Op::Index { at, .. } => {
                    if !gather(&mut defined, at.ix(), 2) {
                        return None;
                    }
                }
                // A pin keeps a value its statement still uses.
                Op::Pin { src } => {
                    if !defined.get(src.ix()).copied().unwrap_or(false) {
                        return None;
                    }
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
            (Op::Continue, End::Continue)
                if reads.is_empty() && !program.contains(&Event::Formula) =>
            {
                None
            }
            (Op::SetIndex { target, src }, End::Store)
                if take(&mut defined, src.ix())
                    && take(&mut defined, target.ix())
                    && take(&mut defined, target.ix() + 1) =>
            {
                None
            }
            (Op::SetField { target, src, field }, End::Store)
                if take(&mut defined, src.ix())
                    && take(&mut defined, target.ix())
                    && lash_kernel_doc::arena::ShortText::new(
                        &stream.fields.get(field.ix())?.name,
                    )
                    .is_some() =>
            {
                None
            }
            (Op::End, End::Discard) => {
                // A `do` discards what it computed last, in its first
                // register.
                if !take(&mut defined, 0) {
                    return None;
                }
                None
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
            program: lean(program),
            reads: reads.into(),
            write,
            general: allocates,
            calls: calls.into(),
            action: None,
        })
    }

    /// Operator `lib` called on the `count` registers from `start`, which a
    /// region runs when it has a prepared entry and its operands are the
    /// values the statement computed last: its result replaces them, and
    /// its scalar table's maximum, if it has one, adds to `formula_max`.
    fn site(
        &self,
        lib: LibId,
        start: usize,
        count: usize,
        defined: &mut Vec<bool>,
        formula_max: &mut Option<u64>,
        depth: &mut Option<u32>,
    ) -> Option<()> {
        let function = (self.lowered.lib)(lib);
        if !operator_site(function, count) {
            return None;
        }
        if !(start..start + count).all(|at| take(defined, at)) || !define(defined, start) {
            return None;
        }
        let (entry, fast) = function.prepared.as_ref()?;
        *formula_max = match (*formula_max, fast.scalar()) {
            (Some(sum), Some(table)) if scalar_site(function, count) => {
                Some(sum.checked_add(table.max())?)
            }
            _ => None,
        };
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
        if !end.ends(ops.last()) {
            return Err(Refused::Shape { statement: index });
        }
        let mut allocates = false;
        let mut bounded = true;
        for op in ops {
            allocates |= general(stream, self.lowered.lib, *op);
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
                Op::Operator { lib, args } => {
                    bounded &= scalar_site((self.lowered.lib)(LibId(lib.0)), args.len as usize);
                    expected.push(Event::Formula);
                }
                Op::Call { .. } => {
                    bounded = false;
                    expected.push(Event::Formula);
                }
                Op::Act => {
                    let id = summary.action.ok_or(Refused::Shape { statement: index })?;
                    let (action, lib, args) = library_invoke(&(self.lowered.stmt)(id).form)
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
                    let scalar = scalar_site((self.lowered.lib)(lib), args.len());
                    bounded &= scalar;
                    allocates |= !scalar;
                    expected.push(Event::Formula);
                }
                Op::Nested { act, args } => {
                    if let Some(nested) = stream.nested.get(act.ix()) {
                        if matches!(nested.kind, NestedKind::Call(_))
                            && !matches!(nested.site.unit, lash_kernel_doc::Unit::Library(_))
                        {
                            calls.push(nested.occurrence);
                        }
                        if let Some(lib) = nested.kind.operator_lib() {
                            bounded &= scalar_site((self.lowered.lib)(lib), args.len as usize);
                        }
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
        if summary.general != allocates || summary.formula_max.is_some() != bounded {
            return Err(Refused::Allocates { statement: index });
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
            // Each statement's summary covers exactly its own ops.
            let own = match step {
                Step::Write(summary)
                | Step::Bind(summary)
                | Step::Continue(summary)
                | Step::Store(summary)
                | Step::Discard(summary) => {
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
                    self.summary(summary, unit, End::Write)?;
                }
                (Step::Bind(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Bind)?;
                }
                (Step::Continue(summary), Form::Plain) => {
                    self.summary(summary, unit, End::Continue)?;
                }
                (Step::Store(summary), Form::Plain) => self.summary(summary, unit, End::Store)?,
                (Step::Discard(summary), Form::Plain) => {
                    self.summary(summary, unit, End::Discard)?;
                }
                // A region leaves only a barrier to the reference.
                (Step::Leave, _) if barrier(self.lowered, statement) => {}
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
    /// declarations are the blocks', it leaves only barriers to the
    /// reference, each summary's fixed units are its program's fixed
    /// events, every region's head states the total its `RegionSummary`
    /// gives, the programs are the stream's events in order, each summary
    /// allocates where its ops do, and the iteration maximum is the
    /// head's, the dearest path's and the loop-test unit's sum, or 0 for
    /// an allocating plan. Summaries are numbered in order: the head 0,
    /// then the body's statements depth first.
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
                let scalar = item.is_none_or(|item| scalar_site((lowered.lib)(item), 2));
                if charge.general != !scalar || charge.formula_max.is_none() == scalar {
                    return Err(Refused::Allocates { statement: 0 });
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
        if matches!(self.body.steps.first(), None | Some(Step::Leave)) {
            return Err(Refused::Shape { statement: 1 });
        }
        let allocates = self.head.summary().allocates() || self.body.allocates();
        if self.allocates != allocates {
            return Err(Refused::Allocates { statement: 0 });
        }
        let iteration = if allocates {
            Some(0)
        } else {
            self.head
                .summary()
                .max()
                .zip(self.body.max())
                .and_then(|(head, body)| head.checked_add(body)?.checked_add(loop_test))
        };
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
            allocates: self.allocates,
            binds: self.binds,
            depth: self.depth,
            calls: self.calls.clone(),
            compiled: OnceLock::new(),
            unproductive: AtomicU32::new(0),
        }
    }

    /// The plan's compiled form, built on its first entry.
    pub(crate) fn compiled(&self, exe: &Executable) -> Option<&CompiledLoop> {
        self.compiled
            .get_or_init(|| {
                let (stream, _, _) = exe.statement(self.stmt);
                let libs = Libs {
                    exe,
                    general: self.allocates,
                };
                let head = match &self.head {
                    Head::While(test) => CompiledHead::While {
                        test: nodes::compile(stream, &libs, test.ops)?.root()?,
                        commit: test.into(),
                    },
                    Head::For {
                        charge,
                        binding,
                        item,
                    } => CompiledHead::For {
                        hook: match item {
                            Some(item) => Some(nodes::hook(&libs, *item)?),
                            None => None,
                        },
                        commit: charge.into(),
                        binding: *binding,
                    },
                };
                Some(CompiledLoop {
                    head,
                    body: compile_body(stream, &libs, &self.body)?,
                })
            })
            .as_ref()
    }
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

/// The value a write or a declaration stores: its expression's, or a
/// statement-root invoke's operator call.
fn value(stream: &Stream, libs: &Libs<'_>, summary: &Summary) -> Option<nodes::Node> {
    match summary.action {
        Some(id) => {
            let (_, target, args) = library_invoke(&libs.exe.stmt(id).form)?;
            nodes::action(libs, target, args)
        }
        None => nodes::compile(stream, libs, summary.ops)?.root(),
    }
}

/// `body`'s statements, compiled.
fn compile_body(stream: &Stream, libs: &Libs<'_>, body: &Body) -> Option<CompiledBody> {
    let steps = body
        .steps
        .iter()
        .map(|step| {
            Some(match step {
                Step::Write(summary) => CompiledStep::Write {
                    value: value(stream, libs, summary)?,
                    at: summary.write?,
                    commit: summary.into(),
                },
                Step::Bind(summary) => CompiledStep::Bind {
                    value: value(stream, libs, summary)?,
                    at: summary.write?,
                    commit: summary.into(),
                },
                Step::If {
                    test,
                    then,
                    otherwise,
                } => CompiledStep::If {
                    condition: nodes::compile(stream, libs, test.ops)?.root()?,
                    commit: test.into(),
                    then: compile_body(stream, libs, then)?,
                    otherwise: compile_body(stream, libs, otherwise)?,
                },
                Step::Continue(summary) => CompiledStep::Continue(summary.into()),
                Step::Store(summary) => {
                    let mut compiled = nodes::compile(stream, libs, summary.ops)?;
                    let (roles, place) = match compiled.last {
                        Op::SetIndex { target, src } => (
                            [
                                (Role::Target, target.ix()),
                                (Role::Index, target.ix() + 1),
                                (Role::Value, src.ix()),
                            ]
                            .to_vec(),
                            Place::Index,
                        ),
                        Op::SetField { target, field, src } => (
                            [(Role::Target, target.ix()), (Role::Value, src.ix())].to_vec(),
                            Place::Field(lash_kernel_doc::Value::Text(
                                lash_kernel_doc::arena::ShortText::new(
                                    &stream.fields.get(field.ix())?.name,
                                )?,
                            )),
                        ),
                        _ => return None,
                    };
                    CompiledStep::Store {
                        operands: compiled.ordered(&roles)?,
                        place,
                        commit: summary.into(),
                    }
                }
                Step::Discard(summary) => CompiledStep::Discard {
                    value: nodes::compile(stream, libs, summary.ops)?.take(0)?,
                    commit: summary.into(),
                },
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
