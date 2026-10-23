//! A workflow module's entries: its top-level `async def`s, lowered under
//! the workflow contract every dialect shares
//! (`lash_kernel_dialect::workflow`).
//!
//! An entry's declared function is the author's body. The machine binds its
//! source parameters under `Direct`; there is no closure or call wrapper.
//! An entry that makes tasks declares `tasks.cancel_all` as its run-root
//! cleanup instead (`PyDirect`), which the machine calls once the body has
//! left, as `asyncio.run` does.
//! Annotation and capture refusals are shared with every workflow dialect.

use std::collections::HashSet;

use lash_kernel_dialect::workflow::{Entry, ModuleError, ModuleRefusal};
use lash_kernel_doc::{AssignOrder, Expr, Literal, Place, Rhs, Stmt};

use lash_vocabulary::{Name, Signature};
use ruff_python_ast::{self as ast, Stmt as PyStmt};
use ruff_text_size::{Ranged, TextRange};

use super::statements::Body;
use super::{Buf, Lowerer, Lowering, Scope, ScopeKind};
use crate::diagnostics;
use ruff_python_ast::visitor::{self, Visitor};
use std::collections::{BTreeMap, BTreeSet};

/// The entry declarations of a module's top level: each `async def` that
/// is a statement of the module itself, not of a block.
pub(super) fn declarations(body: &[PyStmt]) -> HashSet<TextRange> {
    body.iter()
        .filter_map(|statement| match statement {
            PyStmt::FunctionDef(def) => Some(def.range),
            _ => None,
        })
        .collect()
}

