//! What a module body or a function body binds.
//!
//! Python decides a name's scope from the whole body: a name the body
//! assigns anywhere is local to it throughout, unless a `global` or
//! `nonlocal` statement says otherwise. The lowerer declares every local at
//! the top of the kernel block it lowers the body to, so these facts are
//! gathered before any statement is lowered.

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
                name: lash_kernel_doc::Name::new(param.parameter.name.id.as_str()),
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
        match statement {
            Stmt::FunctionDef(def) => {
                let name = def.name.id.as_str();
                self.bind(name);
                self.defs
                    .insert(name.to_string(), call_shape(&def.parameters, def.is_async));
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

/// Whether `body` holds a `break` of the loop it is the body of.
pub(crate) fn breaks(body: &[Stmt]) -> bool {
    body.iter().any(|statement| match statement {
        Stmt::Break(_) => true,
        Stmt::If(branch) => {
            breaks(&branch.body)
                || branch
                    .elif_else_clauses
                    .iter()
                    .any(|clause| breaks(&clause.body))
        }
        Stmt::Try(attempt) => {
            breaks(&attempt.body)
                || attempt
                    .handlers
                    .iter()
                    .any(|ast::ExceptHandler::ExceptHandler(handler)| breaks(&handler.body))
                || breaks(&attempt.orelse)
                || breaks(&attempt.finalbody)
        }
        // A loop's `else` belongs to the loop around it.
        Stmt::For(for_loop) => breaks(&for_loop.orelse),
        Stmt::While(while_loop) => breaks(&while_loop.orelse),
        _ => false,
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
    let mut candidates: BTreeSet<String> = bindings.locals.difference(excluded).cloned().collect();
    let range_builtin = range_builtin && !bindings.locals.contains("range");
    loop {
        let mut proof = IntegerProof {
            candidates: &candidates,
            rejected: BTreeSet::new(),
            range_builtin,
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

struct IntegerProof<'s> {
    candidates: &'s BTreeSet<String>,
    rejected: BTreeSet<String>,
    range_builtin: bool,
}

impl IntegerProof<'_> {
    fn target(&mut self, target: &Expr, integer: bool) {
        match target {
            Expr::Name(name) if !integer => {
                self.rejected.insert(name.id.as_str().to_string());
            }
            Expr::Tuple(tuple) => tuple.elts.iter().for_each(|elt| self.target(elt, false)),
            Expr::List(list) => list.elts.iter().for_each(|elt| self.target(elt, false)),
            Expr::Starred(starred) => self.target(&starred.value, false),
            _ => {}
        }
    }

    fn integer(&self, expr: &Expr) -> bool {
        match expr {
            Expr::NumberLiteral(number) => matches!(number.value, ast::Number::Int(_)),
            Expr::Name(name) => self.candidates.contains(name.id.as_str()),
            Expr::BinOp(op) => {
                integer_closed(op.op) && self.integer(&op.left) && self.integer(&op.right)
            }
            Expr::UnaryOp(op) => {
                matches!(op.op, ast::UnaryOp::UAdd | ast::UnaryOp::USub)
                    && self.integer(&op.operand)
            }
            Expr::If(branch) => self.integer(&branch.body) && self.integer(&branch.orelse),
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

impl<'a> Visitor<'a> for IntegerProof<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Assign(assign) => {
                let integer = self.integer(&assign.value);
                for target in &assign.targets {
                    self.target(target, integer);
                }
            }
            Stmt::AnnAssign(assign) => {
                if let Some(value) = &assign.value {
                    self.target(&assign.target, self.integer(value));
                }
            }
            Stmt::AugAssign(assign) => self.target(
                &assign.target,
                integer_closed(assign.op)
                    && self.integer(&assign.target)
                    && self.integer(&assign.value),
            ),
            Stmt::For(for_loop) => {
                let integer = self.range_builtin
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
        if matches!(expr, Expr::Lambda(_)) {
            self.captured(expr);
        } else {
            visitor::walk_expr(self, expr);
        }
    }
}
