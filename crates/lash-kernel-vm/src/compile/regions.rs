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
//! the statements it leaves to the reference. Formation reads each region
//! statement as its resolved expression tree ([`super::expr`]) and derives
//! the summary from it, keeping the tree, from which the compiled form
//! ([`nodes`]) is built on first entry and cached in the plan.
//! [`LoopPlan::verify`] checks the plan against the lowered code, its
//! stream's `RegionTable` events and the statement units, never against the
//! tree, before lowering admits it. Plans belong to the executable:
//! deterministic, immutable and never saved. A
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
//! An operator whose heap paths write a guest object
//! ([`lash_kernel_doc::plan::HeapTemplate::writes`]) runs only as its
//! statement's last evaluation before a `do` ends it: its write is held
//! until the statement commits and applied then, so that a statement
//! that misses leaves nothing written for the reference to write again,
//! and nothing the statement evaluates reads the object the write is
//! held for. Anywhere else such a call is a barrier.
//!
//! A session binding is a region slot between boundaries (§3.6): a plan
//! names the bindings its statements read and write ([`LoopPlan::sessions`]),
//! and a region holds their resident values from its entry and publishes
//! what it wrote at its exit.

use std::sync::OnceLock;

use lash_kernel_doc::FunctionId;
use lash_kernel_doc::arena::ShortText;
use lash_kernel_exec::{ActIx, Op, OpRange, Reg, Weight};

mod accumulators;
mod calls;
mod cx;
mod nodes;
mod typed;
mod verify;

pub(crate) use cx::{Accumulator, Cx, Guests, Made, measured_formula};
pub(crate) use nodes::{
    Commit, CompiledBody, CompiledHead, CompiledLoop, CompiledStep, Fresh, Hook, Meter, Node,
    Place, Role, SessionSlot, Taken, position, scalar, store,
};
pub(crate) use typed::{Headroom, Ran, Slots, Typed};

use super::expr::{self, Expr, Kind};
use super::{
    BindingName, Block, BlockId, Code, CodeId, Constant, Executable, Form, Lib, LibId, LibRun,
    NestedCallee, NestedKind, Stmt, StmtId, Stream,
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
    /// A guest call through its callee's island ([`GuestCall`]), which
    /// charges and records its own events as the reference does.
    Call,
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

/// A guest call a region statement makes through its callee's island
/// (`docs/design/kernel-regions.md` §5 R4): a call nested in a plain
/// statement whose operands before it are reads, or a statement's root
/// call action. The region pays what the statement charges before the call
/// when the island runs, counting the call's occurrence there as the
/// reference does; once it ran, a statement that does not commit leaves its
/// frame in the statement, for the reference to run on.
#[derive(Clone, Debug)]
pub(crate) struct GuestCall {
    pub(crate) stmt: StmtId,
    /// A nested call's entry; `None` for a root call action.
    pub(crate) act: Option<ActIx>,
    pub(crate) occurrence: usize,
    /// Its declared callee; `None` for a closure its operand reads.
    pub(crate) callee: Option<CodeId>,
    /// The fixed events the statement charges before the call, in order,
    /// and their units.
    pub(crate) before: Box<[Weight]>,
    pub(crate) units: u64,
    /// The program's events up to and including the call's.
    pub(crate) through: usize,
    /// The least the statement and the next test's unit charge: what must
    /// fit the bound before it pays anything.
    pub(crate) least: u64,
    /// The op the statement runs on from once the call returned, and the
    /// register its value goes to.
    pub(crate) resume: u32,
    pub(crate) reg: u32,
    /// The registers the statement holds across the call: each with the
    /// read that gave it, as the continuation's roles take them.
    pub(crate) holds: Box<[(u32, Kept)]>,
}

/// What a register a statement holds across its guest call was read from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Kept {
    Slot(RegionSlot),
    Value(lash_kernel_doc::Value),
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
    /// The values its last op takes, read from its ops' tree, each with
    /// its role, in the order the statement completes them: none for a
    /// `continue` or a statement-root invoke's write.
    roots: Box<[(Role, Expr)]>,
    /// Where a store writes.
    place: Option<Place>,
    /// Its guest call, if it makes one.
    pub(crate) guest: Option<GuestCall>,
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

    /// The occurrences it counts, in op order: its guest call's, which
    /// its operands before it never count, then those it commits.
    fn counted(&self, calls: &mut Vec<usize>) {
        calls.extend(self.guest.as_ref().map(|guest| guest.occurrence));
        calls.extend_from_slice(&self.calls);
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
                | Step::Discard(summary) => summary.counted(calls),
                Step::If {
                    test,
                    then,
                    otherwise,
                } => {
                    test.counted(calls);
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
    /// A `for` stepped by the loop's own cursor over a list's or a tuple's
    /// items, or a map's or a set's keys: its scheduled units and its
    /// `item` hook's formula, in `charge` (no ops); a guarded table's step
    /// also runs its protocol's `check` hook first, in `guarded`, which
    /// only an allocating plan states; the variable each element is bound
    /// to, and the hooks.
    For {
        charge: Summary,
        guarded: Option<Box<Summary>>,
        binding: u32,
        item: Option<LibId>,
        check: Option<LibId>,
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
    /// Whether declared code has a call island.
    pub(crate) island: &'a dyn Fn(CodeId) -> bool,
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
    if !writes_last(lowered, ops) {
        return true;
    }
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
            Some(nested) => !guest_site(nested),
            None => true,
        },
        Op::Call { lib, args } => !native_site((lowered.lib)(LibId(lib.0)), args.len as usize),
        _ => true,
    })
}

