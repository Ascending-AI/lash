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
    pub(super) fn branch_expression(&self, buf: &Buf, mut body: Expr) -> Option<Expr> {
        for stmt in buf.stmts.iter().rev() {
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

    fn branch_written(buf: &Buf, prefix: &[u32]) -> Vec<(Vec<u32>, lash_kernel_dialect::Written)> {
        let mut out = Vec::new();
        let mut prefix = prefix.to_vec();
        for note in &buf.notes {
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

    /// Both branches stay lazy; only statement-only source constructs keep block control flow.
    pub(super) fn choose(
        &mut self,
        condition: Expr,
        then_branch: (Buf, Operand),
        else_branch: (Buf, Operand),
    ) -> Operand {
        let (mut then_buf, then_value) = then_branch;
        let (mut else_buf, else_value) = else_branch;
        let ty = if then_value.ty == else_value.ty {
            then_value.ty
        } else {
            Ty::Unknown
        };
        if let (Some(then), Some(otherwise)) = (
            self.branch_expression(&then_buf, then_value.expr.clone()),
            self.branch_expression(&else_buf, else_value.expr.clone()),
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
            // Keep closure-body annotation buffers with the expression that now owns them.
            if !nested_written.is_empty()
                || then_buf
                    .notes
                    .iter()
                    .chain(&else_buf.notes)
                    .any(|note| !note.blocks.is_empty())
            {
                let result = self.let_rhs(Rhs::Expr(value), ty);
                if let Some(note) = self.buf.notes.last_mut() {
                    note.blocks = then_buf
                        .notes
                        .into_iter()
                        .chain(else_buf.notes)
                        .flat_map(|note| note.blocks)
                        .collect();
                    note.nested_written = nested_written;
                }
                return result;
            }
            return Operand::inline(value, ty);
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
        Operand::temp(result, ty)
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
        if buf.is_empty() {
            return Ok(value);
        }
        if let Some(expr) = self.branch_expression(&buf, value.expr.clone()) {
            let nested_written = Self::branch_written(&buf, &[0]);
            let blocks: Vec<_> = buf.notes.into_iter().flat_map(|note| note.blocks).collect();
            if blocks.is_empty() && nested_written.is_empty() {
                return Ok(Operand::inline(expr, value.ty));
            }
            let result = self.let_rhs(Rhs::Expr(expr), value.ty);
            if let Some(note) = self.buf.notes.last_mut() {
                note.blocks = blocks;
                note.nested_written = nested_written;
            }
            return Ok(result);
        }
        self.append(buf);
        Ok(value)
    }
}
