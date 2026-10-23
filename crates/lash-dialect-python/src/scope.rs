//! What a module body or a function body binds.
//!
//! Python decides a name's scope from the whole body: a name the body
//! assigns anywhere is local to it throughout, unless a `global` or
//! `nonlocal` statement says otherwise. These facts are gathered before
//! lowering; declaration dominance then keeps only the absent slots needed
//! before their first assignment or by an earlier capture.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{CallParam, CallShape};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, Stmt};

/// What the front end knows of an operand before the program runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ty {
    Unknown,
    None,
    Bool,
    Int,
    Float,
    Str,
    List,
    Tuple,
    Dict,
    Set,
    /// A value of a package's value type: the brand at this position of
    /// the lowerer's brands.
    Package(usize),
}

impl Ty {
    pub(crate) fn is_number(self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }

    /// The type an annotation names, as far as the front end uses it.
    pub(crate) fn of_annotation(annotation: &Expr) -> Ty {
        let name = match annotation {
            Expr::Name(name) => name.id.as_str(),
            Expr::Subscript(subscript) => match subscript.value.as_ref() {
                Expr::Name(name) => name.id.as_str(),
                _ => return Ty::Unknown,
            },
            Expr::NoneLiteral(_) => return Ty::None,
            _ => return Ty::Unknown,
        };
        match name {
            "int" => Ty::Int,
            "float" => Ty::Float,
            "bool" => Ty::Bool,
            "str" => Ty::Str,
            "list" => Ty::List,
            "tuple" => Ty::Tuple,
            "dict" => Ty::Dict,
            "set" => Ty::Set,
            _ => Ty::Unknown,
        }
    }
}

/// The call shape of a `def`'s or a lambda's parameters: their names and
/// which have defaults, in order, and whether a call makes a coroutine.
pub(crate) fn call_shape(parameters: &ast::Parameters, is_async: bool) -> CallShape {
    CallShape {
        params: parameters
            .args
            .iter()
            .map(|param| CallParam {
                name: lash_vocabulary::names::Name::new(param.parameter.name.id.as_str()),
                has_default: param.default.is_some(),
            })
            .collect(),
        is_async,
    }
}

/// The names one body binds and what is known of them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Bindings {
    /// Names the body binds itself, in name order.
    pub(crate) locals: BTreeSet<String>,
    /// Names a `global` statement of the body gives to the module.
    pub(crate) globals: BTreeSet<String>,
    /// The annotated type of a name, where every annotation agrees.
    pub(crate) types: BTreeMap<String, Ty>,
    /// The functions bound once, by a `def`.
    pub(crate) defs: BTreeMap<String, CallShape>,
    /// Those of `defs` that take the exception their caller handles, for a
    /// bare `raise` they reach (`reraisers`).
    pub(crate) reraisers: BTreeSet<String>,
    /// Those of a function's `defs` no call can leave a parameter without
    /// a default unfilled (`always_filled`). Empty for a module.
    pub(crate) filled: BTreeSet<String>,
}

#[derive(Default)]
struct Collector {
    bound: BTreeMap<String, usize>,
    globals: BTreeSet<String>,
    nonlocals: BTreeSet<String>,
    types: BTreeMap<String, Ty>,
    defs: BTreeMap<String, CallShape>,
}

impl Collector {
    fn bind(&mut self, name: &str) {
        *self.bound.entry(name.to_string()).or_default() += 1;
    }

    fn annotate(&mut self, name: &str, ty: Ty) {
        self.types
            .entry(name.to_string())
            .and_modify(|known| {
                if *known != ty {
                    *known = Ty::Unknown;
                }
            })
            .or_insert(ty);
    }

    fn target(&mut self, target: &Expr) {
        match target {
            Expr::Name(name) => self.bind(name.id.as_str()),
            Expr::Tuple(tuple) => tuple.elts.iter().for_each(|elt| self.target(elt)),
            Expr::List(list) => list.elts.iter().for_each(|elt| self.target(elt)),
            Expr::Starred(starred) => self.target(&starred.value),
            _ => {}
        }
    }

