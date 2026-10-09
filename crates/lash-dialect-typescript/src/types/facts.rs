//! The type of every binding of a source, by name.
//!
//! One pass over the whole tree collects, for each name the source
//! declares, every declaration and every assignment that names it, in
//! whatever scope or function. A name's type is what all of them agree on,
//! so the answer holds for every binding that spells the name and whichever
//! one a read resolves to; two bindings of one name that disagree are
//! unknown. That is coarser than scoping and needs no second resolver.
//!
//! A binding with an annotation is believed to be what the annotation says,
//! whatever is assigned to it. One without is inferred: a `const`, or a
//! `let` with an initialiser, is a type when its initialiser and every
//! assignment to its name are that type. A `var` is not inferred, because
//! it is `undefined` until its declaration runs; a parameter is not,
//! because a caller may pass anything.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lash_kernel_doc::Name;

use super::{Ty, believed, binary_result, unary_result};
use crate::adapter::{
    ArrayElement, AssignOp, AssignTarget, BinaryOp, CallArg, Catch, Expr, Function, FunctionBody,
    MemberProperty, ObjectProperty, OptionalOperation, Pattern, Program, PropertyKey, Stmt,
    TypeAnnotation, VarKind,
};

/// How many times the types are recomputed before the analysis gives up
/// and believes nothing. Types only widen, three steps at most each, so
/// real sources settle in a handful of rounds.
const MAX_ROUNDS: usize = 32;

