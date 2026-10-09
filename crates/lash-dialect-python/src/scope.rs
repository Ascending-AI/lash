//! What a module body or a function body binds.
//!
//! Python decides a name's scope from the whole body: a name the body
//! assigns anywhere is local to it throughout, unless a `global` or
//! `nonlocal` statement says otherwise. The lowerer declares every local at
//! the top of the kernel block it lowers the body to, so these facts are
//! gathered before any statement is lowered.

use std::collections::{BTreeMap, BTreeSet};

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

/// One parameter of a function the front end can see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Param {
    pub(crate) name: String,
    pub(crate) has_default: bool,
}

/// A `def` whose name nothing else binds, so a call of the name is a call
/// of this function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signature {
    pub(crate) params: Vec<Param>,
    pub(crate) is_async: bool,
}

impl Signature {
    pub(crate) fn of(parameters: &ast::Parameters, is_async: bool) -> Self {
        Self {
            params: parameters
                .args
                .iter()
                .map(|param| Param {
                    name: param.parameter.name.id.as_str().to_string(),
                    has_default: param.default.is_some(),
                })
                .collect(),
            is_async,
        }
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
    pub(crate) defs: BTreeMap<String, Signature>,
}

#[derive(Default)]
struct Collector {
    bound: BTreeMap<String, usize>,
    globals: BTreeSet<String>,
    nonlocals: BTreeSet<String>,
    types: BTreeMap<String, Ty>,
    defs: BTreeMap<String, Signature>,
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
                self.defs.insert(
                    name.to_string(),
                    Signature::of(&def.parameters, def.is_async),
                );
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
/// function bound once.
pub(crate) fn bindings(
    body: &[Stmt],
    parameters: Option<&ast::Parameters>,
    rebound: &BTreeSet<String>,
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
    Bindings {
        locals,
        globals,
        types,
        defs,
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