    fn body(&mut self, statements: &[Stmt]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Stmt) {
        visitor::walk_stmt(&mut AssignmentExpressions(self), statement);
        match statement {
            Stmt::FunctionDef(def) => {
                let name = def.name.id.as_str();
                self.bind(name);
                if def.decorator_list.is_empty() {
                    self.defs
                        .insert(name.to_string(), call_shape(&def.parameters, def.is_async));
                }
            }
            Stmt::Assign(assign) => assign.targets.iter().for_each(|target| self.target(target)),
            Stmt::AugAssign(assign) => self.target(&assign.target),
            Stmt::AnnAssign(assign) => {
                if let Expr::Name(name) = assign.target.as_ref() {
                    self.bind(name.id.as_str());
                    self.annotate(name.id.as_str(), Ty::of_annotation(&assign.annotation));
                }
            }
            Stmt::For(for_loop) => {
                self.target(&for_loop.target);
                self.body(&for_loop.body);
                self.body(&for_loop.orelse);
            }
            Stmt::While(while_loop) => {
                self.body(&while_loop.body);
                self.body(&while_loop.orelse);
            }
            Stmt::If(branch) => {
                self.body(&branch.body);
                for clause in &branch.elif_else_clauses {
                    self.body(&clause.body);
                }
            }
            Stmt::Try(attempt) => {
                self.body(&attempt.body);
                for ast::ExceptHandler::ExceptHandler(handler) in &attempt.handlers {
                    if let Some(name) = &handler.name {
                        self.bind(name.id.as_str());
                    }
                    self.body(&handler.body);
                }
                self.body(&attempt.orelse);
                self.body(&attempt.finalbody);
            }
            Stmt::With(with) => {
                for item in &with.items {
                    if let Some(vars) = &item.optional_vars {
                        self.target(vars);
                    }
                }
                self.body(&with.body);
            }
            Stmt::Delete(delete) => {
                for target in &delete.targets {
                    self.target(target);
                }
            }
            Stmt::Match(node) => {
                for case in &node.cases {
                    let mut names = BTreeSet::new();
                    pattern_names(&case.pattern, &mut names);
                    for name in names {
                        self.bind(&name);
                    }
                    self.body(&case.body);
                }
            }
            Stmt::Global(global) => {
                self.globals
                    .extend(global.names.iter().map(|name| name.id.as_str().to_string()));
            }
            Stmt::Nonlocal(nonlocal) => {
                self.nonlocals.extend(
                    nonlocal
                        .names
                        .iter()
                        .map(|name| name.id.as_str().to_string()),
                );
            }
            _ => {}
        }
    }
}

/// Assignment expressions bind in the containing function, including those
/// in comprehensions. Nested functions and lambda bodies own their bindings.
struct AssignmentExpressions<'s>(&'s mut Collector);
impl<'a> Visitor<'a> for AssignmentExpressions<'_> {
    fn visit_stmt(&mut self, _: &'a Stmt) {}
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Named(named) = expr {
            self.0.target(&named.target);
        }
        if let Expr::Lambda(lambda) = expr {
            if let Some(parameters) = &lambda.parameters {
                for param in &parameters.args {
                    if let Some(default) = &param.default {
                        self.visit_expr(default);
                    }
                }
            }
        } else {
            visitor::walk_expr(self, expr);
        }
    }
}

pub(crate) fn expression_bindings(expression: &Expr) -> BTreeSet<String> {
    let mut collector = Collector::default();
    AssignmentExpressions(&mut collector).visit_expr(expression);
    collector.bound.into_keys().collect()
}

pub(crate) fn pattern_names(pattern: &ast::Pattern, out: &mut BTreeSet<String>) {
    match pattern {
        ast::Pattern::MatchAs(p) => {
            if let Some(name) = &p.name {
                out.insert(name.id.to_string());
            }
            if let Some(pattern) = &p.pattern {
                pattern_names(pattern, out);
            }
        }
        ast::Pattern::MatchStar(p) => {
            if let Some(name) = &p.name {
                out.insert(name.id.to_string());
            }
        }
        ast::Pattern::MatchSequence(p) => {
            for p in &p.patterns {
                pattern_names(p, out);
            }
        }
        ast::Pattern::MatchMapping(p) => {
            if let Some(name) = &p.rest {
                out.insert(name.id.to_string());
            }
            for p in &p.patterns {
                pattern_names(p, out);
            }
        }
        ast::Pattern::MatchOr(p) => {
            for p in &p.patterns {
                pattern_names(p, out);
            }
        }
        _ => {}
    }
}

