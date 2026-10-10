//! Partial evaluation of library bodies over the prepared form (FIG-5863
//! spike, SC-DESIGN §5).
//!
//! A library body or a library closure is specialized on facts about its
//! inputs, the parameters and the captured variables a guard reads at
//! activation entry: a kind, a constant, or a record that lacks a field.
//! The residual is ordinary prepared code that the interpreter runs. It
//! keeps every statement, so every step boundary, slice and park, and it
//! keeps every block, loop and `try` at its site, so a frame running it
//! exports the state the generic body's frame exports. What it changes is
//! expressions: a subexpression whose value the facts determine, such as
//! `same(kind(v), "record")` for a record `v`, becomes a [`Folded`] that
//! replays the charges and pins the original made, in order, and returns
//! the value without doing the work.
//!
//! What may fold:
//! - a literal, and a local variable whose value is known;
//! - a field read of a record known to lack the field;
//! - a native call, without a guard, whose arguments are all known, which
//!   is called here once: natives are functions of their arguments
//!   (`K-LIB-006`). Its formula is computed here on the same arguments and
//!   result; a call that reserves memory, raises, or returns a heap value
//!   does not fold;
//! - the kernel's `kind` of a value whose kind is known.
//!
//! Facts about the contents of a record (`lacks`) hold until the next
//! mutation point: an action, a member write or a remove. Facts about a
//! local hold until it is written. A variable a closure shares holds a fact
//! only when it is a closure's capture that nothing writes once the
//! closure exists.
//!
//! Variants are chosen by binding-time analysis: the inputs the body tests
//! (`kind(p)` against a kind name, `same(p, c)`, `same(p.f, absent)`), each
//! alone and in pairs, are partially evaluated, scored by the native calls
//! they fold (a fold inside `n` loops counts `8^n`), and the best few kept.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use lash_kernel_doc::{Measure, NativeCall, Value, ValueKind, WorkCounter};

use super::{
    Block, BlockId, Code, CodeId, Event, Expr, Folded, Lib, LibId, LibRun, Local, Member, Place,
    Rhs, Slot, Source, Stmt, StmtId, Tables, Target, Var,
};
use crate::data::{deep_size, magnitude, nested_size, size};
use crate::heap::{Heap, MAX_VALUE_DEPTH, NativeView, within_depth};

/// The bounds of partial evaluation (SC-DESIGN §5.2).
#[derive(Clone, Debug)]
pub struct Options {
    /// The library functions whose bodies, and the closures they make, are
    /// specialized, by name; empty is every one.
    pub functions: Vec<String>,
    /// Variants kept per code.
    pub variants_per_code: usize,
    /// Inputs one variant specializes.
    pub inputs_per_variant: usize,
    /// Abstract steps one variant may take before it is dropped.
    pub fuel: u64,
    /// Count each variant's activations.
    pub count: bool,
    /// How variants are chosen.
    pub selection: Selection,
}

/// How partial evaluation chooses the variants it keeps.
#[derive(Clone, Debug, Default)]
pub enum Selection {
    /// By static score alone.
    #[default]
    Static,
    /// Keeps every candidate, and an activation counts every candidate
    /// whose guards it passes but runs the generic code: a profiling run.
    Profiling,
    /// By static score times the activations a profiling run counted for
    /// the candidate, by code name and guards.
    Profiled(BTreeMap<(String, String), u64>),
}

impl Default for Options {
    fn default() -> Self {
        Self {
            functions: Vec::new(),
            variants_per_code: 4,
            inputs_per_variant: 2,
            fuel: 20_000,
            count: false,
            selection: Selection::Static,
        }
    }
}

/// What is known about a value.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Fact {
    kind: Option<ValueKind>,
    /// Its value, when it holds no heap object.
    value: Option<Value>,
    /// Fields a record does not have, while no mutation point has passed.
    lacks: Vec<String>,
}

impl Fact {
    fn constant(value: &Value) -> Self {
        Self {
            kind: Some(value.kind()),
            value: heap_free(value).then(|| value.clone()),
            lacks: Vec::new(),
        }
    }

    fn of_kind(kind: ValueKind) -> Self {
        Self {
            kind: Some(kind),
            ..Self::default()
        }
    }

    fn join(&self, other: &Self) -> Self {
        Self {
            kind: self.kind.filter(|kind| other.kind == Some(*kind)),
            value: self
                .value
                .clone()
                .filter(|value| other.value.as_ref() == Some(value)),
            lacks: self
                .lacks
                .iter()
                .filter(|field| other.lacks.contains(field))
                .cloned()
                .collect(),
        }
    }

    /// Whether `value` has this fact: the guard's test. Guards are physical
    /// work and charge nothing.
    fn holds(&self, value: &Value, heap: &Heap) -> bool {
        if self.kind.is_some_and(|kind| value.kind() != kind) {
            return false;
        }
        if self.value.as_ref().is_some_and(|known| known != value) {
            return false;
        }
        if !self.lacks.is_empty() {
            let Value::Record(record) = value else {
                return false;
            };
            let Some(fields) = heap.record(*record) else {
                return false;
            };
            if fields
                .iter()
                .any(|(name, _)| self.lacks.iter().any(|lacked| lacked == name))
            {
                return false;
            }
        }
        true
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(value) = &self.value {
            parts.push(format!("= {value:?}"));
        } else if let Some(kind) = self.kind {
            parts.push(format!("{kind:?}"));
        }
        for field in &self.lacks {
            parts.push(format!("no .{field}"));
        }
        parts.join(" ")
    }
}

/// A value that holds no heap object, so it is the same in every run.
fn heap_free(value: &Value) -> bool {
    match value {
        Value::Null
        | Value::Absent
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Timestamp(_) => true,
        Value::Tuple(members) => members.iter().all(heap_free),
        _ => false,
    }
}

