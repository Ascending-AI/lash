//! A loop plan's compiled form (`docs/design/kernel-regions.md` §2.1):
//! each region statement's ops evaluated symbolically into a tree of
//! closures over the frame's slots and a region's context ([`Cx`]). A node
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

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::heap::{HeapFault, LogicalBytes};
use lash_kernel_doc::{
    Args, KernelDataLimits, Measure, NativeCall, NewObject, Nursery, Value, WorkCounter,
};
use lash_kernel_exec::{Op, OpRange, RegRange};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::PagedHeap;

use super::super::{
    BindingName, BlockId, Charge, Constant, Executable, Lib, LibId, LibRun, Plan, ReadSite, Source,
    Stream, find_property,
};
use crate::machine::SlotState;
use crate::machine::registers::Arguments;
use crate::machine::sites::SiteCaches;

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

    /// A scalar table's formula: its units are bounded, and the plan's
    /// iteration maximum, which admitted them, is a sum that fits.
    #[inline(always)]
    fn formula(&mut self, units: u64) {
        self.units += units;
        if self.record {
            self.formulas.push(units);
        }
    }

    /// A formula measured on the values it is given, which no maximum
    /// bounds: the sum saturates, so that the bound refuses it.
    #[inline(always)]
    fn measured(&mut self, units: u64) {
        self.units = self.units.saturating_add(units);
        if self.record {
            self.formulas.push(units);
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

/// What a region's statements read, build and hold with: the meter of
/// the running statement, the executable's libraries, the arena, the
/// heap's census, the machine's property caches and the region's session
/// slots.
pub(crate) struct Cx<'a> {
    pub(crate) meter: Meter,
    pub(crate) args: Arguments,
    pub(crate) args_reserved: bool,
    pub(crate) exe: &'a Executable,
    pub(crate) arena: &'a mut Arena,
    pub(crate) heap: &'a mut PagedHeap,
    pub(crate) sites: &'a mut SiteCaches,
    /// The session bindings the plan names, by [`super::LoopPlan::sessions`]
    /// index.
    pub(crate) sessions: &'a mut [SessionSlot],
    pub(crate) limits: KernelDataLimits,
    /// Whether memory is accounted (`MemoryMeter` not off).
    pub(crate) memory: bool,
    /// The room the nursery admitted at the region's entry still has: the
    /// growth the region may hold before it exits (§3.5).
    pub(crate) room: u64,
    /// What the running statement reserves until it ends, out of the
    /// room: its pins and its native calls' work, as the reference's
    /// statement credit.
    pub(crate) transient: u64,
    /// The fault a node met, which ends the run: a miss is no fault.
    pub(crate) fault: Option<HeapFault>,
}

impl Cx<'_> {
    /// Admits the entire retained inline capacity once per statement,
    /// before exposing a window. Exhaustion exits to the reference path.
    fn argument_window(&mut self, count: usize) -> Option<Range<usize>> {
        if !self.args_reserved {
            if !self.reserve(std::mem::size_of::<Arguments>() as u64) {
                return None;
            }
            self.args_reserved = true;
        }
        self.args.window(count)
    }

    /// The room the running statement may still take.
    #[inline]
    fn free(&self) -> u64 {
        self.room.saturating_sub(self.transient)
    }

    /// Whether `bytes` of growth fit the room, so that holding them after
    /// they are made cannot refuse.
    #[inline]
    pub(crate) fn fits(&self, bytes: u64) -> bool {
        !self.memory || bytes <= self.free()
    }

    /// Holds `bytes` of growth the room admitted (`fits`), as the
    /// reference's growth anchor holds it; `false` with a fault when the
    /// census refuses what the room admitted.
    pub(crate) fn hold(&mut self, bytes: u64) -> bool {
        if !self.memory || bytes == 0 {
            return true;
        }
        match self.heap.hold_growth_metered(bytes) {
            Ok(true) => {
                self.room = self.room.saturating_sub(bytes);
                true
            }
            Ok(false) => {
                self.fault = Some(HeapFault::Malformed(
                    "the census refused growth a region's nursery admitted".into(),
                ));
                false
            }
            Err(fault) => {
                self.fault = Some(fault);
                false
            }
        }
    }

    /// Reserves `bytes` until the statement ends, as the reference's
    /// memory anchor reserves them: statement credit, which the
    /// statement's growth spends first; `false` where the room does not
    /// admit them, or with a fault where the census refuses what it did.
    fn reserve(&mut self, bytes: u64) -> bool {
        if !self.memory || bytes == 0 {
            return true;
        }
        if bytes > self.free() {
            return false;
        }
        match self.heap.hold_metered(bytes) {
            Ok(true) => {
                self.transient += bytes;
                true
            }
            Ok(false) => {
                self.fault = Some(HeapFault::Malformed(
                    "the census refused a reservation a region's nursery admitted".into(),
                ));
                false
            }
            Err(fault) => {
                self.fault = Some(fault);
                false
            }
        }
    }

    /// The reference's pin anchor: a value other than a scalar the
    /// statement still uses is reserved at its cost until it ends.
    fn pin(&mut self, value: &Value) -> bool {
        scalar(value) || self.reserve(self.arena.cost_of(value))
    }

    /// The statement ended, as the reference's step ends it: its
    /// reservations are released, what it made is held by a root or
    /// garbage (its kill set), and the arena is at a statement boundary.
    pub(crate) fn statement_ended(&mut self) {
        self.transient = 0;
        self.args_reserved = false;
        if self.memory
            && let Err(fault) = self.heap.release_statement_roots()
        {
            self.fault = Some(fault);
        }
        self.arena.statement_ended();
        self.arena.statement_boundary();
    }

    /// Installs what a nursery built and holds its growth, which
    /// [`Self::admitted`] admitted.
    fn install(&mut self, new: Vec<(ArenaHandle, NewObject)>) -> Option<()> {
        if new.is_empty() {
            return Some(());
        }
        match self.arena.install(new) {
            Ok(grown) => self.hold(grown).then_some(()),
            Err(error) => {
                self.fault = Some(HeapFault::Malformed(error.to_string()));
                None
            }
        }
    }

    /// Whether what `nursery` built, with `held` for its work, fits the
    /// room.
    fn admitted(&self, nursery: &Nursery<'_>, held: u64) -> bool {
        !self.memory
            || self
                .arena
                .install_bound(nursery.built())
                .checked_add(held)
                .is_some_and(|bytes| bytes <= self.free())
    }

    /// Builds `object`, as the reference's construction does.
    fn build(&mut self, object: NewObject) -> Option<Value> {
        let (value, new) = {
            let mut nursery = Nursery::new(self.arena.objects(), LogicalBytes(u64::MAX));
            let value = nursery.allocate(object);
            if !self.admitted(&nursery, 0) {
                nursery.discard();
                return None;
            }
            (value, nursery.into_new())
        };
        self.install(new)?;
        Some(value)
    }

    fn formula(&self, plan: &Plan, args: &[Value], result: Option<&Value>) -> Option<u64> {
        measured_formula(self.arena, plan, args, result)
    }

    /// The general call of native `function` on `args`, one per
    /// parameter, as the reference's call answers it: charged its formula
    /// when it answers, a miss when it raises, declines or asks again.
    fn native(&mut self, function: &Lib, args: Range<usize>) -> Option<Value> {
        let LibRun::Native { native, .. } = &function.run else {
            return None;
        };
        let limit = match &function.limit {
            Some(limit) => Some(self.formula(limit, &self.args[args.clone()], None)?),
            None => None,
        };
        let mut counter = WorkCounter::new(limit);
        let (value, held, new) = {
            let semantics = self.arena.semantics().with_limits(self.limits);
            let objects = self.arena.objects();
            let mut nursery = if self.memory {
                Nursery::new(objects, LogicalBytes(self.free()))
            } else {
                Nursery::unaccounted(objects)
            };
            let answer = native.call(NativeCall {
                args: Args::new(&self.args[args.clone()], objects, &semantics),
                new: &mut nursery,
                counter: &mut counter,
            });
            let held = nursery.held().0;
            match answer {
                Ok(value) if self.admitted(&nursery, held) => (value, held, nursery.into_new()),
                _ => {
                    nursery.discard();
                    return None;
                }
            }
        };
        if !self.reserve(held) {
            return None;
        }
        self.install(new)?;
        if self.arena.summary_of(&value).depth as usize > self.limits.value_depth()
            || !self.pin(&value)
        {
            return None;
        }
        let units = self.formula(&function.charge, &self.args[args], Some(&value))?;
        self.meter.measured(units);
        Some(value)
    }

    /// Operator `function`'s call on `args`: its prepared entry, charged
    /// as the reference's prepared answer is, or the native's general
    /// call where the entry declines.
    fn operate(&mut self, function: &Lib, args: Range<usize>) -> Option<Value> {
        let (entry, fast) = function.prepared.as_ref()?;
        let Some(answer) = entry.answer(&self.args[args.clone()], true) else {
            return self.native(function, args);
        };
        let value = answer.value;
        if !self.pin(&value) {
            return None;
        }
        let units = match answer.scalar_sizes.and_then(|mask| fast.scalar_units(mask)) {
            Some(units) => units,
            None => match fast.charge(&function.charge, &self.args[args.clone()], &value) {
                Charge::Units(units) => units,
                Charge::Plan(plan) => self.formula(plan, &self.args[args], Some(&value))?,
            },
        };
        self.meter.measured(units);
        Some(value)
    }
}

