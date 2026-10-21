//! Partial evaluation of helper bodies over immediate operand kinds.
//!
//! Kind dispatch and constant tests disappear during generation. Remaining
//! scalar calls use the registered native's prepared entry, never a second
//! implementation of its semantics. An unsupported path simply declines.
//!
//! A second pass also takes lists, maps, records and long texts as operand
//! kinds, and keeps the paths that read, build or write arena objects: each
//! such form goes through the machine's [`lash_kernel_doc::plan::TemplateHeap`],
//! and every other call through the native's general call. A heap path runs
//! where the helper's plan would, so it declares its variables as the plan
//! binds them and calls other helpers as the plan invokes them, never
//! inlined. It writes at most once, as its last fallible step, and may
//! decline after building an object only in its arm's last alternative, so
//! that a path that declines leaves nothing behind but what a miss leaves.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use lash_kernel_doc::ActKind;
use lash_kernel_doc::Action;
use lash_kernel_doc::Atom;
use lash_kernel_doc::Callee;
use lash_kernel_doc::Expr;
use lash_kernel_doc::FunctionDefinition;
use lash_kernel_doc::Literal;
use lash_kernel_doc::Member;
use lash_kernel_doc::Node;
use lash_kernel_doc::Place;
use lash_kernel_doc::Rhs;
use lash_kernel_doc::Stmt;
use lash_kernel_doc::TryStmt;
use lash_kernel_doc::Value;
use lash_kernel_doc::ValueKind;
use lash_kernel_doc::arena::ArenaHandle;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::Name;

use crate::library::Library;

/// The most native calls and arena forms one path's answer takes.
const PATH_CALLS: usize = 8;

/// The most native calls a scalar path takes when its helper answers
/// anything but a text: one, so that an operator's template stays as
/// small as its native (the hot arithmetic rows lose their inlining to a
/// larger one). No one native spells a text, so a helper that answers one
/// composes up to [`PATH_CALLS`] prepared calls, where its conversion
/// would otherwise go to the heap paths (FIG-6777).
const SCALAR_CALLS: usize = 1;

/// The most alternatives one combination of operand kinds keeps.
const ALTERNATIVES: usize = 16;

#[derive(Clone)]
struct Symbol {
    code: String,
    constant: Option<Value>,
    kind: Option<ValueKind>,
    calls: usize,
}

impl Symbol {
    fn constant(value: Value) -> Option<Self> {
        let code = match value {
            Value::Null => "Value::Null".into(),
            Value::Absent => "Value::Absent".into(),
            Value::Bool(value) => format!("Value::Bool({value})"),
            Value::Int(value) => format!("Value::Int({value})"),
            Value::Float(value) => format!(
                "Value::Float(lash_kernel_doc::Float::new(f64::from_bits({})))",
                value.get().to_bits()
            ),
            Value::Text(value) => format!(
                "Value::Text(const {{ lash_kernel_doc::arena::ShortText::new({:?}) }}?)",
                value.as_str()
            ),
            _ => return None,
        };
        Some(Self {
            code,
            constant: Some(value),
            kind: Some(value.kind()),
            calls: 0,
        })
    }

    /// A value only the running path knows.
    fn computed(code: String, kind: Option<ValueKind>, calls: usize) -> Self {
        Self {
            code,
            constant: None,
            kind,
            calls,
        }
    }
}

/// What a step may do. Alternatives are checked over these, never over
/// the Rust a step renders as.
#[derive(Clone, Copy, Default)]
struct Effects {
    /// The step may decline, ending its path.
    declines: bool,
    /// It builds an object, or makes a call that may.
    builds: bool,
    /// It writes an arena object: only the path's return may follow.
    writes: bool,
    /// It reads, builds or writes an arena object, or calls for what may
    /// be one: what only a heap path answers.
    arena: bool,
}

impl Effects {
    const DECLINES: Self = Self {
        declines: true,
        builds: false,
        writes: false,
        arena: false,
    };

    fn or(self, other: Self) -> Self {
        Self {
            declines: self.declines || other.declines,
            builds: self.builds || other.builds,
            writes: self.writes || other.writes,
            arena: self.arena || other.arena,
        }
    }
}

/// An arena form or general call a heap path names the value of.
#[derive(Clone)]
enum Form {
    /// A tuple of the operands.
    Tuple(String),
    /// A field of the target.
    Field { target: String, field: String },
    /// An element of the target.
    Index { target: String, index: String },
    /// A collection's length, by the [`lash_kernel_doc::plan::Collection`]
    /// variant its native names.
    Len {
        collection: &'static str,
        target: String,
    },
    /// Whether a table holds a key, by the [`lash_kernel_doc::plan::Table`]
    /// variant its native names, and the entry it holds there: one probe
    /// answers both (FIG-6846).
    Lookup {
        table: &'static str,
        target: String,
        key: String,
    },
    /// The general call of a native, with what it answers: an object it
    /// may build, and one only the arena holds.
    Call {
        callee: usize,
        args: String,
        builds: bool,
        arena: bool,
    },
}

/// One step of a path, its operands already rendered.
#[derive(Clone)]
enum Step {
    /// `form`'s value, named.
    Heap { name: String, form: Form },
    /// A prepared entry's value on immediates, named.
    Entry {
        name: String,
        native: usize,
        args: String,
    },
    /// A prepared entry's value, the general call answering where the
    /// entry declines; what it answers may be built.
    Fallback {
        name: String,
        native: usize,
        general: usize,
        args: String,
        builds: bool,
    },
    /// The test that selects an `if`'s branch.
    Guard { test: String, holds: bool },
    /// The plan variable `value` is bound to, grown past what an
    /// assignment replaces.
    Bind {
        value: String,
        previous: Option<String>,
    },
    /// An element write.
    SetIndex {
        target: String,
        index: String,
        value: String,
    },
    /// A helper's guarded alternatives in source order, each its steps
    /// and value: the first to answer is the value, named.
    Alternatives {
        name: String,
        alternatives: Vec<(Vec<Step>, String)>,
    },
}

