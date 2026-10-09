//! Optional chains: `a?.b`, `a?.()`, and the member and call operations that
//! extend one until a parenthesis ends it.

use swc_ecma_ast as swc;

use super::{
    Adapter, Diagnostic, DiagnosticCode, Expr, MemberProperty, OptionalOperation, SourceSpan,
    reject, source_span,
};

/// A parenthesized expression. Parentheses end an optional chain: in `(a?.b).c`
/// the short circuit covers `a?.b` alone, and `.c` reads whatever that
/// produced. The chain becomes the base of a chain of its own, which an
/// operation after the parentheses extends instead.
pub(super) fn parenthesized(expr: Expr) -> Expr {
    match expr {
        chain @ Expr::OptionalChain { .. } => Expr::OptionalChain {
            base: Box::new(chain),
            operations: Vec::new(),
        },
        other => other,
    }
}

impl Adapter<'_> {
    pub(super) fn append_optional_operation(
        &self,
        base: Expr,
        operation: OptionalOperation,
        span: SourceSpan,
    ) -> Expr {
        match base {
            Expr::OptionalChain {
                base,
                mut operations,
            } => {
                operations.push(operation);
                Expr::OptionalChain { base, operations }
            }
            base => match operation {
                OptionalOperation::Member {
                    property,
                    optional: false,
                } => Expr::Member {
                    object: Box::new(base),
                    property,
                    span,
                },
                OptionalOperation::Call {
                    args,
                    optional: false,
                } => Expr::Call {
                    callee: Box::new(base),
                    args,
                    span,
                },
                operation => Expr::OptionalChain {
                    base: Box::new(base),
                    operations: vec![operation],
                },
            },
        }
    }

    pub(super) fn convert_optional_chain(
        &self,
        chain: &swc::OptChainExpr,
    ) -> Result<Expr, Diagnostic> {
        Ok(match chain.base.as_ref() {
            swc::OptChainBase::Member(member) => {
                let object = self.convert_expr(&member.obj)?;
                let property = match &member.prop {
                    swc::MemberProp::Ident(name) => {
                        MemberProperty::Field(self.identifier_name(name)?)
                    }
                    swc::MemberProp::Computed(property) => {
                        MemberProperty::Index(Box::new(self.convert_expr(&property.expr)?))
                    }
                    swc::MemberProp::PrivateName(_) => {
                        return Err(reject(
                            DiagnosticCode::PrivateNameUnsupported,
                            "private names",
                            Some(source_span(member.span)),
                        ));
                    }
                };
                self.append_optional_operation(
                    object,
                    OptionalOperation::Member {
                        property,
                        optional: chain.optional,
                    },
                    source_span(member.span),
                )
            }
            swc::OptChainBase::Call(call) => self.append_optional_operation(
                self.convert_expr(&call.callee)?,
                OptionalOperation::Call {
                    args: self.convert_call_args(&call.args)?,
                    optional: chain.optional,
                },
                source_span(call.span),
            ),
        })
    }
}
