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

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    ActKind, Action, Atom, Block, Callee, Expr, FunctionDefinition, FunctionId, Implementation,
    Literal, Member, Name, Place, Rhs, Stmt, Type,
};

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

/// A value and everything reachable through it. Empty is known non-callable.
#[derive(Clone, Default, PartialEq, Eq)]
struct Origins {
    argument: bool,
    unknown: bool,
    closures: BTreeSet<usize>,
}

impl Origins {
    fn merge(&mut self, other: &Self) {
        self.argument |= other.argument;
        self.unknown |= other.unknown;
        self.closures.extend(&other.closures);
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

struct Call {
    owner: usize,
    callee: Slot,
    args: Vec<Slot>,
    result: Slot,
}

#[derive(Default)]
struct Graph {
    values: Vec<Origins>,
    units: Vec<Unit>,
    functions: BTreeMap<FunctionId, usize>,
    flows: Vec<(Slot, Slot)>,
    calls: Vec<Call>,
    /// Lexically contained bodies must also satisfy the helper's classification.
    contains: Vec<(usize, usize)>,
    /// Escaping closures may subsequently receive arbitrary caller arguments.
    escapes: Vec<Slot>,
}

impl Graph {
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

    fn variable(scope: &Scope, name: &Name) -> Slot {
        *scope
            .get(name)
            .expect("release bodies have validated lexical scopes")
    }

    fn build(definitions: &BTreeMap<FunctionId, FunctionDefinition>, root: FunctionId) -> Self {
        // Analyze each export independently. An argument to an unrelated helper
        // must never contaminate this helper's local closure origins.
        let mut reachable = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if !reachable.insert(id) {
                continue;
            }
            match &definitions[&id].implementation {
                Implementation::Body(body) | Implementation::Both(body) => {
                    pending.extend(body.functions.keys().copied());
                }
                Implementation::Native => {}
            }
        }
        let mut graph = Self::default();
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
                Implementation::Body(body) | Implementation::Both(body) => {
                    graph.block(&body.block, &scopes[id], unit);
                }
                Implementation::Native => {
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
        graph
    }

    fn literal(&mut self, value: &Literal) -> Slot {
        let slot = self.slot();
        // Declared functions belong to the guest document, never the release.
        if matches!(value, Literal::Function(_)) {
            self.values[slot].unknown = true;
        }
        slot
    }

    fn atom(&mut self, atom: &Atom, scope: &Scope) -> Slot {
        match atom {
            Atom::Variable(name) => Self::variable(scope, name),
            Atom::Literal(value) => self.literal(value),
        }
    }

    fn member(&mut self, member: &Member, scope: &Scope, owner: usize) -> Slot {
        let target = match member {
            Member::Field { target, .. } => target,
            Member::Index { target, index } => {
                self.expr(index, scope, owner);
                target
            }
        };
        self.expr(target, scope, owner)
    }

    fn expr(&mut self, expr: &Expr, scope: &Scope, owner: usize) -> Slot {
        match expr {
            Expr::If { cond, then, r#else } => {
                self.expr(cond, scope, owner);
                let result = self.slot();
                let then = self.expr(then, scope, owner);
                let otherwise = self.expr(r#else, scope, owner);
                self.alias(then, result);
                self.alias(otherwise, result);
                result
            }
            Expr::Let { name, value, body } => {
                let value = self.expr(value, scope, owner);
                let binding = self.slot();
                self.alias(value, binding);
                let mut inner = scope.clone();
                inner.insert(name.clone(), binding);
                self.expr(body, &inner, owner)
            }
            Expr::Variable(name) => Self::variable(scope, name),
            Expr::Literal(value) => self.literal(value),
            Expr::Member(member) => self.member(member, scope, owner),
            Expr::Closure(closure) => {
                let mut captured = scope.clone();
                let unit = self.unit(closure.params.clone(), &mut captured);
                self.contains.push((owner, unit));
                self.block(&closure.body, &captured, unit);
                let slot = self.slot();
                self.values[slot].closures.insert(unit);
                slot
            }
            Expr::Call { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.expr(arg, scope, owner))
                    .collect();
                self.library_call(*function, args, owner)
            }
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                let result = self.slot();
                for item in items {
                    let value = self.expr(item, scope, owner);
                    self.alias(value, result);
                }
                result
            }
            Expr::Map(items) => {
                let result = self.slot();
                for item in items {
                    let key = self.expr(&item.key, scope, owner);
                    let value = self.expr(&item.value, scope, owner);
                    self.alias(key, result);
                    self.alias(value, result);
                }
                result
            }
            Expr::Record(items) => {
                let result = self.slot();
                for item in items {
                    let value = self.expr(&item.value, scope, owner);
                    self.alias(value, result);
                }
                result
            }
            Expr::Clock | Expr::Random => {
                self.units[owner].class = Class::Body;
                self.slot()
            }
            Expr::Read(read) => {
                self.units[owner].class = Class::Body;
                self.expr(&read.handle, scope, owner);
                self.expr(&read.request, scope, owner);
                let result = self.slot();
                self.values[result].unknown = true;
                result
            }
            Expr::Act(act) => {
                let args = act
                    .args
                    .iter()
                    .map(|arg| self.expr(arg, scope, owner))
                    .collect();
                match &act.kind {
                    ActKind::Call(Callee::Library(id)) | ActKind::Await(id) => {
                        self.library_call(*id, args, owner)
                    }
                    ActKind::Call(Callee::Value(name)) => {
                        self.call(Self::variable(scope, name), args, owner)
                    }
                    ActKind::Call(Callee::Declared(_))
                    | ActKind::Perform { .. }
                    | ActKind::Sleep { .. }
                    | ActKind::Yield
                    | ActKind::Cancel
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
        }
    }

    fn library_call(&mut self, id: FunctionId, args: Vec<Slot>, owner: usize) -> Slot {
        let unit = *self
            .functions
            .get(&id)
            .expect("the release holds every library callee");
        let callee = self.slot();
        self.values[callee].closures.insert(unit);
        self.call(callee, args, owner)
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

    fn action(&mut self, action: &Action, scope: &Scope, owner: usize) -> Slot {
        match action {
            Action::Call { callee, args } => {
                let args = args.iter().map(|arg| self.atom(arg, scope)).collect();
                match callee {
                    Callee::Library(id) => self.library_call(*id, args, owner),
                    Callee::Value(name) => self.call(Self::variable(scope, name), args, owner),
                    Callee::Declared(_) => {
                        self.units[owner].class = Class::Body;
                        let result = self.slot();
                        self.values[result].unknown = true;
                        result
                    }
                }
            }
            Action::Await { policy, value } => {
                let args = vec![self.atom(value, scope)];
                self.library_call(*policy, args, owner)
            }
            Action::Perform { .. }
            | Action::Sleep { .. }
            | Action::Join { .. }
            | Action::JoinMany { .. }
            | Action::Yield
            | Action::Spawn { .. }
            | Action::Cancel { .. } => {
                self.units[owner].class = Class::Body;
                let result = self.slot();
                self.values[result].unknown = true;
                result
            }
        }
    }

    fn rhs(&mut self, rhs: &Rhs, scope: &Scope, owner: usize) -> Slot {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, scope, owner),
            Rhs::Action(action) => self.action(action, scope, owner),
        }
    }

    fn block(&mut self, block: &Block, outer: &Scope, owner: usize) {
        let mut scope = outer.clone();
        for stmt in block {
            match stmt {
                Stmt::Let { name, value } => {
                    // K-FORM-003: the initializer sees the outer binding. The
                    // new cell is visible only from the following statement.
                    let value = self.rhs(value, &scope, owner);
                    let binding = self.slot();
                    self.alias(value, binding);
                    scope.insert(name.clone(), binding);
                }
                Stmt::Assign { place, value, .. } => {
                    let value = self.rhs(value, &scope, owner);
                    let target = match place {
                        Place::Variable(name) => Self::variable(&scope, name),
                        Place::Member(member) => self.member(member, &scope, owner),
                    };
                    self.alias(value, target);
                }
                Stmt::Do { value } => {
                    self.expr(value, &scope, owner);
                }
                Stmt::Remove { member } => {
                    self.member(member, &scope, owner);
                }
                Stmt::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    self.expr(condition, &scope, owner);
                    self.block(then_block, &scope, owner);
                    self.block(else_block, &scope, owner);
                }
                Stmt::While {
                    condition,
                    body,
                    else_block,
                    ..
                } => {
                    self.expr(condition, &scope, owner);
                    self.block(body, &scope, owner);
                    self.block(else_block, &scope, owner);
                }
                Stmt::For {
                    binding,
                    iterable,
                    via,
                    body,
                    else_block,
                    ..
                } => {
                    let iterable = self.expr(iterable, &scope, owner);
                    let item = self.slot();
                    match via {
                        None => self.alias(iterable, item),
                        // The hooks are library calls: on the iterated
                        // value, what open gives, and its items.
                        Some(via) => {
                            let source = self.library_call(via.open, vec![iterable], owner);
                            for hook in [via.next, via.check].into_iter().flatten() {
                                let stepped = self.library_call(hook, vec![source], owner);
                                self.alias(stepped, item);
                            }
                            if let Some(hook) = via.item {
                                let element = self.library_call(hook, vec![source, item], owner);
                                self.alias(element, item);
                            }
                            self.alias(source, item);
                        }
                    }
                    let mut inner = scope.clone();
                    inner.insert(binding.clone(), item);
                    self.block(body, &inner, owner);
                    self.block(else_block, &scope, owner);
                }
                Stmt::Try(stmt) => {
                    self.block(&stmt.body, &scope, owner);
                    if let Some(catch) = &stmt.catch {
                        let mut inner = scope.clone();
                        let error = self.slot();
                        // A caught value may originate in a guest call or a
                        // kernel error, so an apply through it is not proved safe.
                        self.values[error].unknown = true;
                        inner.insert(catch.binding.clone(), error);
                        self.block(&catch.body, &inner, owner);
                    }
                    if let Some(finally) = &stmt.finally {
                        self.block(finally, &scope, owner);
                    }
                }
                Stmt::ReturnVia { value, policy } => {
                    let held = self.expr(value, &scope, owner);
                    let value = self.library_call(*policy, vec![held], owner);
                    self.flow(value, self.units[owner].result);
                    self.escapes.push(value);
                }
                Stmt::Return { value } => {
                    let value = self.expr(value, &scope, owner);
                    self.flow(value, self.units[owner].result);
                    self.escapes.push(value);
                }
                Stmt::Throw { value } => {
                    self.expr(value, &scope, owner);
                }
                Stmt::Print { value } | Stmt::Finish { value } | Stmt::Fail { value } => {
                    self.units[owner].class = Class::Body;
                    self.expr(value, &scope, owner);
                }
                Stmt::Break | Stmt::Continue => {}
            }
        }
    }

    fn solve(&mut self) {
        loop {
            let previous_values = self.values.clone();
            let previous_classes: Vec<_> = self.units.iter().map(|unit| unit.class).collect();
            for &(from, to) in &self.flows {
                let origin = self.values[from].clone();
                self.values[to].merge(&origin);
            }
            for &value in &self.escapes {
                for unit in self.values[value].closures.clone() {
                    for &param in &self.units[unit].params {
                        self.values[param].argument = true;
                    }
                }
            }
            for call in &self.calls {
                let origins = self.values[call.callee].clone();
                if origins.argument {
                    self.units[call.owner].class =
                        self.units[call.owner].class.max(Class::Operation);
                    self.values[call.result].argument = true;
                }
                if origins.unknown {
                    self.units[call.owner].class = Class::Body;
                    self.values[call.result].unknown = true;
                }
                if origins.argument || origins.unknown {
                    // A guest can call back into any local closure passed to it.
                    for &arg in &call.args {
                        for unit in self.values[arg].closures.clone() {
                            self.units[call.owner].class =
                                self.units[call.owner].class.max(self.units[unit].class);
                            for &param in &self.units[unit].params {
                                self.values[param].argument = true;
                            }
                        }
                    }
                }
                for unit in origins.closures {
                    self.units[call.owner].class =
                        self.units[call.owner].class.max(self.units[unit].class);
                    if let Some(carries_closures) = self.units[unit].native_result {
                        // Native functions only inspect arguments: instantiate
                        // their origin transfer at this call, not across callers.
                        if carries_closures {
                            for &arg in &call.args {
                                let value = self.values[arg].clone();
                                self.values[call.result].merge(&value);
                            }
                        }
                        continue;
                    }
                    for (&arg, &param) in call.args.iter().zip(&self.units[unit].params) {
                        let value = self.values[arg].clone();
                        self.values[param].merge(&value);
                        let reachable = self.values[param].clone();
                        self.values[arg].merge(&reachable);
                    }
                    let result = self.values[self.units[unit].result].clone();
                    self.values[call.result].merge(&result);
                }
            }
            for &(owner, nested) in &self.contains {
                self.units[owner].class = self.units[owner].class.max(self.units[nested].class);
            }
            if previous_values == self.values
                && previous_classes == self.units.iter().map(|unit| unit.class).collect::<Vec<_>>()
            {
                break;
            }
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
                || record.rest.as_deref().is_some_and(can_hold_closure)
        }
        Type::Null
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
) -> String {
    let rows: BTreeMap<_, _> = exports
        .iter()
        .map(|id| {
            let mut graph = Graph::build(definitions, *id);
            graph.solve();
            (
                &definitions[id].name,
                graph.units[graph.functions[id]].class,
            )
        })
        .collect();
    rows.into_iter()
        .map(|(name, class)| format!("{name}\t{}\n", class.label()))
        .collect()
}

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
    let actual = current_census();
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
    let definition = lash_kernel_doc::parse_definition(
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
            &BTreeMap::from([(id, definition.clone())]),
            &BTreeSet::from([id])
        ),
        pinned
    );
    let mut seeded = definition;
    let Implementation::Body(body) = &mut seeded.implementation else {
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
    let changed = census(&BTreeMap::from([(id, seeded)]), &BTreeSet::from([id]));
    assert_eq!(changed, "helper\toperation\n");
    assert_eq!(
        diff(pinned, &changed),
        "- helper total\n+ helper operation\n"
    );

    // K-EVAL-001/002: the initializer sees the outer origin, and even an
    // unselected branch contributes its closure to the conservative census.
    let conditional = lash_kernel_doc::parse_definition(
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
        census(&BTreeMap::from([(id, conditional)]), &BTreeSet::from([id])),
        "helper\toperation\n"
    );
}

#[cfg(all(feature = "dialect-python", feature = "dialect-typescript"))]
pub(super) fn current_census() -> String {
    let mut actual = String::new();
    let mut dialects: Vec<_> = crate::DIALECTS.iter().collect();
    dialects.sort_by_key(|built| built.dialect);
    assert_eq!(dialects.len(), 2, "the census checks both shipped dialects");
    for built in dialects {
        let helpers = lash_kernel_doc::ValidatedFunctions::from_tokens(built.helpers)
            .expect("helpers decode");
        let exports = helpers.iter().map(|(id, _)| *id).collect();
        let held = built.held_releases().expect("releases decode");
        let definitions: BTreeMap<_, _> = held
            .retained
            .last()
            .expect("the building release")
            .functions
            .iter()
            .map(|held| (held.function, held.definition.clone()))
            .collect();
        actual.push_str(&census(&definitions, &exports));
    }
    actual
}
