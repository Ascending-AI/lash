//! The type of every binding of a source.
//!
//! One pass over the whole tree resolves each spelling of a name, whether
//! it declares, assigns or reads, to the binding it names, in the scopes
//! the lowerer gives the source: a function's parameters, its `var`s and
//! its body's own declarations share one scope, and a block, a loop's
//! head, a `switch`'s clauses, a `catch` and a named function expression's
//! name each open one. A binding's type is what all of its
//! declarations and assignments agree on, so two bindings that share a
//! name are typed apart, and a read is the type of the binding it names.
//!
//! A spelling is known by where the program holds its text: the lowerer
//! reads the same tree, so the spelling it reads is the one resolved here,
//! and a name it makes up resolves to nothing and is unknown.
//!
//! A binding with an annotation is believed to be what the annotation says,
//! whatever is assigned to it. One without is inferred: a `const`, or a
//! `let` with an initialiser, is a type when its initialiser and every
//! assignment to its name are that type. A `var` is not inferred, because
//! it is `undefined` until its declaration runs; a parameter is not,
//! because a caller may pass anything.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lash_vocabulary::{Name, Signature};

use super::{FRESH_LIST_METHODS, Ty, believed, binary_result, unary_result};
use crate::adapter::{
    ArrayElement, AssignOp, AssignTarget, BinaryOp, CallArg, Catch, Expr, Function, FunctionBody,
    MemberProperty, ObjectProperty, OptionalOperation, Pattern, Program, PropertyKey, Stmt,
    TypeAnnotation, UnaryOp, VarKind,
};

/// How many times the types are recomputed before the analysis gives up
/// and believes nothing. Types only widen, three steps at most each, so
/// real sources settle in a handful of rounds.
const MAX_ROUNDS: usize = 32;

/// A tool call's static dotted spelling. Shadowed roots are excluded from
/// `effect_results`, conservatively across every scope in the source.
fn effect_path(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Ident(name, _) => Some(name.clone()),
        Expr::Member {
            object,
            property: MemberProperty::Field(field),
            ..
        } => Some(format!("{}.{field}", effect_path(object)?)),
        _ => None,
    }
}

/// Where the analysed program holds a spelling's text (see the module).
fn spelling(name: &str) -> usize {
    name.as_ptr().addr()
}

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
    /// The property `.1` of the namespace `.0` stands for, bound by name
    /// (`const { col } = dataframe`): a package member's function value
    /// when `.0` is a package path.
    PackageMember(&'a Expr, &'a str),
}

/// One binding, with every declaration and assignment of it.
struct Entry<'a> {
    name: String,
    /// Declared in the program's own scope, which is the session's: a name
    /// an earlier cell left is that cell's binding.
    session_scope: bool,
    /// How many functions enclose the declaration.
    depth: usize,
    sources: Vec<Source<'a>>,
    declarations: usize,
    annotated: usize,
    assignments: usize,
}

/// What the source does with a name, in every scope.
#[derive(Default)]
struct Tally {
    declarations: usize,
    assignments: usize,
    /// A `var`, a function or an enum: it exists before its declaration
    /// runs, so a test of it proves nothing about a later read.
    hoisted: bool,
}

/// How a pattern's names come to exist.
#[derive(Clone, Copy)]
enum Role {
    /// Declared in the innermost scope.
    Declared {
        hoisted: bool,
    },
    /// A `var`: declared in its function's scope, before any of it runs.
    Var,
    Assigned,
}

#[derive(Default)]
struct Collector<'a> {
    bindings: Vec<Entry<'a>>,
    /// The bindings each open scope declares, innermost last.
    scopes: Vec<HashMap<String, usize>>,
    /// The binding each spelling names.
    spellings: HashMap<usize, usize>,
    names: HashMap<&'a str, Tally>,
    plain: super::plain::Collector<'a>,
    function_depth: usize,
    aliases: Vec<(&'a str, &'a TypeAnnotation)>,
    direct_calls: HashSet<&'a str>,
}