/// What `body` binds. `parameters` are bound once each and annotated as
/// written. `rebound` are the names some `global` or `nonlocal` statement
/// of the program mentions: another scope may assign them, so none is a
/// function bound once. `outer` are the reraisers of the enclosing scopes.
pub(crate) fn bindings(
    body: &[Stmt],
    parameters: Option<&ast::Parameters>,
    rebound: &BTreeSet<String>,
    outer: &BTreeSet<String>,
) -> Bindings {
    let mut collector = Collector::default();
    if let Some(parameters) = parameters {
        for param in &parameters.args {
            let name = param.parameter.name.id.as_str();
            // A parameter is bound by the call, so a `def` of the same name
            // in the body is not the only binding.
            collector.bind(name);
            if let Some(annotation) = &param.parameter.annotation {
                collector.annotate(name, Ty::of_annotation(annotation));
            }
        }
    }
    collector.body(body);
    let Collector {
        bound,
        globals,
        nonlocals,
        types,
        mut defs,
    } = collector;
    defs.retain(|name, _| {
        bound.get(name) == Some(&1)
            && !rebound.contains(name)
            && !globals.contains(name)
            && !nonlocals.contains(name)
    });
    let locals = bound
        .into_keys()
        .filter(|name| !globals.contains(name) && !nonlocals.contains(name))
        .collect();
    let reraisers = reraisers(body, outer)
        .into_iter()
        .filter(|name| defs.contains_key(name))
        .collect();
    Bindings {
        locals,
        globals,
        types,
        defs,
        reraisers,
        filled: BTreeSet::new(),
    }
}