/// An input a guard reads at activation entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Input {
    Param(usize),
    Capture(usize),
}

#[derive(Clone, Debug)]
pub(crate) struct Guard {
    input: Input,
    fact: Fact,
}

/// A residual variant of a code and the guards that select it.
pub(crate) struct Variant {
    guards: Box<[Guard]>,
    pub(crate) code: CodeId,
    /// Activations that entered it, when the library counts them.
    pub(crate) hits: AtomicU64,
}

impl Variant {
    /// Whether an activation with these arguments and captured cells may
    /// run this variant.
    #[inline]
    pub(crate) fn admits(
        &self,
        args: &[Value],
        captures: &[lash_kernel_doc::ObjectId],
        heap: &Heap,
    ) -> bool {
        self.guards.iter().all(|guard| {
            let value = match guard.input {
                Input::Param(index) => args.get(index).unwrap_or(&Value::Absent),
                Input::Capture(index) => {
                    match captures.get(index).and_then(|cell| heap.variable(*cell)) {
                        Some(value) => value,
                        None => return false,
                    }
                }
            };
            guard.fact.holds(value, heap)
        })
    }
}

/// One specialized code, for the report.
#[derive(Clone, Debug)]
pub struct CodeReport {
    /// The function, and for a closure the path of its body.
    pub name: String,
    /// Statements and expression nodes of the generic code.
    pub generic_nodes: u64,
    pub variants: Vec<VariantReport>,
}

#[derive(Clone, Debug)]
pub struct VariantReport {
    pub guards: String,
    /// The variant's score: folded native calls, weighted by loop depth.
    pub score: u64,
    /// Native calls folded, unweighted.
    pub folds: u64,
    /// Statements, blocks and expression nodes the residual added.
    pub residual_nodes: u64,
    pub code: u32,
    /// Activations that ran the variant, when counted.
    pub hits: u64,
}

impl super::PreparedLibrary {
    /// Counts each variant's activations from now on.
    pub fn pe_hits(&self) -> Vec<(String, String, u64)> {
        let mut rows = Vec::new();
        for report in &self.0.report {
            for variant in &report.variants {
                let hits = self
                    .0
                    .variants
                    .iter()
                    .flat_map(|variants| variants.iter())
                    .find(|candidate| candidate.code.0 == variant.code)
                    .map_or(0, |candidate| candidate.hits.load(Ordering::Relaxed));
                rows.push((report.name.clone(), variant.guards.clone(), hits));
            }
        }
        rows
    }
}

/// The library's catalogue functions partial evaluation knows by their
/// kernel semantics.
struct Known {
    kind: Option<LibId>,
    same: Option<LibId>,
}