impl Lowerer<'_> {
    /// Lowers the entry `def` declares into the module, and binds its name
    /// in `main` to the reference to it.
    pub(super) fn module_entry(&mut self, def: &ast::StmtFunctionDef) -> Lowering<()> {
        let name = def.name.id.as_str();
        self.span_of(def);
        let span = self.span;
        let signature = self.entry_signature(def)?;
        let names: Vec<Name> = signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        let captions = self.signature_captions(def, &names);
        self.declared_captions.extend(captions);
        self.in_entry = true;
        let creates_tasks = std::mem::take(&mut self.creates_tasks);
        let declarations =
            materialized_declarations(self.module_statements, def, &self.scopes[0].bindings.locals);
        let entry_defs = std::mem::take(&mut self.entry_defs);
        let bindings = crate::scope::bindings(&declarations, None, &self.rebound, &BTreeSet::new());
        let mut environment = BTreeMap::new();
        for name in &bindings.locals {
            environment.insert(name.clone(), self.fresh("module"));
        }
        let previous_environment = self
            .module_environment
            .replace((self.scopes.len(), environment));
        self.scopes.push(Scope {
            names: BTreeMap::new(),
            kind: ScopeKind::Module,
            bindings,
            integer_locals: BTreeSet::new(),
            sequence_locals: BTreeMap::new(),
            bound: vec![BTreeSet::new()],
            handlers: Vec::new(),
            loops: 0,
        });
        let body: Lowering<(Vec<Name>, Buf)> = (|| {
            let mut prefix = self.block(|this| {
                let span = this.span.take();
                let Some((_, environment)) = &this.module_environment else {
                    unreachable!("entry environment is active");
                };
                let names: Vec<_> = environment.values().cloned().collect();
                for name in names {
                    this.emit(Stmt::Let {
                        name: name.clone(),
                        value: Rhs::Expr(Expr::Literal(Literal::Absent)),
                    });
                }
                this.span = span;
                this.statements(&declarations)
            })?;
            // A workflow has no module execution phase. Its start adapter
            // initializes omitted arguments in definition order, in the
            // materialized module environment (not the parameter scope).
            let defaults = self.block(|this| {
                for param in &def.parameters.args {
                    if let Some(default) = &param.default {
                        let name = Name::new(param.parameter.name.id.as_str());
                        let missing = this.native(
                            "same",
                            vec![Expr::Variable(name.clone()), Expr::Literal(Literal::Absent)],
                        )?;
                        let fill = this.block(|this| {
                            let value = this.expr(default)?;
                            this.emit(Stmt::Assign {
                                place: Place::Variable(name.clone()),
                                value: Rhs::Expr(value.expr),
                                order: AssignOrder::ValueFirst,
                            });
                            Ok(())
                        })?;
                        this.emit_if(missing, fill, Buf::default());
                    }
                }
                Ok(())
            })?;
            prefix.stmts.extend(defaults.stmts);
            prefix.notes.extend(defaults.notes);
            let (params, body) = self.function_body(
                Some(&def.parameters),
                Body::Suite(&def.body),
                def.is_async,
                &[],
                Some(name),
                None,
            )?;
            prefix.stmts.extend(body.stmts);
            prefix.notes.extend(body.notes);
            Ok((params, prefix))
        })();
        self.scopes.pop();
        self.module_environment = previous_environment;
        self.entry_defs = entry_defs;
        self.in_entry = false;
        let convention = match self.creates_tasks {
            true => self
                .function_target("tasks.cancel_all")
                .map(|cleanup| lash_kernel_doc::StartConvention::PyDirect { cleanup }),
            false => Ok(lash_kernel_doc::StartConvention::Direct),
        };
        self.creates_tasks = creates_tasks;
        if let Some((captured, at)) = self.captured.borrow_mut().take() {
            return Err(ModuleRefusal::DynamicModuleInitialization
                .at(
                    format!("module binding `{captured}` needs dynamic initialization"),
                    at,
                )
                .into_diagnostic());
        }
        let (params, body) = body?;
        let convention = convention?;
        let entry = Name::new(name);
        let definition = lash_kernel_dialect::authoring::local_function(
            params,
            body.stmts,
            &self.imports.as_map(),
            self.library,
            &self.local_functions,
        );
        let identity = definition.identity();
        if let Some(module) = self.module.as_mut() {
            module
                .add(Entry {
                    name: entry.clone(),
                    span,
                    function: definition,
                    signature,
                    convention,
                    asynchronous: def.is_async,
                    rest: def.parameters.vararg.is_some(),
                    notes: body.notes,
                })
                .map_err(ModuleError::into_diagnostic)?;
        }
        let variable = self.written(name, def.name.range)?;
        self.mark_bound(name);
        self.span = span;
        self.emit(Stmt::Assign {
            place: Place::Variable(variable),
            value: Rhs::Expr(Expr::Literal(Literal::Function(identity))),
            order: AssignOrder::ValueFirst,
        });
        Ok(())
    }

    /// A cell's task cleanup belongs to its root, including tasks made by
    /// callees. An entry declares its cleanup in its start convention.
    pub(super) fn task_cleanup(&mut self, body: super::Buf) -> Lowering<super::Buf> {
        if !self.creates_tasks {
            return Ok(body);
        }
        let span = self.span;
        self.span = None;
        let result = self.block(|this| {
            let finally = this.block(|this| this.invoke_do("tasks.cancel_all", &[]))?;
            this.emit_try(body, None, Some(finally));
            Ok(())
        });
        self.span = span;
        result
    }

    /// The signature a host starts an entry under: each parameter a plain
    /// name with no default, of the type its annotation means.
    fn entry_signature(&self, def: &ast::StmtFunctionDef) -> Lowering<Signature> {
        let parameters = &def.parameters;
        let unsupported = parameters
            .posonlyargs
            .first()
            .map(Ranged::range)
            .or_else(|| parameters.kwonlyargs.first().map(Ranged::range))
            .or_else(|| parameters.vararg.as_ref().map(|param| param.range))
            .or_else(|| parameters.kwarg.as_ref().map(|param| param.range));
        if let Some(range) = unsupported {
            return Err(diagnostics::with_repair(
                diagnostics::Code::StarUnsupported,
                "an entry's parameters are plain names: start arguments are keyed by them",
                range,
                "name each parameter; pass a list or a dict for a variable number of values",
            ));
        }
        let signature = self.declared_signature(Some(parameters), def.returns.as_deref());
        self.signature_origin(
            Some(parameters),
            def.returns.as_deref(),
            Some(def.name.id.as_str()),
        )
        .require(
            &signature,
            lash_vocabulary::FunctionLocation {
                name: Some(def.name.id.to_string()),
                site: None,
            },
        )
        .map_err(|refusal| match refusal {
            lash_vocabulary::EntrySignatureRefusal::Required(required) => {
                lash_kernel_dialect::Diagnostic::from(*required)
            }
            lash_vocabulary::EntrySignatureRefusal::Annotation(refused) => {
                lash_kernel_dialect::Diagnostic {
                    code: "ENTRY_ANNOTATION_REFUSED".into(),
                    span: refused.span,
                    fact: Some(Box::new(
                        lash_kernel_dialect::DiagnosticFact::EntryAnnotationRefused(*refused),
                    )),
                    ..Default::default()
                }
            }
        })?;
        Ok(signature)
    }
}