/// The functions `body` defines that may run a bare `raise` with no
/// `except` clause of their own around it, themselves or through a call of
/// another such function, `outer` being those of the enclosing scopes.
/// Python raises again the exception the caller is handling there, so each
/// takes it from the caller.
pub(crate) fn reraisers(body: &[Stmt], outer: &BTreeSet<String>) -> BTreeSet<String> {
    let mut defs = Vec::new();
    defined(body, &mut defs);
    let mut found = BTreeSet::new();
    loop {
        let visible: BTreeSet<String> = outer.union(&found).cloned().collect();
        let before = found.len();
        for def in &defs {
            let name = def.name.id.as_str();
            if found.contains(name) {
                continue;
            }
            let inner = reraisers(&def.body, &visible);
            if reraises(&def.body, &visible.union(&inner).cloned().collect()) {
                found.insert(name.to_string());
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// The functions `body` defines in its own scope.
fn defined<'a>(body: &'a [Stmt], out: &mut Vec<&'a ast::StmtFunctionDef>) {
    for statement in body {
        match statement {
            Stmt::FunctionDef(def) => out.push(def),
            Stmt::For(for_loop) => {
                defined(&for_loop.body, out);
                defined(&for_loop.orelse, out);
            }
            Stmt::While(while_loop) => {
                defined(&while_loop.body, out);
                defined(&while_loop.orelse, out);
            }
            Stmt::If(branch) => {
                defined(&branch.body, out);
                for clause in &branch.elif_else_clauses {
                    defined(&clause.body, out);
                }
            }
            Stmt::Try(attempt) => {
                defined(&attempt.body, out);
                for ast::ExceptHandler::ExceptHandler(handler) in &attempt.handlers {
                    defined(&handler.body, out);
                }
                defined(&attempt.orelse, out);
                defined(&attempt.finalbody, out);
            }
            Stmt::With(with) => defined(&with.body, out),
            Stmt::Match(node) => {
                for case in &node.cases {
                    defined(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Whether `body` may run a bare `raise` outside every `except` clause of
/// its own, itself or by calling one of `reraisers`. Nested functions and
/// lambdas are scopes of their own.
pub(crate) fn reraises(body: &[Stmt], reraisers: &BTreeSet<String>) -> bool {
    let mut finder = Reraise {
        reraisers,
        found: false,
    };
    finder.visit_body(body);
    finder.found
}

struct Reraise<'s> {
    reraisers: &'s BTreeSet<String>,
    found: bool,
}

impl<'a> Visitor<'a> for Reraise<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::Raise(raise) if raise.exc.is_none() => self.found = true,
            // A handler's body raises again what its clause caught.
            Stmt::Try(attempt) => {
                self.visit_body(&attempt.body);
                self.visit_body(&attempt.orelse);
                self.visit_body(&attempt.finalbody);
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Lambda(_) => {}
            Expr::Call(call) => {
                if let Expr::Name(name) = call.func.as_ref()
                    && self.reraisers.contains(name.id.as_str())
                {
                    self.found = true;
                }
                visitor::walk_expr(self, expr);
            }
            _ => visitor::walk_expr(self, expr),
        }
    }
}

/// Every name a `global` or `nonlocal` statement mentions, in `body` and in
/// every function nested in it.
pub(crate) fn rebound(body: &[Stmt]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut globals = BTreeSet::new();
    let mut nonlocals = BTreeSet::new();
    fn walk(body: &[Stmt], globals: &mut BTreeSet<String>, nonlocals: &mut BTreeSet<String>) {
        for statement in body {
            match statement {
                Stmt::Global(global) => {
                    globals.extend(global.names.iter().map(|name| name.id.as_str().to_string()));
                }
                Stmt::Nonlocal(nonlocal) => {
                    nonlocals.extend(
                        nonlocal
                            .names
                            .iter()
                            .map(|name| name.id.as_str().to_string()),
                    );
                }
                Stmt::FunctionDef(def) => walk(&def.body, globals, nonlocals),
                Stmt::For(for_loop) => {
                    walk(&for_loop.body, globals, nonlocals);
                    walk(&for_loop.orelse, globals, nonlocals);
                }
                Stmt::While(while_loop) => {
                    walk(&while_loop.body, globals, nonlocals);
                    walk(&while_loop.orelse, globals, nonlocals);
                }
                Stmt::If(branch) => {
                    walk(&branch.body, globals, nonlocals);
                    for clause in &branch.elif_else_clauses {
                        walk(&clause.body, globals, nonlocals);
                    }
                }
                Stmt::Try(attempt) => {
                    walk(&attempt.body, globals, nonlocals);
                    for ast::ExceptHandler::ExceptHandler(handler) in &attempt.handlers {
                        walk(&handler.body, globals, nonlocals);
                    }
                    walk(&attempt.orelse, globals, nonlocals);
                    walk(&attempt.finalbody, globals, nonlocals);
                }
                _ => {}
            }
        }
    }
    walk(body, &mut globals, &mut nonlocals);
    (globals, nonlocals)
}

/// The functions of `defs`, bound once by a `def` of a function's body,
/// whose every call gives each parameter without a default.
///
/// A function's local is visible only to its body and the scopes nested in
/// it, so `body` holds every read of the name. A read that is not the callee
/// of a call gives the function to code the front end cannot see: stored,
/// passed, returned, or deleted, the value may be called later with any
/// arguments, so its check stays. Recursion and a capture before the `def`
/// read the same binding inside `body`, and are judged as calls like any
/// other. `defs` already leaves out a name bound twice or named by a
/// `global` or `nonlocal` anywhere in the program, so nothing else can put
/// another value in the binding. A nested scope that binds the name itself
/// is still read here: that only keeps a check that was not needed.
///
/// Arguments are values of the program, and no value of a program is
/// `absent`: a read of a slot that may be unbound goes through
/// `py.name_check`. So a call that names every required parameter, by
/// position or keyword, never reaches the missing-argument check.
pub(crate) fn always_filled(body: &[Stmt], defs: &BTreeMap<String, CallShape>) -> BTreeSet<String> {
    let mut reads = Reads {
        defs,
        escaped: BTreeSet::new(),
    };
    reads.visit_body(body);
    defs.keys()
        .filter(|name| !reads.escaped.contains(*name))
        .cloned()
        .collect()
}

struct Reads<'s> {
    defs: &'s BTreeMap<String, CallShape>,
    escaped: BTreeSet<String>,
}

impl<'a> Visitor<'a> for Reads<'_> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Call(call) => match call.func.as_ref() {
                Expr::Name(name) if self.defs.contains_key(name.id.as_str()) => {
                    let shape = &self.defs[name.id.as_str()];
                    if !fills(shape, &call.arguments) {
                        self.escaped.insert(name.id.as_str().to_string());
                    }
                    self.visit_arguments(&call.arguments);
                }
                _ => visitor::walk_expr(self, expr),
            },
            Expr::Name(name) => {
                if self.defs.contains_key(name.id.as_str()) {
                    self.escaped.insert(name.id.as_str().to_string());
                }
            }
            _ => visitor::walk_expr(self, expr),
        }
    }
}

/// Whether `arguments` give every parameter of `shape` without a default:
/// no `*` or `**` argument, and each such parameter by position or by name.
fn fills(shape: &CallShape, arguments: &ast::Arguments) -> bool {
    let positional = arguments.args.len();
    !arguments.args.iter().any(Expr::is_starred_expr)
        && arguments
            .keywords
            .iter()
            .all(|keyword| keyword.arg.is_some())
        && shape.params.iter().enumerate().all(|(index, param)| {
            param.has_default
                || index < positional
                || arguments.keywords.iter().any(|keyword| {
                    keyword
                        .arg
                        .as_ref()
                        .is_some_and(|arg| arg.id.as_str() == param.name.as_str())
                })
        })
}

/// A deliberately scope-wide proof: every definition of a candidate local
/// must produce an integer on successful execution. Remove candidates until
/// their dependencies are closed. This admits loop-carried integer recurrences
/// without speculating about the first iteration or about control-flow merges.
/// A non-integer assignment or deletion anywhere rejects the name everywhere.
/// The lowerer additionally requires a definitely bound name at each use.
pub(crate) fn integer_locals(
    body: &[Stmt],
    bindings: &Bindings,
    excluded: &BTreeSet<String>,
    range_builtin: bool,
) -> BTreeSet<String> {
    typed_locals(body, bindings, excluded, Ty::Int, range_builtin, false)
}

/// Closed list and text definitions use the same scope-wide fixpoint as
/// integers. A shadowed `str` establishes no text fact, even if its name
/// happens to match the builtin. Parameters, captures and restored cells
/// are excluded by the caller, and reads still require definite boundness.
pub(crate) fn sequence_locals(
    body: &[Stmt],
    bindings: &Bindings,
    excluded: &BTreeSet<String>,
    str_builtin: bool,
) -> BTreeMap<String, Ty> {
    [Ty::List, Ty::Str]
        .into_iter()
        .flat_map(|ty| {
            typed_locals(body, bindings, excluded, ty, false, str_builtin)
                .into_iter()
                .map(move |name| (name, ty))
        })
        .collect()
}

fn typed_locals(
    body: &[Stmt],
    bindings: &Bindings,
    excluded: &BTreeSet<String>,
    ty: Ty,
    range_builtin: bool,
    str_builtin: bool,
) -> BTreeSet<String> {
    let mut candidates: BTreeSet<String> = bindings.locals.difference(excluded).cloned().collect();
    let str_builtin = str_builtin && !bindings.locals.contains("str");
    let range_builtin = range_builtin && !bindings.locals.contains("range");
    loop {
        let mut proof = LocalProof {
            candidates: &candidates,
            rejected: BTreeSet::new(),
            range_builtin,
            str_builtin,
            ty,
        };
        proof.visit_body(body);
        let rejected = proof.rejected;
        let before = candidates.len();
        candidates.retain(|name| !rejected.contains(name));
        if candidates.len() == before {
            return candidates;
        }
    }
}

struct LocalProof<'s> {
    candidates: &'s BTreeSet<String>,
    rejected: BTreeSet<String>,
    range_builtin: bool,
    str_builtin: bool,
    ty: Ty,
}

