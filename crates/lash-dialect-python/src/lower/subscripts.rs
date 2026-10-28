//! Proven receivers select typed access helpers with the ordinary Python fees.

use lash_kernel_doc::{Member, Place};
use ruff_python_ast::Expr as PyExpr;

use super::{Lowerer, Lowering, Operand};
use crate::scope::Ty;

impl Lowerer<'_> {
    pub(super) fn subscript_kind(&self, target: &PyExpr) -> Ty {
        match target {
            PyExpr::Dict(_) | PyExpr::DictComp(_) => Ty::Dict,
            PyExpr::List(_) | PyExpr::ListComp(_) => Ty::List,
            PyExpr::Tuple(_) => Ty::Tuple,
            PyExpr::Name(name) => {
                self.local_type(self.variable_scope(name.id.as_str()), name.id.as_str())
            }
            _ => Ty::Unknown,
        }
    }

    pub(super) fn getitem(&mut self, kind: Ty, operands: &[Operand]) -> Lowering<Operand> {
        let declaration = crate::constructs::access::read(kind, &operands[1].expr);
        self.invoke(declaration.helper, operands, Ty::Unknown)
    }

    pub(super) fn setitem(
        &mut self,
        kind: Ty,
        target: Operand,
        index: Operand,
        value: Operand,
    ) -> Lowering<()> {
        match crate::constructs::access::write(kind, &index.expr) {
            crate::constructs::access::Store::Member => {
                self.store(
                    Place::Member(Member::Index {
                        target: target.expr,
                        index: index.expr,
                    }),
                    value,
                );
                Ok(())
            }
            crate::constructs::access::Store::Call(declaration) => {
                self.invoke_do(declaration.helper, &[target, index, value])
            }
        }
    }
}
