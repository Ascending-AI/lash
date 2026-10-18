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

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::{NewObject, Value, ValueKind};

use super::super::expr::{self, Expr, Kind, Make};
use super::super::{
    BindingName, BlockId, Constant, Executable, Lib, LibId, ReadSite, Source, Stream, find_property,
};
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

/// A loop plan's compiled form: its head and its body as trees that hold
/// what each statement charges, so that an iteration reads nothing else.
pub(crate) struct CompiledLoop {
    pub(crate) head: CompiledHead,
    pub(crate) body: CompiledBody,
}

/// What a compiled statement commits: its fixed units and its static
/// charge program (§3.1).
pub(crate) struct Commit {
    /// The fixed units it charges on every path.
    pub(crate) fixed: u64,
    /// The most the statement charges, when its formulas are bounded.
    pub(crate) max: Option<u64>,
    pub(crate) program: Box<[super::Event]>,
    /// Its conditionals' arms, which its program names.
    pub(crate) arms: Box<[super::Arms]>,
    /// The invoke occurrences it advances when it commits.
    pub(crate) calls: Box<[usize]>,
}

impl From<&super::Summary> for Commit {
    fn from(summary: &super::Summary) -> Self {
        Self {
            fixed: summary.fixed,
            max: summary.max(),
            program: summary.program.clone(),
            arms: summary.arms.clone(),
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
/// statement-root invoke's value is `act`'s, taken.
pub(super) fn compile(
    stream: &Stream,
    libs: &Libs<'_>,
    expr: &Expr,
    act: &mut Option<Node>,
) -> Option<Node> {
    let mut node = |expr: &Expr| compile(stream, libs, expr, act);
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
        Kind::Operator { lib, args, .. } => operator(libs, *lib, nodes(args, &mut node)?)?,
        Kind::Native { lib, args, .. } => call(libs, *lib, nodes(args, &mut node)?)?,
        Kind::Call { .. } => return None,
        Kind::Make { make, items } => {
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
        Kind::Field { target, field } => {
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
        // A scalar plan's values need no pin.
        Kind::Pin(value) if !libs.general => node(value)?,
        // The reference's pin anchor on the value the statement made.
        Kind::Pin(value) => {
            let value = node(value)?;
            Box::new(move |slots, cx| {
                let value = value(slots, cx)?;
                cx.pin(&value).then_some(value)
            })
        }
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
/// `value` for `body`: its memory held as the reference's declaration
/// holds it, its value what `body`'s reads of it give.
fn declared(at: u32, value: Node, body: Node) -> Node {
    Box::new(move |slots, cx| {
        let value = value(slots, cx)?;
        if cx.memory {
            let bytes = cx.arena.cost_of_metered(&value);
            if !cx.fits(bytes) || !cx.hold(bytes) {
                return None;
            }
        }
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
        let members = cx.collecting(ValueKind::Tuple, items.len(), |cx| {
            values(&items, slots, cx)
        })?;
        let tuple = cx.arena.tuple(members, cx.limits.value_depth()).ok()?;
        cx.pin(&tuple).then_some(tuple)
    })
}

/// The compiled list of `items`, or set when `set`: a set's members are
/// legal keys.
fn list(items: Vec<Node>, set: bool) -> Node {
    let kind = if set { ValueKind::Set } else { ValueKind::List };
    Box::new(move |slots, cx| {
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

/// The compiled map of the keys and values `entries` holds by turns.
fn map(entries: Vec<Node>) -> Node {
    Box::new(move |slots, cx| {
        let table = cx.collecting(ValueKind::Map, entries.len() / 2, |cx| {
            arguments(&entries, entries.len(), slots, cx, |cx, window| {
                let pairs = cx.args[window].as_chunks::<2>().0;
                let mut table = Vec::with_capacity(pairs.len());
                for &[key, value] in pairs {
                    cx.arena.key(&key).ok()?;
                    table.push((key, value));
                }
                Some(table)
            })
        })?;
        cx.build(NewObject::Map(table))
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