/// Collect dependencies by source identity, retaining declaration order.
/// Over-approximating lexical reads is safe: only inert initializers enter
/// this environment, and parameter scopes still shadow it.
fn materialized_declarations(
    body: &[PyStmt],
    entry: &ast::StmtFunctionDef,
    module_bindings: &BTreeSet<String>,
) -> Vec<PyStmt> {
    struct Reads(BTreeSet<String>);
    impl<'a> Visitor<'a> for Reads {
        fn visit_expr(&mut self, expr: &'a ast::Expr) {
            if let ast::Expr::Name(name) = expr {
                self.0.insert(name.id.to_string());
            }
            visitor::walk_expr(self, expr);
        }
    }
    let mut reads = Reads(BTreeSet::new());
    for statement in &entry.body {
        reads.visit_stmt(statement);
    }
    for param in &entry.parameters.args {
        if let Some(default) = &param.default {
            reads.visit_expr(default);
        }
    }
    let mut known = BTreeSet::new();
    let mut candidates = Vec::new();
    let mut dynamic = false;
    for statement in body {
        let names: Vec<String> = match statement {
            PyStmt::FunctionDef(def)
                if def.decorator_list.is_empty()
                    && def.parameters.args.iter().all(|param| {
                        param
                            .default
                            .as_ref()
                            .is_none_or(|value| static_value(value, &known))
                    }) =>
            {
                vec![def.name.id.to_string()]
            }
            PyStmt::Assign(assign) if static_value(&assign.value, &known) => assign
                .targets
                .iter()
                .filter_map(|target| {
                    if let ast::Expr::Name(name) = target {
                        Some(name.id.to_string())
                    } else {
                        None
                    }
                })
                .collect(),
            PyStmt::AnnAssign(assign)
                if assign
                    .value
                    .as_ref()
                    .is_some_and(|value| static_value(value, &known)) =>
            {
                match assign.target.as_ref() {
                    ast::Expr::Name(name) => vec![name.id.to_string()],
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        if names.is_empty() {
            dynamic |= !matches!(
                statement,
                PyStmt::FunctionDef(_)
                    | PyStmt::ClassDef(_)
                    | PyStmt::Import(_)
                    | PyStmt::ImportFrom(_)
                    | PyStmt::Pass(_)
                    | PyStmt::TypeAlias(_)
            ) && !matches!(statement, PyStmt::Expr(expr) if expr.value.is_string_literal_expr())
                && super::typed_dicts::legacy_alias(statement, module_bindings).is_none();
            continue;
        }
        known.extend(names.iter().cloned());
        candidates.push((names, statement));
    }
    if dynamic {
        candidates.retain(|(_, statement)| matches!(statement, PyStmt::FunctionDef(_)));
    }
    let mut selected = BTreeSet::new();
    loop {
        let before = selected.len();
        for (index, (names, statement)) in candidates.iter().enumerate() {
            if names.iter().any(|name| reads.0.contains(name)) && selected.insert(index) {
                reads.visit_stmt(statement);
            }
        }
        if before == selected.len() {
            break;
        }
    }
    candidates
        .into_iter()
        .enumerate()
        .filter(|(index, _)| selected.contains(index))
        .map(|(_, (_, statement))| statement.clone())
        .collect()
}

fn static_value(value: &ast::Expr, known: &BTreeSet<String>) -> bool {
    match value {
        ast::Expr::NoneLiteral(_)
        | ast::Expr::BooleanLiteral(_)
        | ast::Expr::NumberLiteral(_)
        | ast::Expr::StringLiteral(_)
        | ast::Expr::BytesLiteral(_) => true,
        ast::Expr::Name(name) => known.contains(name.id.as_str()),
        ast::Expr::UnaryOp(op) => static_value(&op.operand, known),
        ast::Expr::BinOp(op) => static_value(&op.left, known) && static_value(&op.right, known),
        ast::Expr::Tuple(tuple) => tuple.elts.iter().all(|value| static_value(value, known)),
        ast::Expr::List(list) => list.elts.iter().all(|value| static_value(value, known)),
        ast::Expr::Dict(dict) => dict.items.iter().all(|item| {
            item.key
                .as_ref()
                .is_some_and(|key| static_value(key, known))
                && static_value(&item.value, known)
        }),
        _ => false,
    }
}
