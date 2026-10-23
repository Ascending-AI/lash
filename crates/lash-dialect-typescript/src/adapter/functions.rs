//! Converts function declarations and expressions from the parser AST.

use super::*;

impl Adapter<'_> {
    pub(super) fn convert_function(
        &self,
        name: Option<String>,
        function: &swc::Function,
    ) -> Result<Function, Diagnostic> {
        let span = Some(source_span(function.span));
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
            reject_defect(
                DiagnosticCode::SyntaxError,
                "function expressions without bodies",
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
        let result_span = SourceSpan {
            start: source_span(body.span).start,
            end: source_span(body.span).start,
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
            param_spans: own_params()
                .map(|param| source_span(param.pat.span()))
                .collect(),
            result_span,
            type_parameters: function
                .type_params
                .as_ref()
                .map(|decl| decl.params.iter().map(|p| p.name.sym.to_string()).collect())
                .unwrap_or_default(),
            body: FunctionBody::Block(body),
            return_ty: function
                .return_type
                .as_ref()
                .map(|annotation| types::convert_type(&annotation.type_ann)),
            is_async: function.is_async,
            is_arrow: false,
            captions,
        })
    }

    pub(super) fn convert_arrow(
        &self,
        function: &swc::ArrowExpr,
        span: Option<SourceSpan>,
    ) -> Result<Expr, Diagnostic> {
        if function.is_generator {
            return Err(reject(
                DiagnosticCode::GeneratorUnsupported,
                "generators",
                span,
            ));
        }
        // An async arrow's parameters reserve `await`; a plain
        // arrow's inherit the enclosing rule (§15.3, §15.9).
        let parameter_rule = self.in_async_function.get() || function.is_async;
        let params = self.in_function(parameter_rule, || {
            self.in_arrow_parameters(|| {
                function
                    .params
                    .iter()
                    .enumerate()
                    .filter(|(index, param)| !self.is_this_parameter(*index, param))
                    .map(|(_, param)| self.convert_pattern(param))
                    .collect::<Result<Vec<_>, _>>()
            })
        })?;
        let body = self.in_function(function.is_async, || {
            Ok(match function.body.as_ref() {
                swc::BlockStmtOrExpr::BlockStmt(block) => {
                    FunctionBody::Block(self.convert_statements(&block.stmts)?)
                }
                swc::BlockStmtOrExpr::Expr(expr) => {
                    FunctionBody::Expression(Box::new(self.convert_expr(expr)?))
                }
            })
        })?;
        Ok(Expr::Function(Function {
            name: None,
            params,
            param_spans: function
                .params
                .iter()
                .filter(|param| !matches!(param, swc::Pat::Ident(name) if name.id.sym == "this"))
                .map(|param| source_span(param.span()))
                .collect(),
            result_span: self.arrow_result_span(function),
            type_parameters: function
                .type_params
                .as_ref()
                .map(|decl| decl.params.iter().map(|p| p.name.sym.to_string()).collect())
                .unwrap_or_default(),
            body,
            return_ty: function
                .return_type
                .as_ref()
                .map(|annotation| types::convert_type(&annotation.type_ann)),
            is_async: function.is_async,
            is_arrow: true,
            captions: Default::default(),
        }))
    }
    /// The result annotation goes before the arrow, including when trivia
    /// between parameters and body contains another `=>` spelling.
    fn arrow_result_span(&self, function: &swc::ArrowExpr) -> SourceSpan {
        let from = function.params.last().map_or_else(
            || {
                function
                    .type_params
                    .as_ref()
                    .map_or(source_span(function.span).start, |params| {
                        source_span(params.span).end
                    })
            },
            |param| source_span(param.span()).end,
        );
        let end = source_span(function.body.span()).start;
        let mut at = from;
        while at < end {
            let suffix = &self.source[at..end];
            if suffix.starts_with("/*") {
                at += suffix.find("*/").map_or(suffix.len(), |end| end + 2);
            } else if suffix.starts_with("//") {
                at += suffix.find('\n').unwrap_or(suffix.len());
            } else if suffix.starts_with("=>") {
                break;
            } else {
                at += suffix.chars().next().map_or(1, char::len_utf8);
            }
        }
        SourceSpan { start: at, end: at }
    }
}

/// A refused public pattern still retains one authored parameter. Trivia is
/// excluded so its diagnostic name cannot make executable identity depend on
/// source formatting; token contents, including string keys, stay intact.
pub(crate) fn public_pattern(source: &str) -> String {
    use swc_common::BytePos;
    use swc_ecma_parser::{StringInput, Syntax, TsSyntax, lexer::Lexer};

    let end = u32::try_from(source.len()).unwrap_or(u32::MAX);
    Lexer::new(
        Syntax::Typescript(TsSyntax::default()),
        Default::default(),
        StringInput::new(source, BytePos(0), BytePos(end)),
        None,
    )
    .map(|token| {
        let span = source_span(token.span);
        &source[span.start..span.end]
    })
    .collect()
}
