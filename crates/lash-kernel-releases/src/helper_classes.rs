//! FIG-6409: a conservative, field-insensitive closure-origin and call graph.
//!
//! This is a release check, not a runtime property or native implementation.
//! Argument origins include everything reachable through the argument's heap.
//! Locally constructed closures retain their identity through aliases, members,
//! calls and returns. Constraints are solved to a fixed point, including recursive
//! closures and helper cycles. Branches are joined; no input-dependent path is
//! assumed dead. Scalar native results cannot carry closures.
//!
//! `total` means no guest apply, action or machine-only read; it is a candidate
//! for a total native, not a proof of purity, charge or implementation parity.
//! `operation` permits only guest applies reached through arguments. Other actions,
//! ambient reads and unknown function origins require `body`. Local closure bodies
//! are checked too, including closures that escape to callers.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use lash_kernel_doc::{
    ActKind, Action, Atom, Block, Callee, Expr, FunctionDefinition, FunctionTarget, Implementation,
    Literal, Member, Node, Place, Rhs, Stmt,
};

use {lash_kernel_value::FunctionId, lash_kernel_value::Name, lash_kernel_value::Type};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    #[default]
    Total,
    Operation,
    Body,
}

impl Class {
    fn label(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Operation => "operation",
            Self::Body => "body",
        }
    }
}

/// Dense unit indices make closure unions word-wise operations. The recursive
/// member graph can carry thousands of origins through tens of thousands of
/// slots; reinserting every origin into a tree on every edge is quadratic work.
#[derive(Clone, Default, PartialEq, Eq)]
struct Closures(Vec<u64>);

impl Closures {
    fn insert(&mut self, unit: usize) {
        self.0.resize(self.0.len().max(unit / 64 + 1), 0);
        self.0[unit / 64] |= 1 << (unit % 64);
    }

    fn merge(&mut self, other: &Self) -> bool {
        self.0.resize(self.0.len().max(other.0.len()), 0);
        let mut changed = false;
        for (target, source) in self.0.iter_mut().zip(&other.0) {
            let merged = *target | *source;
            changed |= merged != *target;
            *target = merged;
        }
        changed
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(word, &bits)| {
            let mut remaining = bits;
            std::iter::from_fn(move || {
                if remaining == 0 {
                    return None;
                }
                let bit = remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                Some(word * 64 + bit)
            })
        })
    }
}

/// A value and everything reachable through it. Empty is known non-callable.
#[derive(Clone, Default, PartialEq, Eq)]
struct Origins {
    argument: bool,
    unknown: bool,
    closures: Closures,
}

impl Origins {
    fn merge(&mut self, other: &Self) -> bool {
        let changed = (other.argument && !self.argument) || (other.unknown && !self.unknown);
        self.argument |= other.argument;
        self.unknown |= other.unknown;
        self.closures.merge(&other.closures) || changed
    }
}

type Slot = usize;
type Scope = BTreeMap<Name, Slot>;

#[derive(Default)]
struct Unit {
    params: Vec<Slot>,
    result: Slot,
    class: Class,
    native_result: Option<bool>,
}

#[derive(Clone)]
struct Call {
    owner: usize,
    callee: Slot,
    args: Vec<Slot>,
    result: Slot,
}

struct Graph<'a> {
    values: Vec<Origins>,
    units: Vec<Unit>,
    functions: BTreeMap<FunctionId, usize>,
    definitions: &'a BTreeMap<FunctionId, FunctionDefinition>,
    flows: Vec<(Slot, Slot)>,
    calls: Vec<Call>,
    /// Lexically contained bodies must also satisfy the helper's classification.
    contains: Vec<(usize, usize)>,
    /// Escaping closures may subsequently receive arbitrary caller arguments.
    escapes: Vec<Slot>,
}

impl<'a> Graph<'a> {
    fn slot(&mut self) -> Slot {
        let slot = self.values.len();
        self.values.push(Origins::default());
        slot
    }

