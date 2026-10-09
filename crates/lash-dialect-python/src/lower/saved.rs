//! Python's saved call signature and the entry a saved function starts as.

use lash_kernel_dialect::SavedFunction;
use lash_kernel_doc::{
    Action, Atom, Callee, Expr, Function, Literal, MapEntry, Name, Param, Rhs, Signature, Stmt,
    Type,
};
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::Ranged;

use super::{Lowerer, Lowering, Operand};
use crate::scope::{self, Ty};

/// A later call needs the parameter names, default slots and coroutine rule.
/// These are dialect metadata; the kernel never interprets them.
pub(super) fn call_signature(saved: &SavedFunction) -> Option<scope::Signature> {
    let written = saved.written.as_ref()?;
    if written.dialect != crate::DIALECT {
        return None;
    }
    serde_json::from_value(written.metadata.clone()?).ok()
}

impl Lowerer<'_> {
    pub(super) fn function_written(
        &self,
        parameters: Option<&ast::Parameters>,
        is_async: bool,
        returns: Option<&PyExpr>,
    ) -> serde_json::Value {
        let params = parameters
            .map(|parameters| parameters.args.as_slice())
            .unwrap_or_default();
        let signature = scope::Signature {
            params: params
                .iter()
                .map(|param| scope::Param {
                    name: param.parameter.name.id.to_string(),
                    has_default: param.default.is_some(),
                })
                .collect(),
            is_async,
        };
        let spelling = params
            .iter()
            .map(|param| self.text(param.range()).to_owned())
            .collect::<Vec<_>>()
            .join(", ");
        let mut text = format!("{}({spelling})", if is_async { "async " } else { "" });
        if let Some(returns) = returns {
            text.push_str(" -> ");
            text.push_str(self.text(returns.range()));
        }
        let start = Signature {
            params: params
                .iter()
                .map(|param| Param {
                    name: Name::new(param.parameter.name.id.as_str()),
                    ty: parameter_type(param.parameter.annotation.as_deref()),
                    optional: param.default.is_some(),
                })
                .collect(),
            result: parameter_type(returns),
        };
        serde_json::json!({ "signature": text, "metadata": signature, "start": start })
    }

    /// Evaluate a start's input in Python order, replacing only a saved
    /// function named in its definition slot with a process entry reference.
    pub(super) fn process_input(&mut self, expr: &PyExpr) -> Lowering<Operand> {
        let PyExpr::Dict(dict) = expr else {
            return self.expr(expr);
        };
        let mut entries = Vec::with_capacity(dict.items.len());
        for item in &dict.items {
            let Some(key) = &item.key else {
                return self.expr(expr);
            };
            let definition =
                matches!(key, PyExpr::StringLiteral(text) if text.value.to_str() == "definition");
            let key = self.expr(key)?;
            let key = self.pin(key);
            let saved = match &item.value {
                PyExpr::Name(name) if definition => {
                    let name = Name::new(name.id.as_str());
                    self.saved
                        .get(&name)
                        .filter(|saved| call_signature(saved).is_some())
                        .and_then(|saved| saved.written.as_ref())
                        .and_then(|written| written.start.clone())
                        .filter(|_| {
                            self.saved_startable.contains(&name)
                                && self
                                    .variable(name.as_str())
                                    .is_some_and(|found| found.scope == Some(0))
                        })
                        .map(|signature| (name, signature))
                }
                _ => None,
            };
            let value = match saved {
                Some((name, signature)) => self.saved_process(&name, signature),
                None => self.expr(&item.value)?,
            };
            let value = self.pin(value);
            entries.push(MapEntry {
                key: key.expr,
                value: value.expr,
            });
        }
        Ok(self.let_rhs(Rhs::Expr(Expr::Map(entries)), Ty::Dict))
    }

    fn saved_process(&mut self, name: &Name, signature: Signature) -> Operand {
        let entry = self.fresh(&format!("{name}_process"));
        let returned = self.temp();
        self.saved_used.insert(name.clone());
        self.declared.insert(
            entry.clone(),
            Function {
                params: signature
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect(),
                body: vec![
                    Stmt::Let {
                        name: returned.clone(),
                        value: Rhs::Action(Action::Call {
                            callee: Callee::Declared(name.clone()),
                            args: signature
                                .params
                                .iter()
                                .map(|param| Atom::Variable(param.name.clone()))
                                .collect(),
                        }),
                    },
                    Stmt::Return {
                        value: Expr::Variable(returned),
                    },
                ],
            },
        );
        self.entries.insert(entry.clone(), signature);
        Operand::literal(Literal::Function(entry), Ty::Unknown)
    }
}

/// Python's trusted scalar/container annotations as the durable entry types.
fn parameter_type(annotation: Option<&PyExpr>) -> Type {
    match annotation.map(Ty::of_annotation).unwrap_or(Ty::Unknown) {
        Ty::None => Type::Null,
        Ty::Bool => Type::Bool,
        Ty::Int => Type::Int,
        Ty::Float => Type::Float,
        Ty::Str => Type::Text,
        // Python's remaining annotations carry no item shape in this lowerer.
        _ => Type::Any,
    }
}
