//! Python's saved call shape and executable start adapter.

use lash_kernel_dialect::{CallParam, CallShape, SavedFunction, Written};
use lash_kernel_doc::{
    CallablePresentation, FunctionRepresentation, SourceLanguage, StartConvention,
};

use lash_vocabulary::{AnnotationOrigin, Name, Param, Signature, SignatureOrigin, Type};
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::Ranged;

use super::Lowerer;

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
        name: Option<&str>,
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
            implicit_text_conversions: Vec::new(),
            captions: Vec::new(),
            language: SourceLanguage::Python,
            signature: text,
            call: Some(call),
            signature_origin: Some(self.signature_origin(parameters, returns, name)),
        }
    }
}

/// How a source function is started when it crosses to a tool: called
/// directly with the start's arguments (`K-CLO-004`). None for a closure
/// that takes more parameters than its source signature names.
impl Lowerer<'_> {
    pub(super) fn callable_presentation(
        &self,
        parameters: Option<&ast::Parameters>,
        returns: Option<&PyExpr>,
        callable_params: usize,
    ) -> Option<CallablePresentation> {
        let signature = self.declared_signature(parameters, returns);
        (signature.params.len() == callable_params).then_some(CallablePresentation {
            signature,
            representation: FunctionRepresentation::Bare,
            convention: StartConvention::Direct,
        })
    }

    pub(super) fn declared_signature(
        &self,
        parameters: Option<&ast::Parameters>,
        returns: Option<&PyExpr>,
    ) -> Signature {
        let params = parameters
            .map(|parameters| parameters.args.as_slice())
            .unwrap_or_default();
        Signature {
            params: params
                .iter()
                .map(|param| Param {
                    name: Name::new(param.parameter.name.id.as_str()),
                    ty: param
                        .parameter
                        .annotation
                        .as_deref()
                        .and_then(|annotation| {
                            super::typed_dicts::annotation_type(
                                annotation,
                                &self.typed_dicts,
                                true,
                                self.max_nesting,
                                self.max_annotation_nodes,
                            )
                            .ok()
                        })
                        .unwrap_or(Type::Any),
                    optional: param.default.is_some(),
                })
                .collect(),
            result: self.annotation_type(returns),
        }
    }

    pub(super) fn resolve_annotation(
        &self,
        annotation: &PyExpr,
    ) -> Result<Type, lash_vocabulary::AnnotationResolutionReason> {
        super::typed_dicts::annotation_type(
            annotation,
            &self.typed_dicts,
            false,
            self.max_nesting,
            self.max_annotation_nodes,
        )
    }

    pub(super) fn signature_origin(
        &self,
        parameters: Option<&ast::Parameters>,
        returns: Option<&PyExpr>,
        name: Option<&str>,
    ) -> SignatureOrigin {
        let origin = |annotation: Option<&PyExpr>, missing| AnnotationOrigin {
            span: annotation
                .map(|annotation| crate::diagnostics::span(annotation.range()))
                .or(missing),
            refusal: annotation.and_then(|annotation| self.resolve_annotation(annotation).err()),
        };
        SignatureOrigin {
            name: name.map(str::to_owned),
            params: parameters
                .map(|parameters| {
                    parameters
                        .args
                        .iter()
                        .map(|param| {
                            origin(
                                param.parameter.annotation.as_deref(),
                                Some(crate::diagnostics::span(param.parameter.name.range)),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            result: origin(
                returns,
                parameters.map(|parameters| {
                    crate::diagnostics::span(ruff_text_size::TextRange::empty(parameters.end()))
                }),
            ),
        }
    }

    // Ordinary calls stay free; promotion consults the retained origin.
    fn annotation_type(&self, annotation: Option<&PyExpr>) -> Type {
        annotation
            .and_then(|annotation| self.resolve_annotation(annotation).ok())
            .unwrap_or(Type::Any)
    }
}