impl LocalProof<'_> {
    fn target(&mut self, target: &Expr, produces: bool) {
        match target {
            Expr::Name(name) if !produces => {
                self.rejected.insert(name.id.as_str().to_string());
            }
            Expr::Tuple(tuple) => tuple.elts.iter().for_each(|elt| self.target(elt, false)),
            Expr::List(list) => list.elts.iter().for_each(|elt| self.target(elt, false)),
            Expr::Starred(starred) => self.target(&starred.value, false),
            _ => {}
        }
    }

    fn produces(&self, expr: &Expr) -> bool {
        if let Expr::Name(name) = expr {
            return self.candidates.contains(name.id.as_str());
        }
        if let Expr::If(branch) = expr {
            return self.produces(&branch.body) && self.produces(&branch.orelse);
        }
        match self.ty {
            Ty::Int => self.integer(expr),
            Ty::List => {
                matches!(expr, Expr::List(_) | Expr::ListComp(_) | Expr::Generator(_))
                    || matches!(expr, Expr::BinOp(op) if op.op == ast::Operator::Add
                    && self.produces(&op.left) && self.produces(&op.right))
            }
            Ty::Str => {
                matches!(expr, Expr::StringLiteral(_) | Expr::FString(_))
                    || matches!(expr, Expr::BinOp(op) if op.op == ast::Operator::Add
                    && self.produces(&op.left) && self.produces(&op.right))
                    || (self.str_builtin
                        && matches!(expr, Expr::Call(call)
                    if matches!(call.func.as_ref(), Expr::Name(name) if name.id.as_str() == "str")))
            }
            _ => unreachable!("only closed integer, list and text definitions are inferred"),
        }
    }

    fn closed(&self, op: ast::Operator) -> bool {
        match self.ty {
            Ty::Int => integer_closed(op),
            Ty::List | Ty::Str => op == ast::Operator::Add,
            _ => false,
        }
    }

    fn integer(&self, expr: &Expr) -> bool {
        match expr {
            Expr::NumberLiteral(number) => matches!(number.value, ast::Number::Int(_)),
            Expr::BinOp(op) => {
                integer_closed(op.op) && self.produces(&op.left) && self.produces(&op.right)
            }
            Expr::UnaryOp(op) => {
                matches!(op.op, ast::UnaryOp::UAdd | ast::UnaryOp::USub)
                    && self.produces(&op.operand)
            }
            // Booleans participate in Python integer arithmetic, but a bool
            // value is not a kernel integer. Keep its Python coercion helper.
            // Powers may produce floats (negative exponents); calls and
            // subscripts do not establish a local integer definition.
            _ => false,
        }
    }

    fn captured(&mut self, node: &Expr) {
        let mut names = MentionedNames(&mut self.rejected);
        names.visit_expr(node);
    }
}

