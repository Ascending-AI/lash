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

    pub(super) fn positional_repair(&self, call: &ast::ExprCall) -> String {
        let args = call
            .arguments
            .args
            .iter()
            .chain(call.arguments.keywords.iter().map(|keyword| &keyword.value))
            .map(|arg| self.text(arg.range()))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}({args})", self.text(call.func.range()))
    }

    pub(super) fn type_repair(&self, call: &ast::ExprCall) -> String {
        match call.arguments.args.as_ref() {
            [value] if call.arguments.keywords.is_empty() => {
                let value = self.text(value.range());
                format!(
                    "write `type({value}).__name__` for the name, or `isinstance({value}, int)`"
                )
            }
            _ => format!(
                "call `{}` with one value and read its `__name__`",
                self.text(call.func.range())
            ),
        }
    }

    pub(super) fn format_repair(&self, call: &ast::ExprCall) -> String {
        let values = call
            .arguments
            .args
            .iter()
            .chain(call.arguments.keywords.iter().map(|keyword| &keyword.value))
            .map(|value| format!("{{{}}}", self.text(value.range())))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "use an f-string for the values passed to `{}`: `f\"{values}\"`",
            self.text(call.func.range())
        )
    }

    pub(super) fn builtin_repair(&self, name: &str, call: &ast::ExprCall) -> String {
        let args = call.arguments.args.as_ref();
        if call.arguments.keywords.is_empty() {
            match (name, args) {
                ("map" | "filter", [function, values]) => {
                    let mut item = "item".to_string();
                    while self.taken.contains(&item) {
                        item.push('_');
                    }
                    let values = self.text(values.range());
                    let applied = if matches!(function, PyExpr::NoneLiteral(_)) {
                        item.clone()
                    } else {
                        format!("({})({item})", self.text(function.range()))
                    };
                    if name == "filter" {
                        return format!("write `[{item} for {item} in ({values}) if {applied}]`");
                    }
                    if !matches!(function, PyExpr::NoneLiteral(_)) {
                        return format!("write `[{applied} for {item} in ({values})]`");
                    }
                }
                ("pow", [base, exponent]) => {
                    return format!(
                        "write `({}) ** ({})`",
                        self.text(base.range()),
                        self.text(exponent.range()),
                    );
                }
                ("format", [value] | [value, _]) => {
                    return format!(
                        "use an f-string for `{}`: `f\"{{{}}}\"`; choose a supported literal format specification",
                        self.text(call.range()),
                        self.text(value.range()),
                    );
                }
                _ => {}
            }
        }
        match name {
            "map" | "filter" => format!(
                "replace `{}` with a comprehension over its input values",
                self.text(call.range())
            ),
            "pow" => format!(
                "replace `{}` with `**` and, for a modulus, `%`",
                self.text(call.range())
            ),
            "format" => self.format_repair(call),
            _ => format!(
                "replace `{}` with a supported built-in or a host tool; see the dialect's README",
                self.text(call.range())
            ),
        }
    }
}