impl Step {
    fn effects(&self) -> Effects {
        match self {
            Self::Heap { form, .. } => {
                let (builds, arena) = match form {
                    Form::Tuple(_) => (true, true),
                    Form::Field { .. }
                    | Form::Index { .. }
                    | Form::Len { .. }
                    | Form::Lookup { .. } => (false, true),
                    Form::Call { builds, arena, .. } => (*builds, *arena),
                };
                Effects {
                    builds,
                    arena,
                    ..Effects::DECLINES
                }
            }
            Self::Fallback { builds, .. } => Effects {
                builds: *builds,
                arena: true,
                ..Effects::DECLINES
            },
            Self::Entry { .. } | Self::Guard { .. } | Self::Bind { .. } => Effects::DECLINES,
            Self::SetIndex { .. } => Effects {
                writes: true,
                arena: true,
                ..Effects::DECLINES
            },
            Self::Alternatives { alternatives, .. } => alternatives
                .iter()
                .flat_map(|(steps, _)| steps)
                .fold(Effects::DECLINES, |effects, step| {
                    effects.or(step.effects())
                }),
        }
    }
}

impl std::fmt::Display for Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tuple(items) => write!(f, "heap.tuple(&[{items}])"),
            Self::Field { target, field } => write!(f, "heap.field({target}, {field:?})"),
            Self::Index { target, index } => write!(f, "heap.index({target}, {index})"),
            Self::Len { collection, target } => {
                write!(f, "heap.len(p::Collection::{collection}, {target})")
            }
            Self::Lookup { table, target, key } => {
                write!(f, "heap.lookup(p::Table::{table}, {target}, {key})")
            }
            Self::Call { callee, args, .. } => write!(f, "heap.call({callee}, &[{args}])"),
        }
    }
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // A lookup's value is whether the table holds the key; the
            // entry it found is read only where a guard proved it held.
            Self::Heap {
                name,
                form: form @ Form::Lookup { .. },
            } => write!(
                f,
                "let {name}_entry = {form}?; let {name} = Value::Bool({name}_entry.is_some()); let {name}_found = {name}_entry.unwrap_or(Value::Absent); let _ = ({name}, {name}_found);"
            ),
            Self::Heap { name, form } => write!(f, "let {name} = {form}?; let _ = {name};"),
            Self::Entry { name, native, args } => write!(
                f,
                "let {name} = (natives[{native}].entry)(&[{args}])?; let _ = {name};"
            ),
            Self::Fallback {
                name,
                native,
                general,
                args,
                ..
            } => write!(
                f,
                "let {name} = match (natives[{native}].entry)(&[{args}]) {{ Some(value) => value, None => heap.call({general}, &[{args}])? }}; let _ = {name};"
            ),
            Self::Guard { test, holds } => {
                write!(
                    f,
                    "if !matches!({test}, Value::Bool({holds})) {{ return None; }}"
                )
            }
            Self::Bind { value, previous } => match previous {
                Some(previous) => write!(f, "heap.bind({value}, Some({previous}))?;"),
                None => write!(f, "heap.bind({value}, None)?;"),
            },
            Self::SetIndex {
                target,
                index,
                value,
            } => write!(f, "heap.set_index({target}, {index}, {value})?;"),
            Self::Alternatives { name, alternatives } => {
                writeln!(f, "let {name} = (|| {{")?;
                let alternatives: Vec<_> = alternatives
                    .iter()
                    .map(|(steps, value)| (steps.as_slice(), value.as_str()))
                    .collect();
                let mut code = String::new();
                tree(&mut code, &alternatives, "");
                f.write_str(&code)?;
                writeln!(f, "None\n}})()?;\nlet _ = {name};")
            }
        }
    }
}

/// Whether `step` computes from its operands alone and changes nothing:
/// two alternatives that begin with it compute it alike, and it declines
/// in both or in neither.
fn pure(step: &Step) -> bool {
    matches!(step, Step::Entry { .. } | Step::Guard { .. })
}

/// `alternatives`, each its steps and value, tried in order until one
/// answers, rendered at `pad`: consecutive ones that begin with the same
/// pure step compute it once, then try what follows it in each, so that
/// where it declines none of them is tried (FIG-6820). The first to answer
/// is the one the alternatives in order give, at the same steps.
fn tree(code: &mut String, alternatives: &[(&[Step], &str)], pad: &str) {
    let mut at = 0;
    while let Some(&(steps, value)) = alternatives.get(at) {
        let first = steps
            .first()
            .filter(|step| pure(step))
            .map(ToString::to_string);
        let run = match &first {
            Some(first) => alternatives[at..]
                .iter()
                .take_while(|(steps, _)| {
                    steps
                        .first()
                        .is_some_and(|step| pure(step) && step.to_string() == *first)
                })
                .count(),
            None => 1,
        };
        let _ = writeln!(code, "{pad}if let Some(value) = (|| {{");
        match first {
            Some(first) if run > 1 => {
                let _ = writeln!(code, "{pad}    {first}");
                let rest: Vec<_> = alternatives[at..at + run]
                    .iter()
                    .map(|&(steps, value)| (&steps[1..], value))
                    .collect();
                tree(code, &rest, &format!("{pad}    "));
                let _ = writeln!(code, "{pad}    None");
            }
            _ => {
                for step in steps {
                    let _ = writeln!(code, "{pad}    {step}");
                }
                let _ = writeln!(code, "{pad}    Some({value})");
            }
        }
        let _ = writeln!(code, "{pad}}})() {{ return Some(value); }}");
        at += run;
    }
}

/// A table lookup a heap path made: the name of whether the key is held,
/// what it looked up, at which step, and whether a guard since proved the
/// key held.
#[derive(Clone)]
struct Probe {
    name: String,
    target: String,
    key: String,
    at: usize,
    held: bool,
}

#[derive(Clone, Default)]
struct State {
    vars: BTreeMap<Name, Symbol>,
    steps: Vec<Step>,
    depth: u32,
    probes: Vec<Probe>,
    /// Whether the path declared or assigned a plan variable, with a bind
    /// step or without one.
    declares: bool,
}

impl State {
    fn effects(&self) -> Effects {
        self.steps.iter().fold(Effects::default(), |effects, step| {
            effects.or(step.effects())
        })
    }

