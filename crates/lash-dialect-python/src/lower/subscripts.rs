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
            PyExpr::Set(_) | PyExpr::SetComp(_) => Ty::Set,
            PyExpr::NumberLiteral(number)
                if matches!(number.value, ruff_python_ast::Number::Int(_)) =>
            {
                Ty::Int
            }
            PyExpr::StringLiteral(_) => Ty::Str,
            PyExpr::BinOp(binary)
                if matches!(
                    binary.op,
                    ruff_python_ast::Operator::Add
                        | ruff_python_ast::Operator::Sub
                        | ruff_python_ast::Operator::Mult
                        | ruff_python_ast::Operator::Mod
                        | ruff_python_ast::Operator::FloorDiv
                ) && self.subscript_kind(&binary.left) == Ty::Int
                    && self.subscript_kind(&binary.right) == Ty::Int =>
            {
                Ty::Int
            }
            PyExpr::UnaryOp(unary)
                if matches!(
                    unary.op,
                    ruff_python_ast::UnaryOp::UAdd | ruff_python_ast::UnaryOp::USub
                ) && self.subscript_kind(&unary.operand) == Ty::Int =>
            {
                Ty::Int
            }
            PyExpr::Name(name) => {
                self.local_type(self.variable_scope(name.id.as_str()), name.id.as_str())
            }
            _ => Ty::Unknown,
        }
    }

    pub(super) fn getitem(
        &mut self,
        kind: Ty,
        index_kind: Ty,
        operands: &[Operand],
    ) -> Lowering<Operand> {
        let declaration = crate::constructs::access::read(kind, index_kind, &operands[1].expr);
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