fn integer_closed(op: ast::Operator) -> bool {
    matches!(
        op,
        ast::Operator::Add
            | ast::Operator::Sub
            | ast::Operator::Mult
            | ast::Operator::FloorDiv
            | ast::Operator::Mod
    )
}

/// Over-approximate captures by every name mentioned in a nested body,
/// including names shadowed there. Missing an optimization is preferable to
/// letting closure writes invalidate a proof between two reads.
struct MentionedNames<'s>(&'s mut BTreeSet<String>);

impl<'a> Visitor<'a> for MentionedNames<'_> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr {
            self.0.insert(name.id.as_str().to_string());
        }
        visitor::walk_expr(self, expr);
    }
}

impl<'a> Visitor<'a> for LocalProof<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Assign(assign) => {
                let produces = self.produces(&assign.value);
                for target in &assign.targets {
                    self.target(target, produces);
                }
            }
            Stmt::AnnAssign(assign) => {
                if let Some(value) = &assign.value {
                    self.target(&assign.target, self.produces(value));
                }
            }
            Stmt::AugAssign(assign) => self.target(
                &assign.target,
                self.closed(assign.op)
                    && self.produces(&assign.target)
                    && self.produces(&assign.value),
            ),
            Stmt::For(for_loop) => {
                let integer = self.ty == Ty::Int
                    && self.range_builtin
                    && matches!(for_loop.iter.as_ref(), Expr::Call(call)
                    if matches!(call.func.as_ref(), Expr::Name(name) if name.id.as_str() == "range"));
                self.target(&for_loop.target, integer);
            }
            Stmt::Delete(delete) => {
                for target in &delete.targets {
                    self.target(target, false);
                }
            }
            Stmt::FunctionDef(def) => {
                self.rejected.insert(def.name.id.as_str().to_string());
                let mut names = MentionedNames(&mut self.rejected);
                names.visit_stmt(stmt);
                return;
            }
            Stmt::ClassDef(class) => {
                self.rejected.insert(class.name.id.as_str().to_string());
                let mut names = MentionedNames(&mut self.rejected);
                names.visit_stmt(stmt);
                return;
            }
            Stmt::Try(attempt) => {
                for ast::ExceptHandler::ExceptHandler(handler) in &attempt.handlers {
                    if let Some(name) = &handler.name {
                        self.rejected.insert(name.id.as_str().to_string());
                    }
                }
            }
            Stmt::Match(node) => {
                for case in &node.cases {
                    pattern_names(&case.pattern, &mut self.rejected);
                }
            }
            Stmt::With(with) => {
                for item in &with.items {
                    if let Some(target) = &item.optional_vars {
                        self.target(target, false);
                    }
                }
            }
            _ => {}
        }
        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Named(named) = expr {
            self.target(&named.target, false);
            visitor::walk_expr(self, expr);
        } else if matches!(expr, Expr::Lambda(_)) {
            self.captured(expr);
        } else {
            visitor::walk_expr(self, expr);
        }
    }
}