/// What the analysis found.
#[derive(Debug, Default)]
pub(crate) struct Facts {
    plain: super::plain::Facts,
    /// Each binding's type, by its index in the collector.
    types: Vec<Ty>,
    /// Whether an assignment or an update writes each binding.
    assigned: Vec<bool>,
    spellings: HashMap<usize, usize>,
    /// Every name the source declares or assigns.
    named: HashSet<String>,
    narrowable: HashSet<String>,
    aliases: BTreeMap<String, Ty>,
    direct_calls: HashSet<String>,
    /// Declared results of tools whose root no source or session binding shadows.
    effect_results: BTreeMap<String, Ty>,
    /// What the packages in view declare their members give.
    packages: super::packages::Packages,
}

impl Facts {
    /// Analyses a program. `session` names the bindings earlier cells left:
    /// nothing is known of them, whatever this cell declares.
    pub(crate) fn analyse(
        program: &Program,
        session: &BTreeSet<Name>,
        terminal: &BTreeSet<String>,
        effects: &BTreeMap<String, &Signature>,
        packages: super::packages::Packages,
    ) -> Self {
        let mut collector = Collector::default();
        collector.scopes.push(HashMap::new());
        collector.declare_vars(&program.statements);
        collector.declare_block(&program.statements);
        collector.statements(&program.statements);
        let mut aliases = resolve_aliases(&collector.aliases, &program.type_parameters);
        // A package's value type is named by its dotted path, which no
        // local alias can spell.
        aliases.extend(
            packages
                .types()
                .map(|(name, ty)| (name.clone(), ty.clone())),
        );
        for (binding, entry) in collector.bindings.iter().enumerate() {
            if entry.session_scope && session.contains(&Name::new(entry.name.as_str())) {
                collector.plain.session(binding);
            }
        }
        let numbers = Numbers::proven(&collector.bindings, &collector.spellings, session);
        let plain = std::mem::take(&mut collector.plain).finish(
            session,
            terminal,
            &|name| collector.spellings.get(&spelling(name)).copied(),
            &|expr| numbers.is(expr),
        );
        let mut facts = Self {
            effect_results: effects
                .iter()
                .filter(|(name, _)| {
                    let root = name.split('.').next().unwrap_or_default();
                    !crate::builtins::is_global(root)
                        && !collector.names.contains_key(root)
                        && !session.contains(&Name::new(root))
                })
                .map(|(name, signature)| (name.clone(), Ty::decoded(&signature.result)))
                .collect(),
            direct_calls: collector
                .direct_calls
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            plain,
            types: vec![Ty::Never; collector.bindings.len()],
            assigned: collector
                .bindings
                .iter()
                .map(|entry| entry.assignments > 0)
                .collect(),
            spellings: std::mem::take(&mut collector.spellings),
            named: collector
                .names
                .keys()
                .map(|name| (*name).to_owned())
                .collect(),
            narrowable: HashSet::new(),
            aliases,
            packages,
        };
        let mut settled = false;
        for _ in 0..MAX_ROUNDS {
            let mut changed = false;
            for (binding, entry) in collector.bindings.iter().enumerate() {
                let ty = facts.entry_type(binding, entry);
                if facts.types[binding] != ty {
                    facts.types[binding] = ty;
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        for (ty, entry) in facts.types.iter_mut().zip(&collector.bindings) {
            if !settled
                || *ty == Ty::Never
                || (entry.session_scope && session.contains(&Name::new(entry.name.as_str())))
            {
                *ty = Ty::Unknown;
            }
        }
        facts.narrowable = collector
            .names
            .iter()
            .filter(|(name, tally)| {
                tally.declarations == 1
                    && tally.assignments == 0
                    && !tally.hoisted
                    && !session.contains(&Name::new(**name))
            })
            .map(|(name, _)| (*name).to_string())
            .collect();
        facts
    }

    /// Only plain identifier calls can use a positional entry. A name
    /// match may include a shadowed binding, which only over-admits a split.
    pub(crate) fn has_direct_call(&self, name: &str) -> bool {
        self.direct_calls.contains(name)
    }

    pub(crate) fn path_definition(&self, name: &str) -> Option<&Expr> {
        self.narrowable(name)
            .then(|| self.plain.definition(name))
            .flatten()
    }

    pub(crate) fn plain(&self, expr: &Expr) -> Option<super::PlainRecord> {
        self.plain.of(expr, &|name| self.binding(name))
    }

    pub(crate) fn primitive(&self, expr: &Expr) -> Option<Ty> {
        self.plain.primitive(expr, &|name| self.binding(name))
    }

    /// The literal text `expr`'s value is proven by construction to begin
    /// with, never by an annotation: `Some` only where it is always a text.
    pub(crate) fn text_prefix(&self, expr: &Expr) -> Option<String> {
        self.plain.text_prefix(expr, &|name| self.binding(name))
    }

    /// The type of the binding a spelling of the program names, or `None`
    /// when it names none of the source's: a session binding, a global, or
    /// a name the lowerer made up. `spelling` is the program's own text of
    /// it, as the lowerer reads it, not a copy.
    pub(crate) fn of(&self, spelling: &str) -> Option<&Ty> {
        self.binding(spelling).map(|binding| &self.types[binding])
    }

    fn binding(&self, name: &str) -> Option<usize> {
        self.spellings.get(&spelling(name)).copied()
    }

    /// Whether an assignment or an update may write the binding a spelling
    /// of the program names. One that names none of the source's bindings
    /// may be written.
    pub(crate) fn assigned(&self, spelling: &str) -> bool {
        self.binding(spelling)
            .is_none_or(|binding| self.assigned[binding])
    }

    /// Whether the source declares or assigns `name` in any scope.
    pub(crate) fn names(&self, name: &str) -> bool {
        self.named.contains(name)
    }

    /// Whether a test of `name` says what a later read of it holds: the
    /// source declares the name once, in any scope, never assigns it, and
    /// it does not exist before its declaration.
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

    fn entry_type(&self, binding: usize, entry: &Entry<'_>) -> Ty {
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
                    binary_result(*op, &self.types[binding], &self.type_of(value))
                }
                Source::ElementOf(iterated) => self.type_of(iterated).element(),
                Source::PackageMember(namespace, name) => effect_path(namespace)
                    .filter(|path| {
                        path.split('.').next().is_some_and(|root| {
                            self.packages.is_root(root) && !self.named.contains(root)
                        })
                    })
                    .and_then(|path| self.packages.call(&format!("{path}.{name}")))
                    .map_or(Ty::Unknown, |gives| {
                        Ty::Function(Box::new(gives.called(false)))
                    }),
            };
            ty = ty.join(&part);
        }
        ty
    }

