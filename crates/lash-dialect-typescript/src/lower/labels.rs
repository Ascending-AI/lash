//! Named departures use private identities while source finalizers unwind.
//! A source catch immediately rethrows them. The target alone consumes them,
//! outside source handlers, so a for-loop update runs in its proper context.
use lash_kernel_doc::{Expr, Stmt};
use lash_kernel_value::Name;

use super::{Buf, Lowerer, Lowering, Ty};
use crate::adapter as ast;
use crate::{Diagnostic, DiagnosticCode};

#[derive(Clone, Debug)]
pub(super) struct Label {
    name: String,
    breaks: Name,
    continues: Option<Name>,
}

impl Lowerer<'_> {
    pub(super) fn lower_labeled(&mut self, name: &str, body: &ast::Stmt) -> Lowering<()> {
        let mut names = vec![name.to_owned()];
        let mut target = body;
        while let ast::Stmt::Labeled { name, body } = target.unspanned() {
            names.push(name.clone());
            target = body;
        }
        let mut target = target.clone();
        let loop_body = match &mut target {
            ast::Stmt::Spanned(_, inner) => inner.as_mut(),
            other => other,
        };
        let iteration = matches!(
            loop_body,
            ast::Stmt::While { .. }
                | ast::Stmt::DoWhile { .. }
                | ast::Stmt::For { .. }
                | ast::Stmt::ForOf { .. }
                | ast::Stmt::ForIn { .. }
        );
        let breaks = self.let_expr(Expr::Record(Vec::new()), Ty::Unknown);
        let breaks = super::statements::variable_of(&breaks);
        let continues = iteration.then(|| {
            let token = self.let_expr(Expr::Record(Vec::new()), Ty::Unknown);
            super::statements::variable_of(&token)
        });
        let depth = self.frame().labels.len();
        for name in names {
            self.frame_mut().labels.push(Label {
                name,
                breaks: breaks.clone(),
                continues: continues.clone(),
            });
        }
        let result = if iteration {
            let body = match loop_body {
                ast::Stmt::While { body, .. }
                | ast::Stmt::DoWhile { body, .. }
                | ast::Stmt::For { body, .. }
                | ast::Stmt::ForOf { body, .. }
                | ast::Stmt::ForIn { body, .. } => body,
                _ => unreachable!("the target is an iteration"),
            };
            **body = ast::Stmt::ExitHandler {
                body: body.clone(),
                break_token: breaks,
                continue_token: continues,
            };
            self.lower_statement(&target)
        } else {
            self.exit_handler(&target, &breaks, None)
        };
        self.frame_mut().labels.truncate(depth);
        result
    }

    pub(super) fn named_exit(&mut self, name: &str, continuing: bool) -> Lowering<()> {
        let label = self
            .frame()
            .labels
            .iter()
            .rev()
            .find(|label| label.name == name);
        let token = label.and_then(|label| {
            if continuing {
                label.continues.clone()
            } else {
                Some(label.breaks.clone())
            }
        });
        let Some(token) = token else {
            return Err(Diagnostic::new(
                DiagnosticCode::SyntaxError,
                format!("invalid named exit `{name}`"),
                self.span,
            ));
        };
        self.emit(Stmt::Throw {
            value: Expr::Variable(token),
        });
        Ok(())
    }

    pub(super) fn rethrow_named_exit(&mut self, caught: &Name) -> Lowering<()> {
        let labels = self.frame().labels.clone();
        for label in labels {
            for token in std::iter::once(label.breaks).chain(label.continues) {
                let same = self.same(Expr::Variable(caught.clone()), Expr::Variable(token))?;
                let rethrow = self.block(|this| {
                    this.emit(Stmt::Throw {
                        value: Expr::Variable(caught.clone()),
                    });
                    Ok(())
                })?;
                self.emit_if(same, rethrow, Buf::default());
            }
        }
        Ok(())
    }

    pub(super) fn exit_handler(
        &mut self,
        body: &ast::Stmt,
        breaks: &Name,
        continues: Option<&Name>,
    ) -> Lowering<()> {
        let body = self.scoped_block(|this| this.lower_statement(body))?;
        let caught = self.temp();
        let handler = self.block(|this| {
            let is_break = this.same(
                Expr::Variable(caught.clone()),
                Expr::Variable(breaks.clone()),
            )?;
            let broken = this.block(|this| {
                if continues.is_some() {
                    this.emit(Stmt::Break);
                }
                Ok(())
            })?;
            let other = this.block(|this| {
                if let Some(continues) = continues {
                    let is_continue = this.same(
                        Expr::Variable(caught.clone()),
                        Expr::Variable(continues.clone()),
                    )?;
                    let onward =
                        this.block(|this| this.lower_continue(this.frame().controls.len()))?;
                    let rethrow = this.block(|this| {
                        this.emit(Stmt::Throw {
                            value: Expr::Variable(caught.clone()),
                        });
                        Ok(())
                    })?;
                    this.emit_if(is_continue, onward, rethrow);
                } else {
                    this.emit(Stmt::Throw {
                        value: Expr::Variable(caught.clone()),
                    });
                }
                Ok(())
            })?;
            this.emit_if(is_break, broken, other);
            Ok(())
        })?;
        self.emit_try(body, Some((caught, handler)), None);
        Ok(())
    }
}
