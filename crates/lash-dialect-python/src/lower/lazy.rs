//! Lazy expressions and the statement boundary for source constructs without expression forms.

use lash_kernel_doc::{Expr, Literal, Place, Rhs, Stmt};

use super::{Buf, Lowerer, Lowering, Operand, Ty};

impl Lowerer<'_> {
    /// Captures a lazy branch without evaluating it in the enclosing buffer.
    pub(super) fn value_block(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Lowering<Operand>,
    ) -> Lowering<(Buf, Operand)> {
        let mut value = Operand::none();
        let buf = self.block(|this| {
            value = lower(this)?;
            Ok(())
        })?;
        Ok((buf, value))
    }

    /// Fold expression bindings without lifting nested actions (§12, FIG-6402 rev2).
    pub(super) fn branch_expression(&self, buf: &Buf, body: Expr) -> Option<Expr> {
        buf.expression(body, self.kernel_depth)
    }
    /// Both branches stay lazy; only statement-only source constructs keep block control flow.
    pub(super) fn choose(
        &mut self,
        condition: Expr,
        then_branch: (Buf, Operand),
        else_branch: (Buf, Operand),
    ) -> Operand {
        let ty = if then_branch.1.ty == else_branch.1.ty {
            then_branch.1.ty
        } else {
            Ty::Unknown
        };
        let choice = lash_kernel_dialect::builder::choose(
            condition,
            then_branch,
            else_branch,
            self.kernel_depth,
            |operand| operand.expr.clone(),
        );
        choice.finish(
            self,
            ty,
            Self::folded_value,
            |this, ty| {
                let result = this.temp();
                this.emit(Stmt::Let {
                    name: result.clone(),
                    value: Rhs::Expr(Expr::Literal(Literal::Absent)),
                });
                (Place::Variable(result.clone()), Operand::temp(result, ty))
            },
            Self::store_branch,
            Self::emit_if,
        )
    }

    /// Gives a statement-only branch its result store without evaluating it early.
    pub(super) fn store_branch(&mut self, branch: (Buf, Operand), place: Place) -> Buf {
        let (buf, value) = branch;
        lash_kernel_dialect::builder::complete(
            self,
            |this| &mut this.buf,
            buf,
            |this| this.store(place, value),
        )
    }

    fn folded_value(&mut self, value: Expr, note: super::Note, ty: Ty) -> Operand {
        lash_kernel_dialect::builder::finish_expression(
            self,
            value,
            note,
            ty,
            |_, expr, ty| Operand::inline(expr, ty),
            |this, expr, ty| this.let_rhs(Rhs::Expr(expr), ty),
            |this| &mut this.buf,
        )
    }

    pub(super) fn expression_block(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Lowering<Operand>,
    ) -> Lowering<Operand> {
        let (buf, value) = self.value_block(lower)?;
        if buf.is_empty() {
            return Ok(value);
        }
        match buf.folded(value.expr.clone(), self.kernel_depth, &[0]) {
            Ok((expr, note)) => Ok(self.folded_value(expr, note, value.ty)),
            Err(buf) => {
                self.append(buf);
                Ok(value)
            }
        }
    }
}