    /// What a call of `callee` gives when it calls a package member: a
    /// function or static by its path, whose root the source does not bind,
    /// or a method of a value whose package type is known.
    fn package_call(&self, callee: &Expr) -> Option<&super::packages::Gives> {
        if let Expr::Member {
            object,
            property: MemberProperty::Field(name),
            ..
        } = callee
            && let Ty::Package(brand) = self.type_of(object)
        {
            return match self.packages.member(&brand, name) {
                Some((super::packages::MemberKind::Method, gives)) => Some(gives),
                _ => None,
            };
        }
        let path = effect_path(callee)?;
        let root = path.split('.').next()?;
        if !self.packages.is_root(root) || self.named.contains(root) {
            return None;
        }
        self.packages.call(&path)
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
            Expr::Ident(name, _) => match self.binding(name) {
                Some(binding) => self.types[binding].clone(),
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
                        .binding(name)
                        .map_or(Ty::Unknown, |binding| self.types[binding].clone());
                    binary_result(*op, &current, &self.type_of(value))
                }
                _ => Ty::Unknown,
            },
            Expr::New { constructor, .. } if self.binding(constructor).is_none() => {
                match constructor.as_str() {
                    "Array" => Ty::Array,
                    "Map" => Ty::Map,
                    "Set" => Ty::Set,
                    _ => Ty::Unknown,
                }
            }
            Expr::Update { .. } => Ty::Float,
            Expr::Delete { .. } => Ty::Bool,
            // A cell's `const` of an `async` arrow may name a process, which
            // is no function token.
            Expr::Function(function) if function.is_async && function.is_arrow => Ty::Unknown,
            Expr::Function(function) => self.function(function.return_ty.as_ref()),
            Expr::Call { callee, .. } if self.package_call(callee).is_some() => self
                .package_call(callee)
                .map_or(Ty::Unknown, |gives| gives.called(false)),
            Expr::Call { callee, .. } => match callee.as_ref() {
                Expr::Ident(name, _) if name == "String" && self.binding(name).is_none() => {
                    Ty::Text
                }
                Expr::Member {
                    object,
                    property: MemberProperty::Field(name),
                    ..
                } if matches!(object.as_ref(), Expr::Ident(root, _) if root == "JSON" && self.binding(root).is_none())
                    && name == "stringify" =>
                {
                    Ty::Union(vec![Ty::Text, Ty::Undefined])
                }
                Expr::Member {
                    object,
                    property: MemberProperty::Field(name),
                    ..
                } if FRESH_LIST_METHODS.contains(&name.as_str())
                    && matches!(self.type_of(object), Ty::List(_) | Ty::Array) =>
                {
                    Ty::Array
                }
                Expr::Member {
                    object,
                    property: MemberProperty::Field(name),
                    ..
                } if name == "split" && self.type_of(object) == Ty::Text => {
                    // Both the string helper and the primitive split build a
                    // kernel list. Keep that representation across bindings.
                    Ty::Array
                }
                Expr::Member { .. } => Ty::Unknown,
                callee => self.type_of(callee).returned(),
            },
            Expr::Member {
                object, property, ..
            } => {
                let object = self.type_of(object);
                if let (Ty::Package(brand), MemberProperty::Field(name)) = (&object, property) {
                    return match self.packages.member(brand, name) {
                        Some((super::packages::MemberKind::Property, gives)) => {
                            gives.result.clone()
                        }
                        _ => Ty::Unknown,
                    };
                }
                match property {
                    MemberProperty::Field(name) => object.property(name),
                    MemberProperty::Index(index) => match (&object, self.type_of(index)) {
                        (Ty::Never, _) | (_, Ty::Never) => Ty::Never,
                        (Ty::List(_), index) if index.is_number() => object.element(),
                        _ => Ty::Unknown,
                    },
                }
            }
            Expr::Array(_) => Ty::Array,
            Expr::Object(_) => Ty::Object,
            Expr::Await { value, .. } => match value.as_ref() {
                Expr::Call { callee, .. } if self.package_call(callee).is_some() => self
                    .package_call(callee)
                    .map_or(Ty::Unknown, |gives| gives.called(true)),
                Expr::Call { callee, .. } => effect_path(callee)
                    .and_then(|path| self.effect_results.get(&path).cloned())
                    .unwrap_or(Ty::Unknown),
                _ => Ty::Unknown,
            },
            Expr::RegExp { .. }
            | Expr::This
            | Expr::New { .. }
            | Expr::OptionalChain { .. }
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
    /// Runs `walk` in a scope of its own.
    fn scoped(&mut self, walk: impl FnOnce(&mut Self)) {
        self.scopes.push(HashMap::new());
        walk(self);
        self.scopes.pop();
    }