    fn unit(&mut self, params: impl IntoIterator<Item = Name>, scope: &mut Scope) -> usize {
        let result = self.slot();
        let params = params
            .into_iter()
            .map(|name| {
                let slot = self.slot();
                scope.insert(name, slot);
                slot
            })
            .collect();
        let unit = self.units.len();
        self.units.push(Unit {
            params,
            result,
            class: Class::Total,
            native_result: None,
        });
        unit
    }

    fn flow(&mut self, from: Slot, to: Slot) {
        self.flows.push((from, to));
    }

    /// A field-insensitive alias: writes through either view reach both views.
    /// This can over-classify a mixed aggregate, but cannot hide a guest closure.
    fn alias(&mut self, left: Slot, right: Slot) {
        self.flow(left, right);
        self.flow(right, left);
    }

    fn variable(scope: &Scope, name: &Name) -> Result<Slot, String> {
        scope
            .get(name)
            .copied()
            .ok_or_else(|| format!("release body has no binding for {name:?}"))
    }

    fn build(
        definitions: &'a BTreeMap<FunctionId, FunctionDefinition>,
        anonymous_bodies: &BTreeMap<FunctionId, BTreeSet<FunctionId>>,
        root: FunctionId,
    ) -> Result<Self, String> {
        // Analyze each export independently. An argument to an unrelated helper
        // must never contaminate this helper's local closure origins.
        let mut reachable = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if !reachable.insert(id) {
                continue;
            }
            match &definitions[&id].implementation {
                Implementation::Guest { body, .. } | Implementation::Both { body, .. } => {
                    pending.extend(body.functions.keys().copied());
                }
                Implementation::Native { .. } => {}
            }
        }
        let anonymous: BTreeSet<_> = reachable
            .iter()
            .flat_map(|id| anonymous_bodies[id].iter().copied())
            .collect();
        // Definitions are immutable census input, shared by every root. Cloning
        // the entire release here made each tiny export pay for all helper ASTs.
        let mut graph = Self {
            definitions,
            values: Vec::new(),
            units: Vec::new(),
            functions: BTreeMap::new(),
            flows: Vec::new(),
            calls: Vec::new(),
            contains: Vec::new(),
            escapes: Vec::new(),
        };
        let mut scopes = BTreeMap::new();
        for id in &reachable {
            let definition = &definitions[id];
            let mut scope = Scope::new();
            let unit = graph.unit(
                definition.signature.params.iter().map(|p| p.name.clone()),
                &mut scope,
            );
            if *id == root {
                for param in &graph.units[unit].params {
                    graph.values[*param].argument = true;
                }
            }
            graph.functions.insert(*id, unit);
            scopes.insert(*id, scope);
        }
        for id in &reachable {
            let definition = &definitions[id];
            let unit = graph.functions[id];
            match &definition.implementation {
                Implementation::Guest { body, .. } | Implementation::Both { body, .. } => {
                    if !anonymous.contains(id) {
                        graph.block(&body.block, &scopes[id], unit)?;
                    }
                }
                Implementation::Native { .. } => {
                    graph.units[unit].native_result =
                        Some(can_hold_closure(&definition.signature.result));
                }
            }
            // K-LIB-010: these are native only for `K-LIB-002`. Their
            // machine access cannot move into a dialect native fast path.
            if matches!(definition.name.as_str(), "deref" | "tasks.unfinished") {
                graph.units[unit].class = Class::Body;
            }
        }
        Ok(graph)
    }

    fn literal(&mut self, value: &Literal) -> Slot {
        let slot = self.slot();
        if let Literal::Function(id) = value {
            match self.functions.get(id) {
                Some(unit) => {
                    self.values[slot].closures.insert(*unit);
                }
                None => self.values[slot].unknown = true,
            }
        }
        slot
    }

    fn atom(&mut self, atom: &Atom, scope: &Scope) -> Result<Slot, String> {
        match atom {
            Atom::Variable(name) => Self::variable(scope, name),
            Atom::Literal(value) => Ok(self.literal(value)),
        }
    }

    fn member(&mut self, member: &Member, scope: &Scope, owner: usize) -> Result<Slot, String> {
        let target = match member {
            Member::Field { target, .. } => target,
            Member::Index { target, index } => {
                self.expr(index, scope, owner)?;
                target
            }
        };
        self.expr(target, scope, owner)
    }

    fn expr(&mut self, expr: &Expr, scope: &Scope, owner: usize) -> Result<Slot, String> {
        Ok(match expr {
            Expr::If { cond, then, r#else } => {
                self.expr(cond, scope, owner)?;
                let result = self.slot();
                let then = self.expr(then, scope, owner)?;
                let otherwise = self.expr(r#else, scope, owner)?;
                self.alias(then, result);
                self.alias(otherwise, result);
                result
            }
            Expr::Let { name, value, body } => {
                let value = self.expr(value, scope, owner)?;
                let binding = self.slot();
                self.alias(value, binding);
                let mut inner = scope.clone();
                inner.insert(name.clone(), binding);
                self.expr(body, &inner, owner)?
            }
            Expr::Variable(name) => Self::variable(scope, name)?,
            Expr::Literal(value) => self.literal(value),
            Expr::Member(member) => self.member(member, scope, owner)?,
            Expr::Closure(closure) => {
                let definition = self
                    .definitions
                    .get(&closure.function)
                    .ok_or_else(|| format!("release lacks closure {}", closure.function))?;
                let body = definition
                    .body()
                    .ok_or_else(|| "closure has no body".to_owned())?;
                let mut captured = scope.clone();
                let unit = self.unit(
                    definition.signature.params.iter().map(|p| p.name.clone()),
                    &mut captured,
                );
                self.contains.push((owner, unit));
                self.block(&body.block, &captured, unit)?;
                let slot = self.slot();
                self.values[slot].closures.insert(unit);
                slot
            }
            Expr::Call { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.expr(arg, scope, owner))
                    .collect::<Result<Vec<_>, _>>()?;
                self.library_call(*function, args, owner)?
            }
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                let result = self.slot();
                for item in items {
                    let value = self.expr(item, scope, owner)?;
                    self.alias(value, result);
                }
                result
            }
            Expr::Map(items) => {
                let result = self.slot();
                for item in items {
                    let key = self.expr(&item.key, scope, owner)?;
                    let value = self.expr(&item.value, scope, owner)?;
                    self.alias(key, result);
                    self.alias(value, result);
                }
                result
            }
            Expr::Record(items) => {
                let result = self.slot();
                for item in items {
                    let value = self.expr(&item.value, scope, owner)?;
                    self.alias(value, result);
                }
                result
            }
            Expr::Act(act) => {
                let args = act
                    .args
                    .iter()
                    .map(|arg| self.expr(arg, scope, owner))
                    .collect::<Result<Vec<_>, _>>()?;
                match &act.kind {
                    ActKind::Call(Callee::Local(id)) => self.library_call(*id, args, owner)?,
                    ActKind::Await(target) => self.target_call(target.clone(), args, owner)?,
                    ActKind::Call(Callee::Value(name)) => {
                        self.call(Self::variable(scope, name)?, args, owner)
                    }
                    ActKind::Call(Callee::Import(_))
                    | ActKind::Perform { .. }
                    | ActKind::Yield
                    | ActKind::Cancel
                    | ActKind::CancelEffect
                    | ActKind::Join
                    | ActKind::JoinMany(_)
                    | ActKind::Spawn(_) => {
                        self.units[owner].class = Class::Body;
                        let result = self.slot();
                        self.values[result].unknown = true;
                        result
                    }
                }
            }
        })
    }

    fn target_call(
        &mut self,
        target: FunctionTarget,
        args: Vec<Slot>,
        owner: usize,
    ) -> Result<Slot, String> {
        match target {
            FunctionTarget::Local(id) => self.library_call(id, args, owner),
            FunctionTarget::Import(_) => {
                self.units[owner].class = Class::Body;
                let result = self.slot();
                self.values[result].unknown = true;
                Ok(result)
            }
        }
    }
    fn library_call(
        &mut self,
        id: FunctionId,
        args: Vec<Slot>,
        owner: usize,
    ) -> Result<Slot, String> {
        let unit = self
            .functions
            .get(&id)
            .copied()
            .ok_or_else(|| format!("release lacks library callee {id}"))?;
        let callee = self.slot();
        self.values[callee].closures.insert(unit);
        Ok(self.call(callee, args, owner))
    }

    fn call(&mut self, callee: Slot, args: Vec<Slot>, owner: usize) -> Slot {
        let result = self.slot();
        self.calls.push(Call {
            owner,
            callee,
            args,
            result,
        });
        result
    }

    fn action(&mut self, action: &Action, scope: &Scope, owner: usize) -> Result<Slot, String> {
        Ok(match action {
            Action::Call { callee, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.atom(arg, scope))
                    .collect::<Result<Vec<_>, _>>()?;
                match callee {
                    Callee::Local(id) => self.library_call(*id, args, owner)?,
                    Callee::Value(name) => self.call(Self::variable(scope, name)?, args, owner),
                    Callee::Import(_) => {
                        self.units[owner].class = Class::Body;
                        let result = self.slot();
                        self.values[result].unknown = true;
                        result
                    }
                }
            }
            Action::Await { policy, value } => {
                let args = vec![self.atom(value, scope)?];
                self.target_call(policy.clone(), args, owner)?
            }
            Action::Perform { .. }
            | Action::Join { .. }
            | Action::JoinMany { .. }
            | Action::Yield
            | Action::Spawn { .. }
            | Action::Cancel { .. }
            | Action::CancelEffect { .. } => {
                self.units[owner].class = Class::Body;
                let result = self.slot();
                self.values[result].unknown = true;
                result
            }
        })
    }

    fn rhs(&mut self, rhs: &Rhs, scope: &Scope, owner: usize) -> Result<Slot, String> {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, scope, owner),
            Rhs::Action(action) => self.action(action, scope, owner),
        }
    }

    fn block(&mut self, block: &Block, outer: &Scope, owner: usize) -> Result<(), String> {
        let mut scope = outer.clone();
        for stmt in block {
            match stmt {
                Stmt::Let { name, value } => {
                    // K-FORM-003: the initializer sees the outer binding. The
                    // new cell is visible only from the following statement.
                    let value = self.rhs(value, &scope, owner)?;
                    let binding = self.slot();
                    self.alias(value, binding);
                    scope.insert(name.clone(), binding);
                }
                Stmt::Assign { place, value, .. } => {
                    let value = self.rhs(value, &scope, owner)?;
                    let target = match place {
                        Place::Variable(name) => Self::variable(&scope, name)?,
                        Place::Member(member) => self.member(member, &scope, owner)?,
                    };
                    self.alias(value, target);
                }
                Stmt::Do { value } => {
                    self.expr(value, &scope, owner)?;
                }
                Stmt::Remove { member } => {
                    self.member(member, &scope, owner)?;
                }
                Stmt::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    self.expr(condition, &scope, owner)?;
                    self.block(then_block, &scope, owner)?;
                    self.block(else_block, &scope, owner)?;
                }
                Stmt::While {
                    condition,
                    body,
                    else_block,
                    ..
                } => {
                    self.expr(condition, &scope, owner)?;
                    self.block(body, &scope, owner)?;
                    self.block(else_block, &scope, owner)?;
                }
                Stmt::For {
                    binding,
                    iterable,
                    via,
                    body,
                    else_block,
                    ..
                } => {
                    let iterable = self.expr(iterable, &scope, owner)?;
                    let item = self.slot();
                    match via {
                        None => self.alias(iterable, item),
                        // The hooks are library calls: on the iterated
                        // value, what open gives, and its items.
                        Some(via) => {
                            let source =
                                self.target_call(via.open.clone(), vec![iterable], owner)?;
                            for hook in [via.next.clone(), via.check.clone()].into_iter().flatten()
                            {
                                let stepped = self.target_call(hook, vec![source], owner)?;
                                self.alias(stepped, item);
                            }
                            if let Some(hook) = via.item.clone() {
                                let element = self.target_call(hook, vec![source, item], owner)?;
                                self.alias(element, item);
                            }
                            self.alias(source, item);
                        }
                    }
                    let mut inner = scope.clone();
                    inner.insert(binding.clone(), item);
                    self.block(body, &inner, owner)?;
                    self.block(else_block, &scope, owner)?;
                }
                Stmt::Try(stmt) => {
                    self.block(&stmt.body, &scope, owner)?;
                    if let Some(catch) = &stmt.catch {
                        let mut inner = scope.clone();
                        let error = self.slot();
                        // A caught value may originate in a guest call or a
                        // kernel error, so an apply through it is not proved safe.
                        self.values[error].unknown = true;
                        inner.insert(catch.binding.clone(), error);
                        self.block(&catch.body, &inner, owner)?;
                    }
                    if let Some(finally) = &stmt.finally {
                        self.block(finally, &scope, owner)?;
                    }
                }
                Stmt::ReturnVia { value, policy } => {
                    let held = self.expr(value, &scope, owner)?;
                    let value = self.target_call(policy.clone(), vec![held], owner)?;
                    self.flow(value, self.units[owner].result);
                    self.escapes.push(value);
                }
                Stmt::Return { value } => {
                    let value = self.expr(value, &scope, owner)?;
                    self.flow(value, self.units[owner].result);
                    self.escapes.push(value);
                }
                Stmt::Throw { value } => {
                    self.expr(value, &scope, owner)?;
                }
                Stmt::Print { value } | Stmt::Finish { value } | Stmt::Fail { value } => {
                    self.units[owner].class = Class::Body;
                    self.expr(value, &scope, owner)?;
                }
                Stmt::Break | Stmt::Continue => {}
            }
        }
        Ok(())
    }

    fn solve(&mut self) {
        Solver::new(self).solve();
    }
}

