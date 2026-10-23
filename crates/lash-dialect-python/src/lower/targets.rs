//! Ordered deletion of Python places.

use lash_kernel_doc::{Literal, Place};
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::Ranged;

use super::{Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::Ty;

impl Lowerer<'_> {
    pub(super) fn delete_target(&mut self, target: &PyExpr) -> Lowering<()> {
        match target {
            PyExpr::Name(name) => {
                let id = name.id.as_str();
                let current = self.read(id, name.range)?;
                self.discard(current);
                let place = self.written(id, name.range)?;
                self.store(
                    Place::Variable(place),
                    Operand::literal(Literal::Absent, Ty::Unknown),
                );
                self.mark_unbound(id);
                Ok(())
            }
            PyExpr::Tuple(ast::ExprTuple { elts, .. })
            | PyExpr::List(ast::ExprList { elts, .. }) => {
                for target in elts {
                    self.delete_target(target)?;
                }
                Ok(())
            }
            PyExpr::Subscript(subscript) => {
                if let PyExpr::Slice(slice) = subscript.slice.as_ref() {
                    let args = self.optional_operands(&[
                        Some(&subscript.value),
                        slice.lower.as_deref(),
                        slice.upper.as_deref(),
                        slice.step.as_deref(),
                    ])?;
                    self.invoke_do("py.delslice", &args)
                } else {
                    let args = self.operands(&[&subscript.value, &subscript.slice])?;
                    self.invoke_do("py.delitem", &args)
                }
            }
            other => Err(diagnostics::with_repair(
                Code::TargetUnsupported,
                "deletion requires a name, item, list slice or tuple of targets",
                other.range(),
                self.target_repair(other),
            )),
        }
    }
}
