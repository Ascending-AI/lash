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
    pub(super) fn branch_expression(&self, buf: &Buf, mut body: Expr) -> Option<Expr> {
        for stmt in buf.stmts().rev() {
            let Stmt::Let { name, value } = stmt else {
                return None;
            };
            let value = match value {
                Rhs::Expr(value) => value.clone(),
                Rhs::Action(action) => action.clone().into(),
            };
            body = Expr::Let {
                name: name.clone(),
                value: Box::new(value),
                body: Box::new(body),
            };
        }
        // K-DOC-006: keep the existing hoists when folding would exceed
        // the document's depth limit (including the surrounding blocks).
        let mut pending = vec![(lash_kernel_doc::Node::Expr(&body), 0)];
        while let Some((node, depth)) = pending.pop() {
            if depth + 2 * self.kernel_depth + 4 > lash_kernel_doc::MAX_NESTING_DEPTH {
                return None;
            }
            pending.extend(node.children().into_iter().map(|child| (child, depth + 1)));
        }
        Some(body)
    }

    pub(super) fn branch_written(
        buf: &Buf,
        prefix: &[u32],
    ) -> Vec<(Vec<u32>, lash_kernel_dialect::Written)> {
        let mut out = Vec::new();
        let mut prefix = prefix.to_vec();
        for note in buf.notes() {
            if let Some(written) = &note.written {
                let mut path = prefix.clone();
                path.push(0);
                out.push((path, written.clone()));
            }
            for (path, written) in &note.nested_written {
                let mut nested = prefix.clone();
                nested.extend(path);
                out.push((nested, written.clone()));
            }
            prefix.push(1);
        }
        out
    }

    /// Both branches stay lazy; statement control flow stays in its own block.
    pub(super) fn choose(
        &mut self,
        condition: Expr,
        then_branch: (Buf, Operand),
        else_branch: (Buf, Operand),
    ) -> Operand {
        let (mut then_buf, then_value) = then_branch;
        let (mut else_buf, else_value) = else_branch;
        let ty = then_value.ty.join(&else_value.ty);
        if let (Some(then), Some(otherwise)) = (
            self.branch_expression(&then_buf, then_value.expr()),
            self.branch_expression(&else_buf, else_value.expr()),
        ) {
            let nested_written: Vec<_> = Self::branch_written(&then_buf, &[0, 1])
                .into_iter()
                .chain(Self::branch_written(&else_buf, &[0, 2]))
                .collect();
            let value = Expr::If {
                cond: Box::new(condition),
                then: Box::new(then),
                r#else: Box::new(otherwise),
            };
            let blocks: Vec<_> = then_buf
                .into_notes()
                .chain(else_buf.into_notes())
                .flat_map(|note| note.blocks)
                .collect();
            if blocks.is_empty() && nested_written.is_empty() {
                return self.inline(value, ty);
            }
            let result = self.let_expr(value, ty);
            if let Some(note) = self.buf.last_note_mut() {
                note.blocks = blocks;
                note.nested_written = nested_written;
            }
            return result;
        }
        let result = self.temp();
        self.emit(Stmt::Let {
            name: result.clone(),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
        let outer = std::mem::take(&mut self.buf);
        self.buf = then_buf;
        self.store(Place::Variable(result.clone()), then_value);
        then_buf = std::mem::take(&mut self.buf);
        self.buf = else_buf;
        self.store(Place::Variable(result.clone()), else_value);
        else_buf = std::mem::replace(&mut self.buf, outer);
        self.emit_if(condition, then_buf, else_buf);
        Operand::variable(result, ty)
    }

    /// Gives a statement-only branch its result store without evaluating it early.
    pub(super) fn store_branch(&mut self, branch: (Buf, Operand), place: Place) -> Buf {
        let (buf, value) = branch;
        let outer = std::mem::replace(&mut self.buf, buf);
        self.store(place, value);
        std::mem::replace(&mut self.buf, outer)
    }

    pub(super) fn expression_block(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Lowering<Operand>,
    ) -> Lowering<Operand> {
        let (buf, value) = self.value_block(lower)?;
        if let Some(expr) = self.branch_expression(&buf, value.expr()) {
            let nested_written = Self::branch_written(&buf, &[0]);
            let blocks: Vec<_> = buf.into_notes().flat_map(|note| note.blocks).collect();
            if blocks.is_empty() && nested_written.is_empty() {
                return Ok(self.inline(expr, value.ty));
            }
            let result = self.let_expr(expr, value.ty);
            if let Some(note) = self.buf.last_note_mut() {
                note.blocks = blocks;
                note.nested_written = nested_written;
            }
            return Ok(result);
        }
        self.append(buf);
        Ok(value)
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
