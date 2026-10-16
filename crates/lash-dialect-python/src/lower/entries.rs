//! A workflow module's entries: its top-level `async def`s, lowered under
//! the workflow contract every dialect shares
//! (`lash_kernel_dialect::workflow`).
//!
//! An entry is a declared function of the document that a host starts. Its
//! body is the `def`'s own closure, made with nothing of the module in
//! scope and called once, directly, with the entry's arguments: a coroutine
//! call in this dialect has run by the time it returns. Its signature is
//! what the annotations mean in Python, mapped structurally; an annotation
//! with no kernel type is refused rather than read as any value.

use std::collections::HashSet;

use lash_kernel_dialect::workflow::{Entry, ModuleError, ModuleRefusal};
use lash_kernel_doc::{
    Action, Atom, Callee, Closure, Expr, Function, Literal, MapType, Name, Param, Place, Rhs,
    Signature, Stmt, Type,
};
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
            PyStmt::FunctionDef(def) if def.is_async => Some(def.range),
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
        let active = self.takes_active(name).then(|| self.fresh("active"));
        let defaults = vec![None; def.parameters.args.len()];
        let written =
            self.function_written(Some(&def.parameters), def.is_async, def.returns.as_deref());
        let params: Vec<Name> = signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        self.in_entry = true;
        let body = self.block(|this| {
            let (closure_params, body) = this.function_body(
                Some(&def.parameters),
                Body::Suite(&def.body),
                true,
                &defaults,
                name,
                active.clone(),
            )?;
            let run = this.temp();
            this.span = span;
            let start = super::saved::start_adapter(&written, closure_params.len());
            this.push(
                Stmt::Let {
                    name: run.clone(),
                    value: Rhs::Expr(Expr::Closure(Box::new(Closure {
                        params: closure_params,
                        body: body.stmts,
                        start,
                    }))),
                },
                vec![body.notes],
            );
            if let Some(note) = this.buf.notes.last_mut() {
                note.written = Some(written);
            }
            let mut args: Vec<Atom> = params.iter().cloned().map(Atom::Variable).collect();
            if active.is_some() {
                args.push(Atom::Literal(Literal::Absent));
            }
            let returned = this.temp();
            this.emit(Stmt::Let {
                name: returned.clone(),
                value: Rhs::Action(Action::Call {
                    callee: Callee::Value(run),
                    args,
                }),
            });
            this.emit(Stmt::Return {
                value: Expr::Variable(returned),
            });
            Ok(())
        });
        self.in_entry = false;
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
        let body = body?;
        let entry = Name::new(name);
        // Only a module's lowering marks entry definitions (`declarations`).
        if let Some(module) = self.module.as_mut() {
            module
                .add(Entry {
                    name: entry.clone(),
                    span,
                    function: Function {
                        params,
                        body: body.stmts,
                        start: None,
                    },
                    signature,
                    notes: body.notes,
                })
                .map_err(ModuleError::into_diagnostic)?;
        }
        let variable = self.written(name, def.name.range)?;
        self.mark_bound(name);
        self.span = span;
        self.emit(Stmt::Assign {
            place: Place::Variable(variable),
            value: Rhs::Expr(Expr::Literal(Literal::Function(entry))),
        });
        Ok(())
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
        annotation_type(annotation).ok_or_else(|| {
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

/// What a Python annotation means as a kernel type: `int`, `float`, `bool`,
/// `str`, `bytes` and `None`; `list`, `set` and `dict`, bare or with their
/// item types; `tuple` with its member types; `Any` and `object`; and
/// unions written with `|`, `Optional` or `Union`. `None` when it is none
/// of these.
fn annotation_type(annotation: &PyExpr) -> Option<Type> {
    match annotation {
        PyExpr::NoneLiteral(_) => Some(Type::Null),
        PyExpr::Name(name) => match name.id.as_str() {
            "int" => Some(Type::Int),
            "float" => Some(Type::Float),
            "bool" => Some(Type::Bool),
            "str" => Some(Type::Text),
            "bytes" => Some(Type::Bytes),
            "None" => Some(Type::Null),
            "Any" | "object" => Some(Type::Any),
            "list" => Some(Type::List(Box::new(Type::Any))),
            "set" => Some(Type::Set(Box::new(Type::Any))),
            "dict" => Some(map(Type::Any, Type::Any)),
            _ => None,
        },
        PyExpr::Subscript(subscript) => {
            let PyExpr::Name(name) = subscript.value.as_ref() else {
                return None;
            };
            let members: Vec<&PyExpr> = match subscript.slice.as_ref() {
                PyExpr::Tuple(tuple) => tuple.elts.iter().collect(),
                single => vec![single],
            };
            let types = members
                .into_iter()
                .map(annotation_type)
                .collect::<Option<Vec<_>>>()?;
            match (name.id.as_str(), types.as_slice()) {
                ("list", [item]) => Some(Type::List(Box::new(item.clone()))),
                ("set", [item]) => Some(Type::Set(Box::new(item.clone()))),
                ("dict", [key, value]) => Some(map(key.clone(), value.clone())),
                ("tuple", members) if !members.is_empty() => Some(Type::Tuple(members.to_vec())),
                ("Optional", [item]) => Some(union(vec![item.clone(), Type::Null])),
                ("Union", members) if !members.is_empty() => Some(union(members.to_vec())),
                _ => None,
            }
        }
        PyExpr::BinOp(operation) if operation.op == ast::Operator::BitOr => Some(union(vec![
            annotation_type(&operation.left)?,
            annotation_type(&operation.right)?,
        ])),
        _ => None,
    }
}

fn map(key: Type, value: Type) -> Type {
    Type::Map(Box::new(MapType { key, value }))
}

/// One type of `members`: none nested, none twice, and any value if one
/// member is.
fn union(members: Vec<Type>) -> Type {
    let mut types: Vec<Type> = Vec::new();
    for member in members {
        let flat = match member {
            Type::Any => return Type::Any,
            Type::Union(inner) => inner,
            other => vec![other],
        };
        for member in flat {
            if !types.contains(&member) {
                types.push(member);
            }
        }
    }
    match types.len() {
        1 => types.remove(0),
        _ => Type::Union(types),
    }
}
