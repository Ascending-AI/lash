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
//! Region ops: reads of unshared slots and of session bindings, resident
//! constants, prepared operators (`Op::Operator`, or `Op::Nested` of an
//! operator), writes of unshared slots and of session bindings,
//! declarations of unshared slots, an `if`'s branch and `continue`; and,
//! in an allocating plan (R5), constructions of tuples, lists, sets, maps
//! and records, general calls of natives, field and index reads, member
//! writes and discarded values. A scalar plan's statements hold scalars
//! and charge each operator its scalar table, so one iteration's most is
//! static and admitted whole. An allocating plan's formulas measure what
//! they are given, so each of its statements is admitted on its own. A
//! statement with any other op is a barrier: the region stops before it
//! and the reference runs it, as it runs a `break` or a `return`.
//!
//! A session binding is a region slot between boundaries (§3.6): a plan
//! names the bindings its statements read and write ([`LoopPlan::sessions`]),
//! and a region holds their resident values from its entry and publishes
//! what it wrote at its exit.

use std::sync::OnceLock;

use lash_kernel_doc::FunctionId;
use lash_kernel_exec::{Op, OpRange, Weight};

mod nodes;
mod verify;

pub(crate) use nodes::{
    Commit, CompiledBody, CompiledHead, CompiledLoop, CompiledStep, Cx, Meter, Place, Role,
    SessionSlot, Taken, measured_formula, position, scalar, store,
};

use super::{
    BindingName, Block, BlockId, Code, Constant, Executable, Form, Lib, LibId, LibRun, NestedKind,
    Stmt, StmtId, Stream,
};
use nodes::Libs;

/// The weight of consecutive unproductive entries that retires a loop
/// plan in a machine: sixteen that missed before their iteration's first
/// body statement, or 128 that missed after one committed. A site whose
/// prepared entry declines what the run gives it costs the region's work
/// and the reference's. Retiring changes which code runs a statement,
/// never what the statement does, and the machine keeps each plan's
/// weight: other runs of the same code start with none.
pub(crate) const UNPRODUCTIVE: u32 = 128;

/// What an entry that missed before its iteration's first body statement
/// weighs; one that missed after one committed weighs 1.
pub(crate) const BARREN: u32 = 8;

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
    /// The arm a conditional expression takes: the program of its arm of
    /// the statement's [`Arms`] at this index.
    Arm(u32),
}

/// A conditional expression's arms (`JumpIfFalse`, then `Jump`): each
/// arm's own charge program and the fixed units it states, which the
/// statement charges only when it takes that arm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Arms {
    pub(crate) then: Box<[Event]>,
    pub(crate) otherwise: Box<[Event]>,
    pub(crate) then_fixed: u64,
    pub(crate) otherwise_fixed: u64,
}

impl Arms {
    /// The program and fixed units of the arm `taken` selects.
    pub(crate) fn arm(&self, taken: bool) -> (&[Event], u64) {
        if taken {
            (&self.then, self.then_fixed)
        } else {
            (&self.otherwise, self.otherwise_fixed)
        }
    }
}

/// What a region statement does with the value its ops compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// A `while`'s test.
    Test,
    /// An assignment of an unshared slot or of a session binding.
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
            Op::WriteLocal { .. } | Op::WriteSession { .. } => Self::Write,
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
                | (
                    Some(Op::WriteLocal { .. } | Op::WriteSession { .. }),
                    Self::Write
                )
                | (Some(Op::Bind { .. }), Self::Bind)
                | (Some(Op::Continue), Self::Continue)
                | (Some(Op::SetField { .. } | Op::SetIndex { .. }), Self::Store)
                | (Some(Op::End), Self::Discard)
        )
    }
}

/// What a region statement reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionSlot {
    /// The frame position of an unshared slot.
    Frame(u32),
    /// The session binding at this index of its plan's
    /// [`LoopPlan::sessions`].
    Session(u32),
}

