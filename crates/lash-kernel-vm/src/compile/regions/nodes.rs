//! A loop plan's compiled form (`docs/design/kernel-regions.md` §2.1):
//! each region statement's expression tree ([`super::super::expr`])
//! compiled into a tree of closures over the frame's slots and a region's
//! context ([`Cx`]). A node
//! answers its value, or `None` on a guard miss, which exits the region
//! before the statement, where the reference runs it.
//!
//! A scalar plan's nodes hold scalars alone and charge each operator its
//! scalar table, whose maximum the plan's iteration maximum sums. An
//! allocating plan's nodes hold any value (§5, R5): they build records,
//! lists, maps, sets and tuples, call natives and read fields and indexes
//! through the per-site property caches, and charge each formula as it
//! computes. What they build is installed as the reference installs it,
//! its growth held first from the nursery the region admitted at its entry
//! (§3.5), so that no admission inside the region refuses. A miss after an
//! object was built leaves it garbage, held as the reference holds what it
//! builds, and released by count as the reference's garbage is.

use std::ops::Range;

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunks};
use lash_kernel_doc::plan::Table;
use lash_kernel_doc::{NewObject, Value, ValueKind};
use lash_kernel_state::arena::VALUE_BYTES;

use super::super::expr::{self, Callee, Expr, Kind, Make};
use super::super::{
    BindingName, BlockId, Constant, Executable, Lib, LibId, LibRun, ReadSite, Source, Stream,
    find_property, write_property,
};
use super::GuestCall;
use super::calls;
use super::cx::Cx;
use crate::machine::SlotState;

/// What a compiled expression charges as it runs.
pub(crate) struct Meter {
    /// Its formulas' units, and the fixed units of the conditional arms
    /// it took.
    pub(crate) units: u64,
    /// Each formula's units and each arm taken, in order, when a charge
    /// tape records them.
    pub(crate) taken: Vec<Taken>,
    record: bool,
    /// The events of its program a charge tape recorded before its guest
    /// call ran, which its commit records on from and clears.
    pub(crate) recorded: usize,
}

/// What a statement's run chose of its static program: a formula's units,
/// or the arm a conditional took.
#[derive(Clone, Copy)]
pub(crate) enum Taken {
    Formula(u64),
    Arm(bool),
}