    /// The name of the entry a lookup of `key` in `target` found, where a
    /// guard proved the table held it and no step since the lookup can
    /// have changed the table: `target[key]` reads it without a second
    /// probe (FIG-6846).
    fn found(&self, target: &str, key: &str) -> Option<String> {
        let probe = self
            .probes
            .iter()
            .rev()
            .find(|probe| probe.held && probe.target == target && probe.key == key)?;
        self.steps[probe.at + 1..]
            .iter()
            .all(|step| {
                matches!(
                    step,
                    Step::Bind { .. }
                        | Step::Guard { .. }
                        | Step::Entry { .. }
                        | Step::Heap {
                            form: Form::Field { .. }
                                | Form::Index { .. }
                                | Form::Len { .. }
                                | Form::Lookup { .. }
                                | Form::Tuple(_),
                            ..
                        }
                )
            })
            .then(|| format!("{}_found", probe.name))
    }

    /// Whether a step that may decline follows the path's first build:
    /// declining there leaves what it built.
    fn exposed(&self) -> bool {
        self.steps
            .iter()
            .skip_while(|step| !step.effects().builds)
            .skip(1)
            .any(|step| step.effects().declines)
    }
}

struct Evaluator<'a> {
    library: &'a Library,
    callees: Vec<FunctionId>,
    /// The natives heap paths call generally.
    heap_callees: Vec<FunctionId>,
    used: usize,
    /// Whether arena forms and general calls are allowed.
    heap: bool,
    /// The names a closure of the body may capture, whose plan variables
    /// are cells: a heap path declares none of them.
    captured: std::collections::BTreeSet<Name>,
}

/// Every name a closure in `block` uses that it does not declare itself,
/// as a parameter or a variable: the names it may capture.
fn captured(block: &[Stmt]) -> std::collections::BTreeSet<Name> {
    use std::collections::BTreeSet;
    /// The names `node` mentions, and those it declares.
    fn names(node: Node<'_>, used: &mut BTreeSet<Name>, declared: &mut BTreeSet<Name>) {
        match node {
            Node::Expr(Expr::Variable(name))
            | Node::Stmt(Stmt::Assign {
                place: Place::Variable(name),
                ..
            }) => {
                used.insert(name.clone());
            }
            Node::Stmt(Stmt::Let { name, .. }) => {
                declared.insert(name.clone());
            }
            Node::Action(Action::Call { args, .. }) => {
                used.extend(args.iter().filter_map(|arg| match arg {
                    Atom::Variable(name) => Some(name.clone()),
                    Atom::Literal(_) => None,
                }));
            }
            _ => {}
        }
        for child in node.children() {
            names(child, used, declared);
        }
    }
    fn walk(node: Node<'_>, captured: &mut BTreeSet<Name>) {
        if let Node::Expr(Expr::Closure(closure)) = node {
            let (mut used, mut declared) = (BTreeSet::new(), BTreeSet::new());
            declared.extend(closure.params.iter().cloned());
            names(Node::Block(&closure.body), &mut used, &mut declared);
            captured.extend(used.difference(&declared).cloned());
        }
        for child in node.children() {
            walk(child, captured);
        }
    }
    let mut captured = BTreeSet::new();
    walk(Node::Block(block), &mut captured);
    captured
}

/// The arena read that collection native `name`'s call on `args` makes,
/// where the heap performs it without the call (FIG-6815): a length or a
/// table's membership test. Every other native's call stays general.
fn read(name: &str, args: &[Symbol]) -> Option<Form> {
    let (collection, operation) = name.split_once('.')?;
    match (operation, args) {
        ("len", [target]) => Some(Form::Len {
            collection: match collection {
                "list" => "List",
                "tuple" => "Tuple",
                "map" => "Map",
                "set" => "Set",
                "record" => "Record",
                _ => return None,
            },
            target: target.code.clone(),
        }),
        ("contains", [target, key]) => Some(Form::Lookup {
            table: match collection {
                "map" => "Map",
                "set" => "Set",
                "record" => "Record",
                _ => return None,
            },
            target: target.code.clone(),
            key: key.code.clone(),
        }),
        _ => None,
    }
}

/// Whether `kind`'s values are immediates every prepared entry takes: a
/// text may be a long one, in the arena.
fn immediate(kind: Option<ValueKind>) -> bool {
    matches!(
        kind,
        Some(
            ValueKind::Null
                | ValueKind::Absent
                | ValueKind::Bool
                | ValueKind::Int
                | ValueKind::Float
        )
    )
}