#[derive(Clone, Copy)]
enum Constraint {
    Flow(Slot, Slot),
    Escape(Slot),
    Call(usize),
    Contains(usize, usize),
}

/// Monotone constraints only need revisiting when one of their inputs grows.
/// A whole-graph sweep repeatedly cloned every origin set, including the large
/// mutually recursive TypeScript member graphs, until their last fact arrived.
struct Solver<'g, 'a> {
    graph: &'g mut Graph<'a>,
    constraints: Vec<Constraint>,
    // Value slots first, then unit classes. Dynamic call targets add subscribers
    // when discovered, preserving the same least fixed point as a full sweep.
    subscribers: Vec<BTreeSet<usize>>,
    pending: VecDeque<usize>,
    queued: Vec<bool>,
}

impl<'g, 'a> Solver<'g, 'a> {
    fn new(graph: &'g mut Graph<'a>) -> Self {
        let constraints: Vec<_> = graph
            .flows
            .iter()
            .map(|&(from, to)| Constraint::Flow(from, to))
            .chain(graph.escapes.iter().copied().map(Constraint::Escape))
            .chain((0..graph.calls.len()).map(Constraint::Call))
            .chain(
                graph
                    .contains
                    .iter()
                    .map(|&(owner, nested)| Constraint::Contains(owner, nested)),
            )
            .collect();
        let mut solver = Self {
            subscribers: vec![BTreeSet::new(); graph.values.len() + graph.units.len()],
            pending: (0..constraints.len()).collect(),
            queued: vec![true; constraints.len()],
            graph,
            constraints,
        };
        for index in 0..solver.constraints.len() {
            match solver.constraints[index] {
                Constraint::Flow(from, _) | Constraint::Escape(from) => solver.watch(from, index),
                Constraint::Call(call) => {
                    let call = solver.graph.calls[call].clone();
                    solver.watch(call.callee, index);
                    for arg in call.args {
                        solver.watch(arg, index);
                    }
                }
                Constraint::Contains(_, nested) => solver.watch_class(nested, index),
            }
        }
        solver
    }