impl Meter {
    pub(crate) fn new(record: bool) -> Self {
        Self {
            units: 0,
            taken: Vec::new(),
            record,
            recorded: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.units = 0;
        self.taken.clear();
    }

    /// A scalar table's formula: its units are bounded, and the plan's
    /// iteration maximum, which admitted them, is a sum that fits.
    #[inline(always)]
    fn formula(&mut self, units: u64) {
        self.units += units;
        if self.record {
            self.taken.push(Taken::Formula(units));
        }
    }

    /// A formula measured on the values it is given, which no maximum
    /// bounds: the sum saturates, so that the bound refuses it.
    #[inline(always)]
    pub(super) fn measured(&mut self, units: u64) {
        self.units = self.units.saturating_add(units);
        if self.record {
            self.taken.push(Taken::Formula(units));
        }
    }

    /// A conditional took its arm `taken`, whose fixed units are `fixed`:
    /// its summary's most bounds them with the dearer arm's.
    #[inline(always)]
    fn arm(&mut self, taken: bool, fixed: u64) {
        self.units = self.units.saturating_add(fixed);
        if self.record {
            self.taken.push(Taken::Arm(taken));
        }
    }
}

/// A session binding a region holds as a slot (`docs/design/kernel-regions.md`
/// §3.6): its resident value from the region's entry, and which of the
/// region's writes wrote it last.
#[derive(Clone, Copy, Default)]
pub(crate) struct SessionSlot {
    /// `None` where the cell holds no resident value for it: a read or a
    /// write of it misses, and the reference loads it.
    pub(crate) value: Option<Value>,
    /// The region's session writes up to and including its last write of
    /// this binding; 0 when the region has not written it.
    pub(crate) written: u64,
}

/// A compiled region expression: it reads the frame's slots and answers
/// its value, or `None` on a guard miss.
pub(crate) type Node = Box<dyn Fn(&[SlotState], &mut Cx<'_>) -> Option<Value> + Send + Sync>;

/// A compiled `item` hook: what the loop walks and the stepped item to the
/// value bound, metered, or `None` on a guard miss.
pub(crate) type Hook = Box<dyn Fn(Value, Value, &mut Cx<'_>) -> Option<Value> + Send + Sync>;

/// A compiled `check` hook: what the loop walks, its size when the loop
/// opened it, the items still allowed and whether a step gave one, to the
/// budget the hook gives, metered; `None` on a guard miss.
pub(crate) type Check = Box<dyn Fn([Value; 4], &mut Cx<'_>) -> Option<Value> + Send + Sync>;

/// A loop plan's compiled form: its head and its body as trees that hold
/// what each statement charges, so that an iteration reads nothing else.
pub(crate) struct CompiledLoop {
    pub(crate) head: CompiledHead,
    pub(crate) body: CompiledBody,
    /// A `while`'s test and its body's first typed run, as one run
    /// (`docs/design/kernel-regions.md` §2.4).
    pub(crate) iteration: Option<super::Typed>,
}

/// What a compiled statement commits: its fixed units and its static
/// charge program (§3.1).
pub(crate) struct Commit {
    /// The fixed units it charges on every path.
    pub(crate) fixed: u64,
    /// The most the statement charges, when its formulas are bounded.
    pub(crate) max: Option<u64>,
    /// The most its formulas and its conditionals' arms charge, when they
    /// are bounded: its tables' maxima and each conditional's dearer arm.
    /// An allocating plan's operator may still measure more, where its
    /// typed path leaves the values to its formula.
    pub(crate) formulas: Option<u64>,
    /// The straight-line run of statements it starts in its block, in an
    /// allocating plan, where its most is bounded and it makes no guest
    /// call ([`CompiledBody::cover`]).
    pub(crate) run: Option<super::cover::Cover>,
    pub(crate) program: Box<[super::Event]>,
    /// Its conditionals' arms, which its program names.
    pub(crate) arms: Box<[super::Arms]>,
    /// The invoke occurrences it advances when it commits.
    pub(crate) calls: Box<[usize]>,
    /// Its guest call, where an exit after the call stands its frame.
    pub(crate) guest: Option<Box<GuestCall>>,
}

impl From<&super::Summary> for Commit {
    fn from(summary: &super::Summary) -> Self {
        Self {
            fixed: summary.fixed,
            max: summary.max(),
            formulas: summary.formula_max,
            run: None,
            program: summary.program.clone(),
            arms: summary.arms.clone(),
            calls: summary.calls.clone(),
            guest: summary.guest.clone().map(Box::new),
        }
    }
}

impl CompiledStep {
    /// What the statement commits; a statement the region leaves commits
    /// nothing.
    #[inline(always)]
    pub(crate) fn commit(&self) -> Option<&Commit> {
        match self {
            Self::Write { commit, .. }
            | Self::Bind { commit, .. }
            | Self::Session { commit, .. }
            | Self::If { commit, .. }
            | Self::Continue(commit)
            | Self::Store { commit, .. }
            | Self::Discard { commit, .. } => Some(commit),
            Self::Typed(typed) => typed.1.commit(),
            Self::Leave => None,
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
        /// A guarded table's `check` hook and what its step commits.
        guard: Option<(Check, Commit)>,
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
    /// `set` of an unshared slot: a linear text accumulator's when
    /// `accumulates` (`accumulators::accumulates`).
    Write {
        value: Node,
        at: u32,
        commit: Commit,
        accumulates: bool,
    },
    Bind {
        value: Node,
        at: u32,
        commit: Commit,
    },
    /// `set` of the session binding at this index of the plan's.
    Session {
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
    /// A heap write: its operands in the order the statement evaluates
    /// them, and the place it writes; a list append's shape where it is
    /// one.
    Store {
        operands: Box<[(Role, Node)]>,
        place: Place,
        commit: Commit,
        append: Option<Append>,
    },
    /// `do`: a value the statement discards.
    Discard {
        value: Node,
        commit: Commit,
    },
    /// The first statement of a typed run (§2.4): the run, and the
    /// statement as it runs alone where the run misses.
    Typed(Box<(super::Typed, CompiledStep)>),
    Leave,
}

/// A list append, `set t[list.len(t)] = v` of a frame variable `t`, in a
/// statement that makes no guest call. Where `t` holds a resident list,
/// its length is read once, for the index and for the write, and only the
/// value's node runs, a statement-root invoke's among them: no value the
/// statement computes can change the list before its write, as only a
/// `do` holds a write.
pub(crate) struct Append {
    /// The slot of the variable that holds the list.
    pub(crate) list: u32,
    /// `list.len`'s units ([`Lib::length_read`]).
    units: u64,
    /// The value's place among the store's operands.
    pub(crate) value: usize,
    /// Whether the statement evaluates its value before its index.
    value_first: bool,
}

/// The append shape of an allocating plan's store `summary`, where it has
/// one.
pub(super) fn append(libs: &Libs<'_>, summary: &super::Summary) -> Option<Append> {
    if !libs.general || summary.guest.is_some() || !matches!(summary.place, Some(Place::Index)) {
        return None;
    }
    let operand = |role| summary.roots.iter().position(|(held, _)| *held == role);
    let slot = |expr: &Expr| match &expr.kind {
        Kind::Local {
            local,
            scoped: false,
            ..
        } => Some(local.at),
        _ => None,
    };
    let (target, index, value) = (
        operand(Role::Target)?,
        operand(Role::Index)?,
        operand(Role::Value)?,
    );
    let list = slot(&summary.roots[target].1)?;
    let Kind::Native { lib, args, .. } = &summary.roots[index].1.kind else {
        return None;
    };
    let [read] = &args[..] else {
        return None;
    };
    if slot(read)? != list {
        return None;
    }
    Some(Append {
        list,
        units: libs.exe.lib(*lib).length_read()?,
        value,
        value_first: value < index,
    })
}

impl Append {
    /// The value the append of `value` writes, its index charged as
    /// `list.len` charges it ([`Cx::length`]), in the statement's order;
    /// `None` on a guard miss.
    #[inline(always)]
    pub(crate) fn value(
        &self,
        value: &Node,
        slots: &[SlotState],
        cx: &mut Cx<'_>,
    ) -> Option<Value> {
        if !self.value_first && cx.charging {
            cx.meter.measured(self.units);
        }
        let value = value(slots, cx)?;
        if self.value_first && cx.charging {
            cx.meter.measured(self.units);
        }
        Some(value)
    }

    /// Writes `value` at `length`, the end of `list`, as [`store`] writes
    /// it there; `None`, having written nothing, where the room does not
    /// hold its slot or the arena must decode first.
    #[inline(always)]
    pub(crate) fn write(
        cx: &mut Cx<'_>,
        list: ArenaHandle,
        length: u64,
        value: Value,
    ) -> Option<()> {
        if !cx.fits(VALUE_BYTES) {
            return None;
        }
        cx.arena.list_set(list, length, value).ok()
    }
}

/// What a store's operand is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Target,
    Index,
    Value,
}

/// Where a store writes in its target.
#[derive(Clone)]
pub(crate) enum Place {
    /// At the index its operands give.
    Index,
    /// The field of this name.
    Field(Value),
}

/// The values a region holds in registers and slots.
#[inline(always)]
pub(crate) fn scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
    )
}

/// The libraries a plan's nodes are compiled against.
pub(super) struct Libs<'e> {
    pub(super) exe: &'e Executable,
    /// Whether the plan allocates: its nodes hold any value.
    pub(super) general: bool,
    /// Whether the run is charged: a paced run's scalar nodes measure no
    /// formula.
    pub(super) charging: bool,
    /// The plan's session bindings.
    pub(super) sessions: &'e [BindingName],
}

impl Libs<'_> {
    /// The read of the session binding `name`, a slot of the plan's.
    fn session(&self, name: &BindingName) -> Option<Node> {
        let at = self
            .sessions
            .iter()
            .position(|held| held.name == name.name)?;
        Some(Box::new(move |_, cx| cx.sessions.get(at)?.value))
    }
}

/// The node of `expr`, a value of a region statement's tree: a
/// statement-root invoke's value is `act`'s, taken; a guest call is the
/// statement's `guest`.
pub(super) fn compile(
    stream: &Stream,
    libs: &Libs<'_>,
    expr: &Expr,
    act: &mut Option<Node>,
    guest: Option<&GuestCall>,
) -> Option<Node> {
    let mut node = |expr: &Expr| compile(stream, libs, expr, act, guest);
    Some(match &expr.kind {
        Kind::Act => act.take()?,
        // A read of an open expression variable is of the value its
        // declaration holds.
        Kind::Local {
            local,
            scoped: true,
            ..
        } => {
            let at = local.at;
            Box::new(move |_, cx| {
                cx.lets
                    .iter()
                    .rev()
                    .find(|(open, _)| *open == at)
                    .map(|(_, value)| *value)
            })
        }
        Kind::Local { local, .. } => {
            let at = local.at as usize;
            Box::new(move |slots, _| match slots.get(at) {
                Some(SlotState::Value(value)) => Some(*value),
                _ => None,
            })
        }
        Kind::Session { name, .. } => libs.session(stream.names.get(name.ix())?)?,
        Kind::Const(value) => {
            let value = *value;
            Box::new(move |_, _| Some(value))
        }
        Kind::Operator {
            lib, args, invoke, ..
        } if !invoke.is_some_and(|invoke| invoke.call)
            && let Some(node) = kind_node(libs, *lib, args, &mut node) =>
        {
            node
        }
        Kind::Operator { lib, args, .. } => {
            match fused(
                libs,
                *lib,
                args,
                |expr| Leaf::of(expr, &mut node),
                fresh(args),
            )
            .or_else(|| {
                calls::operator(
                    libs,
                    *lib,
                    args,
                    |expr| Leaf::of(expr, &mut node),
                    fresh(args),
                )
            }) {
                Some(called) => called,
                None => operator(libs, *lib, nodes(args, &mut node)?)?,
            }
        }
        Kind::Native { lib, args, .. } => call(libs, *lib, nodes(args, &mut node)?, fresh(args))?,
        Kind::Call { callee, args, .. } => {
            let read = match callee {
                Callee::Read(read) => Some(match read.kind {
                    Kind::Local {
                        local,
                        scoped: false,
                        ..
                    } => celled(local.at),
                    _ => node(read)?,
                }),
                Callee::Declared(_) => None,
            };
            guest_call(guest?.clone(), read, nodes(args, &mut node)?)?
        }
        Kind::Make { make, items, .. } => {
            let items = nodes(items, &mut node)?;
            match make {
                Make::Tuple => tuple(items),
                Make::List => list(items, false),
                Make::Set => list(items, true),
                Make::Map => map(items),
            }
        }
        Kind::Record { fields, values } => {
            let names = stream
                .field_lists
                .get(fields.ix())?
                .iter()
                .map(|name| lash_kernel_doc::arena::ShortText::new(name).map(Value::Text))
                .collect::<Option<Box<[Value]>>>()?;
            record(names, nodes(values, &mut node)?)
        }
        Kind::Field { target, field, .. } => {
            let field = stream.fields.get(field.ix())?;
            self::field(field, node(target)?)
        }
        Kind::Index {
            target,
            index,
            site,
            ..
        } => {
            let site = ReadSite::new(site.0, stream.layout);
            self::index(site, node(target)?, node(index)?)
        }
        // The reference's pin anchor roots the value for a sweep, which no
        // region runs.
        Kind::Pin(value) => node(value)?,
        Kind::Let {
            local, value, body, ..
        } => declared(local.at, node(value)?, node(body)?),
        Kind::Cond {
            cond,
            then,
            otherwise,
            ..
        } => {
            let mut arm = |arm: &expr::Arm| Some((node(arm.value.as_deref()?)?, arm.fixed()));
            let (then, otherwise) = (arm(then)?, arm(otherwise)?);
            conditional(node(cond)?, then, otherwise)
        }
    })
}

/// The nodes of `exprs`, in order.
fn nodes(exprs: &[Expr], node: &mut impl FnMut(&Expr) -> Option<Node>) -> Option<Vec<Node>> {
    exprs.iter().map(node).collect()
}

/// The compiled expression variable at frame position `at`, declared with
/// `value` for `body`: its value what `body`'s reads of it give. Its slot
/// is the frame's, counted when the frame was pushed.
fn declared(at: u32, value: Node, body: Node) -> Node {
    Box::new(move |slots, cx| {
        let value = value(slots, cx)?;
        cx.lets.push((at, value));
        let result = body(slots, cx);
        cx.lets.pop();
        result
    })
}

/// The compiled conditional of `condition` over its arms, each its value's
/// node and the fixed units it charges when taken.
fn conditional(condition: Node, then: (Node, u64), otherwise: (Node, u64)) -> Node {
    let ((then, then_fixed), (otherwise, otherwise_fixed)) = (then, otherwise);
    Box::new(move |slots, cx| match condition(slots, cx)? {
        Value::Bool(true) => {
            cx.meter.arm(true, then_fixed);
            then(slots, cx)
        }
        Value::Bool(false) => {
            cx.meter.arm(false, otherwise_fixed);
            otherwise(slots, cx)
        }
        _ => None,
    })
}

/// The compiled statement-root invoke of operator `id` on `args`, each a
/// frame variable or a resident literal.
pub(super) fn action(libs: &Libs<'_>, id: LibId, args: &[super::super::Atom]) -> Option<Node> {
    fused(
        libs,
        id,
        args,
        |atom| Leaf::atom(libs, atom),
        Fresh::default(),
    )
    .or_else(|| {
        calls::operator(
            libs,
            id,
            args,
            |atom| Leaf::atom(libs, atom),
            Fresh::default(),
        )
    })
    .or_else(|| operator(libs, id, atoms(libs, args)?))
}

/// The compiled statement-root guest call `call` of `callee` on `args`,
/// each a frame variable or a resident literal.
pub(super) fn root_call(
    libs: &Libs<'_>,
    call: &GuestCall,
    callee: &super::super::Callee,
    args: &[super::super::Atom],
) -> Option<Node> {
    let read = match callee {
        super::super::Callee::Declared(_) => None,
        super::super::Callee::Value(super::super::Var::Local(local)) => Some(celled(local.at)),
        super::super::Callee::Value(var) => Some(variable(libs, var)?),
        super::super::Callee::Library(_) => return None,
    };
    guest_call(call.clone(), read, atoms(libs, args)?)
}

/// The compiled reads of a root action's atoms `args`.
fn atoms(libs: &Libs<'_>, args: &[super::super::Atom]) -> Option<Vec<Node>> {
    args.iter()
        .map(|arg| -> Option<Node> {
            match arg {
                super::super::Atom::Var(var) => variable(libs, var),
                super::super::Atom::Literal(Constant::Resident(value)) => {
                    let value = *value;
                    Some(Box::new(move |_, _| Some(value)))
                }
                _ => None,
            }
        })
        .collect()
}

/// The compiled read of variable `var`: an unshared slot or a session
/// binding of the plan's.
fn variable(libs: &Libs<'_>, var: &super::super::Var) -> Option<Node> {
    match var {
        super::super::Var::Local(local) if !local.shared => {
            let at = local.at as usize;
            Some(Box::new(move |slots, _| match slots.get(at) {
                Some(SlotState::Value(value)) => Some(*value),
                _ => None,
            }))
        }
        super::super::Var::Session(name) => libs.session(name),
        _ => None,
    }
}

/// The compiled read of the frame variable at `at`, a guest call's callee:
/// its value, or a captured or shared variable's cell's.
fn celled(at: u32) -> Node {
    let at = at as usize;
    Box::new(move |slots, cx| match slots.get(at)? {
        SlotState::Value(value) => Some(*value),
        SlotState::Cell(cell) => cx.arena.cell(*cell).ok(),
        SlotState::Empty => None,
    })
}

/// The most arguments a region's guest call passes, inline.
const GUEST_ARGS: usize = 12;

/// The compiled guest call `call` of the closure `read` gives, or of its
/// declared callee, on `args`: run through the callee's island.
fn guest_call(call: GuestCall, read: Option<Node>, args: Vec<Node>) -> Option<Node> {
    if args.len() > GUEST_ARGS || read.is_some() == call.callee.is_some() {
        return None;
    }
    Some(Box::new(move |slots, cx| {
        let callee = match &read {
            Some(read) => Some(read(slots, cx)?),
            None => None,
        };
        let mut values = [Value::Absent; GUEST_ARGS];
        for (value, arg) in values.iter_mut().zip(&args) {
            *value = arg(slots, cx)?;
        }
        cx.guest(&call, callee, &values[..args.len()])
    }))
}

/// The values of `nodes`, in order, into a buffer of exactly their
/// count, which [`Cx::collecting`] admitted.
#[inline(always)]
fn values(nodes: &[Node], slots: &[SlotState], cx: &mut Cx<'_>) -> Option<Vec<Value>> {
    let mut values = Vec::with_capacity(nodes.len());
    for node in nodes {
        values.push(node(slots, cx)?);
    }
    Some(values)
}

/// Evaluates operands into a disjoint bounded window. A nested call may
/// reuse only the space after it. Every exit releases the window, so a miss
/// falls back before the statement without keeping an arena reference.
fn arguments<T>(
    nodes: &[Node],
    arity: usize,
    slots: &[SlotState],
    cx: &mut Cx<'_>,
    call: impl FnOnce(&mut Cx<'_>, Range<usize>) -> Option<T>,
) -> Option<T> {
    let window = cx.argument_window(arity)?;
    let answer = (|| {
        for (index, node) in nodes.iter().enumerate() {
            let value = node(slots, cx)?;
            cx.args.put(window.start + index, value);
        }
        for index in nodes.len()..arity {
            cx.args.put(window.start + index, Value::Absent);
        }
        call(cx, window.clone())
    })();
    cx.args.release(window.start);
    answer
}

/// A prepared entry and its scalar charge table: a scalar plan's operator
/// site's answer.
struct Answer {
    prepared: super::super::Entry,
    table: super::super::formula::ScalarTable,
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

    /// The entry on `args`: its scalar value, metered by its formula from
    /// the table, at the sizes its guard certifies or, where it certifies
    /// none, the values show. A paced run's (not `CHARGING`) runs the
    /// value-only guard, which answers and declines as the charged one.
    #[inline(always)]
    fn call<const CHARGING: bool>(&self, args: &[Value], meter: &mut Meter) -> Option<Value> {
        if !CHARGING {
            return self.prepared.call(args).filter(scalar);
        }
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
        meter.formula(self.table.units(mask)?);
        Some(result)
    }
}

/// An operand of a fused operator or an operator site: a frame variable or
/// a constant read in place, or any other expression's node.
pub(super) enum Leaf {
    Slot(usize),
    Const(Value),
    Node(Node),
}

impl Leaf {
    fn atom(libs: &Libs<'_>, atom: &super::super::Atom) -> Option<Self> {
        Some(match atom {
            super::super::Atom::Var(super::super::Var::Local(local)) if !local.shared => {
                Self::Slot(local.at as usize)
            }
            super::super::Atom::Literal(Constant::Resident(value)) => Self::Const(*value),
            super::super::Atom::Var(var) => Self::Node(variable(libs, var)?),
            _ => return None,
        })
    }

    fn of(expr: &Expr, node: &mut impl FnMut(&Expr) -> Option<Node>) -> Option<Self> {
        Some(match &expr.kind {
            Kind::Local {
                local,
                scoped: false,
                ..
            } => Self::Slot(local.at as usize),
            Kind::Const(value) => Self::Const(*value),
            _ => Self::Node(node(expr)?),
        })
    }

    /// The constant it reads, if it is one.
    pub(super) fn constant(&self) -> Option<Value> {
        match self {
            Self::Const(value) => Some(*value),
            Self::Slot(_) | Self::Node(_) => None,
        }
    }

    #[inline(always)]
    pub(super) fn eval(&self, slots: &[SlotState], cx: &mut Cx<'_>) -> Option<Value> {
        match self {
            Self::Slot(at) => match slots.get(*at) {
                Some(SlotState::Value(value)) => Some(*value),
                _ => None,
            },
            Self::Const(value) => Some(*value),
            Self::Node(node) => node(slots, cx),
        }
    }
}

/// The scalar size mask of a call on `args` that answered `result`, as
/// the entries certify it: bit `i` for argument `i` and bit 3 for the
/// result, set for a nonzero word integer.
#[inline(always)]
fn pair_sizes(x: Value, y: Value, result: Value) -> u8 {
    let bit = |value: Value| u8::from(matches!(value, Value::Int(word) if word != 0));
    bit(x) | bit(y) << 1 | bit(result) << 3
}

/// The answer of `pair`'s kernel on `x` and `y` when both are floats or
/// both word integers and it has a kernel for them: `None` where the
/// entry's general path answers.
#[inline(always)]
fn typed(pair: &lash_kernel_doc::scalar::Pair, x: Value, y: Value) -> Option<Value> {
    use lash_kernel_doc::scalar;
    match (x, y) {
        (Value::Float(p), Value::Float(q)) => scalar::floats(pair.floats?, p, q),
        (Value::Int(p), Value::Int(q)) => scalar::ints(pair.ints?, p, q),
        _ => None,
    }
}

/// The operator `id` of two operands, fused: its
/// operands read in place where they are variables or constants, and its
/// entry's typed path called on two floats or two word integers, its
/// answer charged by its scalar table as the entry's certified sizes
/// index it. Every other answer runs as the unfused node does.
fn fused<A>(
    libs: &Libs<'_>,
    id: LibId,
    args: &[A],
    mut leaf: impl FnMut(&A) -> Option<Leaf>,
    fresh: Fresh,
) -> Option<Node> {
    let [a, b] = args else {
        return None;
    };
    let function = libs.exe.lib(id);
    let pair = function.prepared.as_ref()?.0.pair()?;
    let answer = Answer::of(function)?;
    let (a, b) = (leaf(a)?, leaf(b)?);
    Some(match (libs.general, libs.charging) {
        (true, _) => {
            let site = calls::Site::of(libs, id, &[a.constant(), b.constant()], fresh)?;
            Box::new(move |slots, cx| {
                let x = a.eval(slots, cx)?;
                let y = b.eval(slots, cx)?;
                let Some(value) = typed(&pair, x, y) else {
                    return site.call(&[x, y], cx);
                };
                if !cx.charging {
                    return Some(value);
                }
                if scalar(&value) {
                    cx.meter
                        .measured(answer.table.units(pair_sizes(x, y, value))?);
                    return Some(value);
                }
                site.charged(&[x, y], value.into(), cx)
            })
        }
        (false, true) => Box::new(move |slots, cx| {
            let x = a.eval(slots, cx)?;
            let y = b.eval(slots, cx)?;
            let Some(value) = typed(&pair, x, y) else {
                if !scalar(&x) || !scalar(&y) {
                    return None;
                }
                return answer.call::<true>(&[x, y], &mut cx.meter);
            };
            cx.meter
                .formula(answer.table.units(pair_sizes(x, y, value))?);
            Some(value)
        }),
        (false, false) => Box::new(move |slots, cx| {
            let x = a.eval(slots, cx)?;
            let y = b.eval(slots, cx)?;
            let Some(value) = typed(&pair, x, y) else {
                if !scalar(&x) || !scalar(&y) {
                    return None;
                }
                return answer.call::<false>(&[x, y], &mut cx.meter);
            };
            Some(value)
        }),
    })
}

/// One kind-pure pair of calls. A region admits a whole statement before
/// committing any of its charge; its meter still records the two formulas
/// separately, in the reference's order, for the statement's replay.
fn kind_node(
    libs: &Libs<'_>,
    id: LibId,
    args: &[Expr],
    node: &mut impl FnMut(&Expr) -> Option<Node>,
) -> Option<Node> {
    if !libs.general {
        return None;
    }
    let (inner, literal) = super::super::kind_test::test(libs.exe, id, args)?;
    let Kind::Operator {
        lib: kind, args, ..
    } = &inner.kind
    else {
        return None;
    };
    let kind = *kind;
    let value = Leaf::of(&args[0], node)?;
    let memo = super::super::kind_test::KindMemo::default();
    Some(Box::new(move |slots, cx| {
        let x = value.eval(slots, cx)?;
        if cx.charging
            && let Some((answer, first, second)) = memo.get(x.kind())
        {
            cx.meter.measured(first);
            cx.meter.measured(second);
            return Some(Value::Bool(answer));
        }
        let before = cx.meter.units;
        let named = cx.operate_on(cx.exe.lib(kind), &[x], Fresh::default())?;
        let between = cx.meter.units;
        let answer = cx.operate_on(cx.exe.lib(id), &[named, literal], Fresh::default())?;
        if cx.charging
            && cx.meter.units != u64::MAX
            && let Value::Bool(answer) = answer
            && matches!(named, Value::Text(_))
        {
            memo.set(
                x.kind(),
                (answer, between - before, cx.meter.units - between),
            );
        }
        Some(answer)
    }))
}

/// The compiled operator `id` over `operands`.
fn operator(libs: &Libs<'_>, id: LibId, operands: Vec<Node>) -> Option<Node> {
    if libs.general {
        // More operands than a site holds in place ([`calls::operator`]):
        // evaluated into an operand window.
        libs.exe.lib(id).prepared.as_ref()?;
        let arity = operands.len();
        return Some(Box::new(move |slots, cx| {
            arguments(&operands, arity, slots, cx, |cx, args| {
                cx.operate(cx.exe.lib(id), args)
            })
        }));
    }
    let answer = Answer::of(libs.exe.lib(id))?;
    if libs.charging {
        scalar_operator::<true>(answer, operands)
    } else {
        scalar_operator::<false>(answer, operands)
    }
}

/// A scalar plan's operator over `operands`, answered by `answer`.
fn scalar_operator<const CHARGING: bool>(answer: Answer, operands: Vec<Node>) -> Option<Node> {
    let mut operands = operands.into_iter();
    let a = operands.next()?;
    Some(match operands.next() {
        None => Box::new(move |slots, cx| {
            let x = a(slots, cx)?;
            if !scalar(&x) {
                return None;
            }
            answer.call::<CHARGING>(&[x], &mut cx.meter)
        }),
        Some(b) => Box::new(move |slots, cx| {
            let x = a(slots, cx)?;
            let y = b(slots, cx)?;
            if !scalar(&x) || !scalar(&y) {
                return None;
            }
            answer.call::<CHARGING>(&[x, y], &mut cx.meter)
        }),
    })
}

/// The compiled general call of native `id` over `operands`, the
/// parameters they leave out absent.
fn call(libs: &Libs<'_>, id: LibId, operands: Vec<Node>, fresh: Fresh) -> Option<Node> {
    let function = libs.exe.lib(id);
    if !libs.general || operands.len() > function.arity {
        return None;
    }
    if let Some(table) = membership(function)
        && operands.len() == 2
    {
        let mut operands = operands.into_iter();
        return Some(contains(id, table, operands.next()?, operands.next()?));
    }
    if let Some(units) = function.length_read()
        && let [_] = operands[..]
    {
        return Some(length(id, units, operands.into_iter().next()?));
    }
    Some(direct(id, function.arity, operands, fresh))
}

/// The compiled `list.len` of native `id` on `list`, charging `units`
/// ([`Lib::length_read`]): its operand evaluated, then [`Cx::length`].
fn length(id: LibId, units: u64, list: Node) -> Node {
    Box::new(move |slots, cx| {
        let value = list(slots, cx)?;
        let function = cx.exe.lib(id);
        cx.length(function, units, value)
    })
}

/// The table whose membership test native `function` is, where the region
/// answers its call by [`Cx::contains`] (FIG-6820): `map.contains`,
/// `set.contains` and `record.contains`, unguarded, with no prepared entry.
fn membership(function: &Lib) -> Option<Table> {
    if !matches!(function.run, LibRun::Native { .. })
        || function.arity != 2
        || function.limit.is_some()
        || function.prepared.is_some()
    {
        return None;
    }
    match function.header.name.as_str() {
        "map.contains" => Some(Table::Map),
        "set.contains" => Some(Table::Set),
        "record.contains" => Some(Table::Record),
        _ => None,
    }
}

/// The compiled membership test of native `id` on `table`: its operands
/// evaluated in order, then [`Cx::contains`].
fn contains(id: LibId, table: Table, target: Node, key: Node) -> Node {
    Box::new(move |slots, cx| {
        let values = [target(slots, cx)?, key(slots, cx)?];
        let function = cx.exe.lib(id);
        cx.contains(function, table, values)
    })
}

/// Which of a call's operands are constructions of no members, by
/// position: a node's result whose measurements its kind proves
/// ([`crate::data::empty_measure`]), which the call's charge takes from it
/// rather than from the arena ([`Cx::operate_on`]).
#[derive(Clone, Copy, Default)]
pub(crate) struct Fresh(u16);

impl Fresh {
    /// The operands whose positions `mask` sets.
    pub(crate) fn of(mask: u16) -> Self {
        Self(mask)
    }

    /// Whether the operand at `at` is a construction of no members.
    #[inline(always)]
    pub(crate) fn empty(self, at: usize) -> bool {
        at < 16 && self.0 & (1 << at) != 0
    }
}

/// The constructions of no members among `args`.
fn fresh(args: &[Expr]) -> Fresh {
    let empty = |expr: &Expr| match &expr.kind {
        Kind::Make { items, .. } => items.is_empty(),
        Kind::Record { values, .. } => values.is_empty(),
        _ => false,
    };
    Fresh(
        args.iter()
            .take(16)
            .enumerate()
            .filter(|(_, arg)| empty(arg))
            .fold(0, |fresh, (at, _)| fresh | 1 << at),
    )
}

/// The most parameters a native's call evaluates in place: as many as an
/// operator site holds.
const DIRECT: usize = calls::IN_PLACE;

/// The call of native `id` of `arity` parameters over `operands`, the
/// parameters they leave out absent. Its operands are evaluated in order
/// in place, where its prepared entry answers them directly
/// ([`Cx::native_on`]); a call of more parameters evaluates them into an
/// operand window.
fn direct(id: LibId, arity: usize, operands: Vec<Node>, fresh: Fresh) -> Node {
    let operands = operands.into_boxed_slice();
    match arity {
        0 => in_place::<0>(id, operands, fresh),
        1 => in_place::<1>(id, operands, fresh),
        2 => in_place::<2>(id, operands, fresh),
        3 => in_place::<3>(id, operands, fresh),
        4 => in_place::<4>(id, operands, fresh),
        5 => in_place::<5>(id, operands, fresh),
        6 => in_place::<6>(id, operands, fresh),
        7 => in_place::<7>(id, operands, fresh),
        DIRECT => in_place::<DIRECT>(id, operands, fresh),
        _ => Box::new(move |slots, cx| {
            arguments(&operands, arity, slots, cx, |cx, args| {
                cx.native(cx.exe.lib(id), args)
            })
        }),
    }
}

/// [`direct`]'s call of `N` parameters, its operands' values held in
/// place.
fn in_place<const N: usize>(id: LibId, operands: Box<[Node]>, fresh: Fresh) -> Node {
    Box::new(move |slots, cx| {
        let mut values = [Value::Absent; N];
        for (value, operand) in values.iter_mut().zip(&operands) {
            *value = operand(slots, cx)?;
        }
        cx.native_on(cx.exe.lib(id), &values, fresh)
    })
}

/// The compiled tuple of `items`, refused where the reference raises
/// `too_deep`.
fn tuple(items: Vec<Node>) -> Node {
    Box::new(move |slots, cx| {
        let members = cx.collecting(ValueKind::Tuple, items.len(), |cx| {
            values(&items, slots, cx)
        })?;
        cx.arena.tuple(members, cx.limits.value_depth()).ok()
    })
}

/// The compiled list of `items`, or set when `set`: a set's members are
/// legal keys. A list its body holds inline collects its members in
/// place.
fn list(items: Vec<Node>, set: bool) -> Node {
    let kind = if set { ValueKind::Set } else { ValueKind::List };
    Box::new(move |slots, cx| {
        if !set && items.len() <= Chunks::INLINE {
            let mut inline = [Value::Absent; Chunks::INLINE];
            cx.collecting(kind, items.len(), |cx| {
                for (member, item) in inline.iter_mut().zip(&items) {
                    *member = item(slots, cx)?;
                }
                Some(())
            })?;
            let members = &inline[..items.len()];
            let bound = cx.arena.sequence_bound(members.len());
            return cx.made(bound, |arena| Ok(arena.create_list(members)));
        }
        let members = cx.collecting(kind, items.len(), |cx| values(&items, slots, cx))?;
        if set {
            if members.iter().any(|member| cx.arena.key(member).is_err()) {
                return None;
            }
            return cx.build(NewObject::Set(members));
        }
        cx.build(NewObject::List(members))
    })
}

/// The compiled map of the keys and values `entries` holds by turns,
/// evaluated into an operand window and built from it ([`Cx::map`]),
/// which every exit releases.
fn map(entries: Vec<Node>) -> Node {
    Box::new(move |slots, cx| {
        let window = cx.argument_window(entries.len())?;
        let map = (|| {
            cx.collecting(ValueKind::Map, entries.len() / 2, |cx| {
                for (at, entry) in entries.iter().enumerate() {
                    let value = entry(slots, cx)?;
                    cx.args.put(window.start + at, value);
                }
                Some(())
            })?;
            cx.map(window.clone())
        })();
        cx.args.release(window.start);
        map
    })
}

/// The compiled record of fields `names`, each with its value's node.
fn record(names: Box<[Value]>, values_of: Vec<Node>) -> Node {
    Box::new(move |slots, cx| {
        let fields = cx.collecting(ValueKind::Record, names.len(), |cx| {
            let mut fields = Vec::with_capacity(names.len());
            for (name, node) in names.iter().zip(&values_of) {
                fields.push((*name, node(slots, cx)?));
            }
            Some(fields)
        })?;
        cx.build(NewObject::Record(fields))
    })
}

/// The compiled read of `field` of the record or the error `target`
/// gives: a record's through its site's property cache, whose guard is
/// the cached slot's key (`compile::reads`).
fn field(field: &super::super::Field, target: Node) -> Node {
    let (name, key, site) = (field.name.clone(), field.key.clone(), field.site);
    Box::new(move |slots, cx| match target(slots, cx)? {
        Value::Record(record) => {
            if cx.arena.unresolved_key(record, &key).ok()? {
                return None;
            }
            let slot = site.and_then(|site| cx.sites.slot(site));
            let (value, found) = find_property(cx.arena, record, slot, &key).ok()?;
            if let Some(site) = site {
                if found != slot {
                    cx.sites.keep(site, found);
                }
                cx.keep_keyed(site, record);
            }
            Some(value.unwrap_or(Value::Absent))
        }
        Value::Error(error) => error_field(cx, error, &name),
        _ => None,
    })
}

/// Field `name` of error `error`, as the reference reads it.
pub(super) fn error_field(cx: &Cx<'_>, error: ArenaHandle, name: &str) -> Option<Value> {
    match cx.arena.body(error).ok()? {
        Body::Error(parts) => Some(match name {
            "kind" => parts.kind,
            "message" => parts.message,
            "data" => parts.data,
            _ => Value::Absent,
        }),
        _ => None,
    }
}

/// The list position `index` is, as the reference reads it; `None` where
/// it raises.
pub(crate) fn position(index: &Value) -> Option<u64> {
    match index {
        Value::Int(word) => u64::try_from(*word).ok(),
        Value::Float(float) => {
            let float = float.get();
            (float.fract() == 0.0 && float >= 0.0 && float < usize::MAX as f64)
                .then_some(float as u64)
        }
        _ => None,
    }
}

/// The compiled read of what `target` holds at `index`: a record's
/// through its site's property cache, if it has one.
fn index(site: Option<ReadSite>, target: Node, index: Node) -> Node {
    Box::new(move |slots, cx| {
        let target = target(slots, cx)?;
        let index = index(slots, cx)?;
        read_index(cx, site, target, index)
    })
}

/// What `target` holds at `index`, as the reference reads it: a record's
/// through `site`'s property cache, if it has one; `None` where the
/// reference raises or must decode first.
pub(super) fn read_index(
    cx: &mut Cx<'_>,
    site: Option<ReadSite>,
    target: Value,
    index: Value,
) -> Option<Value> {
    let arena = &*cx.arena;
    match target {
        Value::Tuple(object) | Value::List(object) => arena.item(object, position(&index)?).ok()?,
        Value::Record(record) => {
            if !matches!(index, Value::Text(_) | Value::LongText(_))
                || (arena.any_cold() && arena.unresolved(record, &index).ok()?.is_some())
            {
                return None;
            }
            let Some(site) = site else {
                return Some(
                    arena
                        .table_get(record, &index)
                        .ok()?
                        .unwrap_or(Value::Absent),
                );
            };
            let slot = cx.sites.slot(site);
            let key = arena.key(&index).ok()?;
            let (found, at) = find_property(arena, record, slot, &key).ok()?;
            if at != slot {
                cx.sites.keep(site, at);
            }
            cx.keep_keyed(site, record);
            Some(found.unwrap_or(Value::Absent))
        }
        Value::Map(map) => {
            if arena.unresolved(map, &index).ok()?.is_some() {
                return None;
            }
            arena.table_get(map, &index).ok()?
        }
        Value::Set(set) => {
            if arena.unresolved(set, &index).ok()?.is_some() {
                return None;
            }
            Some(Value::Bool(arena.table_get(set, &index).ok()?.is_some()))
        }
        _ => None,
    }
}

/// The compiled `item` hook `function`: what the loop walks and an item
/// it stepped to, as the reference's handoff passes them. A scalar plan's
/// hook takes scalar items alone.
pub(super) fn hook(libs: &Libs<'_>, id: LibId) -> Option<Hook> {
    if libs.general {
        libs.exe.lib(id).prepared.as_ref()?;
        return Some(Box::new(move |walked, raw, cx| {
            let function = cx.exe.lib(id);
            cx.operate_on(function, &[walked, raw], Fresh::default())
        }));
    }
    let answer = Answer::of(libs.exe.lib(id))?;
    Some(if libs.charging {
        scalar_hook::<true>(answer)
    } else {
        scalar_hook::<false>(answer)
    })
}

/// A scalar plan's `item` hook, answered by `answer`.
fn scalar_hook<const CHARGING: bool>(answer: Answer) -> Hook {
    Box::new(move |walked, raw, cx| {
        if !scalar(&raw) {
            return None;
        }
        answer.call::<CHARGING>(&[walked, raw], &mut cx.meter)
    })
}

/// A heap path's write to a guest object, checked as the reference's
/// member write checks it and held until its statement commits
/// ([`Cx::commit_write`]), its growth's capacity reserved out of the room. The
/// write is its operator's last evaluation and the operator its
/// statement's (`compile::regions`), so the statement evaluates nothing
/// after it that reads what it writes; a statement that misses drops it.
pub(crate) struct Held {
    pub(super) write: Prepared,
    pub(super) bound: u64,
}

/// A member write checked as the reference's checks it: what it writes
/// where, its growth not yet held.
pub(crate) enum Prepared {
    /// `value` at position `at` of `list`, appending at its length.
    List {
        list: ArenaHandle,
        at: u64,
        value: Value,
    },
    /// `written` under `key` of `table`.
    Table {
        table: ArenaHandle,
        key: Value,
        written: Value,
    },
    /// `key` out of `set`.
    Unset { set: ArenaHandle, key: Value },
}

/// The compiled `check` hook `function` of an allocating plan, as the
/// reference's handoff passes its arguments.
pub(super) fn check(libs: &Libs<'_>, id: LibId) -> Option<Check> {
    if !libs.general {
        return None;
    }
    libs.exe.lib(id).prepared.as_ref()?;
    Some(Box::new(move |args, cx| {
        let function = cx.exe.lib(id);
        cx.operate_on(function, &args, Fresh::default())
    }))
}

/// Writes `value` at `place` of `target`, with `index` for an index, as
/// the reference's member write does, marking what it changes dirty, its
/// growth (a new member's value slots) admitted by the room first; `None`,
/// having written nothing, where the reference raises or must decode
/// first.
pub(crate) fn store(
    cx: &mut Cx<'_>,
    place: &Place,
    target: Value,
    index: Value,
    value: Value,
) -> Option<()> {
    if let Value::Record(record) = target
        && cx.keyed.is_some_and(|(read, _)| read == record)
        && keyed_store(cx, place, record, index, value)
    {
        return Some(());
    }
    let (write, _) = prepare(cx, place, target, index, value)?;
    apply(cx, write)
}

/// Writes `value` under a record's short key where the record's last
/// read through a caching site found that key, and the record is still
/// the version that read proved it in: the write [`prepare`] checks and
/// [`apply`] makes, its growth admitted as theirs, without finding the
/// key again. `false`, having written nothing, otherwise. Out of line,
/// so that the other targets' stores stay [`prepare`]'s and [`apply`]'s.
#[inline(never)]
fn keyed_store(
    cx: &mut Cx<'_>,
    place: &Place,
    record: ArenaHandle,
    index: Value,
    value: Value,
) -> bool {
    let Some((read, site)) = cx.keyed else {
        return false;
    };
    let key = match (place, index) {
        (Place::Field(Value::Text(name)), _) => *name,
        (Place::Index, Value::Text(key)) => key,
        _ => return false,
    };
    // With no cold chunk anywhere, no key is unresolved.
    record == read
        && !cx.arena.any_cold()
        && cx.fits(2 * VALUE_BYTES)
        && cx.sites.slot(site).is_some_and(|slot| {
            write_property(cx.arena, record, slot, &key, value).is_ok_and(|written| written)
        })
}

/// The write of `value` at `place` of `target`, with `index` for an
/// index, checked as the reference's member write checks it, and the most
/// capacity its growth holds, which the room admits; `None` where the
/// reference raises or must decode first.
#[inline(always)]
pub(super) fn prepare(
    cx: &Cx<'_>,
    place: &Place,
    target: Value,
    index: Value,
    value: Value,
) -> Option<(Prepared, u64)> {
    let (table, key, written) = match (target, place) {
        (Value::List(list), Place::Index) => {
            let length = cx.arena.len(list).ok()?;
            let at = position(&index).filter(|at| *at <= length)?;
            if !cx.fits(VALUE_BYTES) {
                return None;
            }
            return Some((Prepared::List { list, at, value }, VALUE_BYTES));
        }
        (Value::Record(record), Place::Field(name)) => (record, *name, value),
        (Value::Record(record), Place::Index)
            if matches!(index, Value::Text(_) | Value::LongText(_)) =>
        {
            (record, index, value)
        }
        (Value::Map(map), Place::Index) => {
            cx.arena.key(&index).ok()?;
            (map, index, value)
        }
        (Value::Set(set), Place::Index) => {
            cx.arena.key(&index).ok()?;
            match value {
                Value::Bool(true) => (set, index, Value::Absent),
                Value::Bool(false) => {
                    if cx.arena.unresolved(set, &index).ok()?.is_some() {
                        return None;
                    }
                    return Some((Prepared::Unset { set, key: index }, 0));
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    let bound = 2 * VALUE_BYTES;
    if (cx.arena.any_cold() && cx.arena.unresolved(table, &key).ok()?.is_some()) || !cx.fits(bound)
    {
        return None;
    }
    Some((
        Prepared::Table {
            table,
            key,
            written,
        },
        bound,
    ))
}

/// Makes `write`, which [`prepare`] checked; `None` where the arena must
/// decode first. A record's short key that a read found still where it
/// was is written there, without finding it again.
#[inline(always)]
pub(super) fn apply(cx: &mut Cx<'_>, write: Prepared) -> Option<()> {
    match write {
        Prepared::List { list, at, value } => cx.arena.list_set(list, at, value).ok(),
        Prepared::Table {
            table,
            key,
            written,
        } => {
            if let (Some((record, site)), Value::Text(text)) = (cx.keyed, key)
                && record == table
                && let Some(slot) = cx.sites.slot(site)
                && write_property(cx.arena, table, slot, &text, written)
                    .is_ok_and(|written| written)
            {
                return Some(());
            }
            cx.arena.table_write(table, key, written).ok()
        }
        Prepared::Unset { set, key } => cx.arena.table_delete(set, &key).ok(),
    }
}