    /// Runs `walk` where the innermost scope is not yet open: the lowerer
    /// lowers a loop's iterated value and a `switch`'s tests before it
    /// opens the scope of the loop's head or the clauses.
    fn outside(&mut self, walk: impl FnOnce(&mut Self)) {
        let scope = self.scopes.pop();
        walk(self);
        self.scopes.extend(scope);
    }

    /// Declares `name` in the innermost scope, or gives the binding it
    /// already declares there: `var` twice, a function over a `var`, a
    /// parameter named again.
    fn declare(&mut self, name: &str) -> usize {
        let session_scope = self.scopes.len() == 1;
        let depth = self.function_depth;
        let Some(scope) = self.scopes.last_mut() else {
            unreachable!("the program's scope is open");
        };
        *scope.entry(name.to_owned()).or_insert_with(|| {
            self.bindings.push(Entry {
                name: name.to_owned(),
                session_scope,
                depth,
                sources: Vec::new(),
                declarations: 0,
                annotated: 0,
                assignments: 0,
            });
            self.bindings.len() - 1
        })
    }

    /// Declares what a pattern binds.
    fn declare_pattern(&mut self, pattern: &'a Pattern) {
        let mut names = Vec::new();
        crate::lower::walk::pattern_names(pattern, &mut names);
        self.declare_names(names);
    }