    fn watch(&mut self, input: usize, constraint: usize) {
        self.subscribers[input].insert(constraint);
    }

    fn watch_class(&mut self, unit: usize, constraint: usize) {
        self.watch(self.graph.values.len() + unit, constraint);
    }

    fn changed(&mut self, input: usize) {
        for &constraint in &self.subscribers[input] {
            if !self.queued[constraint] {
                self.queued[constraint] = true;
                self.pending.push_back(constraint);
            }
        }
    }

    fn class(&mut self, unit: usize, class: Class) {
        if class > self.graph.units[unit].class {
            self.graph.units[unit].class = class;
            self.changed(self.graph.values.len() + unit);
        }
    }

    fn argument(&mut self, slot: Slot) {
        if !self.graph.values[slot].argument {
            self.graph.values[slot].argument = true;
            self.changed(slot);
        }
    }

    fn unknown(&mut self, slot: Slot) {
        if !self.graph.values[slot].unknown {
            self.graph.values[slot].unknown = true;
            self.changed(slot);
        }
    }

    fn flow(&mut self, from: Slot, to: Slot) {
        if from == to {
            return;
        }
        // Borrow the source set; copying it on every edge turns a large alias
        // component into repeated allocation and insertion of the same origins.
        let changed = if from < to {
            let (left, right) = self.graph.values.split_at_mut(to);
            right[0].merge(&left[from])
        } else {
            let (left, right) = self.graph.values.split_at_mut(from);
            left[to].merge(&right[0])
        };
        if changed {
            self.changed(to);
        }
    }