/// Specializes the library's codes. Gives each code's variants, by code
/// id, and the report.
pub(crate) fn specialize(
    tables: &mut Tables,
    libs: &[Lib],
    options: &Options,
) -> (Vec<Box<[Variant]>>, Vec<CodeReport>) {
    let lib_named = |name: &str| {
        libs.iter()
            .position(|lib| {
                lib.definition.name.to_string() == name && matches!(lib.run, LibRun::Native(_))
            })
            .map(|index| LibId(index as u32))
    };
    let known = Known {
        kind: lib_named("kind"),
        same: lib_named("same"),
    };
    // Each library body, by the function it is the body of.
    let mut owner: BTreeMap<CodeId, String> = BTreeMap::new();
    for lib in libs {
        if let LibRun::Body(code) = lib.run {
            owner.insert(code, lib.definition.name.to_string());
        }
    }
    let parents = closure_parents(tables);
    // A closure is named by the function whose body makes it.
    let mut names: BTreeMap<CodeId, String> = owner.clone();
    let mut pending: Vec<CodeId> = parents.keys().copied().collect();
    while let Some(code) = pending.pop() {
        if names.contains_key(&code) {
            continue;
        }
        let (parent, _) = parents[&code];
        match names.get(&parent) {
            Some(name) => {
                let path = &tables.codes[code.0 as usize].site.path;
                names.insert(code, format!("{name}@{path:?}"));
            }
            None if parents.contains_key(&parent) => {
                pending.push(code);
                pending.push(parent);
            }
            None => {}
        }
    }
    let wanted = |name: &str| {
        options.functions.is_empty()
            || options
                .functions
                .iter()
                .any(|function| name == function || name.starts_with(&format!("{function}@")))
    };
    let generic_codes = tables.codes.len();
    let mut variants: Vec<Box<[Variant]>> = Vec::new();
    let mut report = Vec::new();
    for index in 0..generic_codes {
        let code = CodeId(index as u32);
        let Some(name) = names.get(&code).cloned() else {
            continue;
        };
        if !wanted(&name) || !tables.codes[index].charged {
            continue;
        }
        let stable = stable_captures(tables, &parents, code);
        let candidates = candidates(tables, &known, code, &stable, options);
        // Score every candidate, then keep the best and make them again.
        let mut scored = Vec::new();
        for (order, guards) in candidates.iter().enumerate() {
            let mark = (tables.codes.len(), tables.blocks.len(), tables.stmts.len());
            let outcome = Residual::run(tables, libs, &known, code, &stable, guards, options.fuel);
            tables.codes.truncate(mark.0);
            tables.blocks.truncate(mark.1);
            tables.stmts.truncate(mark.2);
            if let Some(outcome) = outcome
                && outcome.score > 0
            {
                let weight = match &options.selection {
                    Selection::Static => outcome.score,
                    Selection::Profiling => 1,
                    Selection::Profiled(counts) => counts
                        .get(&(name.clone(), describe(tables, code, guards)))
                        .copied()
                        .unwrap_or(0)
                        .saturating_mul(outcome.score),
                };
                if weight > 0 {
                    scored.push((weight, outcome.score, guards.len(), order));
                }
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
        let keep = match options.selection {
            Selection::Profiling => scored.len(),
            _ => options.variants_per_code,
        };
        scored.truncate(keep);
        // The first variant whose guards pass runs: the one that folds most
        // per activation goes first.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
        let mut kept = Vec::new();
        let mut rows = Vec::new();
        for (_, _, _, order) in scored {
            let guards = &candidates[order];
            let Some(outcome) =
                Residual::run(tables, libs, &known, code, &stable, guards, options.fuel)
            else {
                continue;
            };
            rows.push(VariantReport {
                guards: describe(tables, code, guards),
                score: outcome.score,
                folds: outcome.folds,
                residual_nodes: outcome.nodes,
                code: outcome.code.0,
                hits: 0,
            });
            kept.push(Variant {
                guards: guards.clone().into_boxed_slice(),
                code: outcome.code,
                hits: AtomicU64::new(0),
            });
        }
        if kept.is_empty() {
            continue;
        }
        if variants.len() <= index {
            variants.resize_with(index + 1, Default::default);
        }
        variants[index] = kept.into_boxed_slice();
        report.push(CodeReport {
            name,
            generic_nodes: count_block(tables, tables.codes[index].body),
            variants: rows,
        });
    }
    (variants, report)
}

fn describe(tables: &Tables, code: CodeId, guards: &[Guard]) -> String {
    let code = &tables.codes[code.0 as usize];
    guards
        .iter()
        .map(|guard| {
            let name = match guard.input {
                Input::Param(index) => code
                    .params
                    .get(index)
                    .map(|local| code.slots[local.slot as usize].name.to_string()),
                Input::Capture(index) => code
                    .captures
                    .get(index)
                    .map(|capture| format!("^{}", code.slots[capture.inner as usize].name)),
            }
            .unwrap_or_default();
            format!("{name}: {}", guard.fact.describe())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every closure code of the tables: the code whose body makes it, and the
/// index of the statement of that body's own block it is made in.
fn closure_parents(tables: &Tables) -> BTreeMap<CodeId, (CodeId, usize)> {
    let mut parents = BTreeMap::new();
    for (index, code) in tables.codes.iter().enumerate() {
        let parent = CodeId(index as u32);
        for (top, stmt) in tables.blocks[code.body.0 as usize].stmts.iter().enumerate() {
            walk_stmt(tables, *stmt, &mut |item| {
                if let Item::Expr(Expr::Closure(closure)) = item {
                    parents.insert(*closure, (parent, top));
                }
            });
        }
    }
    parents
}

/// A node a walk visits.
enum Item<'t> {
    Stmt(&'t Stmt),
    Expr(&'t Expr),
}

/// Visits a statement and everything in it, but not the bodies of the
/// closures it makes.
fn walk_stmt<'t>(tables: &'t Tables, id: StmtId, visit: &mut impl FnMut(Item<'t>)) {
    let stmt = &tables.stmts[id.0 as usize];
    visit(Item::Stmt(stmt));
    let mut blocks = Vec::new();
    let mut exprs: Vec<&Expr> = Vec::new();
    let member = |member: &'t Member, exprs: &mut Vec<&'t Expr>| match member {
        Member::Field(target, _) => exprs.push(target),
        Member::Index(target, index) => {
            exprs.push(target);
            exprs.push(index);
        }
    };
    match stmt {
        Stmt::Let { value, .. } | Stmt::Assign { value, .. } => {
            if let Stmt::Assign {
                place: Place::Member(target),
                ..
            } = stmt
            {
                member(target, &mut exprs);
            }
            if let Rhs::Expr(expr) = value {
                exprs.push(expr);
            }
        }
        Stmt::Remove(target) => member(target, &mut exprs),
        Stmt::Do(_) | Stmt::Break | Stmt::Continue => {}
        Stmt::If {
            condition,
            then_block,
            else_block,
        } => {
            exprs.push(condition);
            blocks.push(*then_block);
            blocks.push(*else_block);
        }
        Stmt::For { iterable, body, .. } => {
            exprs.push(iterable);
            blocks.push(*body);
        }
        Stmt::While {
            condition, body, ..
        } => {
            exprs.push(condition);
            blocks.push(*body);
        }
        Stmt::Return(expr)
        | Stmt::Throw(expr)
        | Stmt::Print(expr)
        | Stmt::Finish(expr)
        | Stmt::Fail(expr) => exprs.push(expr),
        Stmt::Try {
            body,
            catch,
            finally,
            ..
        } => {
            blocks.push(*body);
            blocks.extend(catch.map(|(_, block)| block));
            blocks.extend(*finally);
        }
    }
    while let Some(expr) = exprs.pop() {
        visit(Item::Expr(expr));
        match expr {
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => exprs.extend(items),
            Expr::Call { args, .. } => exprs.extend(args),
            Expr::Map(entries) => entries.iter().for_each(|(key, value)| {
                exprs.push(key);
                exprs.push(value);
            }),
            Expr::Record(entries) => exprs.extend(entries.iter().map(|(_, value)| value)),
            Expr::Member(target) => member(target, &mut exprs),
            Expr::Read(read) => {
                exprs.push(&read.0);
                exprs.push(&read.1);
            }
            Expr::Literal(_)
            | Expr::Var(_)
            | Expr::Closure(_)
            | Expr::Clock
            | Expr::Random
            | Expr::Folded(_) => {}
        }
    }
    for block in blocks {
        for stmt in &tables.blocks[block.0 as usize].stmts {
            walk_stmt(tables, *stmt, visit);
        }
    }
}

fn walk_block<'t>(tables: &'t Tables, block: BlockId, visit: &mut impl FnMut(Item<'t>)) {
    for stmt in &tables.blocks[block.0 as usize].stmts {
        walk_stmt(tables, *stmt, visit);
    }
}

fn count_block(tables: &Tables, block: BlockId) -> u64 {
    let mut count = 0;
    walk_block(tables, block, &mut |_| count += 1);
    count
}

/// The slots a statement writes: its `let`, assignments to variables, a
/// loop's binding and a `catch`'s.
fn written(stmt: &Stmt, into: &mut BTreeSet<Slot>) {
    match stmt {
        Stmt::Let {
            target: Target::Slot(local),
            ..
        }
        | Stmt::Assign {
            place: Place::Var(Var::Local(local)),
            ..
        }
        | Stmt::For { binding: local, .. } => {
            into.insert(local.slot);
        }
        Stmt::Try {
            catch: Some((local, _)),
            ..
        } => {
            into.insert(local.slot);
        }
        _ => {}
    }
}

/// Whether a statement is a mutation point: it can change a heap object's
/// contents, or run other code that can.
fn mutates(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Do(_)
            | Stmt::Remove(_)
            | Stmt::Let {
                value: Rhs::Action(_),
                ..
            }
            | Stmt::Assign {
                value: Rhs::Action(_),
                ..
            }
            | Stmt::Assign {
                place: Place::Member(_),
                ..
            }
    )
}

/// The captures of a closure code whose cells nothing writes once the
/// closure exists, by the closure's own slot: the code that makes the
/// closure writes the variable only in statements of its body before the
/// one that makes it, and no closure it makes, at any depth, writes it.
fn stable_captures(
    tables: &Tables,
    parents: &BTreeMap<CodeId, (CodeId, usize)>,
    code: CodeId,
) -> BTreeSet<Slot> {
    let mut stable = BTreeSet::new();
    let Some(&(parent, made_at)) = parents.get(&code) else {
        return stable;
    };
    let parent_code = &tables.codes[parent.0 as usize];
    // Each closure made under `parent`, at any depth, with the slot of
    // `parent` that each of its own captured slots stands for.
    let mut descendants: Vec<(CodeId, BTreeMap<Slot, Slot>)> = Vec::new();
    let mut pending: Vec<(CodeId, BTreeMap<Slot, Slot>)> = vec![(parent, BTreeMap::new())];
    while let Some((outer, origin)) = pending.pop() {
        for (closure, (made_by, _)) in parents {
            if *made_by != outer {
                continue;
            }
            let mut map = BTreeMap::new();
            for capture in &tables.codes[closure.0 as usize].captures {
                let root = if outer == parent {
                    Some(capture.outer)
                } else {
                    origin.get(&capture.outer).copied()
                };
                if let Some(root) = root {
                    map.insert(capture.inner, root);
                }
            }
            descendants.push((*closure, map.clone()));
            pending.push((*closure, map));
        }
    }
    for capture in &tables.codes[code.0 as usize].captures {
        let slot = capture.outer;
        let mut ok = true;
        for (top, stmt) in tables.blocks[parent_code.body.0 as usize]
            .stmts
            .iter()
            .enumerate()
        {
            let mut writes = BTreeSet::new();
            walk_stmt(tables, *stmt, &mut |item| {
                if let Item::Stmt(stmt) = item {
                    written(stmt, &mut writes);
                }
            });
            if writes.contains(&slot) && top >= made_at {
                ok = false;
            }
        }
        for (closure, map) in &descendants {
            let mut writes = BTreeSet::new();
            walk_block(tables, tables.codes[closure.0 as usize].body, &mut |item| {
                if let Item::Stmt(stmt) = item {
                    written(stmt, &mut writes);
                }
            });
            if writes.iter().any(|inner| map.get(inner) == Some(&slot)) {
                ok = false;
            }
        }
        if ok {
            stable.insert(capture.inner);
        }
    }
    stable
}

/// The kernel's name of a kind, as `kind` gives it.
fn kind_name(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Null => "null",
        ValueKind::Absent => "absent",
        ValueKind::Bool => "bool",
        ValueKind::Int => "integer",
        ValueKind::Float => "float",
        ValueKind::Text => "text",
        ValueKind::Bytes => "bytes",
        ValueKind::Timestamp => "timestamp",
        ValueKind::Tuple => "tuple",
        ValueKind::List => "list",
        ValueKind::Map => "map",
        ValueKind::Set => "set",
        ValueKind::Record => "record",
        ValueKind::Closure => "closure",
        ValueKind::Error => "error",
        ValueKind::Task => "task",
        ValueKind::Function => "function",
        ValueKind::Handle => "handle",
        ValueKind::Ref => "ref",
    }
}

const KINDS: [ValueKind; 19] = [
    ValueKind::Null,
    ValueKind::Absent,
    ValueKind::Bool,
    ValueKind::Int,
    ValueKind::Float,
    ValueKind::Text,
    ValueKind::Bytes,
    ValueKind::Timestamp,
    ValueKind::Tuple,
    ValueKind::List,
    ValueKind::Map,
    ValueKind::Set,
    ValueKind::Record,
    ValueKind::Closure,
    ValueKind::Error,
    ValueKind::Task,
    ValueKind::Function,
    ValueKind::Handle,
    ValueKind::Ref,
];

fn kind_named(name: &str) -> Option<ValueKind> {
    KINDS.into_iter().find(|kind| kind_name(*kind) == name)
}

/// The guard sets worth trying for a code: binding-time analysis of what
/// its body tests about its inputs.
fn candidates(
    tables: &Tables,
    known: &Known,
    code: CodeId,
    stable: &BTreeSet<Slot>,
    options: &Options,
) -> Vec<Vec<Guard>> {
    let generic = &tables.codes[code.0 as usize];
    let mut input_of: BTreeMap<Slot, Input> = BTreeMap::new();
    for (index, param) in generic.params.iter().enumerate() {
        if !param.shared {
            input_of.insert(param.slot, Input::Param(index));
        }
    }
    for (index, capture) in generic.captures.iter().enumerate() {
        if stable.contains(&capture.inner) {
            input_of.insert(capture.inner, Input::Capture(index));
        }
    }
    let input = |expr: &Expr| match expr {
        Expr::Var(Var::Local(local)) => input_of.get(&local.slot).copied(),
        _ => None,
    };
    // Slots that hold `kind(input)` somewhere.
    let mut kind_alias: BTreeMap<Slot, Input> = BTreeMap::new();
    walk_block(tables, generic.body, &mut |item| {
        if let Item::Stmt(
            Stmt::Let {
                target: Target::Slot(local),
                value: Rhs::Expr(Expr::Call { lib, args }),
            }
            | Stmt::Assign {
                place: Place::Var(Var::Local(local)),
                value: Rhs::Expr(Expr::Call { lib, args }),
            },
        ) = item
            && Some(*lib) == known.kind
            && let [arg] = args.as_slice()
            && let Some(input) = input(arg)
        {
            kind_alias.insert(local.slot, input);
        }
    });
    let kind_of = |expr: &Expr| match expr {
        Expr::Call { lib, args } if Some(*lib) == known.kind => match args.as_slice() {
            [arg] => input(arg),
            _ => None,
        },
        Expr::Var(Var::Local(local)) => kind_alias.get(&local.slot).copied(),
        _ => None,
    };
    let mut options_of: BTreeMap<Input, Vec<Fact>> = BTreeMap::new();
    let mut lacks_of: BTreeMap<Input, Vec<String>> = BTreeMap::new();
    fn add(options_of: &mut BTreeMap<Input, Vec<Fact>>, input: Input, fact: Fact) {
        let facts = options_of.entry(input).or_default();
        if !facts.contains(&fact) {
            facts.push(fact);
        }
    }
    walk_block(tables, generic.body, &mut |item| {
        let Item::Expr(Expr::Call { lib, args }) = item else {
            return;
        };
        if Some(*lib) != known.same {
            return;
        }
        let [a, b] = args.as_slice() else {
            return;
        };
        for (tested, other) in [(a, b), (b, a)] {
            let Expr::Literal(literal) = other else {
                continue;
            };
            if let (Some(input), Value::Text(name)) = (kind_of(tested), literal)
                && let Some(kind) = kind_named(name)
            {
                add(&mut options_of, input, Fact::of_kind(kind));
            }
            if let Some(input) = input(tested)
                && heap_free(literal)
            {
                add(&mut options_of, input, Fact::constant(literal));
            }
            if let Expr::Member(member) = tested
                && let Member::Field(target, field) = member.as_ref()
                && let Some(input) = input(target)
                && matches!(literal, Value::Absent)
            {
                let fields = lacks_of.entry(input).or_default();
                if !fields.contains(field) {
                    fields.push(field.clone());
                }
            }
        }
    });
    // A kind test names the kinds it looks for; the inputs that reach it
    // most often may be of kinds it names none of, so every kind-tested
    // input is also tried as each of the common kinds.
    let tested: Vec<Input> = options_of
        .iter()
        .filter(|(_, facts)| {
            facts
                .iter()
                .any(|fact| fact.value.is_none() && fact.kind.is_some())
        })
        .map(|(input, _)| *input)
        .collect();
    for input in tested {
        for kind in [
            ValueKind::Absent,
            ValueKind::Null,
            ValueKind::Bool,
            ValueKind::Float,
            ValueKind::Text,
            ValueKind::List,
            ValueKind::Record,
            ValueKind::Closure,
            ValueKind::Tuple,
        ] {
            add(&mut options_of, input, Fact::of_kind(kind));
        }
    }
    for (input, fields) in lacks_of {
        add(
            &mut options_of,
            input,
            Fact {
                kind: Some(ValueKind::Record),
                value: None,
                lacks: fields,
            },
        );
    }
    let singles: Vec<Guard> = options_of
        .into_iter()
        .flat_map(|(input, facts)| facts.into_iter().map(move |fact| Guard { input, fact }))
        .collect();
    let mut sets: Vec<Vec<Guard>> = singles.iter().map(|guard| vec![guard.clone()]).collect();
    if options.inputs_per_variant >= 2 {
        for (index, first) in singles.iter().enumerate() {
            for second in &singles[index + 1..] {
                if first.input != second.input {
                    sets.push(vec![first.clone(), second.clone()]);
                }
            }
        }
    }
    if options.inputs_per_variant >= 3 {
        for (index, first) in singles.iter().enumerate() {
            for (offset, second) in singles[index + 1..].iter().enumerate() {
                for third in &singles[index + 1 + offset + 1..] {
                    let inputs = [first.input, second.input, third.input];
                    if inputs[0] != inputs[1] && inputs[1] != inputs[2] && inputs[0] != inputs[2] {
                        sets.push(vec![first.clone(), second.clone(), third.clone()]);
                    }
                }
            }
        }
    }
    sets.truncate(512);
    sets
}

/// What is known at a point of a code: a fact per slot, and whether the
/// point can be reached.
#[derive(Clone)]
struct Env {
    facts: Vec<Fact>,
    live: bool,
}

impl Env {
    fn join(&mut self, other: &Env) {
        match (self.live, other.live) {
            (_, false) => {}
            (false, true) => *self = other.clone(),
            (true, true) => {
                for (mine, theirs) in self.facts.iter_mut().zip(&other.facts) {
                    *mine = mine.join(theirs);
                }
            }
        }
    }

    /// A mutation point passed: no record's contents are known.
    fn mutated(&mut self) {
        for fact in &mut self.facts {
            fact.lacks.clear();
        }
    }
}

/// What abstract evaluation knows of an expression.
struct Abs {
    fact: Fact,
    /// When evaluating it cannot raise and does nothing a run observes but
    /// these events: its charges and pins.
    events: Option<Vec<Event>>,
}

impl Abs {
    fn unknown() -> Self {
        Self {
            fact: Fact::default(),
            events: None,
        }
    }
}

fn charge(units: u64, lib: Option<LibId>) -> Event {
    Event::Charge {
        total: units,
        parts: vec![units].into_boxed_slice(),
        lib,
    }
}

/// Merges adjacent charges with the same attribution.
fn coalesce(events: Vec<Event>) -> Box<[Event]> {
    let mut merged: Vec<Event> = Vec::with_capacity(events.len());
    for event in events {
        if let (
            Some(Event::Charge {
                total,
                parts,
                lib: last,
            }),
            Event::Charge {
                total: more,
                parts: more_parts,
                lib,
            },
        ) = (merged.last_mut(), &event)
            && *last == *lib
        {
            *total = total.saturating_add(*more);
            let mut joined = parts.to_vec();
            joined.extend_from_slice(more_parts);
            *parts = joined.into_boxed_slice();
            continue;
        }
        merged.push(event);
    }
    merged.into_boxed_slice()
}

/// The outcome of specializing one code under one guard set.
struct Outcome {
    code: CodeId,
    score: u64,
    folds: u64,
    nodes: u64,
}

struct Residual<'a> {
    tables: &'a mut Tables,
    libs: &'a [Lib],
    known: &'a Known,
    /// Slots whose facts hold though a closure shares them.
    stable: &'a BTreeSet<Slot>,
    scratch: Heap,
    fuel: u64,
    depth: u32,
    score: u64,
    folds: u64,
    nodes: u64,
    /// Edits made: a statement whose subtree made none is kept as it is.
    edits: u64,
}

impl Residual<'_> {
    fn run(
        tables: &mut Tables,
        libs: &[Lib],
        known: &Known,
        code: CodeId,
        stable: &BTreeSet<Slot>,
        guards: &[Guard],
        fuel: u64,
    ) -> Option<Outcome> {
        let generic = &tables.codes[code.0 as usize];
        let mut env = Env {
            facts: vec![Fact::default(); generic.slots.len()],
            live: true,
        };
        for guard in guards {
            let slot = match guard.input {
                Input::Param(index) => generic.params.get(index)?.slot,
                Input::Capture(index) => generic.captures.get(index)?.inner,
            };
            env.facts[slot as usize] = guard.fact.clone();
        }
        let body = generic.body;
        let mut residual = Residual {
            tables,
            libs,
            known,
            stable,
            scratch: Heap::new(u64::MAX),
            fuel,
            depth: 0,
            score: 0,
            folds: 0,
            nodes: 0,
            edits: 0,
        };
        let body = residual.block(body, &mut env)?;
        let generic = &residual.tables.codes[code.0 as usize];
        let variant = Code {
            site: generic.site.clone(),
            params: generic.params.clone(),
            body,
            slots: generic.slots.clone(),
            positions: generic.positions.clone(),
            captures: generic.captures.clone(),
            charged: generic.charged,
        };
        let id = CodeId(residual.tables.codes.len() as u32);
        residual.tables.codes.push(variant);
        Some(Outcome {
            code: id,
            score: residual.score,
            folds: residual.folds,
            nodes: residual.nodes,
        })
    }

    fn spend(&mut self) -> Option<()> {
        self.fuel = self.fuel.checked_sub(1)?;
        Some(())
    }

    fn tracked(&self, local: &Local) -> bool {
        !local.shared || self.stable.contains(&local.slot)
    }

    /// Sets what a write of a slot leaves known.
    fn write(&self, env: &mut Env, local: &Local, fact: Fact) {
        if !local.shared {
            env.facts[local.slot as usize] = fact;
        }
    }

    fn block(&mut self, id: BlockId, env: &mut Env) -> Option<BlockId> {
        let before = self.edits;
        let stmts = self.tables.blocks[id.0 as usize].stmts.clone();
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            if env.live {
                out.push(self.stmt(stmt, env)?);
            } else {
                // Not reached: kept as it is.
                out.push(stmt);
            }
        }
        if self.edits == before {
            return Some(id);
        }
        let generic = &self.tables.blocks[id.0 as usize];
        let block = Block {
            site: generic.site.clone(),
            stmts: out,
            declares: generic.declares.clone(),
        };
        let new = BlockId(self.tables.blocks.len() as u32);
        self.tables.blocks.push(block);
        self.nodes += 1;
        Some(new)
    }

    /// Kills what a loop or a `try` body may change before control comes
    /// back to its head: the slots it writes and, when it can mutate, the
    /// records' contents.
    fn killed(&self, env: &Env, blocks: &[BlockId], extra: Option<Slot>) -> Env {
        let mut writes = BTreeSet::new();
        let mut mutation = false;
        for block in blocks {
            walk_block(self.tables, *block, &mut |item| {
                if let Item::Stmt(stmt) = item {
                    written(stmt, &mut writes);
                    mutation |= mutates(stmt);
                }
            });
        }
        writes.extend(extra);
        let mut killed = env.clone();
        for slot in writes {
            if let Some(fact) = killed.facts.get_mut(slot as usize) {
                *fact = Fact::default();
            }
        }
        if mutation {
            killed.mutated();
        }
        killed
    }

    fn stmt(&mut self, id: StmtId, env: &mut Env) -> Option<StmtId> {
        self.spend()?;
        let before = self.edits;
        let generic = self.tables.stmts[id.0 as usize].clone();
        let stmt = match generic {
            Stmt::Let { target, value } => {
                let (value, fact) = self.rhs(value, env)?;
                if let Target::Slot(local) = &target {
                    self.write(env, local, fact);
                }
                Stmt::Let { target, value }
            }
            Stmt::Assign { place, value } => {
                let (value, fact) = self.rhs(value, env)?;
                let place = match place {
                    Place::Var(var) => {
                        if let Var::Local(local) = &var {
                            self.write(env, local, fact);
                        }
                        Place::Var(var)
                    }
                    Place::Member(member) => {
                        let member = self.member(member, env)?;
                        env.mutated();
                        Place::Member(member)
                    }
                };
                Stmt::Assign { place, value }
            }
            Stmt::Remove(member) => {
                let member = self.member(member, env)?;
                env.mutated();
                Stmt::Remove(member)
            }
            Stmt::Do(action) => {
                env.mutated();
                Stmt::Do(action)
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                let (condition, abs) = self.expr(&condition, env)?;
                match abs.fact.value {
                    Some(Value::Bool(taken)) => {
                        let (then_block, else_block) = if taken {
                            (self.block(then_block, env)?, else_block)
                        } else {
                            (then_block, self.block(else_block, env)?)
                        };
                        Stmt::If {
                            condition,
                            then_block,
                            else_block,
                        }
                    }
                    _ => {
                        let mut other = env.clone();
                        let then_block = self.block(then_block, env)?;
                        let else_block = self.block(else_block, &mut other)?;
                        env.join(&other);
                        Stmt::If {
                            condition,
                            then_block,
                            else_block,
                        }
                    }
                }
            }
            Stmt::For {
                site,
                binding,
                iterable,
                body,
            } => {
                let (iterable, _) = self.expr(&iterable, env)?;
                let mut head = self.killed(env, &[body], Some(binding.slot));
                self.depth += 1;
                let body = self.block(body, &mut head.clone());
                self.depth -= 1;
                head.live = env.live;
                *env = head;
                Stmt::For {
                    site,
                    binding,
                    iterable,
                    body: body?,
                }
            }
            Stmt::While {
                site,
                condition,
                body,
            } => {
                let head = self.killed(env, &[body], None);
                self.depth += 1;
                let condition = self.expr(&condition, &head).map(|(expr, _)| expr);
                let body = self.block(body, &mut head.clone());
                self.depth -= 1;
                *env = head;
                Stmt::While {
                    site,
                    condition: condition?,
                    body: body?,
                }
            }
            Stmt::Try {
                site,
                body,
                catch,
                finally,
            } => {
                let mut blocks = vec![body];
                blocks.extend(catch.map(|(_, block)| block));
                blocks.extend(finally);
                let killed = self.killed(env, &blocks, catch.map(|(local, _)| local.slot));
                let body = self.block(body, &mut env.clone())?;
                let catch = match catch {
                    Some((local, block)) => Some((local, self.block(block, &mut killed.clone())?)),
                    None => None,
                };
                let finally = match finally {
                    Some(block) => Some(self.block(block, &mut killed.clone())?),
                    None => None,
                };
                *env = killed;
                Stmt::Try {
                    site,
                    body,
                    catch,
                    finally,
                }
            }
            Stmt::Break => {
                env.live = false;
                Stmt::Break
            }
            Stmt::Continue => {
                env.live = false;
                Stmt::Continue
            }
            Stmt::Return(value) => {
                let (value, _) = self.expr(&value, env)?;
                env.live = false;
                Stmt::Return(value)
            }
            Stmt::Throw(value) => {
                let (value, _) = self.expr(&value, env)?;
                env.live = false;
                Stmt::Throw(value)
            }
            Stmt::Print(value) => Stmt::Print(self.expr(&value, env)?.0),
            Stmt::Finish(value) => {
                let (value, _) = self.expr(&value, env)?;
                env.live = false;
                Stmt::Finish(value)
            }
            Stmt::Fail(value) => {
                let (value, _) = self.expr(&value, env)?;
                env.live = false;
                Stmt::Fail(value)
            }
        };
        if self.edits == before {
            return Some(id);
        }
        let new = StmtId(self.tables.stmts.len() as u32);
        self.tables.stmts.push(stmt);
        self.nodes += 1;
        Some(new)
    }

    fn rhs(&mut self, rhs: Rhs, env: &mut Env) -> Option<(Rhs, Fact)> {
        Some(match rhs {
            Rhs::Expr(expr) => {
                let (expr, abs) = self.expr(&expr, env)?;
                (Rhs::Expr(expr), abs.fact)
            }
            Rhs::Action(action) => {
                env.mutated();
                (Rhs::Action(action), Fact::default())
            }
        })
    }

    fn member(&mut self, member: Member, env: &Env) -> Option<Member> {
        Some(match member {
            Member::Field(target, field) => Member::Field(self.expr(&target, env)?.0, field),
            Member::Index(target, index) => {
                Member::Index(self.expr(&target, env)?.0, self.expr(&index, env)?.0)
            }
        })
    }

    fn exprs(&mut self, exprs: &[Expr], env: &Env) -> Option<Vec<Expr>> {
        exprs
            .iter()
            .map(|expr| self.expr(expr, env).map(|(expr, _)| expr))
            .collect()
    }

    /// A folded expression: its value and the events of evaluating it.
    fn fold(&mut self, value: Value, events: Vec<Event>, natives: u64) -> (Expr, Abs) {
        self.edits += 1;
        self.nodes += 1;
        self.folds += natives;
        self.score += natives.saturating_mul(8u64.saturating_pow(self.depth));
        let abs = Abs {
            fact: Fact::constant(&value),
            events: Some(events.clone()),
        };
        let expr = Expr::Folded(Box::new(Folded {
            events: coalesce(events),
            value,
        }));
        (expr, abs)
    }

    fn expr(&mut self, expr: &Expr, env: &Env) -> Option<(Expr, Abs)> {
        self.spend()?;
        Some(match expr {
            Expr::Literal(value) => (
                expr.clone(),
                Abs {
                    fact: Fact::constant(value),
                    events: Some(vec![charge(1, None)]),
                },
            ),
            Expr::Var(Var::Local(local)) if self.tracked(local) => {
                let fact = env.facts[local.slot as usize].clone();
                let events = Some(vec![charge(1, None)]);
                match &fact.value {
                    // A literal is read as the variable was, for the same
                    // charge.
                    Some(value) => {
                        self.edits += 1;
                        (Expr::Literal(value.clone()), Abs { fact, events })
                    }
                    None => (expr.clone(), Abs { fact, events }),
                }
            }
            // A variable a closure shares is read from its cell.
            Expr::Var(Var::Local(_)) => (
                expr.clone(),
                Abs {
                    fact: Fact::default(),
                    events: Some(vec![charge(1, None)]),
                },
            ),
            Expr::Var(_) | Expr::Clock | Expr::Random | Expr::Closure(_) | Expr::Folded(_) => {
                (expr.clone(), Abs::unknown())
            }
            Expr::Member(member) => match member.as_ref() {
                Member::Field(target, field) => {
                    let (target, abs) = self.expr(target, env)?;
                    match (abs.fact.kind, abs.events) {
                        (Some(ValueKind::Record), Some(mut events)) => {
                            events.insert(0, charge(1, None));
                            if abs.fact.lacks.contains(field) {
                                self.fold(Value::Absent, events, 0)
                            } else {
                                (
                                    Expr::Member(Box::new(Member::Field(target, field.clone()))),
                                    Abs {
                                        fact: Fact::default(),
                                        events: Some(events),
                                    },
                                )
                            }
                        }
                        _ => (
                            Expr::Member(Box::new(Member::Field(target, field.clone()))),
                            Abs::unknown(),
                        ),
                    }
                }
                Member::Index(target, index) => {
                    let target = self.expr(target, env)?.0;
                    let index = self.expr(index, env)?.0;
                    (
                        Expr::Member(Box::new(Member::Index(target, index))),
                        Abs::unknown(),
                    )
                }
            },
            Expr::Call { lib, args } => {
                let mut residual = Vec::with_capacity(args.len());
                let mut abstracts = Vec::with_capacity(args.len());
                for arg in args {
                    let (arg, abs) = self.expr(arg, env)?;
                    residual.push(arg);
                    abstracts.push(abs);
                }
                match self.call(*lib, &abstracts) {
                    Some((value, mut tail)) => {
                        let mut events = vec![charge(1, None)];
                        for abs in abstracts {
                            events.extend(abs.events.unwrap_or_default());
                        }
                        events.append(&mut tail);
                        self.fold(value, events, 1)
                    }
                    None => (
                        Expr::Call {
                            lib: *lib,
                            args: residual,
                        },
                        Abs::unknown(),
                    ),
                }
            }
            Expr::Tuple(items) => (Expr::Tuple(self.exprs(items, env)?), Abs::unknown()),
            Expr::List(items) => (Expr::List(self.exprs(items, env)?), Abs::unknown()),
            Expr::Set(items) => (Expr::Set(self.exprs(items, env)?), Abs::unknown()),
            Expr::Map(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    out.push((self.expr(key, env)?.0, self.expr(value, env)?.0));
                }
                (Expr::Map(out), Abs::unknown())
            }
            Expr::Record(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (name, value) in entries {
                    out.push((name.clone(), self.expr(value, env)?.0));
                }
                (Expr::Record(out), Abs::unknown())
            }
            Expr::Read(read) => (
                Expr::Read(Box::new((
                    self.expr(&read.0, env)?.0,
                    self.expr(&read.1, env)?.0,
                ))),
                Abs::unknown(),
            ),
        })
    }

    /// A native call the facts about its arguments determine: its value,
    /// and its events after the arguments' (the result's pin and the
    /// formula's charge).
    fn call(&mut self, lib: LibId, args: &[Abs]) -> Option<(Value, Vec<Event>)> {
        let function = &self.libs[lib.0 as usize];
        let LibRun::Native(native) = &function.run else {
            return None;
        };
        if function.limit.is_some() || args.len() > function.arity {
            return None;
        }
        if args.iter().any(|abs| abs.events.is_none()) {
            return None;
        }
        let mut values: Vec<Option<Value>> =
            args.iter().map(|abs| abs.fact.value.clone()).collect();
        values.resize(function.arity, Some(Value::Absent));
        let result = if let Some(values) = values.iter().cloned().collect::<Option<Vec<Value>>>() {
            let mut view = NativeView {
                heap: &mut self.scratch,
                bound: u64::MAX,
                reserved: 0,
            };
            let mut counter = WorkCounter::new(None);
            let result = native
                .call(NativeCall {
                    args: &values,
                    heap: &mut view,
                    counter: &mut counter,
                })
                .ok()?;
            if view.reserved != 0 {
                return None;
            }
            result
        } else if Some(lib) == self.known.kind {
            Value::text(kind_name(args.first()?.fact.kind?))
        } else {
            return None;
        };
        if !heap_free(&result) || !within_depth(&result, MAX_VALUE_DEPTH) {
            return None;
        }
        // The formula, measured on what is known; a measurement of an
        // unknown value does not fold.
        let mut unknown = false;
        let units = function.charge.evaluate(|source, measure| {
            let value = match source {
                Source::Arg(index) => values.get(index).cloned().flatten(),
                Source::Result => Some(result.clone()),
                Source::Nothing => return 0,
            };
            let Some(value) = value else {
                unknown = true;
                return 0;
            };
            match measure {
                Measure::Size => size(&self.scratch, &value),
                Measure::DeepSize => deep_size(&self.scratch, &value),
                Measure::NestedSize => nested_size(&self.scratch, &value),
                Measure::Magnitude => magnitude(&value),
            }
        });
        if unknown {
            return None;
        }
        Some((
            result.clone(),
            vec![Event::Pin { value: result, lib }, charge(units, Some(lib))],
        ))
    }
}
