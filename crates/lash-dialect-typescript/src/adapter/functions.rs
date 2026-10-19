//! Converts function declarations and expressions from the parser AST.

use super::*;

impl Adapter<'_> {
    pub(super) fn note_type_parameters(&self, declared: Option<&swc::TsTypeParamDecl>) {
        if let Some(declared) = declared {
            self.type_parameters.borrow_mut().extend(
                declared
                    .params
                    .iter()
                    .map(|param| param.name.sym.to_string()),
            );
        }
    }

    pub(super) fn convert_function(
        &self,
        name: Option<String>,
        function: &swc::Function,
    ) -> Result<Function, Diagnostic> {
        let span = Some(source_span(function.span));
        self.note_type_parameters(function.type_params.as_deref());
        if function.is_generator {
            return Err(reject(
                DiagnosticCode::GeneratorUnsupported,
                "generators",
                span,
            ));
        }
        if !function.decorators.is_empty() {
            return Err(reject(
                DiagnosticCode::DecoratorUnsupported,
                "decorators",
                span,
            ));
        }
        let body = function.body.as_ref().ok_or_else(|| {
            reject_refusal(
                DiagnosticCode::UnsupportedStatement,
                "function declarations without bodies",
                span,
            )
        })?;
        let own_params = || {
            function
                .params
                .iter()
                .enumerate()
                .filter(|(index, param)| !self.is_this_parameter(*index, &param.pat))
                .map(|(_, param)| param)
        };
        let (params, body) = self.in_function(function.is_async, || {
            let params = own_params()
                .map(|param| self.convert_pattern(&param.pat))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((params, self.convert_statements(&body.stmts)?))
        })?;
        let captions = self.comments.signature(
            own_params(),
            function
                .return_type
                .as_ref()
                .map(|annotation| annotation.type_ann.as_ref()),
        );
        Ok(Function {
            name,
            params,
            body: FunctionBody::Block(body),
            return_ty: function
                .return_type
                .as_ref()
                .map(|annotation| types::convert_return_type(&annotation.type_ann)),
            is_async: function.is_async,
            is_arrow: false,
            captions,
        })
    }
}