    fn solve(mut self) {
        while let Some(index) = self.pending.pop_front() {
            self.queued[index] = false;
            match self.constraints[index] {
                Constraint::Flow(from, to) => self.flow(from, to),
                Constraint::Escape(value) => {
                    for unit in self.graph.values[value].closures.clone().iter() {
                        for param in self.graph.units[unit].params.clone() {
                            self.argument(param);
                        }
                    }
                }
                Constraint::Contains(owner, nested) => {
                    self.class(owner, self.graph.units[nested].class);
                }
                Constraint::Call(call) => self.call(index, call),
            }
        }
    }

    fn call(&mut self, index: usize, call: usize) {
        let call = self.graph.calls[call].clone();
        let origins = self.graph.values[call.callee].clone();
        if origins.argument {
            self.class(call.owner, Class::Operation);
            self.argument(call.result);
        }
        if origins.unknown {
            self.class(call.owner, Class::Body);
            self.unknown(call.result);
        }
        if origins.argument || origins.unknown {
            // A guest can call back into any local closure passed to it.
            for &arg in &call.args {
                for unit in self.graph.values[arg].closures.clone().iter() {
                    self.watch_class(unit, index);
                    self.class(call.owner, self.graph.units[unit].class);
                    for param in self.graph.units[unit].params.clone() {
                        self.argument(param);
                    }
                }
            }
        }
        for unit in origins.closures.iter() {
            self.watch_class(unit, index);
            self.class(call.owner, self.graph.units[unit].class);
            if let Some(carries_closures) = self.graph.units[unit].native_result {
                // Native transfer stays call-local; unrelated callers cannot
                // contaminate each other's arguments through a native unit.
                if carries_closures {
                    for &arg in &call.args {
                        self.flow(arg, call.result);
                    }
                }
                continue;
            }
            for (&arg, param) in call.args.iter().zip(self.graph.units[unit].params.clone()) {
                self.watch(param, index);
                self.flow(arg, param);
                self.flow(param, arg);
            }
            let result = self.graph.units[unit].result;
            self.watch(result, index);
            self.flow(result, call.result);
        }
    }
}