/// What `plan` charges a call of `args` that gave `result`, measured in
/// `arena` as the reference measures it; `None` when a value cannot be
/// measured where it is.
pub(crate) fn measured_formula(
    arena: &Arena,
    plan: &Plan,
    args: &[Value],
    result: Option<&Value>,
) -> Option<u64> {
    let mut failed = false;
    let units = plan.evaluate(|source, measure| {
        if failed {
            return 0;
        }
        let value = match source {
            Source::Arg(index) => args.get(index),
            Source::Result => result,
            Source::Nothing => None,
        };
        let measured = match (value, measure) {
            (None, _) => Ok(0),
            (Some(value), Measure::Size) => crate::data::size(arena, value),
            (Some(value), Measure::DeepSize) => crate::data::deep_size(arena, value),
            (Some(value), Measure::NestedSize) => crate::data::nested_size(arena, value),
            (Some(value), Measure::Magnitude) => Ok(crate::data::magnitude(arena, value)),
        };
        measured.unwrap_or_else(|_| {
            failed = true;
            0
        })
    });
    (!failed).then_some(units)
}

/// A compiled region expression: it reads the frame's slots and answers
/// its value, or `None` on a guard miss.
pub(crate) type Node = Box<dyn Fn(&[SlotState], &mut Cx<'_>) -> Option<Value> + Send + Sync>;

/// A compiled `item` hook: what the loop walks and the stepped item to the
/// value bound, metered, or `None` on a guard miss.
pub(crate) type Hook = Box<dyn Fn(Value, Value, &mut Cx<'_>) -> Option<Value> + Send + Sync>;

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
    /// The most the statement charges, when its formulas are bounded.
    pub(crate) max: Option<u64>,
    pub(crate) program: Box<[super::Event]>,
    /// The invoke occurrences it advances when it commits.
    pub(crate) calls: Box<[usize]>,
}

impl From<&super::Summary> for Commit {
    fn from(summary: &super::Summary) -> Self {
        Self {
            fixed: summary.fixed,
            max: summary.max(),
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
    /// them, and the place it writes.
    Store {
        operands: Box<[(Role, Node)]>,
        place: Place,
        commit: Commit,
    },
    /// `do`: a value the statement discards.
    Discard {
        value: Node,
        commit: Commit,
    },
    Leave,
}

/// What a store's operand is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Target,
    Index,
    Value,
}

/// Where a store writes in its target.
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

/// What the region statement in `range` computes: its last op, and the
/// node of each register, with where it was completed.
pub(super) struct Compiled {
    pub(super) last: Op,
    regs: Vec<Option<(Node, u32)>>,
}

impl Compiled {
    /// The node of the value the last op ends the statement with: a
    /// test's, a branch's, or the one a write or a declaration stores.
    pub(super) fn root(mut self) -> Option<Node> {
        let at = match self.last {
            Op::Test { cond } | Op::Branch { cond, .. } => cond.ix(),
            Op::WriteLocal { src, .. } | Op::WriteSession { src, .. } | Op::Bind { src, .. } => {
                src.ix()
            }
            _ => return None,
        };
        self.take(at)
    }

    /// The node of register `at`, taken.
    pub(super) fn take(&mut self, at: usize) -> Option<Node> {
        self.regs.get_mut(at)?.take().map(|(node, _)| node)
    }

    /// The nodes of registers `roles`, taken, in the order the statement
    /// completed them.
    pub(super) fn ordered(&mut self, roles: &[(Role, usize)]) -> Option<Box<[(Role, Node)]>> {
        let mut taken = roles
            .iter()
            .map(|(role, at)| {
                let (node, done) = self.regs.get_mut(*at)?.take()?;
                Some((done, *role, node))
            })
            .collect::<Option<Vec<_>>>()?;
        taken.sort_by_key(|(done, _, _)| *done);
        Some(
            taken
                .into_iter()
                .map(|(_, role, node)| (role, node))
                .collect(),
        )
    }
}

/// The ops in `range` of `stream`, up to the op that ends them, as one
/// node per register.
pub(super) fn compile(stream: &Stream, libs: &Libs<'_>, range: OpRange) -> Option<Compiled> {
    let mut regs: Vec<Option<(Node, u32)>> = Vec::new();
    let set = |regs: &mut Vec<Option<(Node, u32)>>, at: usize, node: Node, done: u32| {
        if regs.len() <= at {
            regs.resize_with(at + 1, || None);
        }
        regs[at] = Some((node, done));
    };
    let ops = stream.ops.get(range.start as usize..range.end() as usize)?;
    let (last, body) = ops.split_last()?;
    for (done, op) in body.iter().enumerate() {
        let done = done as u32;
        let node = match *op {
            Op::Region { .. } => continue,
            // A scalar plan's values need no pin.
            Op::Pin { .. } if !libs.general => continue,
            // The reference's pin anchor on the value the statement made.
            Op::Pin { src } => {
                let (value, done) = regs.get_mut(src.ix())?.take()?;
                let node: Node = Box::new(move |slots, cx| {
                    let value = value(slots, cx)?;
                    cx.pin(&value).then_some(value)
                });
                set(&mut regs, src.ix(), node, done);
                continue;
            }
            Op::Local { dst, local } => {
                let at = local.at as usize;
                let node: Node = Box::new(move |slots, _| match slots.get(at) {
                    Some(SlotState::Value(value)) => Some(*value),
                    _ => None,
                });
                (dst.ix(), node)
            }
            Op::Session { dst, name } => (dst.ix(), libs.session(stream.names.get(name.ix())?)?),
            Op::Const { dst, c } => match stream.consts.get(c.ix())? {
                Constant::Resident(value) => {
                    let value = *value;
                    let node: Node = Box::new(move |_, _| Some(value));
                    (dst.ix(), node)
                }
                Constant::Deferred(_) => return None,
            },
            Op::Operator { lib, args } => {
                let operands = operands(&mut regs, args)?;
                (args.start.ix(), operator(libs, LibId(lib.0), operands)?)
            }
            Op::Nested { act, args } => {
                let id = stream.nested.get(act.ix())?.kind.operator_lib()?;
                let operands = operands(&mut regs, args)?;
                (args.start.ix(), operator(libs, id, operands)?)
            }
            Op::Call { lib, args } => {
                let operands = operands(&mut regs, args)?;
                (args.start.ix(), call(libs, LibId(lib.0), operands)?)
            }
            Op::MakeTuple { items } => (items.start.ix(), tuple(operands(&mut regs, items)?)),
            Op::MakeList { items } => {
                let items_at = items.start.ix();
                (items_at, list(operands(&mut regs, items)?, false))
            }
            Op::MakeSet { items } => (items.start.ix(), list(operands(&mut regs, items)?, true)),
            Op::MakeMap { entries } => (entries.start.ix(), map(operands(&mut regs, entries)?)),
            Op::MakeRecord { fields, values } => {
                let names = stream
                    .field_lists
                    .get(fields.ix())?
                    .iter()
                    .map(|name| lash_kernel_doc::arena::ShortText::new(name).map(Value::Text))
                    .collect::<Option<Box<[Value]>>>()?;
                (
                    values.start.ix(),
                    record(names, operands(&mut regs, values)?),
                )
            }
            Op::Field { at, field } => {
                let field = stream.fields.get(field.ix())?;
                let target = regs.get_mut(at.ix())?.take()?.0;
                (at.ix(), self::field(field, target))
            }
            Op::Index { at, site } => {
                let target = regs.get_mut(at.ix())?.take()?.0;
                let index = regs.get_mut(at.ix() + 1)?.take()?.0;
                let site = ReadSite::new(site.0, stream.layout);
                (at.ix(), self::index(site, target, index))
            }
            _ => return None,
        };
        set(&mut regs, node.0, node.1, done);
    }
    Some(Compiled { last: *last, regs })
}

/// The compiled statement-root invoke of operator `id` on `args`, each a
/// frame variable or a resident literal.
pub(super) fn action(libs: &Libs<'_>, id: LibId, args: &[super::super::Atom]) -> Option<Node> {
    let operands = args
        .iter()
        .map(|arg| -> Option<Node> {
            match arg {
                super::super::Atom::Var(super::super::Var::Local(local)) => {
                    let at = local.at as usize;
                    Some(Box::new(move |slots, _| match slots.get(at) {
                        Some(SlotState::Value(value)) => Some(*value),
                        _ => None,
                    }))
                }
                super::super::Atom::Var(super::super::Var::Session(name)) => libs.session(name),
                super::super::Atom::Literal(Constant::Resident(value)) => {
                    let value = *value;
                    Some(Box::new(move |_, _| Some(value)))
                }
                _ => None,
            }
        })
        .collect::<Option<Vec<_>>>()?;
    operator(libs, id, operands)
}

/// The nodes of the registers `args`, taken.
fn operands(regs: &mut [Option<(Node, u32)>], args: RegRange) -> Option<Vec<Node>> {
    (args.start.ix()..args.end() as usize)
        .map(|at| regs.get_mut(at)?.take().map(|(node, _)| node))
        .collect()
}

/// The values of `nodes`, in order.
#[inline(always)]
fn values(nodes: &[Node], slots: &[SlotState], cx: &mut Cx<'_>) -> Option<Vec<Value>> {
    nodes.iter().map(|node| node(slots, cx)).collect()
}

/// Evaluates operands into a disjoint bounded window. A nested call may
/// reuse only the space after it. Every exit releases the window, so a miss
/// falls back before the statement without keeping an arena reference.
fn arguments(
    nodes: &[Node],
    arity: usize,
    slots: &[SlotState],
    cx: &mut Cx<'_>,
    call: impl FnOnce(&mut Cx<'_>, Range<usize>) -> Option<Value>,
) -> Option<Value> {
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

/// The compiled operator `id` over `operands`.
fn operator(libs: &Libs<'_>, id: LibId, operands: Vec<Node>) -> Option<Node> {
    if libs.general {
        libs.exe.lib(id).prepared.as_ref()?;
        return Some(Box::new(move |slots, cx| {
            arguments(&operands, operands.len(), slots, cx, |cx, args| {
                let exe = cx.exe;
                cx.operate(exe.lib(id), args)
            })
        }));
    }
    let answer = Answer::of(libs.exe.lib(id))?;
    let mut operands = operands.into_iter();
    let a = operands.next()?;
    Some(match operands.next() {
        None => Box::new(move |slots, cx| {
            let x = a(slots, cx)?;
            if !scalar(&x) {
                return None;
            }
            let (result, units) = answer.call(&[x])?;
            cx.meter.formula(units);
            Some(result)
        }),
        Some(b) => Box::new(move |slots, cx| {
            let x = a(slots, cx)?;
            let y = b(slots, cx)?;
            if !scalar(&x) || !scalar(&y) {
                return None;
            }
            let (result, units) = answer.call(&[x, y])?;
            cx.meter.formula(units);
            Some(result)
        }),
    })
}

/// The compiled general call of native `id` over `operands`, the
/// parameters they leave out absent.
fn call(libs: &Libs<'_>, id: LibId, operands: Vec<Node>) -> Option<Node> {
    let function = libs.exe.lib(id);
    if !libs.general || operands.len() > function.arity {
        return None;
    }
    let arity = function.arity;
    Some(Box::new(move |slots, cx| {
        arguments(&operands, arity, slots, cx, |cx, args| {
            let exe = cx.exe;
            cx.native(exe.lib(id), args)
        })
    }))
}

/// The compiled tuple of `items`, refused where the reference raises
/// `too_deep`.
fn tuple(items: Vec<Node>) -> Node {
    Box::new(move |slots, cx| {
        let members = values(&items, slots, cx)?;
        let tuple = cx.arena.tuple(members, cx.limits.value_depth()).ok()?;
        cx.pin(&tuple).then_some(tuple)
    })
}

/// The compiled list of `items`, or set when `set`: a set's members are
/// legal keys.
fn list(items: Vec<Node>, set: bool) -> Node {
    Box::new(move |slots, cx| {
        let members = values(&items, slots, cx)?;
        if set {
            if members.iter().any(|member| cx.arena.key(member).is_err()) {
                return None;
            }
            return cx.build(NewObject::Set(members));
        }
        cx.build(NewObject::List(members))
    })
}

/// The compiled map of the keys and values `entries` holds by turns.
fn map(entries: Vec<Node>) -> Node {
    Box::new(move |slots, cx| {
        arguments(&entries, entries.len(), slots, cx, |cx, window| {
            let pairs = cx.args[window].as_chunks::<2>().0;
            let mut table = Vec::with_capacity(pairs.len());
            for &[key, value] in pairs {
                cx.arena.key(&key).ok()?;
                table.push((key, value));
            }
            cx.build(NewObject::Map(table))
        })
    })
}

/// The compiled record of fields `names`, each with its value's node.
fn record(names: Box<[Value]>, values_of: Vec<Node>) -> Node {
    Box::new(move |slots, cx| {
        let fields = names
            .iter()
            .zip(&values_of)
            .map(|(name, node)| Some((*name, node(slots, cx)?)))
            .collect::<Option<Vec<_>>>()?;
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
            if cx.arena.unresolved_normal(record, &key).ok()? {
                return None;
            }
            let slot = site.and_then(|site| cx.sites.slot(site));
            let (value, found) =
                find_property(cx.arena, record, slot, name.as_bytes(), || Ok(key.clone())).ok()?;
            if let Some(site) = site
                && found != slot
            {
                cx.sites.keep(site, found);
            }
            Some(value.unwrap_or(Value::Absent))
        }
        Value::Error(error) => match cx.arena.body(error).ok()? {
            Body::Error(parts) => Some(match name.as_str() {
                "kind" => parts.kind,
                "message" => parts.message,
                "data" => parts.data,
                _ => Value::Absent,
            }),
            _ => None,
        },
        _ => None,
    })
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
        let arena = &*cx.arena;
        match target {
            Value::Tuple(object) | Value::List(object) => {
                arena.item(object, position(&index)?).ok()?
            }
            Value::Record(record) => {
                if !matches!(index, Value::Text(_) | Value::LongText(_))
                    || arena.unresolved(record, &index).ok()?.is_some()
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
                let name = match &index {
                    Value::Text(short) => short.as_bytes(),
                    _ => arena.semantics().text(&index).ok()?.as_bytes(),
                };
                let (found, at) =
                    find_property(arena, record, slot, name, || arena.key(&index)).ok()?;
                if at != slot {
                    cx.sites.keep(site, at);
                }
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
    })
}

/// The compiled `item` hook `function`: what the loop walks and an item
/// it stepped to, as the reference's handoff passes them. A scalar plan's
/// hook takes scalar items alone.
pub(super) fn hook(libs: &Libs<'_>, id: LibId) -> Option<Hook> {
    if libs.general {
        libs.exe.lib(id).prepared.as_ref()?;
        return Some(Box::new(move |walked, raw, cx| {
            let window = cx.argument_window(2)?;
            cx.args.put(window.start, walked);
            cx.args.put(window.start + 1, raw);
            let exe = cx.exe;
            let answer = cx.operate(exe.lib(id), window.clone());
            cx.args.release(window.start);
            answer
        }));
    }
    let answer = Answer::of(libs.exe.lib(id))?;
    Some(Box::new(move |walked, raw, cx| {
        if !scalar(&raw) {
            return None;
        }
        let (result, units) = answer.call(&[walked, raw])?;
        cx.meter.formula(units);
        Some(result)
    }))
}

/// Writes `value` at `place` of `target`, with `index` for an index, as
/// the reference's member write does, marking what it changes dirty and
/// holding its growth, which the room admits first; `None`, having
/// written nothing, where the reference raises or must decode first.
pub(crate) fn store(
    cx: &mut Cx<'_>,
    place: &Place,
    target: Value,
    index: Value,
    value: Value,
) -> Option<()> {
    let (table, key, written) = match (target, place) {
        (Value::List(list), Place::Index) => {
            let length = cx.arena.len(list).ok()?;
            let at = position(&index).filter(|at| *at <= length)?;
            if !cx.fits(cx.arena.cost_of(&value)) {
                return None;
            }
            let grown = cx.arena.list_set(list, at, value).ok()?;
            return cx.hold(grown).then_some(());
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
                    return cx.arena.table_delete(set, &index).ok();
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    let bound = cx
        .arena
        .cost_of(&key)
        .saturating_add(cx.arena.cost_of(&written));
    if cx.arena.unresolved(table, &key).ok()?.is_some() || !cx.fits(bound) {
        return None;
    }
    let grown = cx.arena.table_write(table, key, written).ok()?;
    cx.hold(grown).then_some(())
}
