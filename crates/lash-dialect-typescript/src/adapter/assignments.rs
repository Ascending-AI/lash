//! Assignment targets and their early refusals.

use super::prototype_chain::builtin_prototype_mutation;
use std::collections::BTreeSet;

use super::{
    Adapter, AssignTarget, Diagnostic, DiagnosticCode, Expr, Pattern, SourceSpan, source_span,
    unasserted,
};
use swc_common::Spanned;
use swc_ecma_ast as swc;

impl Adapter<'_> {
    /// IsValidSimpleAssignmentTarget rejects these names in strict code,
    /// including nested destructuring heads the parser can leave unchecked.
    pub(super) fn check_assignment_pattern(
        &self,
        pattern: &Pattern,
        span: Option<SourceSpan>,
    ) -> Result<(), Diagnostic> {
        match pattern {
            Pattern::Ident(name, _) if matches!(name.as_str(), "eval" | "arguments") => {
                Err(super::early_errors::syntax_error(
                    format!("strict assignment cannot write `{name}`"),
                    span,
                ))
            }
            Pattern::Rest(inner) | Pattern::Assign { target: inner, .. } => {
                self.check_assignment_pattern(inner, span)
            }
            Pattern::Array { elements, rest } => {
                for element in elements.iter().flatten() {
                    self.check_assignment_pattern(element, span)?;
                }
                if let Some(rest) = rest {
                    self.check_assignment_pattern(rest, span)?;
                }
                Ok(())
            }
            Pattern::Object { properties, rest } => {
                for property in properties {
                    self.check_assignment_pattern(&property.value, span)?;
                }
                if let Some(rest) = rest {
                    self.check_assignment_pattern(rest, span)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

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
            Expr::Ident(name, _) => {
                self.assigned.borrow_mut().insert(name.clone());
                Ok(AssignTarget::Ident(name))
            }
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
                let name = self.identifier(&name.id)?;
                self.assigned.borrow_mut().insert(name.clone());
                Ok(AssignTarget::Ident(name))
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
                    Expr::Ident(name, _) => {
                        self.assigned.borrow_mut().insert(name.clone());
                        Ok(AssignTarget::ParenIdent(name))
                    }
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
                let converted = self.convert_pattern(&pattern)?;
                self.check_assignment_pattern(&converted, Some(source_span(pattern.span())))?;
                written_names(&converted, &mut self.assigned.borrow_mut());
                Ok(AssignTarget::Pattern(Box::new(converted)))
            }
            _ => Err(Diagnostic::refusal(
                DiagnosticCode::UnsupportedExpression,
                "Unsupported: this assignment target. Assign to an identifier, member, index, or destructuring pattern.",
                Some(source_span(target.span())),
            )),
        }
    }
}

/// The names a destructuring assignment writes.
pub(super) fn written_names(pattern: &Pattern, names: &mut BTreeSet<String>) {
    match pattern {
        Pattern::Ident(name, _) => {
            names.insert(name.clone());
        }
        Pattern::Member { .. } => {}
        Pattern::Rest(inner) | Pattern::Assign { target: inner, .. } => {
            written_names(inner, names);
        }
        Pattern::Array { elements, rest } => {
            for element in elements.iter().flatten() {
                written_names(element, names);
            }
            if let Some(rest) = rest {
                written_names(rest, names);
            }
        }
        Pattern::Object { properties, rest } => {
            for property in properties {
                written_names(&property.value, names);
            }
            if let Some(rest) = rest {
                written_names(rest, names);
            }
        }
    }
}