impl Evaluator<'_> {
    fn literal(literal: &Literal) -> Option<Symbol> {
        Symbol::constant(match literal {
            Literal::Null => Value::Null,
            Literal::Absent => Value::Absent,
            Literal::Bool(value) => Value::Bool(*value),
            Literal::Int(value) => Value::Int(value.word()?),
            Literal::Float(value) => Value::Float(*value),
            Literal::Text(value) => Value::Text(lash_kernel_doc::arena::ShortText::new(value)?),
            _ => return None,
        })
    }

    fn atom(atom: &Atom, state: &State) -> Option<Symbol> {
        match atom {
            Atom::Literal(literal) => Self::literal(literal),
            Atom::Variable(name) => state.vars.get(name).cloned(),
        }
    }

    fn rhs(&mut self, rhs: &Rhs, state: &mut State, depth: u32) -> Option<Symbol> {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, state, depth),
            Rhs::Action(Action::Call {
                callee: Callee::Library(function),
                args,
            }) => {
                let args = args
                    .iter()
                    .map(|arg| Self::atom(arg, state))
                    .collect::<Option<Vec<_>>>()?;
                self.call(*function, args, state, depth)
            }
            _ => None,
        }
    }

    /// A fresh name for a value a step computes.
    fn fresh(&mut self) -> String {
        let code = format!("v{}", self.used);
        self.used += 1;
        code
    }

    /// The heap form `form` as the next step of a heap path, its value
    /// named.
    fn heap_step(
        &mut self,
        state: &mut State,
        form: Form,
        (kind, calls): (Option<ValueKind>, usize),
    ) -> Symbol {
        let name = self.fresh();
        if let Form::Lookup { target, key, .. } = &form {
            state.probes.push(Probe {
                name: name.clone(),
                target: target.clone(),
                key: key.clone(),
                at: state.steps.len(),
                held: false,
            });
        }
        state.steps.push(Step::Heap {
            name: name.clone(),
            form,
        });
        Symbol::computed(name, kind, calls + 1)
    }

    fn expr(&mut self, expr: &Expr, state: &mut State, depth: u32) -> Option<Symbol> {
        match expr {
            Expr::Literal(literal) => Self::literal(literal),
            Expr::Variable(name) => state.vars.get(name).cloned(),
            Expr::Call { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.expr(arg, state, depth))
                    .collect::<Option<Vec<_>>>()?;
                self.call(*function, args, state, depth)
            }
            Expr::Act(act) => {
                let ActKind::Call(Callee::Library(function)) = act.kind else {
                    return None;
                };
                let args = act
                    .args
                    .iter()
                    .map(|arg| self.expr(arg, state, depth))
                    .collect::<Option<Vec<_>>>()?;
                self.call(function, args, state, depth)
            }
            Expr::Let { name, value, body } => {
                let value = self.expr(value, state, depth)?;
                let previous = state.vars.insert(name.clone(), value);
                let result = self.expr(body, state, depth);
                match previous {
                    Some(value) => {
                        state.vars.insert(name.clone(), value);
                    }
                    None => {
                        state.vars.remove(name);
                    }
                }
                result
            }
            Expr::If { cond, then, r#else } => {
                let test = self.expr(cond, state, depth)?.constant?;
                match test {
                    Value::Bool(true) => self.expr(then, state, depth),
                    Value::Bool(false) => self.expr(r#else, state, depth),
                    _ => None,
                }
            }
            Expr::Tuple(items) if self.heap => {
                let items = items
                    .iter()
                    .map(|item| self.expr(item, state, depth))
                    .collect::<Option<Vec<_>>>()?;
                let calls = items.iter().map(|item| item.calls).sum();
                let form = Form::Tuple(codes(&items));
                let kind = Some(ValueKind::Tuple);
                Some(self.heap_step(state, form, (kind, calls)))
            }
            Expr::Member(member) if self.heap => match &**member {
                Member::Field { target, field } => {
                    let target = self.expr(target, state, depth)?;
                    let calls = target.calls;
                    let form = Form::Field {
                        target: target.code,
                        field: field.clone(),
                    };
                    Some(self.heap_step(state, form, (None, calls)))
                }
                Member::Index { target, index } => {
                    let target = self.expr(target, state, depth)?;
                    let index = self.expr(index, state, depth)?;
                    let calls = target.calls + index.calls;
                    if let Some(found) = state.found(&target.code, &index.code) {
                        return Some(Symbol::computed(found, None, calls));
                    }
                    let form = Form::Index {
                        target: target.code,
                        index: index.code,
                    };
                    Some(self.heap_step(state, form, (None, calls)))
                }
            },
            _ => None,
        }
    }

    /// The index of `function` among the general callees.
    fn heap_callee(&mut self, function: FunctionId) -> usize {
        match self
            .heap_callees
            .iter()
            .position(|callee| *callee == function)
        {
            Some(index) => index,
            None => {
                self.heap_callees.push(function);
                self.heap_callees.len() - 1
            }
        }
    }

    /// The kind a call of `definition` answers, from its signature.
    fn result_kind(&self, definition: &FunctionDefinition) -> Option<ValueKind> {
        match definition.signature.result {
            lash_vocabulary::types::Type::Bool => Some(ValueKind::Bool),
            lash_vocabulary::types::Type::Float => Some(ValueKind::Float),
            lash_vocabulary::types::Type::Int => Some(ValueKind::Int),
            lash_vocabulary::types::Type::Text if !self.heap => Some(ValueKind::Text),
            lash_vocabulary::types::Type::Null => Some(ValueKind::Null),
            _ => None,
        }
    }

    /// The general call of `function` on `args`, a heap path's step: what
    /// it answers may be built unless it is a truth, a count of a
    /// collection or a number of immediates. A helper's that answers a
    /// number or a truth is one its entry or plan answers alike from
    /// immediates, and only an object operand makes it the arena's.
    fn general(
        &mut self,
        function: FunctionId,
        definition: &FunctionDefinition,
        args: &[Symbol],
        state: &mut State,
    ) -> Symbol {
        let kind = self.result_kind(definition);
        let counts = kind == Some(ValueKind::Int) && args.iter().all(|arg| !immediate(arg.kind));
        let number = matches!(
            definition.signature.result,
            lash_vocabulary::types::Type::Int
                | lash_vocabulary::types::Type::Float
                | lash_vocabulary::types::Type::Number
        ) && args.iter().all(|arg| immediate(arg.kind));
        let builds = !matches!(kind, Some(ValueKind::Bool | ValueKind::Null)) && !counts && !number;
        let helper = definition.name.as_str().starts_with("py.")
            || definition.name.as_str().starts_with("ts.");
        let arena = !helper
            || !matches!(
                definition.signature.result,
                lash_vocabulary::types::Type::Bool
                    | lash_vocabulary::types::Type::Int
                    | lash_vocabulary::types::Type::Float
                    | lash_vocabulary::types::Type::Number
                    | lash_vocabulary::types::Type::Null
            );
        let form = Form::Call {
            callee: self.heap_callee(function),
            args: codes(args),
            builds,
            arena,
        };
        let calls = args.iter().map(|arg| arg.calls).sum();
        self.heap_step(state, form, (kind, calls))
    }

    fn call(
        &mut self,
        function: FunctionId,
        args: Vec<Symbol>,
        state: &mut State,
        depth: u32,
    ) -> Option<Symbol> {
        let registered = self.library.registry.get(&function)?;
        let definition = registered.definition();
        // `brand.built` answers the body it is written in (`K-LIB-010`),
        // which a template entry is not.
        if definition.name.as_str() == "brand.built" && definition.body().is_none() {
            return None;
        }
        // Kind is known even though the value is not. This is the only
        // intrinsic reduction; all value computations use native entries.
        if definition.name.as_str() == "kind" {
            let kind = args.first()?.kind?;
            let handle = ArenaHandle::new(0, 0);
            let sample = match kind {
                ValueKind::Null => Value::Null,
                ValueKind::Absent => Value::Absent,
                ValueKind::Bool => Value::Bool(false),
                ValueKind::Int => Value::Int(0),
                ValueKind::Float => Value::Float(lash_kernel_doc::Float::new(0.0)),
                ValueKind::Text => Value::Text(lash_kernel_doc::arena::ShortText::new("")?),
                ValueKind::List if self.heap => Value::List(handle),
                ValueKind::Map if self.heap => Value::Map(handle),
                ValueKind::Record if self.heap => Value::Record(handle),
                ValueKind::Tuple if self.heap => Value::Tuple(handle),
                _ => return None,
            };
            return Symbol::constant((registered.native()?.prepare()?.entry)(&[sample])?);
        }
        if definition.name.as_str().starts_with("py.")
            || definition.name.as_str().starts_with("ts.")
        {
            if !self.heap {
                return self.helper(definition, args, state, depth);
            }
            // A plan invokes another helper, whose own entry or plan
            // answers: the call is the general one, its admission one
            // frame deeper.
            state.depth = state.depth.max(depth + 1);
            return Some(self.general(function, definition, &args, state));
        }
        if self.heap
            && definition.guard.is_none()
            && registered.native().is_some()
            && let Some(form) = read(definition.name.as_str(), &args)
        {
            let kind = self.result_kind(definition);
            let calls = args.iter().map(|arg| arg.calls).sum();
            return Some(self.heap_step(state, form, (kind, calls)));
        }
        if definition.guard.is_some() {
            if self.heap {
                return Some(self.general(function, definition, &args, state));
            }
            return None;
        }
        let entry = match registered.native()?.prepare() {
            Some(entry) => entry,
            None if self.heap => return Some(self.general(function, definition, &args, state)),
            None => return None,
        };
        if let Some(values) = args
            .iter()
            .map(|arg| arg.constant)
            .collect::<Option<Vec<_>>>()
        {
            return Symbol::constant((entry.entry)(&values)?);
        }
        let index = match self.callees.iter().position(|callee| *callee == function) {
            Some(index) => index,
            None => {
                self.callees.push(function);
                self.callees.len() - 1
            }
        };
        let kind = self
            .result_kind(definition)
            .filter(|kind| *kind != ValueKind::Null);
        // Boolean negation is a result wrapper; the underlying comparison
        // or truth test remains the one value-producing native.
        let calls = usize::from(definition.name.as_str() != "bool.not")
            + args.iter().map(|arg| arg.calls).sum::<usize>();
        let name = self.fresh();
        let result = &definition.signature.result;
        let reserved = matches!(result, lash_vocabulary::types::Type::Text);
        state.steps.push(
            if self.heap && (reserved || !args.iter().all(|arg| immediate(arg.kind))) {
                // An operand the entry may decline, or a text whose room
                // only the general call reserves: the general call answers.
                Step::Fallback {
                    name: name.clone(),
                    native: index,
                    general: self.heap_callee(function),
                    args: codes(&args),
                    builds: !matches!(
                        result,
                        lash_vocabulary::types::Type::Bool
                            | lash_vocabulary::types::Type::Int
                            | lash_vocabulary::types::Type::Float
                            | lash_vocabulary::types::Type::Number
                            | lash_vocabulary::types::Type::Null
                    ),
                }
            } else {
                Step::Entry {
                    name: name.clone(),
                    native: index,
                    args: codes(&args),
                }
            },
        );
        Some(Symbol::computed(name, kind, calls))
    }

    /// A call of helper `definition` on `args`, its body followed: a
    /// scalar path's, which no heap path shares.
    fn helper(
        &mut self,
        definition: &FunctionDefinition,
        args: Vec<Symbol>,
        state: &mut State,
        depth: u32,
    ) -> Option<Symbol> {
        let body = definition.body()?;
        let mut nested = State {
            steps: Vec::new(),
            vars: definition
                .signature
                .params
                .iter()
                .zip(args)
                .map(|(param, arg)| (param.name.clone(), arg))
                .collect(),
            depth: depth + 1,
            probes: Vec::new(),
            declares: false,
        };
        let mut results = self.block(&body.block, &mut nested, depth + 1);
        if results.is_empty() {
            return None;
        }
        for (nested, _) in &results {
            state.depth = state.depth.max(nested.depth);
        }
        if results.len() == 1 {
            let (nested, value) = results.pop()?;
            state.steps.extend(nested.steps);
            return value.or_else(|| Symbol::constant(Value::Null));
        }
        // A helper used as an operand can itself branch on a scalar
        // value (ToBoolean on NaN, for example). Keep those guarded
        // alternatives in its expression, in source evaluation order.
        let name = self.fresh();
        let mut alternatives = Vec::new();
        let mut kind = None;
        let mut calls = 0;
        for (index, (nested, value)) in results.into_iter().enumerate() {
            let value = value.or_else(|| Symbol::constant(Value::Null))?;
            calls = calls.max(value.calls);
            kind = if index == 0 {
                value.kind
            } else {
                kind.filter(|kind| Some(*kind) == value.kind)
            };
            alternatives.push((nested.steps, value.code));
        }
        state.steps.push(Step::Alternatives {
            name: name.clone(),
            alternatives,
        });
        Some(Symbol::computed(name, kind, calls))
    }

    /// The outcomes of running `branch` from `state`, then `rest`: a
    /// branch's declarations end with it, assignments to outer variables
    /// survive it.
    fn enter(
        &mut self,
        branch: &[Stmt],
        mut next: State,
        state: &State,
        rest: &[Stmt],
        depth: u32,
        outcomes: &mut Vec<(State, Option<Symbol>)>,
    ) {
        for (mut next, returned) in self.block(branch, &mut next, depth) {
            if returned.is_some() {
                outcomes.push((next, returned));
                continue;
            }
            if next.effects().writes {
                continue;
            }
            for stmt in branch {
                if let Stmt::Let { name, .. } = stmt {
                    match state.vars.get(name) {
                        Some(previous) => {
                            next.vars.insert(name.clone(), previous.clone());
                        }
                        None => {
                            next.vars.remove(name);
                        }
                    }
                }
            }
            outcomes.extend(self.block(rest, &mut next, depth));
        }
    }

    fn block(
        &mut self,
        block: &[Stmt],
        state: &mut State,
        depth: u32,
    ) -> Vec<(State, Option<Symbol>)> {
        let Some((stmt, rest)) = block.split_first() else {
            return vec![(state.clone(), None)];
        };
        // A write is a path's last fallible step: only a return of what
        // the path already holds may follow it.
        if state.effects().writes {
            let Stmt::Return { value } = stmt else {
                return Vec::new();
            };
            let steps = state.steps.len();
            return match self.expr(value, state, depth) {
                Some(value) if state.steps.len() == steps => vec![(state.clone(), Some(value))],
                _ => Vec::new(),
            };
        }
        match stmt {
            Stmt::Return { value } => self
                .expr(value, state, depth)
                .map(|value| vec![(state.clone(), Some(value))])
                .unwrap_or_default(),
            Stmt::Let { name, value }
            | Stmt::Assign {
                place: Place::Variable(name),
                value,
                ..
            } => {
                let Some(value) = self.rhs(value, state, depth) else {
                    return Vec::new();
                };
                if self.heap {
                    // The plan's variable grows by the value, past what an
                    // assignment replaces; a captured one is a cell. An
                    // immediate grows nothing the arena holds: a constant,
                    // or a null, absent, bool or float (an integer may be a
                    // big one) is bound without a step (FIG-6846).
                    let previous = match stmt {
                        Stmt::Let { .. } => Some(None),
                        _ => state.vars.get(name).map(|old| Some(old.code.clone())),
                    };
                    let Some(previous) = previous.filter(|_| !self.captured.contains(name)) else {
                        return Vec::new();
                    };
                    state.declares = true;
                    let immediate = value.constant.is_some()
                        || matches!(
                            value.kind,
                            Some(
                                ValueKind::Null
                                    | ValueKind::Absent
                                    | ValueKind::Bool
                                    | ValueKind::Float
                            )
                        );
                    if !immediate {
                        state.steps.push(Step::Bind {
                            value: value.code.clone(),
                            previous,
                        });
                    }
                }
                state.vars.insert(name.clone(), value);
                self.block(rest, state, depth)
            }
            Stmt::Assign {
                place: Place::Member(Member::Index { target, index }),
                value: Rhs::Expr(value),
                ..
            } if self.heap => {
                // Every part is computed before the write, so the order of
                // their evaluation changes nothing a declined path shows.
                let parts = (|| {
                    let value = self.expr(value, state, depth)?;
                    let target = self.expr(target, state, depth)?;
                    let index = self.expr(index, state, depth)?;
                    Some((target, index, value))
                })();
                let Some((target, index, value)) = parts else {
                    return Vec::new();
                };
                state.steps.push(Step::SetIndex {
                    target: target.code,
                    index: index.code,
                    value: value.code,
                });
                self.block(rest, state, depth)
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                let Some(test) = self.expr(condition, state, depth) else {
                    return Vec::new();
                };
                let branches = match test.constant {
                    Some(Value::Bool(true)) => vec![(then_block, None)],
                    Some(Value::Bool(false)) => vec![(else_block, None)],
                    None => vec![(then_block, Some(true)), (else_block, Some(false))],
                    _ => return Vec::new(),
                };
                let mut outcomes = Vec::new();
                for (branch, guard) in branches {
                    let mut next = state.clone();
                    if let Some(guard) = guard {
                        next.steps.push(Step::Guard {
                            test: test.code.clone(),
                            holds: guard,
                        });
                        for probe in &mut next.probes {
                            probe.held |= guard && probe.name == test.code;
                        }
                    }
                    self.enter(branch, next, state, rest, depth, &mut outcomes);
                }
                outcomes
            }
            // A `try` whose body runs through without raising is that
            // body: a raise declines, and the body's own handler runs.
            Stmt::Try(TryStmt {
                body,
                finally: None,
                ..
            }) if self.heap => {
                let mut outcomes = Vec::new();
                self.enter(body, state.clone(), state, rest, depth, &mut outcomes);
                outcomes
            }
            // Discarded calls, allocations, effects, loops and handlers
            // never disappear into a scalar template.
            _ => Vec::new(),
        }
    }
}