/// One declaration of a name, or one assignment to it.
enum Source<'a> {
    /// A value the analysis says nothing about.
    Opaque,
    Is(Ty),
    Annotated(&'a TypeAnnotation),
    /// A function, with the return type it declares.
    Returns(Option<&'a TypeAnnotation>),
    Value(&'a Expr),
    /// `name op= value`.
    Compound(BinaryOp, &'a Expr),
    /// One element of an iterated value.
    ElementOf(&'a Expr),
}

#[derive(Default)]
struct Entry<'a> {
    sources: Vec<Source<'a>>,
    declarations: usize,
    annotated: usize,
    assignments: usize,
    /// A `var`, a function or an enum: it exists before its declaration
    /// runs, so a test of it proves nothing about a later read.
    hoisted: bool,
}

/// How a pattern's names come to exist.
#[derive(Clone, Copy)]
enum Role {
    Declared { hoisted: bool },
    Assigned,
}

#[derive(Default)]
struct Collector<'a> {
    entries: HashMap<&'a str, Entry<'a>>,
    order: Vec<&'a str>,
    aliases: Vec<(&'a str, &'a TypeAnnotation)>,
}

/// What the analysis found.
#[derive(Debug, Default)]
pub(crate) struct Facts {
    types: HashMap<String, Ty>,
    narrowable: HashSet<String>,
    aliases: BTreeMap<String, Ty>,
}

impl Facts {
    /// Analyses a program. `session` names the bindings earlier cells left:
    /// nothing is known of them, whatever this cell declares.
    pub(crate) fn analyse(program: &Program, session: &BTreeSet<Name>) -> Self {
        let mut collector = Collector::default();
        collector.statements(&program.statements);
        let aliases = resolve_aliases(&collector.aliases, &program.type_parameters);
        let mut facts = Self {
            types: collector
                .order
                .iter()
                .map(|name| ((*name).to_string(), Ty::Never))
                .collect(),
            narrowable: HashSet::new(),
            aliases,
        };
        let mut settled = false;
        for _ in 0..MAX_ROUNDS {
            let mut changed = false;
            for name in &collector.order {
                let ty = facts.entry_type(name, &collector.entries[name]);
                if facts.types.get(*name) != Some(&ty) {
                    facts.types.insert((*name).to_string(), ty);
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        for (name, ty) in &mut facts.types {
            if !settled || *ty == Ty::Never || session.contains(&Name::new(name.as_str())) {
                *ty = Ty::Unknown;
            }
        }
        facts.narrowable = collector
            .entries
            .iter()
            .filter(|(name, entry)| {
                entry.declarations == 1
                    && entry.assignments == 0
                    && !entry.hoisted
                    && !session.contains(&Name::new(**name))
            })
            .map(|(name, _)| (*name).to_string())
            .collect();
        facts
    }

    /// The type of the source's bindings named `name`, or `None` when the
    /// source declares no such name.
    pub(crate) fn of(&self, name: &str) -> Option<&Ty> {
        self.types.get(name)
    }

    /// Whether a test of `name` says what a later read of it holds: the
    /// source declares the name once, never assigns it, and it does not
    /// exist before its declaration.
    pub(crate) fn narrowable(&self, name: &str) -> bool {
        self.narrowable.contains(name)
    }

    /// What an annotation lets the lowerer believe.
    pub(crate) fn believed(&self, annotation: &TypeAnnotation) -> Ty {
        believed(annotation, &self.aliases)
    }

    /// What a function's declared return type lets a call of it give.
    pub(crate) fn function(&self, return_ty: Option<&TypeAnnotation>) -> Ty {
        Ty::Function(Box::new(
            return_ty.map_or(Ty::Unknown, |annotation| self.believed(annotation)),
        ))
    }

    fn entry_type(&self, name: &str, entry: &Entry<'_>) -> Ty {
        // A name every declaration annotates is what the annotations say.
        let trusted = entry.declarations > 0 && entry.annotated == entry.declarations;
        let mut ty = Ty::Never;
        for source in &entry.sources {
            let part = match source {
                Source::Annotated(annotation) => self.believed(annotation),
                _ if trusted => continue,
                Source::Opaque => Ty::Unknown,
                Source::Is(ty) => ty.clone(),
                Source::Returns(return_ty) => self.function(*return_ty),
                Source::Value(value) => self.type_of(value),
                Source::Compound(op, value) => {
                    let current = self.types.get(name).cloned().unwrap_or(Ty::Unknown);
                    binary_result(*op, &current, &self.type_of(value))
                }
                Source::ElementOf(iterated) => self.type_of(iterated).element(),
            };
            ty = ty.join(&part);
        }
        ty
    }

    /// The type of an expression's value, read off its shape. It agrees
    /// with, and is never more exact than, the type the lowerer gives the
    /// same expression's operand.
    fn type_of(&self, expr: &Expr) -> Ty {
        match expr {
            Expr::Null => Ty::Null,
            Expr::Bool(_) => Ty::Bool,
            Expr::Number(_) => Ty::Float,
            Expr::String(_) => Ty::Text,
            Expr::Template { .. } => Ty::Text,
            Expr::Ident(name, _) => match self.types.get(name.as_str()) {
                Some(ty) => ty.clone(),
                None => match name.as_str() {
                    "undefined" => Ty::Undefined,
                    "NaN" | "Infinity" => Ty::Float,
                    _ => Ty::Unknown,
                },
            },
            Expr::As { ty, .. } => self.believed(ty),
            Expr::Unary { op, .. } => unary_result(*op),
            Expr::Binary {
                left, op, right, ..
            } => binary_result(*op, &self.type_of(left), &self.type_of(right)),
            Expr::Logical { left, right, .. } => self.type_of(left).join(&self.type_of(right)),
            Expr::Conditional {
                consequent,
                alternate,
                ..
            } => self.type_of(consequent).join(&self.type_of(alternate)),
            Expr::Assign { target, op, value } => match (target, op) {
                (_, AssignOp::Assign) => self.type_of(value),
                (
                    AssignTarget::Ident(name) | AssignTarget::ParenIdent(name),
                    AssignOp::Binary(op),
                ) => {
                    let current = self
                        .types
                        .get(name.as_str())
                        .cloned()
                        .unwrap_or(Ty::Unknown);
                    binary_result(*op, &current, &self.type_of(value))
                }
                _ => Ty::Unknown,
            },
            Expr::Update { .. } => Ty::Float,
            Expr::Delete { .. } => Ty::Bool,
            Expr::Function(function) => self.function(function.return_ty.as_ref()),
            Expr::Call { callee, .. } => match callee.as_ref() {
                Expr::Member { .. } => Ty::Unknown,
                callee => self.type_of(callee).returned(),
            },
            Expr::Member {
                object, property, ..
            } => {
                let object = self.type_of(object);
                match property {
                    MemberProperty::Field(name) => object.property(name),
                    MemberProperty::Index(index) => match (&object, self.type_of(index)) {
                        (Ty::Never, _) | (_, Ty::Never) => Ty::Never,
                        (Ty::List(_), index) if index.is_number() => object.element(),
                        _ => Ty::Unknown,
                    },
                }
            }
            Expr::RegExp { .. }
            | Expr::This
            | Expr::Array(_)
            | Expr::Object(_)
            | Expr::New { .. }
            | Expr::OptionalChain { .. }
            | Expr::Await { .. }
            | Expr::LoneSurrogateString => Ty::Unknown,
        }
    }
}

/// What each `type` and `interface` of the source stands for. A name
/// declared twice, one that refers to a name declared after it, and one a
/// function's type parameter shares stand for nothing.
fn resolve_aliases(
    declared: &[(&str, &TypeAnnotation)],
    type_parameters: &BTreeSet<String>,
) -> BTreeMap<String, Ty> {
    let mut aliases = BTreeMap::new();
    let mut twice = BTreeSet::new();
    for (name, annotation) in declared {
        let ty = believed(annotation, &aliases);
        if aliases.insert((*name).to_string(), ty).is_some() {
            twice.insert((*name).to_string());
        }
    }
    for name in twice.iter().chain(type_parameters) {
        aliases.insert(name.clone(), Ty::Unknown);
    }
    aliases
}

impl<'a> Collector<'a> {
    fn entry(&mut self, name: &'a str) -> &mut Entry<'a> {
        if !self.entries.contains_key(name) {
            self.order.push(name);
        }
        self.entries.entry(name).or_default()
    }

    fn name(&mut self, name: &'a str, role: Role, source: Source<'a>) {
        let entry = self.entry(name);
        match role {
            Role::Declared { hoisted } => {
                entry.declarations += 1;
                entry.hoisted |= hoisted;
                if matches!(source, Source::Annotated(_)) {
                    entry.annotated += 1;
                }
            }
            Role::Assigned => entry.assignments += 1,
        }
        entry.sources.push(source);
    }

    /// A pattern whose names take values the analysis does not follow.
    fn pattern(&mut self, pattern: &'a Pattern, role: Role) {
        match pattern {
            Pattern::Ident(name, annotation) => {
                let source = match (annotation, role) {
                    (Some(annotation), Role::Declared { .. }) => Source::Annotated(annotation),
                    _ => Source::Opaque,
                };
                self.name(name, role, source);
            }
            Pattern::Rest(inner) => self.pattern(inner, role),
            Pattern::Member { object, property } => {
                self.expr(object);
                self.property(property);
            }
            Pattern::Assign { target, default } => {
                self.pattern(target, role);
                self.expr(default);
            }
            Pattern::Array { elements, rest } => {
                for element in elements.iter().flatten() {
                    self.pattern(element, role);
                }
                if let Some(rest) = rest {
                    self.pattern(rest, role);
                }
            }
            Pattern::Object { properties, rest } => {
                for property in properties {
                    if let PropertyKey::Computed(key) = &property.key {
                        self.expr(key);
                    }
                    self.pattern(&property.value, role);
                }
                if let Some(rest) = rest {
                    self.pattern(rest, role);
                }
            }
        }
    }

    /// A pattern that is one name takes `source`; any other is opaque.
    fn bound(&mut self, pattern: &'a Pattern, role: Role, source: Source<'a>) {
        match pattern {
            Pattern::Ident(name, None) => self.name(name, role, source),
            _ => self.pattern(pattern, role),
        }
    }

    fn statements(&mut self, statements: &'a [Stmt]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Spanned(_, inner) | Stmt::Labeled { stmt: inner, .. } => self.statement(inner),
            Stmt::Empty | Stmt::Break | Stmt::Continue => {}
            Stmt::TypeAlias { name, ty } => self.aliases.push((name, ty)),
            Stmt::Expr(expr) | Stmt::Throw(expr) => self.expr(expr),
            Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            Stmt::Block(body) => self.statements(body),
            Stmt::Var { kind, declarations } => {
                for declaration in declarations {
                    let hoisted = *kind == VarKind::Var;
                    let source = match &declaration.init {
                        Some(init) if !hoisted => Source::Value(init),
                        _ => Source::Opaque,
                    };
                    self.bound(&declaration.pattern, Role::Declared { hoisted }, source);
                    if let Some(init) = &declaration.init {
                        self.expr(init);
                    }
                }
            }
            Stmt::Enum { name, members } => {
                self.name(name, Role::Declared { hoisted: true }, Source::Opaque);
                for member in members {
                    self.expr(&member.value);
                }
            }
            Stmt::Function { name, function } => {
                self.name(
                    name,
                    Role::Declared { hoisted: true },
                    Source::Returns(function.return_ty.as_ref()),
                );
                self.function(function);
            }
            Stmt::If {
                test,
                consequent,
                alternate,
            } => {
                self.expr(test);
                self.statement(consequent);
                if let Some(alternate) = alternate {
                    self.statement(alternate);
                }
            }
            Stmt::While { test, body } | Stmt::DoWhile { body, test, .. } => {
                self.expr(test);
                self.statement(body);
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                if let Some(init) = init {
                    self.statement(init);
                }
                for expr in [test, update].into_iter().flatten() {
                    self.expr(expr);
                }
                self.statement(body);
            }
            Stmt::ForOf {
                pattern,
                kind,
                iterable,
                body,
            } => {
                self.bound(
                    pattern,
                    loop_role(*kind),
                    loop_source(*kind, Source::ElementOf(iterable)),
                );
                self.expr(iterable);
                self.statement(body);
            }
            Stmt::ForIn {
                pattern,
                kind,
                object,
                body,
            } => {
                // `for...in` visits property names.
                self.bound(
                    pattern,
                    loop_role(*kind),
                    loop_source(*kind, Source::Is(Ty::Text)),
                );
                self.expr(object);
                self.statement(body);
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.expr(discriminant);
                for case in cases {
                    if let Some(test) = &case.test {
                        self.expr(test);
                    }
                    self.statements(&case.consequent);
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                self.statements(body);
                if let Some(Catch { binding, body }) = catch {
                    if let Some(binding) = binding {
                        self.bound(binding, Role::Declared { hoisted: false }, Source::Opaque);
                    }
                    self.statements(body);
                }
                if let Some(finally) = finally {
                    self.statements(finally);
                }
            }
        }
    }

    fn function(&mut self, function: &'a Function) {
        for param in &function.params {
            self.pattern(param, Role::Declared { hoisted: false });
        }
        match &function.body {
            FunctionBody::Block(body) => self.statements(body),
            FunctionBody::Expression(value) => self.expr(value),
        }
    }

    fn property(&mut self, property: &'a MemberProperty) {
        if let MemberProperty::Index(index) = property {
            self.expr(index);
        }
    }

    fn args(&mut self, args: &'a [CallArg]) {
        for arg in args {
            match arg {
                CallArg::Value(value) | CallArg::Spread(value) => self.expr(value),
            }
        }
    }

    fn target(&mut self, target: &'a AssignTarget, source: Source<'a>) {
        match target {
            AssignTarget::Ident(name) | AssignTarget::ParenIdent(name) => {
                self.name(name, Role::Assigned, source);
            }
            AssignTarget::Member { object, property } => {
                self.expr(object);
                self.property(property);
            }
            AssignTarget::Pattern(pattern) => self.pattern(pattern, Role::Assigned),
        }
    }

    fn expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::RegExp { .. }
            | Expr::Ident(..)
            | Expr::This
            | Expr::LoneSurrogateString => {}
            Expr::Array(elements) => {
                for element in elements {
                    match element {
                        ArrayElement::Value(value) | ArrayElement::Spread(value) => {
                            self.expr(value);
                        }
                        ArrayElement::Hole => {}
                    }
                }
            }
            Expr::Object(properties) => {
                for property in properties {
                    match property {
                        ObjectProperty::KeyValue(key, value) => {
                            if let PropertyKey::Computed(key) = key {
                                self.expr(key);
                            }
                            self.expr(value);
                        }
                        ObjectProperty::Spread(value) => self.expr(value),
                    }
                }
            }
            Expr::Assign { target, op, value } => {
                let source = match op {
                    AssignOp::Assign => Source::Value(value),
                    AssignOp::Binary(op) => Source::Compound(*op, value),
                    AssignOp::Logical(_) => Source::Opaque,
                };
                self.target(target, source);
                self.expr(value);
            }
            Expr::Update { target, .. } => self.target(target, Source::Is(Ty::Float)),
            Expr::Member {
                object, property, ..
            }
            | Expr::Delete { object, property } => {
                self.expr(object);
                self.property(property);
            }
            Expr::Unary { value, .. } | Expr::Await { value, .. } | Expr::As { value, .. } => {
                self.expr(value);
            }
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                self.expr(test);
                self.expr(consequent);
                self.expr(alternate);
            }
            Expr::Template { expressions, .. } => {
                for expression in expressions {
                    self.expr(expression);
                }
            }
            Expr::Function(function) => {
                if let Some(name) = function.name.as_deref().filter(|_| !function.is_arrow) {
                    self.name(
                        name,
                        Role::Declared { hoisted: true },
                        Source::Returns(function.return_ty.as_ref()),
                    );
                }
                self.function(function);
            }
            Expr::Call { callee, args, .. } => {
                self.expr(callee);
                self.args(args);
            }
            Expr::New { args, .. } => self.args(args),
            Expr::OptionalChain { base, operations } => {
                self.expr(base);
                for operation in operations {
                    match operation {
                        OptionalOperation::Member { property, .. } => self.property(property),
                        OptionalOperation::Call { args, .. } => self.args(args),
                    }
                }
            }
        }
    }
}

/// A `for...of` or `for...in` head declares its names, or assigns names
/// that exist.
fn loop_role(kind: Option<VarKind>) -> Role {
    match kind {
        Some(kind) => Role::Declared {
            hoisted: kind == VarKind::Var,
        },
        None => Role::Assigned,
    }
}

/// What a loop's head gives its name. A `var` is `undefined` before the
/// loop's first pass, so it is not what the loop gives it.
fn loop_source(kind: Option<VarKind>, source: Source<'_>) -> Source<'_> {
    if kind == Some(VarKind::Var) {
        Source::Opaque
    } else {
        source
    }
}
