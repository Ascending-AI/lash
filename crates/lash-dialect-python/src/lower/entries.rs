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
use lash_vocabulary::{Name, Param, Signature, Type};
use ruff_python_ast::{self as ast, Expr as PyExpr, Stmt as PyStmt};
use ruff_text_size::{Ranged, TextRange};

use super::statements::Body;
use super::{Lowerer, Lowering};
use crate::diagnostics;

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
        let signature = self.entry_signature(&def.parameters, def.returns.as_deref())?;
        let names: Vec<Name> = signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        let captions = self.signature_captions(def, &names);
        self.declared_captions.extend(captions);
        self.in_entry = true;
        let creates_tasks = std::mem::take(&mut self.creates_tasks);
        let body = self.function_body(
            Some(&def.parameters),
            Body::Suite(&def.body),
            true,
            &[],
            Some(name),
            None,
        );
        self.in_entry = false;
        let convention = match self.creates_tasks {
            true => self
                .function_target("tasks.cancel_all")
                .map(|cleanup| lash_kernel_doc::StartConvention::PyDirect { cleanup }),
            false => Ok(lash_kernel_doc::StartConvention::Direct),
        };
        self.creates_tasks = creates_tasks;
        if let Some((captured, at)) = self.captured.borrow_mut().take() {
            return Err(ModuleRefusal::CapturedModuleBinding
                .at(
                    format!(
                        "an entry runs apart from the module and cannot read the module's `{captured}`"
                    ),
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
    fn entry_signature(
        &self,
        parameters: &ast::Parameters,
        returns: Option<&PyExpr>,
    ) -> Lowering<Signature> {
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
        let mut params = Vec::with_capacity(parameters.args.len());
        for param in &parameters.args {
            if let Some(default) = &param.default {
                return Err(ModuleRefusal::EntryDefault
                    .at(
                        format!(
                            "the entry parameter `{}` has a default: a start passes every argument",
                            param.parameter.name.id
                        ),
                        Some(diagnostics::span(default.range())),
                    )
                    .into_diagnostic());
            }
            params.push(Param {
                name: Name::new(param.parameter.name.id.as_str()),
                ty: self.entry_type(param.parameter.annotation.as_deref())?,
                optional: false,
            });
        }
        Ok(Signature {
            params,
            result: self.entry_type(returns)?,
        })
    }

    /// The kernel type an entry annotation means; none is any value.
    fn entry_type(&self, annotation: Option<&PyExpr>) -> Lowering<Type> {
        let Some(annotation) = annotation else {
            return Ok(Type::Any);
        };
        super::typed_dicts::annotation_type(annotation, &self.typed_dicts).ok_or_else(|| {
            ModuleRefusal::UnmappableEntryAnnotation
                .at(
                    format!(
                        "the annotation `{}` has no kernel type an entry is started or ended with",
                        self.text(annotation.range())
                    ),
                    Some(diagnostics::span(annotation.range())),
                )
                .into_diagnostic()
        })
    }
}
