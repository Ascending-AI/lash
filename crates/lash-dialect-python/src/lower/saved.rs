//! Python's saved call shape and executable start adapter.

use lash_kernel_dialect::{CallParam, CallShape, SavedFunction, Written};
use lash_kernel_doc::{
    FunctionRepresentation, Name, Param, Signature, SourceLanguage, StartAdapter, StartConvention,
    Type,
};
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::Ranged;

use super::Lowerer;
use crate::scope::Ty;

/// A later call needs the parameter names, default slots and coroutine
/// rule: the call shape Python wrote of the function, which the kernel never
/// interprets.
pub(super) fn call_signature(saved: &SavedFunction) -> Option<CallShape> {
    let written = saved.written.as_ref()?;
    if written.language != SourceLanguage::Python {
        return None;
    }
    written.call.clone()
}

impl Lowerer<'_> {
    pub(super) fn function_written(
        &self,
        parameters: Option<&ast::Parameters>,
        is_async: bool,
        returns: Option<&PyExpr>,
    ) -> Written {
        let params = parameters
            .map(|parameters| parameters.args.as_slice())
            .unwrap_or_default();
        let call = CallShape {
            params: params
                .iter()
                .map(|param| CallParam {
                    name: Name::new(param.parameter.name.id.as_str()),
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
        Written {
            language: SourceLanguage::Python,
            signature: text,
            call: Some(call),
        }
    }
}

/// How a source function is started when it crosses to a tool: called
/// directly with the start's arguments (`K-CLO-004`). None for a closure
/// that takes more parameters than its source signature names.
pub(super) fn start_adapter(
    parameters: Option<&ast::Parameters>,
    returns: Option<&PyExpr>,
    callable_params: usize,
) -> Option<StartAdapter> {
    let params = parameters
        .map(|parameters| parameters.args.as_slice())
        .unwrap_or_default();
    let signature = Signature {
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
    (signature.params.len() == callable_params).then_some(StartAdapter {
        signature,
        representation: FunctionRepresentation::Bare,
        convention: StartConvention::Direct,
    })
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
