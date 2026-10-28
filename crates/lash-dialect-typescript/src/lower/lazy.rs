//! Lazy expression regions and the boundary for statement control flow.

use lash_kernel_doc::{Expr, Literal, Place, Rhs, Stmt};

use super::{Buf, Lowerer, Lowering, Operand};
use crate::adapter::{self as ast, LogicalOp};

impl Lowerer<'_> {
    // Keep branch buffers and narrowing facts out of the recursive expression
    // dispatch frame: even a binary operand otherwise reserves their stack space.
    pub(super) fn lower_logical(
        &mut self,
        left: &ast::Expr,
        op: LogicalOp,
        right: &ast::Expr,
    ) -> Lowering<Operand> {
        let shown = self.narrowing(left);
        let left = self.lower_expr(left)?;
        // The right operand runs only where the left one did not decide.
        let shown = match op {
            LogicalOp::And => shown.when_true,
            LogicalOp::Or => shown.when_false,
            LogicalOp::Nullish => Vec::new(),
        };
        self.short_circuit(op, left, |this| {
            this.narrowed(shown, |this| this.lower_expr(right))
        })
    }

    pub(super) fn lower_conditional(
        &mut self,
        test: &ast::Expr,
        consequent: &ast::Expr,
        alternate: &ast::Expr,
    ) -> Lowering<Operand> {
        let shown = self.narrowing(test);
        let test = self.lower_expr(test)?;
        let condition = self.condition(test)?;
        let then_branch = self.value_block(|this| {
            this.narrowed(shown.when_true, |this| this.lower_expr(consequent))
        })?;
        let else_branch = self.value_block(|this| {
            this.narrowed(shown.when_false, |this| this.lower_expr(alternate))
        })?;
        Ok(self.choose(condition, then_branch, else_branch))
    }

    /// Captures a lazy branch without evaluating it in the enclosing buffer.
    pub(super) fn value_block(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Lowering<Operand>,
    ) -> Lowering<(Buf, Operand)> {
        let mut value = Operand::undefined();
        let buf = self.block(|this| {
            value = lower(this)?;
            Ok(())
        })?;
        Ok((buf, value))
    }

    /// Ordered bindings, including actions, nest without evaluating a branch early.
    pub(super) fn branch_expression(&self, buf: &Buf, body: Expr) -> Option<Expr> {
        buf.expression(body, self.kernel_depth)
    }
    /// Both branches stay lazy; statement control flow stays in its own block.
    pub(super) fn choose(
        &mut self,
        condition: Expr,
        then_branch: (Buf, Operand),
        else_branch: (Buf, Operand),
    ) -> Operand {
        let ty = then_branch.1.ty.join(&else_branch.1.ty);
        let choice = lash_kernel_dialect::builder::choose(
            condition,
            then_branch,
            else_branch,
            self.kernel_depth,
            Operand::expr,
        );
        let note = choice.note;
        let (condition, then_branch, else_branch) = match choice.branches {
            lash_kernel_dialect::builder::Branches::Expression { value } => {
                if note.blocks.is_empty() && note.nested_written.is_empty() {
                    return self.inline(value, ty);
                }
                let result = self.let_expr(value, ty);
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
        Operand::variable(result, ty)
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
        match buf.folded(value.expr(), self.kernel_depth, &[0]) {
            Ok((expr, note)) => {
                if note.blocks.is_empty() && note.nested_written.is_empty() {
                    return Ok(self.inline(expr, value.ty));
                }
                let result = self.let_expr(expr, value.ty);
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

    /// Null and absent are distinct values; their identity tests run no guest code.
    pub(super) fn nullish_condition(&mut self, value: &Operand) -> Lowering<Expr> {
        let null = self.native("same", vec![value.expr(), Expr::Literal(Literal::Null)])?;
        let absent = self.native("same", vec![value.expr(), Expr::Literal(Literal::Absent)])?;
        Ok(Expr::If {
            cond: Box::new(null),
            then: Box::new(Expr::Literal(Literal::Bool(true))),
            r#else: Box::new(absent),
        })
    }
}
