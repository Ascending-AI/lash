//! Proven receivers select typed access helpers with the ordinary Python fees.

use lash_kernel_doc::{Expr, Literal, Member, Place};
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
        let helper = match (kind, &operands[1].expr) {
            (Ty::Dict, Expr::Literal(Literal::Text(_))) => "py.dict_getitem",
            (Ty::List, Expr::Literal(Literal::Int(_))) => "py.list_getitem",
            (Ty::Tuple, Expr::Literal(Literal::Int(_))) => "py.tuple_getitem",
            _ => "py.getitem",
        };
        self.invoke(helper, operands, Ty::Unknown)
    }

    pub(super) fn setitem(
        &mut self,
        kind: Ty,
        target: Operand,
        index: Operand,
        value: Operand,
    ) -> Lowering<()> {
        if kind == Ty::Dict {
            self.store(
                Place::Member(Member::Index {
                    target: target.expr,
                    index: index.expr,
                }),
                value,
            );
            return Ok(());
        }
        let helper = if kind == Ty::List && matches!(&index.expr, Expr::Literal(Literal::Int(_))) {
            "py.list_setitem"
        } else {
            "py.setitem"
        };
        self.invoke_do(helper, &[target, index, value])
    }
}