/// Whether operator `lib`'s heap paths, or a callee's entry below them,
/// may write a guest object: an installed native answers where its entry
/// declines, so its heap paths never run.
fn writes(lowered: &Lowered<'_>, lib: LibId) -> bool {
    let function = (lowered.lib)(lib);
    match &function.prepared {
        Some((entry, _)) if !matches!(function.run, LibRun::Native { .. }) => {
            entry.writes(lowered.lib, lowered.lib_of)
        }
        _ => false,
    }
}

/// Whether the statement of `ops` calls an operator that writes
/// ([`writes`]) only as its last evaluation before the `do` that ends it,
/// the call whose value the `do` discards: then what it writes commits
/// with the statement, and nothing the statement evaluates after reads
/// the object the write is held for.
fn writes_last(lowered: &Lowered<'_>, ops: &[Op]) -> bool {
    let stream = lowered.stream;
    let writer = |op: &Op| {
        let lib = match *op {
            Op::Operator { lib, .. } => Some(LibId(lib.0)),
            Op::Nested { act, .. } => stream
                .nested
                .get(act.ix())
                .and_then(|nested| nested.kind.operator_lib()),
            _ => None,
        };
        lib.is_some_and(|lib| writes(lowered, lib))
    };
    let before = match ops {
        [before @ .., _, Op::End] => before,
        _ => ops,
    };
    !before.iter().any(writer)
}

/// Whether nested action `nested` is a guest call that may enter its
/// callee's island: a closure's, or a declared function's that has one.
fn guest_site(nested: &super::Nested) -> bool {
    nested.island
        && matches!(
            nested.kind,
            NestedKind::Call(NestedCallee::Read | NestedCallee::Declared(_))
        )
}

