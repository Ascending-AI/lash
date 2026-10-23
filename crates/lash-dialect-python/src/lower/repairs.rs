//! Repair forms built while the submitted operands are still available.

use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::{Ranged, TextRange};

use super::{Lowerer, Lowering};
use crate::diagnostics::{self, Code};

impl Lowerer<'_> {
    pub(super) fn check_assignment_repair(
        &self,
        target: &PyExpr,
        value: &PyExpr,
        op: &str,
    ) -> Lowering<()> {
        if let PyExpr::Attribute(attribute) = target {
            return Err(diagnostics::with_repair(
                Code::TargetUnsupported,
                "assignment to an attribute is not in the dialect",
                target.range(),
                format!(
                    "keep the receiver in a dict and write `({})[{:?}] {op} {}`",
                    self.text(attribute.value.range()),
                    attribute.attr.id.as_str(),
                    self.text(value.range())
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn target_repair(&self, target: &PyExpr) -> String {
        match target {
            PyExpr::Attribute(attribute) => format!(
                "keep the receiver in a dict and assign to `({})[{:?}]`",
                self.text(attribute.value.range()),
                attribute.attr.id.as_str(),
            ),
            _ => format!(
                "replace `{}` with a variable or an item target",
                self.text(target.range())
            ),
        }
    }

    pub(super) fn text(&self, range: TextRange) -> &str {
        &self.source[range.start().to_usize()..range.end().to_usize()]
    }

    pub(super) fn builtin_repair(&self, _name: &str, call: &ast::ExprCall) -> String {
        format!(
            "replace `{}` with a supported built-in or a declared tool; see the dialect's README",
            self.text(call.range())
        )
    }
}