fn can_hold_closure(ty: &Type) -> bool {
    match ty {
        Type::Any | Type::Function(_) | Type::Error => true,
        Type::Tuple(items) | Type::Union(items) => items.iter().any(can_hold_closure),
        Type::List(item) | Type::Set(item) | Type::Task(item) => can_hold_closure(item),
        Type::Map(map) => can_hold_closure(&map.key) || can_hold_closure(&map.value),
        Type::Record(record) => {
            record
                .fields
                .iter()
                .any(|field| can_hold_closure(&field.ty))
                || record.rest.kept_type().is_some_and(can_hold_closure)
        }
        Type::Json
        | Type::Null
        | Type::Absent
        | Type::Bool
        | Type::Int
        | Type::Float
        | Type::Number
        | Type::Text
        | Type::Bytes
        | Type::Timestamp
        | Type::Enum(_)
        | Type::Handle(_) => false,
    }
}

fn census(
    definitions: &BTreeMap<FunctionId, FunctionDefinition>,
    exports: &BTreeSet<FunctionId>,
) -> Result<String, String> {
    // Identify anonymous bodies once. Their lexical construction sites are
    // immutable; origins and classifications still get a fresh graph per root.
    let mut anonymous_bodies = BTreeMap::new();
    for (id, definition) in definitions {
        let mut anonymous = BTreeSet::new();
        if let Some(body) = definition.body() {
            let mut pending = vec![Node::Block(&body.block)];
            while let Some(node) = pending.pop() {
                if let Node::Expr(Expr::Closure(closure)) = node {
                    anonymous.insert(closure.function);
                }
                pending.extend(node.children());
            }
        }
        anonymous_bodies.insert(*id, anonymous);
    }
    let rows: BTreeMap<_, _> = exports
        .iter()
        .map(|id| {
            let mut graph = Graph::build(definitions, &anonymous_bodies, *id)?;
            graph.solve();

            Ok((
                &definitions[id].name,
                graph.units[graph.functions[id]].class,
            ))
        })
        .collect::<Result<_, String>>()?;
    Ok(rows
        .into_iter()
        .map(|(name, class)| format!("{name}\t{}\n", class.label()))
        .collect())
}