/// Whether `statement` makes a guest call a region may run through its
/// callee's island: one its derivation may refuse, which leaves the
/// statement to the reference as a barrier is.
fn guest_call(lowered: &Lowered<'_>, statement: &Stmt) -> bool {
    if let Form::Action(action) = &statement.form {
        return matches!(
            action.kind,
            super::ActionKind::Call {
                callee: super::Callee::Declared(_) | super::Callee::Value(_),
                ..
            }
        );
    }
    let stream = lowered.stream;
    stream
        .ops
        .get(statement.ops.start as usize..statement.ops.end() as usize)
        .is_some_and(|ops| {
            ops.iter().any(|op| {
                matches!(op, Op::Nested { act, .. }
                    if stream.nested.get(act.ix()).is_some_and(guest_site))
            })
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
    /// loop region. Charged code forms one whether its run charges it or,
    /// with charge Off, paces it: lowered under Off, its summaries' fixed
    /// units are its pace (statement, action-atom and loop-test units).
    pub(super) fn form(&self, code: &Code, id: StmtId) -> Option<LoopPlan> {
        if !code.charged {
            return None;
        }
        let statement = (self.lowered.stmt)(id);
        let mut gathered = Gathered::default();
        let (mut head, block) = match &statement.form {
            Form::While {
                metered,
                test,
                body,
                ..
            } => {
                let units = vec![Event::Fixed(Weight::Atomic(*metered))];
                let test = self.summarize(
                    code,
                    *test,
                    End::Test,
                    (*metered, units),
                    None,
                    &mut gathered,
                )?;
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
                let charge = self.head_charge(*metered, binding.at, item, None, &mut gathered)?;
                let head = Head::For {
                    charge,
                    guarded: None,
                    binding: binding.at,
                    item,
                    check: via.and_then(|via| via.check),
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
        // An allocating plan also walks a map's or a set's keys under its
        // protocol's guard: `check` runs before `item` at each step. A
        // scalar plan walks no guarded table.
        if allocates
            && let Head::For {
                charge,
                guarded,
                binding,
                item,
                check: Some(check),
            } = &mut head
        {
            let metered = charge.fixed;
            *guarded = self
                .head_charge(metered, *binding, *item, Some(*check), &mut gathered)
                .map(Box::new);
        }
        let iteration_max = if allocates {
            0
        } else {
            head.summary()
                .max()?
                .checked_add(body.max()?)?
                .checked_add(self.loop_test)?
        };
        let binds = matches!(head, Head::For { .. }) || body.binds();
        let mut calls = Vec::new();
        if let Head::While(test) = &head {
            test.counted(&mut calls);
        }
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

    /// What a `for`'s head charges each step: its scheduled units, its
    /// protocol's `check` formula when it guards the step, then its `item`
    /// hook's formula, binding the variable at `binding`.
    fn head_charge(
        &self,
        metered: u64,
        binding: u32,
        item: Option<LibId>,
        check: Option<LibId>,
        gathered: &mut Gathered,
    ) -> Option<Summary> {
        let mut program = vec![Event::Fixed(Weight::Atomic(metered))];
        let mut formula_max = Some(0);
        let mut general = false;
        for (hook, count) in check
            .map(|check| (check, 4))
            .into_iter()
            .chain(item.map(|item| (item, 2)))
        {
            // A hook's write is the reference's: what an operator writes
            // commits only with a `do`.
            if writes(&self.lowered, hook) {
                return None;
            }
            general |= !scalar_site((self.lowered.lib)(hook), count);
            self.site(hook, count, &mut formula_max, gathered)?;
            program.push(Event::Formula);
        }
        Some(Summary {
            ops: OpRange { start: 0, len: 0 },
            fixed: metered,
            formula_max,
            program: lean(program),
            arms: Box::default(),
            reads: Box::default(),
            write: Some(RegionSlot::Frame(binding)),
            general,
            calls: Box::default(),
            action: None,
            roots: Box::default(),
            place: None,
            guest: None,
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
                // A statement-root invoke of a prepared operator, or a
                // guest call through its callee's island, runs in the
                // region; any other action is a barrier.
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
                let used = gathered.sessions.len();
                let step = match &statement.form {
                    Form::Plain => self.plain(code, *id, gathered),
                    Form::If {
                        then_block,
                        else_block,
                    } => (|| {
                        Some(Step::If {
                            test: self.summarize(
                                code,
                                statement.ops,
                                End::Branch,
                                (1, unit()),
                                None,
                                gathered,
                            )?,
                            then: self.body(code, *then_block, gathered, nesting + 1)?,
                            otherwise: self.body(code, *else_block, gathered, nesting + 1)?,
                        })
                    })(),
                    _ => None,
                };
                // A guest call its derivation refuses leaves the statement
                // to the reference, as a barrier does.
                if step.is_none() && guest_call(&self.lowered, statement) {
                    gathered.sessions.truncate(used);
                    left = true;
                    return Some(Step::Leave);
                }
                step
            })
            .collect::<Option<Box<[Step]>>>()?;
        Some(Body {
            block,
            steps,
            declares: lowered.declares.clone().into(),
        })
    }

    /// The step of plain body statement `id`, whose guest call, if it
    /// makes one, runs through its callee's island.
    fn plain(&self, code: &Code, id: StmtId, gathered: &mut Gathered) -> Option<Step> {
        let statement = (self.lowered.stmt)(id);
        let last = self.stream_op(statement.ops.end().checked_sub(1)?)?;
        let end = End::of(last)?;
        let unit = vec![Event::Fixed(Weight::Atomic(1))];
        let summary = self.summarize(code, statement.ops, end, (1, unit), Some(id), gathered)?;
        Some(match end {
            End::Write => Step::Write(summary),
            End::Bind => Step::Bind(summary),
            End::Store => Step::Store(summary),
            End::Discard => Step::Discard(summary),
            _ => Step::Continue(summary),
        })
    }

    /// A root invoke has atom charge events rather than expression ops.
    /// Keep its action form so a decline resumes the original continuation:
    /// a write or a declaration of its value, or a store whose ops take it
    /// from the first register; a guest call's value may also be
    /// discarded.
    fn action_summary(&self, code: &Code, id: StmtId, gathered: &mut Gathered) -> Option<Step> {
        let statement = (self.lowered.stmt)(id);
        let (action, callee, args) = root_call(&statement.form)?;
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
        let (walk, shared, guest) = self.action_head(action, callee, args, gathered)?;
        let (write, step): (_, fn(Summary) -> Step) = match end {
            Op::WriteLocal { local, .. } | Op::Bind { local, .. }
                if code.slots.get(local.slot as usize)?.shared =>
            {
                return None;
            }
            Op::WriteLocal { local, .. } => (Some(RegionSlot::Frame(local.at)), Step::Write),
            Op::Bind { local, .. } => (Some(RegionSlot::Frame(local.at)), Step::Bind),
            Op::WriteSession { name, .. } => (
                Some(gathered.session(stream.names.get(name.ix())?)),
                Step::Write,
            ),
            Op::End if guest.is_some() => (None, Step::Discard),
            _ => return None,
        };
        let Walk {
            fixed,
            formula_max,
            program,
            allocates,
        } = walk;
        let guest = match guest {
            Some(callee) => Some(self.guest(
                id,
                (None, action.occurrence, callee),
                (&program, fixed),
                (statement.ops.start + 1, 0),
                Box::default(),
            )?),
            None => None,
        };
        Some(step(Summary {
            ops: statement.ops,
            fixed,
            formula_max,
            program: program.into(),
            arms: Box::default(),
            reads: shared.reads.into(),
            write,
            general: allocates,
            calls: shared.calls.into(),
            action: Some(id),
            roots: Box::default(),
            place: None,
            guest,
        }))
    }

    /// The summary of statement-root invoke `id` whose continuation stores
    /// its value: the invoke's events, then its continuation's, as a plain
    /// store's, the value in the first register.
    fn action_store(&self, code: &Code, id: StmtId, gathered: &mut Gathered) -> Option<Summary> {
        let statement = (self.lowered.stmt)(id);
        let (action, callee, args) = root_call(&statement.form)?;
        let (walk, shared, guest) = self.action_head(action, callee, args, gathered)?;
        let tree = expr::statement(self.lowered.stream, statement.ops, true)?;
        let mut summary = self.finish(
            code,
            statement.ops,
            End::Store,
            walk,
            shared,
            tree,
            gathered,
        )?;
        summary.action = Some(id);
        if let Some(callee) = guest {
            summary.guest = Some(self.guest(
                id,
                (None, action.occurrence, callee),
                (&summary.program, summary.fixed),
                (statement.ops.start + 1, 0),
                Box::default(),
            )?);
        }
        Some(summary)
    }

    /// What statement-root call `action` of `callee` on `args` charges and
    /// reads before its continuation: its unit, a unit per argument, then
    /// a library function's formula, counting its occurrence when the
    /// statement commits; or a guest call through its callee's island,
    /// which counts it as it runs. A guest call's callee is read before
    /// its arguments; it is given as the declared code it calls, if any.
    fn action_head(
        &self,
        action: &super::Action,
        callee: &super::Callee,
        args: &[super::Atom],
        gathered: &mut Gathered,
    ) -> Option<(Walk, Shared, Option<Option<CodeId>>)> {
        let mut shared = Shared::default();
        let guest = match callee {
            super::Callee::Library(_) => None,
            super::Callee::Declared(code) => (self.lowered.island)(*code).then_some(Some(*code)),
            super::Callee::Value(var) => {
                shared.reads.push(Self::var(var, gathered)?);
                Some(None)
            }
        };
        for arg in args {
            shared.reads.extend(Self::atom(arg, gathered)?);
        }
        let counted = !matches!(action.site.unit, lash_kernel_doc::Unit::Library(_));
        let units = 1 + args.len() as u64;
        let mut program = vec![Event::Fixed(Weight::Atomic(1)); 1 + args.len()];
        let walk = match callee {
            super::Callee::Library(lib) => {
                // A write its value then takes is the reference's: what
                // the operator writes commits only with a `do`.
                if writes(&self.lowered, *lib) {
                    return None;
                }
                if counted {
                    shared.calls.push(action.occurrence);
                }
                program.push(Event::Formula);
                let mut walk = Walk::new(units, program);
                walk.allocates = !scalar_site((self.lowered.lib)(*lib), args.len());
                self.site(*lib, args.len(), &mut walk.formula_max, gathered)?;
                walk
            }
            _ => {
                guest?;
                if !counted {
                    return None;
                }
                program.push(Event::Call);
                let mut walk = Walk::new(units, program);
                walk.allocates = true;
                walk.formula_max = None;
                walk
            }
        };
        Some((walk, shared, guest))
    }

    /// The guest call `(act, occurrence, callee)` of statement `stmt`,
    /// whose leaned program and fixed units are `(program, fixed)`: it
    /// runs on at `resume` with its value in register `reg`, holding
    /// `holds` across the call. Only fixed events precede it.
    fn guest(
        &self,
        stmt: StmtId,
        (act, occurrence, callee): (Option<ActIx>, usize, Option<CodeId>),
        (program, fixed): (&[Event], u64),
        (resume, reg): (u32, u32),
        holds: Box<[(u32, Kept)]>,
    ) -> Option<GuestCall> {
        let at = program.iter().position(|event| *event == Event::Call)?;
        let before = program[..at]
            .iter()
            .map(|event| match event {
                Event::Fixed(weight) => Some(*weight),
                _ => None,
            })
            .collect::<Option<Box<[Weight]>>>()?;
        Some(GuestCall {
            stmt,
            act,
            occurrence,
            callee,
            units: before.iter().map(|weight| weight.units()).sum(),
            before,
            through: at + 1,
            least: fixed.checked_add(self.loop_test)?,
            resume,
            reg,
            holds,
        })
    }

    /// The guest call nested at `site` of the statement whose values are
    /// `roots`: every value computed before it is a read, and each register
    /// the statement holds across it is one, which an exit after it reads
    /// again.
    fn nested_guest(
        &self,
        roots: &[(Role, Expr)],
        (program, fixed): (&[Event], u64),
        site: GuestSite,
        gathered: &mut Gathered,
    ) -> Option<GuestCall> {
        let stream = self.lowered.stream;
        if !roots.iter().all(|(_, root)| reads_before(root, site.pc)) {
            return None;
        }
        let entry = stream.nested.get(site.act.ix())?;
        let holds = entry
            .roles
            .iter()
            .map(|role| {
                let mut found = None;
                for (_, root) in roots {
                    latest_read(root, role.reg.0, site.pc, &mut found);
                }
                let held = match &found?.kind {
                    Kind::Local { local, .. } => Kept::Slot(RegionSlot::Frame(local.at)),
                    Kind::Session { name, .. } => {
                        Kept::Slot(gathered.session(stream.names.get(name.ix())?))
                    }
                    Kind::Const(value) => Kept::Value(*value),
                    _ => return None,
                };
                Some((role.reg.0, held))
            })
            .collect::<Option<_>>()?;
        self.guest(
            site.stmt,
            (Some(site.act), site.occurrence, site.callee),
            (program, fixed),
            (site.pc + 1, site.reg),
            holds,
        )
    }

    /// The slot a guest call's callee variable `var` reads: a frame
    /// variable, a captured or shared one from its cell, or a session
    /// binding.
    fn var(var: &super::Var, gathered: &mut Gathered) -> Option<RegionSlot> {
        match var {
            super::Var::Local(local) => Some(RegionSlot::Frame(local.at)),
            super::Var::Session(name) => Some(gathered.session(name)),
            super::Var::Unbound(_) => None,
        }
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
    /// `end`, charged `fixed` units and `program` before its ops; a guest
    /// call of it is admitted when it is body statement `guest`'s.
    fn summarize(
        &self,
        code: &Code,
        range: OpRange,
        end: End,
        (fixed, program): (u64, Vec<Event>),
        guest: Option<StmtId>,
        gathered: &mut Gathered,
    ) -> Option<Summary> {
        let tree = expr::statement(self.lowered.stream, range, false)?;
        let walk = Walk::new(fixed, program);
        let shared = Shared {
            stmt: guest,
            ..Shared::default()
        };
        self.finish(code, range, end, walk, shared, tree, gathered)
    }

    /// The summary of the region statement in `range`, read as `tree`,
    /// which ends as `end`: `walk` and `shared` hold what it charges and
    /// reads before its ops.
    #[expect(
        clippy::too_many_arguments,
        reason = "a statement's summary continues what its start charged"
    )]
    fn finish(
        &self,
        code: &Code,
        range: OpRange,
        end: End,
        mut walk: Walk,
        mut shared: Shared,
        mut tree: expr::Statement,
        gathered: &mut Gathered,
    ) -> Option<Summary> {
        let stream = self.lowered.stream;
        let ops = stream.ops.get(range.start as usize..range.end() as usize)?;
        if !writes_last(&self.lowered, ops) {
            return None;
        }
        walk.allocates |= general(stream, self.lowered.lib, tree.last);
        let unshared = |slot: u32| code.slots.get(slot as usize).is_some_and(|s| !s.shared);
        // The values its last op takes, by role, and where a store writes.
        let (mut roots, place) = match (tree.last, end) {
            (Op::Test { cond }, End::Test) | (Op::Branch { cond, .. }, End::Branch) => {
                (vec![(Role::Value, tree.take(cond)?)], None)
            }
            (Op::WriteLocal { local, src }, End::Write) | (Op::Bind { local, src }, End::Bind)
                if unshared(local.slot) =>
            {
                (vec![(Role::Value, tree.take(src)?)], None)
            }
            (Op::WriteSession { src, .. }, End::Write) => {
                (vec![(Role::Value, tree.take(src)?)], None)
            }
            (Op::Continue, End::Continue) => (Vec::new(), None),
            (Op::SetIndex { target, src }, End::Store) => {
                let value = tree.take(src)?;
                let object = tree.take(target)?;
                let index = tree.take(Reg(target.0 + 1))?;
                let roots = vec![
                    (Role::Target, object),
                    (Role::Index, index),
                    (Role::Value, value),
                ];
                (roots, Some(Place::Index))
            }
            (Op::SetField { target, src, field }, End::Store) => {
                let value = tree.take(src)?;
                let object = tree.take(target)?;
                let name = ShortText::new(&stream.fields.get(field.ix())?.name)?;
                let roots = vec![(Role::Target, object), (Role::Value, value)];
                (
                    roots,
                    Some(Place::Field(lash_kernel_doc::Value::Text(name))),
                )
            }
            // A `do` discards what it computed last, in its first register.
            (Op::End, End::Discard) => (vec![(Role::Value, tree.take(Reg(0))?)], None),
            _ => return None,
        };
        // Every value the statement computes reaches its end.
        if !tree.spent() {
            return None;
        }
        // Its values are computed in the order the statement completes them.
        roots.sort_by_key(|(_, root)| root.pc);
        for (_, root) in &roots {
            self.expr(code, root, &mut walk, &mut shared, gathered, false)?;
        }
        walk.close(stream, &tree.closes)?;
        let site = shared.guest.take();
        let write = match tree.last {
            Op::WriteLocal { local, .. } | Op::Bind { local, .. } => {
                Some(RegionSlot::Frame(local.at))
            }
            Op::WriteSession { name, .. } => Some(gathered.session(stream.names.get(name.ix())?)),
            _ => None,
        };
        let Walk {
            fixed,
            formula_max,
            program,
            allocates,
        } = walk;
        let program = lean(program);
        let guest = match site {
            Some(site) => Some(self.nested_guest(&roots, (&program, fixed), site, gathered)?),
            None => None,
        };
        let Shared {
            reads, calls, arms, ..
        } = shared;
        Some(Summary {
            ops: range,
            fixed,
            formula_max,
            program,
            arms: arms.into(),
            reads: reads.into(),
            write,
            general: allocates,
            calls: calls.into(),
            action: None,
            roots: roots.into(),
            place,
            guest,
        })
    }

    /// Derives what `expr` charges, reads and counts into `walk` and
    /// `shared`, in the order the reference runs its ops: an arm's of a
    /// conditional expression when `arm`, whose invokes would be counted
    /// only on its path, which no static program states.
    fn expr(
        &self,
        code: &Code,
        expr: &Expr,
        walk: &mut Walk,
        shared: &mut Shared,
        gathered: &mut Gathered,
        arm: bool,
    ) -> Option<()> {
        let unshared = |slot: u32| code.slots.get(slot as usize).is_some_and(|s| !s.shared);
        match &expr.kind {
            // A guest call's callee may be a captured or shared variable,
            // read from its cell, which nothing writes while the region
            // runs: the region writes no cell, and an island only its own
            // frame's variables.
            Kind::Call {
                callee: expr::Callee::Read(read),
                args,
                ..
            } if matches!(read.kind, Kind::Local { local, scoped: false, .. } if !unshared(local.slot)) =>
            {
                let Kind::Local { local, .. } = read.kind else {
                    return None;
                };
                shared.reads.push(RegionSlot::Frame(local.at));
                for arg in args {
                    self.expr(code, arg, walk, shared, gathered, arm)?;
                }
            }
            _ => {
                for operand in expr.operands() {
                    self.expr(code, operand, walk, shared, gathered, arm)?;
                }
            }
        }
        let stream = self.lowered.stream;
        match &expr.kind {
            // A statement-root invoke's value: its events precede the ops.
            Kind::Act => {}
            Kind::Local { local, .. } => {
                if !unshared(local.slot) {
                    return None;
                }
                shared.reads.push(RegionSlot::Frame(local.at));
            }
            Kind::Session { name, .. } => {
                let name = stream.names.get(name.ix())?;
                shared.reads.push(gathered.session(name));
            }
            Kind::Const(value) => walk.allocates |= !scalar(value),
            Kind::Operator {
                lib,
                invoke,
                args,
                closes,
            } => {
                if invoke.is_some_and(|invoke| !invoke.operator) {
                    return None;
                }
                self.site(*lib, args.len(), &mut walk.formula_max, gathered)?;
                walk.allocates |= !scalar_site((self.lowered.lib)(*lib), args.len());
                if let Some(occurrence) = invoke.and_then(|invoke| invoke.counted()) {
                    if arm {
                        return None;
                    }
                    shared.calls.push(occurrence);
                }
                walk.close(stream, closes)?;
                walk.program.push(Event::Formula);
            }
            Kind::Native { closes, .. } => {
                walk.allocates = true;
                walk.formula_max = None;
                walk.close(stream, closes)?;
                walk.program.push(Event::Formula);
            }
            // A guest call that may enter its callee's island, one in a
            // body statement, on no conditional arm: what its island
            // charges no static program states.
            Kind::Call {
                invoke,
                callee,
                closes,
                ..
            } => {
                let stmt = shared.stmt?;
                let nested = stream.nested.get(invoke.act.ix())?;
                if arm || shared.guest.is_some() || !invoke.guest || !guest_site(nested) {
                    return None;
                }
                walk.close(stream, closes)?;
                walk.program.push(Event::Call);
                walk.formula_max = None;
                walk.allocates = true;
                shared.guest = Some(GuestSite {
                    stmt,
                    pc: expr.pc,
                    reg: expr.reg,
                    act: invoke.act,
                    occurrence: invoke.occurrence,
                    callee: match callee {
                        expr::Callee::Declared(code) => Some(*code),
                        expr::Callee::Read(_) => None,
                    },
                });
            }
            // A record's names are short texts the region writes as they
            // are.
            Kind::Record { fields, .. } => {
                let names = stream.field_lists.get(fields.ix())?;
                if !names.iter().all(|name| ShortText::new(name).is_some()) {
                    return None;
                }
                walk.allocates = true;
            }
            Kind::Make { .. } | Kind::Field { .. } | Kind::Index { .. } | Kind::Pin(_) => {
                walk.allocates = true;
            }
            // An expression's variable, declared for its body, whose memory
            // it holds.
            Kind::Let { local, .. } => {
                if !unshared(local.slot) {
                    return None;
                }
                walk.allocates = true;
            }
            Kind::Cond {
                closes,
                then,
                otherwise,
                ..
            } => {
                walk.close(stream, closes)?;
                let mut arm = |arm: &expr::Arm| -> Option<Walk> {
                    let mut taken = Walk::new(0, Vec::new());
                    self.expr(
                        code,
                        arm.value.as_deref()?,
                        &mut taken,
                        shared,
                        gathered,
                        true,
                    )?;
                    taken.close(stream, &arm.closes)?;
                    Some(taken)
                };
                let then = arm(then)?;
                let otherwise = arm(otherwise)?;
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
            }
        }
        Some(())
    }

    /// Operator `lib` called with `count` arguments, which a region runs
    /// when it has a prepared entry: its scalar table's maximum, if it has
    /// one, adds to `formula_max`.
    fn site(
        &self,
        lib: LibId,
        count: usize,
        formula_max: &mut Option<u64>,
        gathered: &mut Gathered,
    ) -> Option<()> {
        let function = (self.lowered.lib)(lib);
        if !operator_site(function, count) {
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

/// What a statement's derivation, or an arm's, has seen so far: the fixed
/// units and the formulas' most it charges on its path, its program and
/// whether it allocates.
struct Walk {
    fixed: u64,
    formula_max: Option<u64>,
    program: Vec<Event>,
    allocates: bool,
}

impl Walk {
    fn new(fixed: u64, program: Vec<Event>) -> Self {
        Self {
            fixed,
            formula_max: Some(0),
            program,
            allocates: false,
        }
    }

    /// The charge regions `closes` of `stream`, charged where an op closes
    /// them: their totals and their events.
    fn close(&mut self, stream: &Stream, closes: &[expr::Close]) -> Option<()> {
        for close in closes {
            self.fixed = self.fixed.checked_add(u64::from(close.total))?;
            self.program.extend(
                stream
                    .regions
                    .events(close.region)
                    .iter()
                    .map(|event| Event::Fixed(event.weight)),
            );
        }
        Some(())
    }
}

/// What a statement's derivation and its arms share: the slots read, the
/// invoke occurrences and the arms; the body statement whose guest call
/// it may admit, and the one it admitted.
#[derive(Default)]
struct Shared {
    reads: Vec<RegionSlot>,
    calls: Vec<usize>,
    arms: Vec<Arms>,
    stmt: Option<StmtId>,
    guest: Option<GuestSite>,
}

/// A guest call a derivation admitted, nested in statement `stmt`: its
/// op and register, its entry and occurrence, and its declared callee.
struct GuestSite {
    stmt: StmtId,
    pc: u32,
    reg: u32,
    act: ActIx,
    occurrence: usize,
    callee: Option<CodeId>,
}

/// Whether every value of `expr` computed before op `pc` is a read.
fn reads_before(expr: &Expr, pc: u32) -> bool {
    let read = matches!(
        expr.kind,
        Kind::Local { scoped: false, .. } | Kind::Session { .. } | Kind::Const(_)
    );
    let arms = match &expr.kind {
        Kind::Cond {
            then, otherwise, ..
        } => [then, otherwise].iter().all(|arm| {
            arm.value
                .as_deref()
                .is_none_or(|value| reads_before(value, pc))
        }),
        _ => true,
    };
    (read || expr.pc >= pc) && arms && expr.operands().all(|operand| reads_before(operand, pc))
}

/// The read of `expr` into register `reg` latest before op `pc`.
fn latest_read<'e>(expr: &'e Expr, reg: u32, pc: u32, found: &mut Option<&'e Expr>) {
    if expr.reg == reg && expr.pc < pc && found.is_none_or(|found| found.pc < expr.pc) {
        *found = Some(expr);
    }
    for operand in expr.operands() {
        latest_read(operand, reg, pc, found);
    }
}

/// `program` without the zero-unit events the reference never records.
fn lean(program: Vec<Event>) -> Box<[Event]> {
    program
        .into_iter()
        .filter(|event| *event != Event::Fixed(Weight::Atomic(0)))
        .collect()
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
                    charging: exe.charging(),
                    sessions: &self.sessions,
                };
                let head = match &self.head {
                    Head::While(test) => CompiledHead::While {
                        test: value(stream, &libs, test)?,
                        commit: test.into(),
                    },
                    Head::For {
                        charge,
                        guarded,
                        binding,
                        item,
                        check,
                    } => CompiledHead::For {
                        hook: match item {
                            Some(item) => Some(nodes::hook(&libs, *item)?),
                            None => None,
                        },
                        guard: match (guarded, check) {
                            (Some(guarded), Some(check)) => {
                                Some((nodes::check(&libs, *check)?, (&**guarded).into()))
                            }
                            _ => None,
                        },
                        commit: charge.into(),
                        binding: *binding,
                    },
                };
                // A scalar plan's `while` runs its test and its body's
                // first typed run as one.
                let iteration = match &self.head {
                    Head::While(test) if !libs.general => {
                        typed::compile(exe, libs.charging, Some(test), &self.body, 0)
                    }
                    _ => None,
                };
                Some(CompiledLoop {
                    head,
                    body: compile_body(stream, &libs, self, &self.body)?,
                    iteration,
                })
            })
            .as_ref()
    }
}

/// The call a statement's root call action makes: its callee and its
/// arguments.
fn root_call(form: &Form) -> Option<(&super::Action, &super::Callee, &[super::Atom])> {
    let Form::Action(action) = form else {
        return None;
    };
    let super::ActionKind::Call { callee, args } = &action.kind else {
        return None;
    };
    Some((action, callee, args))
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
/// statement-root invoke's operator call or guest call.
fn value(stream: &Stream, libs: &Libs<'_>, summary: &Summary) -> Option<nodes::Node> {
    match (summary.action, &*summary.roots) {
        (Some(id), []) => root(libs, summary, id),
        (None, [(Role::Value, root)]) => {
            nodes::compile(stream, libs, root, &mut None, summary.guest.as_ref())
        }
        _ => None,
    }
}

/// The value of statement `id`'s root call action, whose summary is
/// `summary`: its library function's, or its guest call's.
fn root(libs: &Libs<'_>, summary: &Summary, id: StmtId) -> Option<nodes::Node> {
    let (_, callee, args) = root_call(&libs.exe.stmt(id).form)?;
    match (callee, &summary.guest) {
        (super::Callee::Library(lib), None) => nodes::action(libs, *lib, args),
        (_, Some(guest)) => nodes::root_call(libs, guest, callee, args),
        _ => None,
    }
}

/// `body`'s statements, compiled; `plan` is the loop's.
fn compile_body(
    stream: &Stream,
    libs: &Libs<'_>,
    plan: &LoopPlan,
    body: &Body,
) -> Option<CompiledBody> {
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
                        accumulates: accumulators::accumulates(libs, plan, summary, at),
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
                    condition: value(stream, libs, test)?,
                    commit: test.into(),
                    then: compile_body(stream, libs, plan, then)?,
                    otherwise: compile_body(stream, libs, plan, otherwise)?,
                },
                Step::Continue(summary) => CompiledStep::Continue(summary.into()),
                Step::Store(summary) => {
                    // A statement-root invoke's value is its continuation's
                    // first operand.
                    let mut act = match summary.action {
                        Some(id) => Some(root(libs, summary, id)?),
                        None => None,
                    };
                    let guest = summary.guest.as_ref().filter(|_| summary.action.is_none());
                    let operands = summary
                        .roots
                        .iter()
                        .map(|(role, root)| {
                            Some((*role, nodes::compile(stream, libs, root, &mut act, guest)?))
                        })
                        .collect::<Option<_>>()?;
                    CompiledStep::Store {
                        operands,
                        place: summary.place.clone()?,
                        commit: summary.into(),
                    }
                }
                Step::Discard(summary) => CompiledStep::Discard {
                    value: value(stream, libs, summary)?,
                    commit: summary.into(),
                },
                Step::Leave => CompiledStep::Leave,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    // A scalar plan's straight-line runs of typed statements each run as
    // one from their first statement (§2.4).
    let mut steps = steps;
    let mut at = 0;
    while at < steps.len() && !libs.general {
        let Some(run) = typed::compile(libs.exe, libs.charging, None, body, at) else {
            at += 1;
            continue;
        };
        let covers = run.covers;
        let step = std::mem::replace(&mut steps[at], CompiledStep::Leave);
        steps[at] = CompiledStep::Typed(Box::new((run, step)));
        at += covers;
    }
    let steps = steps.into();
    Some(CompiledBody {
        block: body.block,
        declares: body.declares.clone(),
        steps,
    })
}
