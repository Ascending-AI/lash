//! Repairs authored where the rejected source operands are still available.

use super::prototype_chain::{is_builtin_prototype_object, is_prototype_chain_property};
use super::{Adapter, Diagnostic, DiagnosticCode, source_span};
use swc_common::Spanned;
use swc_ecma_ast as swc;

impl Adapter<'_> {
    fn text(&self, span: swc_common::Span) -> &str {
        let Some(text) = self.source_text(span) else {
            unreachable!("a parsed operand has source text");
        };
        text
    }

    pub(super) fn tagged_template_repair(&self, template: &swc::TaggedTpl) -> Diagnostic {
        let tag_text = self.text(template.tag.span());
        let tag = if matches!(
            template.tag.as_ref(),
            swc::Expr::Ident(_) | swc::Expr::Member(_) | swc::Expr::Paren(_)
        ) {
            tag_text.to_string()
        } else {
            format!("({tag_text})")
        };
        let argument = self.text(template.tpl.span);
        // Invalid escapes are legal under a tag, but cannot be carried into
        // the untagged template we suggest. In that case offer plain text.
        let repair = if self.convert_template(&template.tpl).is_ok() {
            format!("call the function with an ordinary template literal: `{tag}({argument})`")
        } else {
            let Ok(quoted) = serde_json::to_string(argument) else {
                unreachable!("source text encodes as JSON");
            };
            format!("call the function with escaped plain text: `{tag}({quoted})`")
        };
        Diagnostic::with_repair(
            DiagnosticCode::TaggedTemplateUnsupported,
            "tagged templates are not in the TypeScript dialect",
            repair,
            Some(source_span(template.span)),
        )
    }

    pub(super) fn prototype_member_repair(&self, member: &swc::MemberExpr) -> Diagnostic {
        let receiver = self.text(member.obj.span());
        let property = self.text(member.prop.span());
        Diagnostic::with_repair(
            DiagnosticCode::PrototypeMutationUnsupported,
            "prototype access is not in the TypeScript dialect",
            format!(
                "give `{receiver}` plain data properties instead of reaching through `{property}`"
            ),
            Some(source_span(member.span)),
        )
    }

    pub(super) fn check_assignment_repair(
        &self,
        assignment: &swc::AssignExpr,
    ) -> Result<(), Diagnostic> {
        let swc::AssignTarget::Simple(swc::SimpleAssignTarget::Member(member)) = &assignment.left
        else {
            return Ok(());
        };
        if matches!(member.prop, swc::MemberProp::PrivateName(_)) {
            return Ok(());
        }
        let property = match &member.prop {
            swc::MemberProp::Ident(name) => Some(name.sym.to_string()),
            swc::MemberProp::Computed(computed) => match computed.expr.as_ref() {
                swc::Expr::Lit(swc::Lit::Str(name)) => {
                    Some(name.value.to_string_lossy().into_owned())
                }
                _ => None,
            },
            _ => None,
        };
        let builtin = is_builtin_prototype_object(&member.obj);
        if !builtin && !property.as_deref().is_some_and(is_prototype_chain_property) {
            return Ok(());
        }
        let receiver = self.text(member.obj.span());
        let value = self.text(assignment.right.span());
        let property_text = self.text(member.prop.span());
        let repair = if assignment.op != swc::AssignOp::Assign {
            format!(
                "build a plain object for `{receiver}` with `{property_text}` computed explicitly from `{value}`"
            )
        } else if builtin {
            // Copying a built-in prototype would suggest another forbidden
            // access. Put the actual property and value on a plain object.
            let key = match &member.prop {
                swc::MemberProp::Ident(_) => property_text.to_string(),
                swc::MemberProp::Computed(computed) => {
                    format!("[{}]", self.text(computed.expr.span()))
                }
                _ => unreachable!("private properties are rejected separately"),
            };
            format!(
                "replace the write to `{receiver}` with a plain object: `({{ {key}: {value} }})`"
            )
        } else {
            format!(
                "replace the `{property_text}` write with a new plain object: `Object.assign({{}}, {receiver}, {value})`"
            )
        };
        Err(Diagnostic::with_repair(
            DiagnosticCode::PrototypeMutationUnsupported,
            "prototype mutation is not in the TypeScript dialect",
            repair,
            Some(source_span(assignment.span)),
        ))
    }
}