/// A region statement on its one path: a loop's test, the `for`'s head, a
/// body statement, or an `if`'s test.
#[derive(Clone)]
pub(crate) struct Summary {
    /// Its ops, up to the op that ends it.
    ops: OpRange,
    /// The fixed units it charges on every path: its statement or test
    /// units and the totals its charge regions' heads state, outside its
    /// conditional expressions' arms.
    pub(crate) fixed: u64,
    /// The most its formulas and its arms charge: each operator site's
    /// table maximum, and each conditional's dearer arm. `None` when a
    /// formula has no scalar table.
    pub(crate) formula_max: Option<u64>,
    /// What it charges, in order.
    pub(crate) program: Box<[Event]>,
    /// Its conditional expressions' arms, which its program's
    /// [`Event::Arm`]s name.
    pub(crate) arms: Box<[Arms]>,
    /// The slots it reads.
    pub(crate) reads: Box<[RegionSlot]>,
    /// The slot it writes or declares: a declaration's is a frame's.
    pub(crate) write: Option<RegionSlot>,
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
    /// `set` of an unshared slot or of a session binding.
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
    /// The session bindings its statements read and write, in the order
    /// they first use them: the region's session slots (§3.6).
    pub(crate) sessions: Box<[BindingName]>,
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
    /// loop-test unit's sum, or not 0 for an allocating plan.
    Iteration,
    /// A summary's reads or write are not the slots its ops read and
    /// write, or the plan's session bindings are not the ones its ops
    /// use, in order.
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
    /// The library function a template's heap path calls.
    pub(crate) lib_of: &'a dyn Fn(&FunctionId) -> Option<LibId>,
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
        | Op::Session { .. }
        | Op::WriteSession { .. }
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
        | Op::LetBind { .. }
        | Op::LetEnd { .. }
        | Op::JumpIfFalse { .. }
        | Op::Jump { .. }
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

/// Whether `op` allocates, calls a native, reads a member, holds a value
/// other than a scalar or declares an expression's variable, whose memory
/// it holds: what makes its statement's plan allocate.
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
        | Op::LetBind { .. }
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
/// a scalar plan charges it, over one or two operands, and an entry that
/// answers immediates: a template with heap paths alone is an allocating
/// plan's. A scalar plan's operator takes no heap path.
fn scalar_site(function: &Lib, count: usize) -> bool {
    (1..=2).contains(&count)
        && function
            .prepared
            .as_ref()
            .is_some_and(|(entry, fast)| fast.scalar().is_some() && entry.immediate())
}

/// What forming a plan gathers across its statements: the call depth its
/// sites need, and its session bindings in the order they are first used.
#[derive(Default)]
struct Gathered {
    depth: Option<u32>,
    sessions: Vec<BindingName>,
}

impl Gathered {
    /// The session slot of `name`, given one on its first use.
    fn session(&mut self, name: &BindingName) -> RegionSlot {
        let at = match self.sessions.iter().position(|held| held.name == name.name) {
            Some(at) => at,
            None => {
                self.sessions.push(name.clone());
                self.sessions.len() - 1
            }
        };
        RegionSlot::Session(at as u32)
    }
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
        let mut gathered = Gathered::default();
        let (head, block) = match &statement.form {
            Form::While {
                metered,
                test,
                body,
                ..
            } => {
                let units = vec![Event::Fixed(Weight::Atomic(*metered))];
                let test =
                    self.summarize(code, *test, End::Test, *metered, units, &mut gathered)?;
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
                    self.site(item, 0, 2, &mut defined, &mut formula_max, &mut gathered)?;
                    program.push(Event::Formula);
                }
                let charge = Summary {
                    ops: OpRange { start: 0, len: 0 },
                    fixed: *metered,
                    formula_max,
                    program: lean(program),
                    arms: Box::default(),
                    reads: Box::default(),
                    write: Some(RegionSlot::Frame(binding.at)),
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
        let body = self.body(code, block, &mut gathered, 0)?;
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
            depth: gathered.depth,
            calls: calls.into(),
            sessions: gathered.sessions.into(),
            compiled: OnceLock::new(),
        })
    }