    fn declare_names(&mut self, names: Vec<String>) {
        for name in names {
            self.declare(&name);
        }
    }

    /// The binding `name` names where the walk stands.
    fn lookup(&self, name: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    /// A read: the binding its spelling names, for the lowerer's read of
    /// the same spelling.
    fn read(&mut self, name: &'a str) {
        match self.lookup(name) {
            Some(binding) => {
                self.spellings.insert(spelling(name), binding);
                self.capture(binding);
            }
            None if self.function_depth > 0 => {
                self.plain
                    .exclude(super::plain::Key::Free(name.to_string()));
            }
            None => {}
        }
    }

    /// A function nested in the one declaring `binding` names it: no
    /// record it holds is proven plain.
    fn capture(&mut self, binding: usize) {
        if self.function_depth > self.bindings[binding].depth {
            self.plain.exclude(super::plain::Key::Binding(binding));
        }
    }

    fn name(&mut self, name: &'a str, role: Role, source: Source<'a>) {
        let tally = self.names.entry(name).or_default();
        match role {
            Role::Declared { hoisted } => {
                tally.declarations += 1;
                tally.hoisted |= hoisted;
            }
            Role::Var => {
                tally.declarations += 1;
                tally.hoisted = true;
            }
            Role::Assigned => tally.assignments += 1,
        }
        let binding = match role {
            Role::Declared { .. } => Some(self.declare(name)),
            // The function's scope declared every `var` of its body, and no
            // scope between can declare the name again.
            Role::Var => Some(self.lookup(name).unwrap_or_else(|| self.declare(name))),
            // A name no scope declares is a session binding or a global,
            // which nothing here types.
            Role::Assigned => self.lookup(name),
        };
        let value = match &source {
            Source::Value(value) => Some(*value),
            _ => None,
        };
        self.plain.name(name, binding, value);
        let Some(binding) = binding else {
            return;
        };
        self.spellings.insert(spelling(name), binding);
        self.capture(binding);
        let entry = &mut self.bindings[binding];
        if let Role::Assigned = role {
            entry.assignments += 1;
        } else {
            entry.declarations += 1;
            if matches!(source, Source::Annotated(_)) {
                entry.annotated += 1;
            }
        }
        entry.sources.push(source);
    }

    /// A pattern whose names take values the analysis does not follow.
    fn pattern(&mut self, pattern: &'a Pattern, role: Role) {
        match pattern {
            Pattern::Ident(name, annotation) => {
                let source = match (annotation, role) {
                    (Some(annotation), Role::Declared { .. } | Role::Var) => {
                        Source::Annotated(annotation)
                    }
                    _ => Source::Opaque,
                };
                self.name(name, role, source);
            }
            Pattern::Rest(inner) => self.pattern(inner, role),
            Pattern::Member { object, property } => {
                self.plain.write(property);
                self.expr(object);
                self.property(property);
            }
            // The default runs before its target is declared.
            Pattern::Assign { target, default } => {
                self.expr(default);
                self.pattern(target, role);
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
                        self.plain.escape(key);
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
        match (pattern, &source) {
            (
                Pattern::Object {
                    properties,
                    rest: None,
                },
                Source::Value(namespace),
            ) if effect_path(namespace).is_some() => {
                // Each name bound from a static path, which may be a
                // package's namespace.
                self.plain.escape(namespace);
                for property in properties {
                    match (&property.key, &property.value) {
                        (PropertyKey::Static(key), Pattern::Ident(name, None)) => {
                            self.name(name, role, Source::PackageMember(namespace, key));
                        }
                        (key, value) => {
                            if let PropertyKey::Computed(key) = key {
                                self.plain.escape(key);
                                self.expr(key);
                            }
                            self.pattern(value, role);
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        match pattern {
            Pattern::Ident(name, None) => self.name(name, role, source),
            _ => {
                if let Source::Value(value) = source {
                    self.plain.escape(value);
                }
                self.pattern(pattern, role);
            }
        }
    }

    fn statements(&mut self, statements: &'a [Stmt]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Spanned(_, inner) => self.statement(inner),
            Stmt::Empty | Stmt::Break | Stmt::Continue => {}
            Stmt::TypeAlias { name, ty } => self.aliases.push((name, ty)),
            Stmt::Expr(expr) => self.expr(expr),
            Stmt::Throw(expr) => {
                // A catch binding can expose the same guest heap value.
                self.plain.escape(expr);
                self.expr(expr);
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            Stmt::Block(body) => self.scoped(|this| {
                this.declare_block(body);
                this.statements(body);
            }),
            Stmt::Var { kind, declarations } => {
                for declaration in declarations {
                    let (role, source) = match (&declaration.init, kind) {
                        (_, VarKind::Var) => (Role::Var, Source::Opaque),
                        (Some(init), _) => (Role::Declared { hoisted: false }, Source::Value(init)),
                        (None, _) => (Role::Declared { hoisted: false }, Source::Opaque),
                    };
                    self.bound(&declaration.pattern, role, source);
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
            // The head's `let` and `const` are declared before it runs.
            Stmt::For {
                init,
                test,
                update,
                body,
            } => self.scoped(|this| {
                if let Some(init) = init {
                    if let Stmt::Var { kind, declarations } = init.unspanned()
                        && *kind != VarKind::Var
                    {
                        for declaration in declarations {
                            this.declare_pattern(&declaration.pattern);
                        }
                    }
                    this.statement(init);
                }
                for expr in [test, update].into_iter().flatten() {
                    this.expr(expr);
                }
                this.statement(body);
            }),
            Stmt::ForOf {
                pattern,
                kind,
                iterable,
                body,
            } => self.scoped(|this| {
                this.bound(
                    pattern,
                    loop_role(*kind),
                    loop_source(*kind, Source::ElementOf(iterable)),
                );
                this.outside(|this| {
                    this.plain.escape(iterable);
                    this.expr(iterable);
                });
                this.statement(body);
            }),
            Stmt::ForIn {
                pattern,
                kind,
                object,
                body,
            } => self.scoped(|this| {
                // `for...in` visits property names.
                this.bound(
                    pattern,
                    loop_role(*kind),
                    loop_source(*kind, Source::Is(Ty::Text)),
                );
                this.outside(|this| {
                    this.plain.escape(object);
                    this.expr(object);
                });
                this.statement(body);
            }),
            // Every clause shares one scope.
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.expr(discriminant);
                self.scoped(|this| {
                    for case in cases {
                        this.declare_block(&case.consequent);
                    }
                    for case in cases {
                        if let Some(test) = &case.test {
                            this.outside(|this| this.expr(test));
                        }
                        this.statements(&case.consequent);
                    }
                });
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                self.scoped(|this| this.block(body));
                if let Some(Catch { binding, body }) = catch {
                    self.scoped(|this| {
                        if let Some(binding) = binding {
                            this.bound(binding, Role::Declared { hoisted: false }, Source::Opaque);
                        }
                        this.block(body);
                    });
                }
                if let Some(finally) = finally {
                    self.scoped(|this| this.block(finally));
                }
            }
        }
    }

    /// A block's statements, in the scope just opened for them.
    fn block(&mut self, statements: &'a [Stmt]) {
        self.declare_block(statements);
        self.statements(statements);
    }

    /// Declares what a block's own statements declare lexically, which
    /// exists for the whole block: `let`, `const`, functions and enums.
    fn declare_block(&mut self, statements: &'a [Stmt]) {
        for statement in statements {
            match statement.unspanned() {
                Stmt::Var { kind, declarations } if *kind != VarKind::Var => {
                    for declaration in declarations {
                        self.declare_pattern(&declaration.pattern);
                    }
                }
                Stmt::Function { name, .. } | Stmt::Enum { name, .. } => {
                    self.declare(name);
                }
                _ => {}
            }
        }
    }

    /// Declares every `var` of a function's body, or of the program, in
    /// the scope just opened for it.
    fn declare_vars(&mut self, statements: &'a [Stmt]) {
        let mut names = Vec::new();
        crate::lower::walk::var_names(statements, &mut names);
        self.declare_names(names);
    }

    /// A function's parameters, each declared once what comes before it has
    /// run, and its body, in one scope.
    fn function(&mut self, function: &'a Function) {
        self.function_depth += 1;
        self.scoped(|this| {
            for param in &function.params {
                this.pattern(param, Role::Declared { hoisted: false });
            }
            match &function.body {
                FunctionBody::Block(body) => {
                    this.declare_vars(body);
                    this.block(body);
                }
                FunctionBody::Expression(value) => this.expr(value),
            }
        });
        self.function_depth -= 1;
    }

    fn property(&mut self, property: &'a MemberProperty) {
        if let MemberProperty::Index(index) = property {
            self.plain.escape(index);
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
                self.plain.write(property);
                if let Source::Value(value) = source {
                    self.plain.escape(value);
                }
                self.expr(object);
                self.property(property);
            }
            AssignTarget::Pattern(pattern) => {
                if let Source::Value(value) = source {
                    self.plain.escape(value);
                }
                self.pattern(pattern, Role::Assigned);
            }
        }
    }

    fn expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::RegExp { .. }
            | Expr::This
            | Expr::LoneSurrogateString => {}
            Expr::Ident(name, _) => self.read(name),
            Expr::Array(elements) => {
                for element in elements {
                    match element {
                        ArrayElement::Value(value) | ArrayElement::Spread(value) => {
                            self.plain.escape(value);
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
                                self.plain.escape(key);
                                self.expr(key);
                            }
                            self.expr(value);
                        }
                        ObjectProperty::Spread(value) => {
                            self.plain.escape(value);
                            self.expr(value);
                        }
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
            Expr::Delete { object, property } => {
                self.plain.write(property);
                self.expr(object);
                self.property(property);
            }
            Expr::Member {
                object, property, ..
            } => {
                self.expr(object);
                self.property(property);
            }
            Expr::Await { value, .. } => {
                self.plain.await_value(value);
                self.expr(value);
            }
            Expr::Unary { value, .. } => {
                self.plain.escape(value);
                self.expr(value);
            }
            Expr::As { value, .. } => {
                self.expr(value);
            }
            Expr::Binary { left, right, .. } => {
                self.plain.escape(left);
                self.plain.escape(right);
                self.expr(left);
                self.expr(right);
            }
            Expr::Logical { left, right, .. } => {
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
                for (_, expression) in expressions {
                    self.plain.escape(expression);
                    self.expr(expression);
                }
            }
            // A function expression's name is a binding only its own body
            // sees.
            Expr::Function(function) => match function.name.as_deref() {
                Some(name) if !function.is_arrow => self.scoped(|this| {
                    this.name(
                        name,
                        Role::Declared { hoisted: true },
                        Source::Returns(function.return_ty.as_ref()),
                    );
                    this.function(function);
                }),
                _ => self.function(function),
            },
            Expr::Call { callee, args, .. } => {
                if let Expr::Ident(name, _) = callee.as_ref()
                    && args.iter().all(|arg| matches!(arg, CallArg::Value(_)))
                {
                    self.direct_calls.insert(name);
                }
                if let Expr::Member {
                    property: MemberProperty::Field(method),
                    ..
                } = callee.as_ref()
                    && crate::builtins::array_callback(method).is_some()
                    && args.iter().all(|arg| matches!(arg, CallArg::Value(_)))
                    && let Some(CallArg::Value(Expr::Ident(name, _))) = args.first()
                {
                    self.direct_calls.insert(name);
                }
                self.plain.call(callee, args);
                self.expr(callee);
                self.args(args);
            }
            Expr::New { args, .. } => {
                for arg in args {
                    let (CallArg::Value(value) | CallArg::Spread(value)) = arg;
                    self.plain.escape(value);
                }
                self.args(args);
            }
            Expr::OptionalChain { base, operations } => {
                self.plain.escape(base);
                self.expr(base);
                for operation in operations {
                    match operation {
                        OptionalOperation::Member { property, .. } => self.property(property),
                        OptionalOperation::Call { args, .. } => {
                            for arg in args {
                                let (CallArg::Value(value) | CallArg::Spread(value)) = arg;
                                self.plain.escape(value);
                            }
                            self.args(args);
                        }
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
        Some(VarKind::Var) => Role::Var,
        Some(_) => Role::Declared { hoisted: false },
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

/// The bindings that only ever hold numbers, from every value given to
/// them; an annotation, which is only believed, proves nothing.
struct Numbers<'s> {
    numeric: Vec<bool>,
    spellings: &'s HashMap<usize, usize>,
}

impl<'s> Numbers<'s> {
    /// Starts from every binding and drops one that is given anything else
    /// until none is: a binding given only numbers and its own sums holds
    /// numbers. A binding no value reaches, or one an earlier cell left,
    /// is not proven.
    fn proven(
        bindings: &[Entry<'_>],
        spellings: &'s HashMap<usize, usize>,
        session: &BTreeSet<Name>,
    ) -> Self {
        let mut numbers = Self {
            numeric: bindings
                .iter()
                .map(|entry| {
                    !entry.sources.is_empty()
                        && !(entry.session_scope
                            && session.contains(&Name::new(entry.name.as_str())))
                })
                .collect(),
            spellings,
        };
        loop {
            let mut changed = false;
            for (binding, entry) in bindings.iter().enumerate() {
                if numbers.numeric[binding]
                    && !entry.sources.iter().all(|source| match source {
                        Source::Is(Ty::Float) => true,
                        Source::Value(value) => numbers.is(value),
                        Source::Compound(op, value) => {
                            arithmetic(*op) || (*op == BinaryOp::Add && numbers.is(value))
                        }
                        _ => false,
                    })
                {
                    numbers.numeric[binding] = false;
                    changed = true;
                }
            }
            if !changed {
                return numbers;
            }
        }
    }

    /// Whether `expr` gives a number whenever it gives a value.
    fn is(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Number(_) | Expr::Update { .. } => true,
            Expr::Unary { op, .. } => {
                matches!(op, UnaryOp::Plus | UnaryOp::Minus | UnaryOp::BitNot)
            }
            Expr::Binary {
                left, op, right, ..
            } => arithmetic(*op) || (*op == BinaryOp::Add && self.is(left) && self.is(right)),
            Expr::Ident(name, _) => self
                .spellings
                .get(&spelling(name))
                .is_some_and(|binding| self.numeric[*binding]),
            Expr::As { value, .. }
            | Expr::Assign {
                op: AssignOp::Assign,
                value,
                ..
            } => self.is(value),
            Expr::Assign {
                op: AssignOp::Binary(op),
                ..
            } => arithmetic(*op),
            Expr::Conditional {
                consequent: left,
                alternate: right,
                ..
            }
            | Expr::Logical { left, right, .. } => self.is(left) && self.is(right),
            _ => false,
        }
    }
}

/// An operator whose result is a number whatever its operands are.
fn arithmetic(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Subtract
            | BinaryOp::Multiply
            | BinaryOp::Divide
            | BinaryOp::Remainder
            | BinaryOp::Exponent
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::ShiftLeft
            | BinaryOp::ShiftRight
            | BinaryOp::ShiftRightUnsigned
    )
}