fn codes(symbols: &[Symbol]) -> String {
    symbols
        .iter()
        .map(|symbol| symbol.code.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a parameter of type `ty` takes a value of `kind`.
fn accepts(ty: &lash_vocabulary::types::Type, kind: ValueKind) -> bool {
    use lash_vocabulary::types::Type;
    match ty {
        Type::Any => true,
        Type::Null => kind == ValueKind::Null,
        Type::Absent => kind == ValueKind::Absent,
        Type::Bool => kind == ValueKind::Bool,
        Type::Int => kind == ValueKind::Int,
        Type::Float => kind == ValueKind::Float,
        Type::Number => matches!(kind, ValueKind::Int | ValueKind::Float),
        Type::Text | Type::Enum(_) => kind == ValueKind::Text,
        Type::List(_) => kind == ValueKind::List,
        Type::Map(_) => kind == ValueKind::Map,
        Type::Record(_) => kind == ValueKind::Record,
        Type::Union(types) => types.iter().any(|ty| accepts(ty, kind)),
        _ => false,
    }
}

/// The operand kinds a pass specializes over: the pattern each matches,
/// its kind, and whether it is an arena object's.
const IMMEDIATE: [(&str, ValueKind, bool); 6] = [
    ("Null", ValueKind::Null, false),
    ("Absent", ValueKind::Absent, false),
    ("Bool(_)", ValueKind::Bool, false),
    ("Int(_)", ValueKind::Int, false),
    ("Float(_)", ValueKind::Float, false),
    ("Text(_)", ValueKind::Text, false),
];

const HEAP: [(&str, ValueKind, bool); 4] = [
    ("LongText(_)", ValueKind::Text, true),
    ("List(_)", ValueKind::List, true),
    ("Map(_)", ValueKind::Map, true),
    ("Record(_)", ValueKind::Record, true),
];

/// One pass's arms: each code with the operand kinds that select it, a
/// pattern name per parameter.
struct Pass {
    arms: BTreeMap<String, Vec<Vec<&'static str>>>,
    depth: u32,
    forwards: lash_kernel_doc::plan::Forwards,
    /// Whether a path of an arm writes an arena object.
    writes: bool,
}

impl Evaluator<'_> {
    /// A path returning one native's unchanged arguments. Integer floor
    /// kernels also cover a preceding nonzero-divisor guard: they decline
    /// zero (and word overflow), leaving the template's error path intact.
    /// Float paths retain their guard and never use this exception.
    fn forwarded(&self, kinds: &[&str], steps: &[Step], value: &Symbol) -> Option<u8> {
        use lash_kernel_doc::scalar::ScalarOp;
        let (Step::Entry { name, native, args }, prefix) = steps.split_last()? else {
            return None;
        };
        if value.code != *name || args != "args[0], args[1]" {
            return None;
        }
        match prefix {
            [] => {}
            [
                Step::Entry {
                    name: zero,
                    native: equal,
                    args,
                },
                Step::Guard { test, holds: false },
            ] if kinds == ["Int(_)", "Int(_)"]
                && test == zero
                && args == "args[1], Value::Int(0)" =>
            {
                let pair = |native: usize| {
                    self.library
                        .registry
                        .get(self.callees.get(native)?)?
                        .native()?
                        .prepare()?
                        .pair
                };
                if pair(*equal)?.ints != Some(ScalarOp::Eq)
                    || !matches!(
                        pair(*native)?.ints,
                        Some(ScalarOp::RemFloor | ScalarOp::FloorDiv)
                    )
                {
                    return None;
                }
            }
            _ => return None,
        }
        u8::try_from(*native).ok()
    }

    /// Every combination of `kinds` over `definition`'s parameters whose
    /// paths reduce.
    fn pass(
        &mut self,
        definition: &FunctionDefinition,
        kinds: &[(&'static str, ValueKind, bool)],
    ) -> Option<Pass> {
        let body = definition.body()?;
        let choices: Vec<Vec<_>> = definition
            .signature
            .params
            .iter()
            .map(|param| {
                kinds
                    .iter()
                    .copied()
                    .filter(|(_, kind, _)| {
                        accepts(&param.ty, *kind) || param.optional && *kind == ValueKind::Absent
                    })
                    .collect()
            })
            .collect();
        let combinations = choices
            .iter()
            .try_fold(1usize, |count, choices| count.checked_mul(choices.len()))?;
        let mut pass = Pass {
            arms: BTreeMap::new(),
            forwards: Default::default(),
            depth: 1,
            writes: false,
        };
        for mask in 0..combinations {
            let mut state = State {
                depth: 1,
                ..State::default()
            };
            let mut selected = mask;
            let mut names = Vec::new();
            let mut objects = false;
            for (index, param) in definition.signature.params.iter().enumerate() {
                let (name, kind, object) = choices[index][selected % choices[index].len()];
                selected /= choices[index].len();
                objects |= object;
                names.push(name);
                state.vars.insert(
                    param.name.clone(),
                    Symbol {
                        code: format!("args[{index}]"),
                        constant: match kind {
                            ValueKind::Null => Some(Value::Null),
                            ValueKind::Absent => Some(Value::Absent),
                            _ => None,
                        },
                        kind: Some(kind),
                        calls: 0,
                    },
                );
            }
            self.used = 0;
            let paths = self.block(&body.block, &mut state, 1);
            let reduced = paths.len();
            let mut results: Vec<_> = paths
                .into_iter()
                .filter(|(_, value)| {
                    value.as_ref().is_none_or(|value| {
                        // The path's answer owns its budget: an Any-returning
                        // operator may have both numeric and textual arms.
                        let most = if self.heap
                            || definition.signature.result == lash_vocabulary::types::Type::Text
                            || value.kind == Some(ValueKind::Text)
                        {
                            PATH_CALLS
                        } else {
                            SCALAR_CALLS
                        };
                        value.calls <= most
                    })
                })
                .collect();
            // A scalar arm that lost a path to the call budget declines
            // wherever that path answers, after re-testing what the general
            // call then tests again: it has no arm at all.
            if !self.heap && results.len() != reduced {
                continue;
            }
            if results.is_empty() || results.len() > ALTERNATIVES {
                continue;
            }
            if self.heap {
                // A heap pass keeps what only the arena answers. The `if`s
                // that part alternatives let at most one's tests hold, so
                // the one that may decline after building runs last, where
                // an earlier one declining has built nothing.
                if !objects && results.iter().all(|(state, _)| !state.effects().arena) {
                    continue;
                }
                results.sort_by_key(|(state, _)| state.exposed());
                let last = results.len() - 1;
                if results[..last].iter().any(|(state, _)| state.exposed()) {
                    continue;
                }
            }
            let mut code = String::new();
            if !self.heap {
                // A scalar path's steps are pure: alternatives share what
                // they begin with alike.
                let mut values = Vec::new();
                for (state, value) in &results {
                    pass.depth = pass.depth.max(state.depth);
                    values.push(
                        value
                            .clone()
                            .or_else(|| Symbol::constant(Value::Null))?
                            .code,
                    );
                }
                // Preserve the forwarding fact before rendering the arm.
                if let [(state, Some(value))] = results.as_slice()
                    && let Some(native) = self.forwarded(&names, &state.steps, value)
                {
                    match names.as_slice() {
                        ["Float(_)", "Float(_)"] => pass.forwards.floats = Some(native),
                        ["Int(_)", "Int(_)"] => pass.forwards.ints = Some(native),
                        _ => {}
                    }
                }
                let alternatives: Vec<_> = results
                    .iter()
                    .zip(&values)
                    .map(|((state, _), value)| (state.steps.as_slice(), value.as_str()))
                    .collect();
                tree(&mut code, &alternatives, "            ");
                pass.arms.entry(code).or_default().push(names);
                continue;
            }
            // What an earlier alternative declared is not a later one's;
            // the first begins where the paths do.
            let binds = results.iter().any(|(state, _)| state.declares);
            for (index, (state, value)) in results.into_iter().enumerate() {
                let value = value.or_else(|| Symbol::constant(Value::Null))?;
                pass.depth = pass.depth.max(state.depth);
                pass.writes |= state.effects().writes;
                if binds && index > 0 {
                    let _ = writeln!(code, "            heap.path();");
                }
                let _ = writeln!(code, "            if let Some(value) = (|| {{");
                for step in &state.steps {
                    let _ = writeln!(code, "                {step}");
                }
                let _ = writeln!(
                    code,
                    "                Some({})\n            }})() {{ return Some(value); }}",
                    value.code
                );
            }
            pass.arms.entry(code).or_default().push(names);
        }
        Some(pass)
    }
}

/// `combinations`, each a pattern name per parameter, as few slice
/// patterns as merging them one parameter at a time gives: two that differ
/// in one parameter alone become one with both names there.
fn patterns(combinations: Vec<Vec<&'static str>>) -> String {
    let mut merged: Vec<Vec<Vec<&'static str>>> = combinations
        .into_iter()
        .map(|names| names.into_iter().map(|name| vec![name]).collect())
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        let mut index = 0;
        while index < merged.len() {
            let found = (index + 1..merged.len()).find(|other| {
                let differ = merged[index]
                    .iter()
                    .zip(&merged[*other])
                    .filter(|(a, b)| a != b)
                    .count();
                differ == 1
            });
            if let Some(other) = found {
                let absorbed = merged.remove(other);
                for (names, more) in merged[index].iter_mut().zip(absorbed) {
                    for name in more {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
                changed = true;
            } else {
                index += 1;
            }
        }
    }
    merged
        .iter()
        .map(|pattern| {
            let parts = pattern
                .iter()
                .map(|names| {
                    names
                        .iter()
                        .map(|name| format!("Value::{name}"))
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .collect::<Vec<_>>();
            format!("[{}]", parts.join(", "))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// A match over `arms`, each a code with its patterns, and declining
/// otherwise.
fn arms(arms: BTreeMap<String, Vec<Vec<&'static str>>>) -> String {
    arms.into_iter()
        .map(|(code, combinations)| {
            format!(
                "        {} => {{\n{code}            None\n        }},",
                patterns(combinations)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The helpers whose body, on a list, a float that is a whole number at
/// least zero and below both the list's length and 2³² − 1, and any
/// value, makes the element write `set target[key] = value` and answers
/// `value`, as [`lash_kernel_doc::plan::HeapTemplate::element_write`]
/// declares: the TypeScript member write, whose list branch converts such
/// a key to itself (`ts.number_index`) and writes there. The regions law
/// that a region's element writes agree with the reference's pins each.
const ELEMENT_WRITES: &[&str] = &["ts.set"];

/// A generated template descriptor and its prepared code, if a typed path
/// of this body can run using certified native entries, or the arena.
pub(crate) fn emit(
    library: &Library,
    definition: &FunctionDefinition,
    ident: &str,
) -> Option<(String, String)> {
    if definition.guard.is_some() {
        return None;
    }
    let mut evaluator = Evaluator {
        library,
        callees: Vec::new(),
        heap_callees: Vec::new(),
        used: 0,
        heap: false,
        captured: captured(&definition.body()?.block),
    };
    let scalar = evaluator.pass(definition, &IMMEDIATE)?;
    // A helper whose native an embedder installs answers through it where
    // its scalar paths decline; the machine takes the heap paths only of
    // one whose body runs. They cover what the scalar paths decline, the
    // same operand kinds included.
    evaluator.heap = true;
    let kinds: Vec<_> = IMMEDIATE.iter().chain(&HEAP).copied().collect();
    let heap = evaluator
        .pass(definition, &kinds)
        .filter(|pass| !pass.arms.is_empty());
    if scalar.arms.is_empty() && heap.is_none() {
        return None;
    }
    let forward = |native: Option<u8>| native.map_or("None".to_string(), |k| format!("Some({k})"));
    let forwards = format!(
        "p::Forwards {{ floats: {}, ints: {} }}",
        forward(scalar.forwards.floats),
        forward(scalar.forwards.ints)
    );
    let mut descriptor = String::from("Some(p::HelperTemplate { entry: ");
    let mut code = String::new();
    if scalar.arms.is_empty() {
        descriptor.push_str("None");
    } else {
        let _ = write!(descriptor, "Some({ident}_template)");
        let _ = write!(
            code,
            "fn {ident}_template(args: &[Value], natives: &[lash_kernel_doc::PreparedNative]) -> Option<Value> {{\n    let _ = natives;\n    match args {{\n{}\n        _ => None,\n    }}\n}}\n",
            arms(scalar.arms)
        );
    }
    let _ = write!(descriptor, ", depth: {}, callees: &[", scalar.depth);
    for callee in &evaluator.callees {
        let _ = write!(descriptor, "p::function({:?}), ", callee.to_string());
    }
    descriptor.push_str("], heap: ");
    match heap {
        None => descriptor.push_str("None"),
        Some(heap) => {
            let element_write = heap.writes && ELEMENT_WRITES.contains(&definition.name.as_str());
            let _ = write!(
                descriptor,
                "Some(p::HeapTemplate {{ entry: {ident}_heap, depth: {}, writes: {}, element_write: {element_write}, callees: &[",
                heap.depth, heap.writes
            );
            for callee in &evaluator.heap_callees {
                let _ = write!(descriptor, "p::function({:?}), ", callee.to_string());
            }
            descriptor.push_str("] })");
            let _ = write!(
                code,
                "fn {ident}_heap(heap: &mut dyn p::TemplateHeap, args: &[Value], natives: &[lash_kernel_doc::PreparedNative]) -> Option<Value> {{\n    let _ = (&heap, natives);\n    match args {{\n{}\n        _ => None,\n    }}\n}}\n",
                arms(heap.arms)
            );
        }
    }
    let _ = write!(descriptor, ", forwards: {forwards}");
    descriptor.push_str(" })");
    Some((descriptor, code))
}