#[cfg(test)]
fn diff(expected: &str, actual: &str) -> String {
    let pinned: BTreeSet<_> = expected.lines().collect();
    let computed: BTreeSet<_> = actual.lines().collect();
    let changes: String = pinned
        .difference(&computed)
        .map(|line| format!("- {}\n", line.replace('\t', " ")))
        .chain(
            computed
                .difference(&pinned)
                .map(|line| format!("+ {}\n", line.replace('\t', " "))),
        )
        .collect();
    if changes.is_empty() && expected != actual {
        // Set differences alone hide reordered or duplicated rows and a missing
        // final newline; print both spellings when only the spelling differs.
        return format!("- {expected:?}\n+ {actual:?}\n");
    }
    changes
}

/// FIG-6409: every exported helper has a reviewed class; changes in guest
/// reachability or machine actions must change the census visibly.
#[test]
#[cfg(all(feature = "dialect-python", feature = "dialect-typescript"))]
fn helper_release_census_matches_checked_call_graph() {
    let actual = current_census().expect("the release census analyzes");
    let expected = include_str!("helper-classes.tsv");

    assert!(
        expected == actual,
        "helper census differs (- pinned, + recomputed):\n{}",
        diff(expected, &actual)
    );
}

/// FIG-6409 closure-origin rule and K-FORM-003: a local closure stays local
/// through aliases, and shadowing preserves the initializer's outer origin.
/// Adding an apply of an argument closure changes total to operation and must
/// fail the very same comparison used for the reviewed release census.
#[test]
fn argument_apply_changes_total_census() {
    let (definition, locals) = lash_kernel_doc::parse_definition_with_locals(
        "function helper(value: Any) -> Any\nkernel 1\ncharge 1\nbody {\n\
         let value = value\n\
         let local = fn() { return value }\n\
         let alias = local\n\
         let outcome = apply alias()\n\
         return outcome\n}\n",
    )
    .expect("a helper with a locally created closure");
    let id = definition.identity();
    let pinned = "helper\ttotal\n";
    assert_eq!(
        census(
            &locals
                .iter()
                .cloned()
                .map(|d| (d.identity(), d))
                .chain([(id, definition.clone())])
                .collect(),
            &BTreeSet::from([id])
        )
        .expect("the fixture analyzes"),
        pinned
    );
    let mut seeded = definition;
    let Implementation::Guest { body, .. } = &mut seeded.implementation else {
        panic!("a kernel body")
    };
    body.block.insert(
        1,
        Stmt::Do {
            value: (Action::Call {
                callee: Callee::Value(Name::new("value")),
                args: Vec::new(),
            })
            .into(),
        },
    );
    let id = seeded.identity();
    let changed = census(
        &locals
            .iter()
            .cloned()
            .map(|d| (d.identity(), d))
            .chain([(id, seeded)])
            .collect(),
        &BTreeSet::from([id]),
    )
    .expect("the seeded fixture analyzes");
    assert_eq!(changed, "helper\toperation\n");
    assert_eq!(
        diff(pinned, &changed),
        "- helper total\n+ helper operation\n"
    );

    // K-EVAL-001/002: the initializer sees the outer origin, and even an
    // unselected branch contributes its closure to the conservative census.
    let (conditional, locals) = lash_kernel_doc::parse_definition_with_locals(
        r#"function helper(value: Any) -> Any
kernel 1
charge 1
body {
  return let(value = value, if(true, fn() { return null }, fn() {
    let observed = apply value()
    return observed
  }))
}
"#,
    )
    .expect("conditional closures and an expression-local alias");
    let id = conditional.identity();
    assert_eq!(
        census(
            &locals
                .into_iter()
                .map(|d| (d.identity(), d))
                .chain([(id, conditional)])
                .collect(),
            &BTreeSet::from([id])
        )
        .expect("conditional closures analyze"),
        "helper\toperation\n"
    );
}

#[cfg(all(feature = "dialect-python", feature = "dialect-typescript"))]
pub(super) fn current_census() -> Result<String, String> {
    let read = |built: &crate::BuiltPackage| {
        lash_kernel_doc::ReleasedFunctions::read(built.image).map_err(|error| error.to_string())
    };
    let mut definitions = BTreeMap::new();
    for built in crate::PACKAGES {
        let code = read(built)?;
        for at in 0..code.len() {
            let definition = code.definition(at).map_err(|error| error.to_string())?;
            definitions.insert(definition.identity(), definition);
        }
    }
    let roots = crate::helper_release()
        .map_err(|error| error.to_string())?
        .resolver_roots();
    let mut actual = String::new();
    for dialect in [crate::PYTHON, crate::TYPESCRIPT] {
        let code = read(
            crate::package(dialect).ok_or_else(|| format!("release lacks {dialect} package"))?,
        )?;
        // Released images include anonymous bodies; analyze those in their
        // defining scope, reached from the package's authored exports.
        let exports = code.ids().filter(|id| roots.contains(id)).collect();
        actual.push_str(&census(&definitions, &exports)?);
    }
    Ok(actual)
}