    /// The plan of block `block`, `nesting` `if`s deep in a loop's body.
    fn body(
        &self,
        code: &Code,
        block: BlockId,
        gathered: &mut Gathered,
        nesting: usize,
    ) -> Option<Body> {
        if nesting > NESTING {
            return None;
        }
        let lowered = (self.lowered.block)(block);
        // The region stops at the block's first statement it leaves, so it
        // never reaches the ones after it: they are the reference's, unread.
        let mut left = false;
        let steps = lowered
            .stmts
            .iter()
            .map(|id| {
                if left {
                    return Some(Step::Leave);
                }
                let statement = (self.lowered.stmt)(*id);
                // A statement-root invoke of a prepared operator runs in
                // the region; any other action is a barrier.
                if let Form::Action(_) = statement.form {
                    let used = gathered.sessions.len();
                    let step = self.action_summary(code, *id, gathered);
                    if step.is_none() {
                        gathered.sessions.truncate(used);
                        left = true;
                    }
                    return Some(step.unwrap_or(Step::Leave));
                }
                if barrier(&self.lowered, statement) {
                    left = true;
                    return Some(Step::Leave);
                }
                let unit = || vec![Event::Fixed(Weight::Atomic(1))];
                match &statement.form {
                    Form::Plain => {
                        let last = self.stream_op(statement.ops.end().checked_sub(1)?)?;
                        let end = End::of(last)?;
                        let summary =
                            self.summarize(code, statement.ops, end, 1, unit(), gathered)?;
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
                        test: self.summarize(
                            code,
                            statement.ops,
                            End::Branch,
                            1,
                            unit(),
                            gathered,
                        )?,
                        then: self.body(code, *then_block, gathered, nesting + 1)?,
                        otherwise: self.body(code, *else_block, gathered, nesting + 1)?,
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
    /// Keep its action form so a decline resumes the original continuation:
    /// a write or a declaration of its value, or a store whose ops take it
    /// from the first register.
    fn action_summary(&self, code: &Code, id: StmtId, gathered: &mut Gathered) -> Option<Step> {
        let statement = (self.lowered.stmt)(id);
        let (action, lib, args) = library_invoke(&statement.form)?;
        let stream = self.lowered.stream;
        let ops = stream
            .ops
            .get(statement.ops.start as usize..statement.ops.end() as usize)?;
        let end = match ops {
            [Op::Act, end] => end,
            [Op::Act, .., Op::SetIndex { .. } | Op::SetField { .. }] => {
                return self.action_store(code, id, gathered).map(Step::Store);
            }
            _ => return None,
        };
        let mut reads = Vec::new();
        for arg in args {
            reads.extend(Self::atom(arg, gathered)?);
        }
        let (write, bind) = match end {
            Op::WriteLocal { local, .. } | Op::Bind { local, .. }
                if code.slots.get(local.slot as usize)?.shared =>
            {
                return None;
            }
            Op::WriteLocal { local, .. } => (RegionSlot::Frame(local.at), false),
            Op::Bind { local, .. } => (RegionSlot::Frame(local.at), true),
            Op::WriteSession { name, .. } => {
                (gathered.session(stream.names.get(name.ix())?), false)
            }
            _ => return None,
        };
        let mut defined = vec![true; args.len()];
        let mut formula_max = Some(0);
        self.site(lib, 0, args.len(), &mut defined, &mut formula_max, gathered)?;
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
            arms: Box::default(),
            reads: reads.into(),
            write: Some(write),
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

    /// The summary of statement-root invoke `id` whose continuation stores
    /// its value: the invoke's events, then its ops' after the action, as
    /// a plain store's, the value in the first register.
    fn action_store(&self, code: &Code, id: StmtId, gathered: &mut Gathered) -> Option<Summary> {
        let statement = (self.lowered.stmt)(id);
        let (action, lib, args) = library_invoke(&statement.form)?;
        let mut shared = Shared::default();
        for arg in args {
            shared.reads.extend(Self::atom(arg, gathered)?);
        }
        if !matches!(action.site.unit, lash_kernel_doc::Unit::Library(_)) {
            shared.calls.push(action.occurrence);
        }
        let mut walk = Walk {
            defined: vec![true; args.len()],
            fixed: 1 + args.len() as u64,
            formula_max: Some(0),
            program: vec![Event::Fixed(Weight::Atomic(1)); 1 + args.len()],
            allocates: !scalar_site((self.lowered.lib)(lib), args.len()),
        };
        walk.program.push(Event::Formula);
        self.site(
            lib,
            0,
            args.len(),
            &mut walk.defined,
            &mut walk.formula_max,
            gathered,
        )?;
        let mut summary = self.finish(
            code,
            statement.ops,
            statement.ops.start + 1,
            End::Store,
            walk,
            shared,
            gathered,
        )?;
        summary.action = Some(id);
        Some(summary)
    }

    /// The slot a statement-root invoke's argument `atom` reads, if any:
    /// an unshared slot, a session binding or a resident scalar.
    fn atom(atom: &super::Atom, gathered: &mut Gathered) -> Option<Option<RegionSlot>> {
        match atom {
            super::Atom::Var(super::Var::Local(local)) if !local.shared => {
                Some(Some(RegionSlot::Frame(local.at)))
            }
            super::Atom::Var(super::Var::Session(name)) => Some(Some(gathered.session(name))),
            super::Atom::Literal(Constant::Resident(value)) if scalar(value) => Some(None),
            _ => None,
        }
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
        fixed: u64,
        program: Vec<Event>,
        gathered: &mut Gathered,
    ) -> Option<Summary> {
        let walk = Walk {
            defined: Vec::new(),
            fixed,
            formula_max: Some(0),
            program,
            allocates: false,
        };
        self.finish(
            code,
            range,
            range.start,
            end,
            walk,
            Shared::default(),
            gathered,
        )
    }

    /// The summary of the region statement in `range`, which ends as
    /// `end`, from its ops `from..` on: `walk` and `shared` hold what its
    /// ops before `from` did.
    #[expect(
        clippy::too_many_arguments,
        reason = "a statement's summary continues the walk its start made"
    )]
    fn finish(
        &self,
        code: &Code,
        range: OpRange,
        from: u32,
        end: End,
        mut walk: Walk,
        mut shared: Shared,
        gathered: &mut Gathered,
    ) -> Option<Summary> {
        let stream = self.lowered.stream;
        let ops = stream.ops.get(range.start as usize..range.end() as usize)?;
        let (last, _) = ops.split_last()?;
        walk.allocates |= general(stream, self.lowered.lib, *last);
        self.walk(
            code,
            from,
            range.end() - 1,
            &mut walk,
            &mut shared,
            gathered,
            false,
        )?;
        if !shared.lets.is_empty() {
            return None;
        }
        let Walk {
            mut defined,
            fixed,
            formula_max,
            program,
            allocates,
        } = walk;
        let Shared {
            reads, calls, arms, ..
        } = shared;
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
                Some(RegionSlot::Frame(local.at))
            }
            (Op::WriteSession { name, src }, End::Write) if take(&mut defined, src.ix()) => {
                Some(gathered.session(stream.names.get(name.ix())?))
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
            arms: arms.into(),
            reads: reads.into(),
            write,
            general: allocates,
            calls: calls.into(),
            action: None,
        })
    }

    /// Walks the ops `start..end` of a region statement into `walk`: an
    /// arm of a conditional expression when `arm`, whose invokes would be
    /// counted only on its path, which no static program states.
    #[expect(
        clippy::too_many_arguments,
        reason = "a statement's walk and its arms share the statement's tables"
    )]
    fn walk(
        &self,
        code: &Code,
        start: u32,
        end: u32,
        walk: &mut Walk,
        shared: &mut Shared,
        gathered: &mut Gathered,
        arm: bool,
    ) -> Option<()> {
        let stream = self.lowered.stream;
        // Takes the registers `start..start + count` and defines `start`.
        let gather = |defined: &mut Vec<bool>, start: usize, count: usize| {
            (start..start + count).all(|at| take(defined, at)) && define(defined, start)
        };
        let mut pc = start;
        while pc < end {
            let op = *stream.ops.get(pc as usize)?;
            walk.allocates |= general(stream, self.lowered.lib, op);
            let defined = &mut walk.defined;
            match op {
                Op::Region { region, total } => {
                    if total == u32::MAX {
                        return None;
                    }
                    walk.fixed = walk.fixed.checked_add(u64::from(total))?;
                    let events = stream.regions.events(region.0);
                    walk.program
                        .extend(events.iter().map(|event| Event::Fixed(event.weight)));
                }
                Op::Local { dst, local } => {
                    if code.slots.get(local.slot as usize)?.shared || !define(defined, dst.ix()) {
                        return None;
                    }
                    shared.reads.push(RegionSlot::Frame(local.at));
                }
                Op::Session { dst, name } => {
                    if !define(defined, dst.ix()) {
                        return None;
                    }
                    shared
                        .reads
                        .push(gathered.session(stream.names.get(name.ix())?));
                }
                Op::Const { dst, c } => match stream.consts.get(c.ix())? {
                    Constant::Resident(_) if define(defined, dst.ix()) => {}
                    _ => return None,
                },
                Op::Operator { lib, args } => {
                    let (start, count) = (args.start.ix(), args.len as usize);
                    self.site(
                        LibId(lib.0),
                        start,
                        count,
                        defined,
                        &mut walk.formula_max,
                        gathered,
                    )?;
                    walk.program.push(Event::Formula);
                }
                Op::Nested { act, args } => {
                    let nested = stream.nested.get(act.ix())?;
                    let lib = nested.kind.operator_lib()?;
                    if !nested.operator {
                        return None;
                    }
                    let (start, count) = (args.start.ix(), args.len as usize);
                    self.site(lib, start, count, defined, &mut walk.formula_max, gathered)?;
                    if matches!(nested.kind, NestedKind::Call(_))
                        && !matches!(nested.site.unit, lash_kernel_doc::Unit::Library(_))
                    {
                        if arm {
                            return None;
                        }
                        shared.calls.push(nested.occurrence);
                    }
                    walk.program.push(Event::Formula);
                }
                Op::Call { args, .. } => {
                    if !gather(defined, args.start.ix(), args.len as usize) {
                        return None;
                    }
                    walk.formula_max = None;
                    walk.program.push(Event::Formula);
                }
                Op::MakeTuple { items } | Op::MakeList { items } | Op::MakeSet { items } => {
                    if !gather(defined, items.start.ix(), items.len as usize) {
                        return None;
                    }
                }
                Op::MakeMap { entries } => {
                    if !gather(defined, entries.start.ix(), entries.len as usize) {
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
                    if !short || !gather(defined, values.start.ix(), values.len as usize) {
                        return None;
                    }
                }
                Op::Field { at, .. } => {
                    if !gather(defined, at.ix(), 1) {
                        return None;
                    }
                }
                Op::Index { at, .. } => {
                    if !gather(defined, at.ix(), 2) {
                        return None;
                    }
                }
                // A pin keeps a value its statement still uses.
                Op::Pin { src } => {
                    if !defined.get(src.ix()).copied().unwrap_or(false) {
                        return None;
                    }
                }
                // An expression's variable, declared for its body: the
                // value its body computes, in the same register, is the
                // expression's.
                Op::LetBind { local, src } => {
                    if code.slots.get(local.slot as usize)?.shared || !take(defined, src.ix()) {
                        return None;
                    }
                    shared.lets.push((local.at, src.ix()));
                }
                Op::LetEnd { local } => {
                    let (at, reg) = shared.lets.pop()?;
                    if at != local.at || !defined.get(reg).copied().unwrap_or(false) {
                        return None;
                    }
                }
                Op::JumpIfFalse { cond, skip } => {
                    if !take(defined, cond.ix()) {
                        return None;
                    }
                    let (then_end, else_end) = arm_ends(stream, pc, skip)?;
                    let lets = shared.lets.len();
                    let mut arms: [Option<Walk>; 2] = [None, None];
                    for (at, (from, to)) in [(pc + 1, then_end - 1), (then_end, else_end)]
                        .into_iter()
                        .enumerate()
                    {
                        let mut taken = Walk {
                            defined: Vec::new(),
                            fixed: 0,
                            formula_max: Some(0),
                            program: Vec::new(),
                            allocates: false,
                        };
                        self.walk(code, from, to, &mut taken, shared, gathered, true)?;
                        // An arm computes the expression's value alone,
                        // into the condition's register.
                        if shared.lets.len() != lets
                            || !take(&mut taken.defined, cond.ix())
                            || taken.defined.iter().any(|held| *held)
                        {
                            return None;
                        }
                        arms[at] = Some(taken);
                    }
                    let [Some(then), Some(otherwise)] = arms else {
                        return None;
                    };
                    walk.allocates |= then.allocates || otherwise.allocates;
                    let most = |arm: &Walk| arm.fixed.checked_add(arm.formula_max?);
                    walk.formula_max = match (walk.formula_max, most(&then), most(&otherwise)) {
                        (Some(sum), Some(a), Some(b)) => sum.checked_add(a.max(b)),
                        _ => None,
                    };
                    walk.program
                        .push(Event::Arm(u32::try_from(shared.arms.len()).ok()?));
                    shared.arms.push(Arms {
                        then: lean(then.program),
                        otherwise: lean(otherwise.program),
                        then_fixed: then.fixed,
                        otherwise_fixed: otherwise.fixed,
                    });
                    if !define(&mut walk.defined, cond.ix()) {
                        return None;
                    }
                    pc = else_end;
                    continue;
                }
                _ => return None,
            }
            pc += 1;
        }
        Some(())
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
        gathered: &mut Gathered,
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
        // An installed native answers where the entry declines: its heap
        // paths never run.
        let deepest = match function.run {
            LibRun::Native { .. } => entry.depth(),
            _ => entry.deepest(self.lowered.lib, self.lowered.lib_of),
        };
        if function.operation() || deepest > 0 {
            gathered.depth = Some(gathered.depth.unwrap_or(0).max(deepest));
        }
        Some(())
    }
}

/// What a statement's walk, or an arm's, has seen so far: which registers
/// hold a value not yet used, the fixed units and the formulas' most it
/// charges on its path, its program and whether it allocates.
struct Walk {
    defined: Vec<bool>,
    fixed: u64,
    formula_max: Option<u64>,
    program: Vec<Event>,
    allocates: bool,
}

/// What a statement's walk and its arms share: the slots read, the invoke
/// occurrences, the arms and the open expression variables.
#[derive(Default)]
struct Shared {
    reads: Vec<RegionSlot>,
    calls: Vec<usize>,
    arms: Vec<Arms>,
    /// Each open expression variable's frame position and the register
    /// its body's value goes to.
    lets: Vec<(u32, usize)>,
}

/// Where the arms of the conditional whose `JumpIfFalse` at `pc` skips
/// `skip` ops end: the then-arm runs to its `Jump` before the first, the
/// else-arm from there to the second.
pub(crate) fn arm_ends(stream: &Stream, pc: u32, skip: u32) -> Option<(u32, u32)> {
    let then_end = pc.checked_add(1)?.checked_add(skip)?;
    let Op::Jump { skip: rest } = *stream.ops.get(then_end.checked_sub(1)? as usize)? else {
        return None;
    };
    Some((then_end, then_end.checked_add(rest)?))
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

impl LoopPlan {
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
            sessions: self.sessions.clone(),
            compiled: OnceLock::new(),
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
                    sessions: &self.sessions,
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
                Step::Write(summary) => match summary.write? {
                    RegionSlot::Frame(at) => CompiledStep::Write {
                        value: value(stream, libs, summary)?,
                        at,
                        commit: summary.into(),
                    },
                    RegionSlot::Session(at) => CompiledStep::Session {
                        value: value(stream, libs, summary)?,
                        at,
                        commit: summary.into(),
                    },
                },
                Step::Bind(summary) => CompiledStep::Bind {
                    value: value(stream, libs, summary)?,
                    at: match summary.write? {
                        RegionSlot::Frame(at) => at,
                        RegionSlot::Session(_) => return None,
                    },
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
                    let act = match summary.action {
                        Some(id) => {
                            let (_, target, args) = library_invoke(&libs.exe.stmt(id).form)?;
                            Some(nodes::action(libs, target, args)?)
                        }
                        None => None,
                    };
                    let mut compiled = nodes::compile_from(stream, libs, summary.ops, act)?;
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
