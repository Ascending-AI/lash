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
        let note = choice.note;
        let (condition, then_branch, else_branch) = match choice.branches {
            lash_kernel_dialect::builder::Branches::Expression { value } => {
                if note.blocks.is_empty() && note.nested_written.is_empty() {
                    return Operand::inline(value, ty);
                }
                let result = self.let_rhs(Rhs::Expr(value), ty);
                if let Some(placed) = self.buf.last_note_mut() {
                    placed.blocks = note.blocks;
                    placed.nested_written = note.nested_written;
                }
                return result;
            }
            lash_kernel_dialect::builder::Branches::Statements {
                condition,
                then_branch,
                else_branch,
            } => (condition, then_branch, else_branch),
        };
        let (mut then_buf, then_value) = then_branch;
        let (mut else_buf, else_value) = else_branch;
        let result = self.temp();
        self.emit(Stmt::Let {
            name: result.clone(),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
        then_buf = self.store_branch((then_buf, then_value), Place::Variable(result.clone()));
        else_buf = self.store_branch((else_buf, else_value), Place::Variable(result.clone()));
        self.emit_if(condition, then_buf, else_buf);
        Operand::temp(result, ty)
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

    pub(super) fn expression_block(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Lowering<Operand>,
    ) -> Lowering<Operand> {
        let (buf, value) = self.value_block(lower)?;
        if buf.is_empty() {
            return Ok(value);
        }
        match buf.folded(value.expr.clone(), self.kernel_depth, &[0]) {
            Ok((expr, note)) => {
                if note.blocks.is_empty() && note.nested_written.is_empty() {
                    return Ok(Operand::inline(expr, value.ty));
                }
                let result = self.let_rhs(Rhs::Expr(expr), value.ty);
                if let Some(placed) = self.buf.last_note_mut() {
                    placed.blocks = note.blocks;
                    placed.nested_written = note.nested_written;
                }
                Ok(result)
            }
            Err(buf) => {
                self.append(buf);
                Ok(value)
            }
        }
    }
}
