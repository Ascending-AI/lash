//! Context managers own an exit immediately after a successful enter.

use lash_kernel_doc::{ActExpr, ActKind, Callee, Expr, Literal, Place, Rhs, Stmt};
use ruff_python_ast::{self as ast, Stmt as PyStmt};

use super::{Active, Buf, Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::Ty;

impl Lowerer<'_> {
    pub(super) fn apply_value(&mut self, callee: Operand, args: &[Operand]) -> Lowering<Operand> {
        let callee = match callee.expr {
            Expr::Variable(name) => name,
            value => {
                let bound = self.let_rhs(Rhs::Expr(value), callee.ty);
                let Expr::Variable(name) = bound.expr else {
                    unreachable!("temporary")
                };
                name
            }
        };
        Ok(Operand::inline(
            Expr::Act(Box::new(ActExpr {
                kind: ActKind::Call(Callee::Value(callee)),
                args: args.iter().map(|arg| arg.expr.clone()).collect(),
            })),
            Ty::Unknown,
        ))
    }

    pub(super) fn with_items(
        &mut self,
        items: &[ast::WithItem],
        body: &[PyStmt],
        asynchronous: bool,
    ) -> Lowering<()> {
        if asynchronous && !self.in_async() {
            return Err(diagnostics::unplaced(
                Code::AwaitOutsideAsync,
                "`async with` outside an async function",
            ));
        }
        let Some((item, rest)) = items.split_first() else {
            return self.statements(body);
        };
        let manager = self.expr(&item.context_expr)?;
        let manager = self.pin(manager);
        let (enter, exit) = if asynchronous {
            ("__aenter__", "__aexit__")
        } else {
            ("__enter__", "__exit__")
        };
        // Both lookups precede enter; exit remains the same callable even if
        // enter mutates the guest object's member table.
        let enter = self.context_member(manager.clone(), enter, asynchronous)?;
        let enter = self.pin(enter);
        let exit = self.context_member(manager, exit, asynchronous)?;
        let exit = self.pin(exit);
        let entered = self.apply_value(enter, &[])?;
        let entered = self.pin(entered);
        let failed = self.fresh("with_failed");
        self.emit(Stmt::Let {
            name: failed.clone(),
            value: Rhs::Expr(Expr::Literal(Literal::Bool(false))),
        });
        let protected = self.block(|this| {
            if let Some(target) = &item.optional_vars {
                this.assign(target, entered)?;
            }
            this.with_items(rest, body, asynchronous)
        })?;
        let caught = self.temp();
        let handler = self.block(|this| {
            this.store(
                Place::Variable(failed.clone()),
                Operand::literal(Literal::Bool(true), Ty::Bool),
            );
            this.scope_mut()
                .handlers
                .push(Active::Caught(caught.clone()));
            let error = Operand::temp(caught.clone(), Ty::Unknown);
            let kind = this.invoke("py.exc.name", std::slice::from_ref(&error), Ty::Str)?;
            let suppressed = this.apply_value(exit.clone(), &[kind, error, Operand::none()])?;
            let suppressed = this.truth(suppressed)?;
            let propagate = this.not(suppressed)?;
            let throw = this.single(Stmt::Throw {
                value: Expr::Variable(caught.clone()),
            });
            this.emit_if(propagate, throw, Buf::default());
            this.scope_mut().handlers.pop();
            Ok(())
        })?;
        let finally = self.block(|this| {
            let normal = this.not(Expr::Variable(failed))?;
            let cleanup = this.block(|this| {
                let value =
                    this.apply_value(exit, &[Operand::none(), Operand::none(), Operand::none()])?;
                this.discard(value);
                Ok(())
            })?;
            this.emit_if(normal, cleanup, Buf::default());
            Ok(())
        })?;
        self.emit_try(protected, Some((caught, handler)), Some(finally));
        Ok(())
    }
}
