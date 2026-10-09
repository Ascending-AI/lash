//! Assignment targets and their early refusals.

use super::prototype_chain::builtin_prototype_mutation;
use super::{Adapter, AssignTarget, Diagnostic, DiagnosticCode, Expr, source_span, unasserted};
use swc_common::Spanned;
use swc_ecma_ast as swc;

impl Adapter<'_> {
    pub(super) fn convert_update_target(
        &self,
        expr: &swc::Expr,
    ) -> Result<AssignTarget, Diagnostic> {
        if let Some(diagnostic) = expr
            .as_member()
            .and_then(|member| builtin_prototype_mutation(self, member))
        {
            return Err(diagnostic);
        }
        match unasserted(self.convert_expr(expr)?) {
            Expr::Ident(name, _) => Ok(AssignTarget::Ident(name)),
            Expr::Member {
                object, property, ..
            } => Ok(AssignTarget::Member { object, property }),
            _ => Err(Diagnostic::refusal(
                DiagnosticCode::UnsupportedExpression,
                "Unsupported: update on a non-assignment target. Assign the expression to a variable first.",
                Some(source_span(expr.span())),
            )),
        }
    }

    pub(super) fn convert_assign_target(
        &self,
        target: &swc::AssignTarget,
    ) -> Result<AssignTarget, Diagnostic> {
        match target {
            swc::AssignTarget::Simple(swc::SimpleAssignTarget::Ident(name)) => {
                Ok(AssignTarget::Ident(self.identifier(&name.id)?))
            }
            swc::AssignTarget::Simple(swc::SimpleAssignTarget::Member(member)) => {
                if let Some(diagnostic) = builtin_prototype_mutation(self, member) {
                    return Err(diagnostic);
                }
                let Expr::Member {
                    object, property, ..
                } = self.convert_member(member)?
                else {
                    unreachable!()
                };
                Ok(AssignTarget::Member { object, property })
            }
            swc::AssignTarget::Simple(swc::SimpleAssignTarget::Paren(paren)) => {
                match unasserted(self.convert_expr(&paren.expr)?) {
                    Expr::Ident(name, _) => Ok(AssignTarget::ParenIdent(name)),
                    Expr::Member {
                        object, property, ..
                    } => Ok(AssignTarget::Member { object, property }),
                    _ => Err(Diagnostic::refusal(
                        DiagnosticCode::UnsupportedExpression,
                        "Unsupported: this assignment target. Assign to an identifier, member, index, or destructuring pattern.",
                        Some(source_span(paren.expr.span())),
                    )),
                }
            }
            swc::AssignTarget::Pat(pattern) => {
                let pattern: swc::Pat = pattern.clone().into();
                Ok(AssignTarget::Pattern(Box::new(
                    self.convert_pattern(&pattern)?,
                )))
            }
            _ => Err(Diagnostic::refusal(
                DiagnosticCode::UnsupportedExpression,
                "Unsupported: this assignment target. Assign to an identifier, member, index, or destructuring pattern.",
                Some(source_span(target.span())),
            )),
        }
    }
}
