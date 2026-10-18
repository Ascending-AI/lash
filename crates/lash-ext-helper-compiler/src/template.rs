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

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{
    ActKind, Action, Atom, Callee, Expr, FunctionDefinition, FunctionId, Literal, Member, Name,
    Node, Place, Rhs, Stmt, TryStmt, Value, ValueKind,
};

use crate::library::Library;

/// The most calls and arena forms one heap path performs.
const HEAP_CALLS: usize = 8;

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
                "Value::Text(lash_kernel_doc::arena::ShortText::new({:?})?)",
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

impl State {
    /// `step` as the path's next, exposing what the path built when the
    /// step may decline.
    fn step(&mut self, step: String) {
        self.exposed |= self.builds && (step.contains('?') || step.contains("return None"));
        self.steps.push(step);
    }
}

#[derive(Clone, Default)]
struct State {
    vars: BTreeMap<Name, Symbol>,
    steps: Vec<String>,
    depth: u32,
    /// The path reads, builds or writes an arena object, or calls for what
    /// may be one: what only a heap path answers.
    heap: bool,
    /// The path builds an object, or makes a call that may.
    builds: bool,
    /// A fallible step follows the path's first build: declining there
    /// leaves what it built.
    exposed: bool,
    /// The path wrote: only its return may follow.
    written: bool,
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
    /// named: one that may build, and one that `arena`, reaching what only
    /// the arena holds.
    fn heap_step(
        &mut self,
        state: &mut State,
        form: String,
        (kind, calls): (Option<ValueKind>, usize),
        builds: bool,
        arena: bool,
    ) -> Symbol {
        let code = self.fresh();
        state.step(format!("let {code} = {form}?; let _ = {code};"));
        state.heap |= arena;
        state.builds |= builds;
        Symbol::computed(code, kind, calls + 1)
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
                let form = format!("heap.tuple(&[{}])", codes(&items));
                let kind = Some(ValueKind::Tuple);
                Some(self.heap_step(state, form, (kind, calls), true, true))
            }
            Expr::Member(member) if self.heap => match &**member {
                Member::Field { target, field } => {
                    let target = self.expr(target, state, depth)?;
                    let form = format!("heap.field({}, {field:?})", target.code);
                    Some(self.heap_step(state, form, (None, target.calls), false, true))
                }
                Member::Index { target, index } => {
                    let target = self.expr(target, state, depth)?;
                    let index = self.expr(index, state, depth)?;
                    let form = format!("heap.index({}, {})", target.code, index.code);
                    let calls = target.calls + index.calls;
                    Some(self.heap_step(state, form, (None, calls), false, true))
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
    fn result_kind(definition: &FunctionDefinition) -> Option<ValueKind> {
        match definition.signature.result {
            lash_kernel_doc::Type::Bool => Some(ValueKind::Bool),
            lash_kernel_doc::Type::Float => Some(ValueKind::Float),
            lash_kernel_doc::Type::Int => Some(ValueKind::Int),
            lash_kernel_doc::Type::Null => Some(ValueKind::Null),
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
        let kind = Self::result_kind(definition);
        let counts = kind == Some(ValueKind::Int) && args.iter().all(|arg| !immediate(arg.kind));
        let number = matches!(
            definition.signature.result,
            lash_kernel_doc::Type::Int
                | lash_kernel_doc::Type::Float
                | lash_kernel_doc::Type::Number
        ) && args.iter().all(|arg| immediate(arg.kind));
        let builds = !matches!(kind, Some(ValueKind::Bool | ValueKind::Null)) && !counts && !number;
        let helper = definition.name.as_str().starts_with("py.")
            || definition.name.as_str().starts_with("ts.");
        let arena = !helper
            || !matches!(
                definition.signature.result,
                lash_kernel_doc::Type::Bool
                    | lash_kernel_doc::Type::Int
                    | lash_kernel_doc::Type::Float
                    | lash_kernel_doc::Type::Number
                    | lash_kernel_doc::Type::Null
            );
        let index = self.heap_callee(function);
        let calls = args.iter().map(|arg| arg.calls).sum();
        let form = format!("heap.call({index}, &[{}])", codes(args));
        self.heap_step(state, form, (kind, calls), builds, arena)
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
        let kind = Self::result_kind(definition).filter(|kind| *kind != ValueKind::Null);
        // Boolean negation is a result wrapper; the underlying comparison
        // or truth test remains the one value-producing native.
        let calls = usize::from(definition.name.as_str() != "bool.not")
            + args.iter().map(|arg| arg.calls).sum::<usize>();
        let code = self.fresh();
        let result = &definition.signature.result;
        let reserved = matches!(result, lash_kernel_doc::Type::Text);
        if self.heap && (reserved || !args.iter().all(|arg| immediate(arg.kind))) {
            // An operand the entry may decline, or a text whose room only
            // the general call reserves: the general call answers.
            let general = self.heap_callee(function);
            state.heap = true;
            state.step(format!(
                "let {code} = match (natives[{index}].entry)(&[{0}]) {{ Some(value) => value, None => heap.call({general}, &[{0}])? }}; let _ = {code};",
                codes(&args)
            ));
            state.builds |= !matches!(
                result,
                lash_kernel_doc::Type::Bool
                    | lash_kernel_doc::Type::Int
                    | lash_kernel_doc::Type::Float
                    | lash_kernel_doc::Type::Number
                    | lash_kernel_doc::Type::Null
            );
        } else {
            state.step(format!(
                "let {code} = (natives[{index}].entry)(&[{}])?; let _ = {code};",
                codes(&args)
            ));
        }
        Some(Symbol::computed(code, kind, calls))
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
            ..State::default()
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
        let result = self.fresh();
        let mut code = format!("let {result} = (|| {{\n");
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
            code.push_str("if let Some(value) = (|| {\n");
            for step in nested.steps {
                let _ = writeln!(code, "{step}");
            }
            let _ = writeln!(
                code,
                "Some({})\n}})() {{ return Some(value); }}",
                value.code
            );
        }
        let _ = writeln!(code, "None\n}})()?;\nlet _ = {result};");
        state.steps.push(code);
        Some(Symbol::computed(result, kind, calls))
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
            if next.written {
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
        if state.written {
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
                    // assignment replaces; a captured one is a cell.
                    let previous = match stmt {
                        Stmt::Let { .. } => Some("None".to_owned()),
                        _ => state
                            .vars
                            .get(name)
                            .map(|old| format!("Some({})", old.code)),
                    };
                    let Some(previous) = previous.filter(|_| !self.captured.contains(name)) else {
                        return Vec::new();
                    };
                    state.step(format!("heap.bind({}, {previous})?;", value.code));
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
                state.step(format!(
                    "heap.set_index({}, {}, {})?;",
                    target.code, index.code, value.code
                ));
                state.heap = true;
                state.written = true;
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
                        next.step(format!(
                            "if !matches!({}, Value::Bool({guard})) {{ return None; }}",
                            test.code
                        ));
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
fn accepts(ty: &lash_kernel_doc::Type, kind: ValueKind) -> bool {
    use lash_kernel_doc::Type;
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
}

impl Evaluator<'_> {
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
            depth: 1,
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
            let most = if self.heap { HEAP_CALLS } else { 1 };
            let mut results: Vec<_> = self
                .block(&body.block, &mut state, 1)
                .into_iter()
                .filter(|(_, value)| value.as_ref().is_none_or(|value| value.calls <= most))
                .collect();
            if results.is_empty() || results.len() > ALTERNATIVES {
                continue;
            }
            if self.heap {
                // A heap pass keeps what only the arena answers. The `if`s
                // that part alternatives let at most one's tests hold, so
                // the one that may decline after building runs last, where
                // an earlier one declining has built nothing.
                if !objects && results.iter().all(|(state, _)| !state.heap) {
                    continue;
                }
                results.sort_by_key(|(state, _)| state.exposed);
                let last = results.len() - 1;
                if results[..last].iter().any(|(state, _)| state.exposed) {
                    continue;
                }
            }
            let mut code = String::new();
            // What an earlier alternative declared is not a later one's.
            let binds = results
                .iter()
                .any(|(state, _)| state.steps.iter().any(|step| step.contains("heap.bind(")));
            for (state, value) in results {
                let value = value.or_else(|| Symbol::constant(Value::Null))?;
                pass.depth = pass.depth.max(state.depth);
                if binds {
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
            let _ = write!(
                descriptor,
                "Some(p::HeapTemplate {{ entry: {ident}_heap, depth: {}, callees: &[",
                heap.depth
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
    descriptor.push_str(" })");
    Some((descriptor, code))
}
