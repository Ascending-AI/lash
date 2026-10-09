use lash_kernel_doc as k;
use swc_common::Spanned;
use swc_ecma_ast as s;

use super::{Reader, invalid, invocation};
use crate::Diagnostic;

impl Reader<'_> {
    pub(super) fn block(&mut self, statements: &[s::Stmt]) -> Result<k::Block, Diagnostic> {
        statements.iter().map(|stmt| self.statement(stmt)).collect()
    }
    fn body(&mut self, stmt: &s::Stmt) -> Result<k::Block, Diagnostic> {
        if let s::Stmt::Block(body) = stmt {
            self.block(&body.stmts)
        } else {
            Err(invalid("kernel control needs a block", Some(stmt.span())))
        }
    }
    pub(super) fn statement(&mut self, stmt: &s::Stmt) -> Result<k::Stmt, Diagnostic> {
        Ok(match stmt {
            s::Stmt::Decl(s::Decl::Var(var)) => {
                let [binding] = var.decls.as_slice() else {
                    return Err(invalid(
                        "one binding per kernel statement",
                        Some(stmt.span()),
                    ));
                };
                let s::Pat::Ident(name) = &binding.name else {
                    return Err(invalid("kernel bindings are names", Some(stmt.span())));
                };
                let value = binding.init.as_ref().ok_or_else(|| {
                    invalid("kernel bindings need an initializer", Some(stmt.span()))
                })?;
                k::Stmt::Let {
                    name: self.name(&name.id)?,
                    value: self.rhs(value)?,
                }
            }
            s::Stmt::Expr(statement) => return self.expression_statement(&statement.expr),
            s::Stmt::If(statement) => k::Stmt::If {
                condition: self.expression(&statement.test)?,
                then_block: self.body(&statement.cons)?,
                else_block: statement
                    .alt
                    .as_ref()
                    .map(|body| self.body(body))
                    .transpose()?
                    .unwrap_or_default(),
            },
            s::Stmt::ForOf(statement) if !statement.is_await => {
                let s::ForHead::VarDecl(var) = &statement.left else {
                    return Err(invalid(
                        "kernel iteration declares a name",
                        Some(stmt.span()),
                    ));
                };
                let [binding] = var.decls.as_slice() else {
                    return Err(invalid(
                        "kernel iteration binds one name",
                        Some(stmt.span()),
                    ));
                };
                let s::Pat::Ident(name) = &binding.name else {
                    return Err(invalid("kernel iteration binds a name", Some(stmt.span())));
                };
                k::Stmt::For {
                    binding: self.name(&name.id)?,
                    iterable: self.expression(&statement.right)?,
                    body: self.body(&statement.body)?,
                }
            }
            s::Stmt::While(statement) => k::Stmt::While {
                condition: self.expression(&statement.test)?,
                body: self.body(&statement.body)?,
            },
            s::Stmt::Break(statement) if statement.label.is_none() => k::Stmt::Break,
            s::Stmt::Continue(statement) if statement.label.is_none() => k::Stmt::Continue,
            s::Stmt::Return(statement) => {
                k::Stmt::Return {
                    value: self.expression(statement.arg.as_ref().ok_or_else(|| {
                        invalid("kernel return needs a value", Some(stmt.span()))
                    })?)?,
                }
            }
            s::Stmt::Throw(statement) => k::Stmt::Throw {
                value: self.expression(&statement.arg)?,
            },
            s::Stmt::Try(statement) => k::Stmt::Try(k::TryStmt {
                body: self.block(&statement.block.stmts)?,
                catch: statement
                    .handler
                    .as_ref()
                    .map(|catch| {
                        let Some(s::Pat::Ident(name)) = &catch.param else {
                            return Err(invalid("kernel catch binds a name", Some(catch.span())));
                        };
                        Ok(k::Catch {
                            binding: self.name(&name.id)?,
                            body: self.block(&catch.body.stmts)?,
                        })
                    })
                    .transpose()?,
                finally: statement
                    .finalizer
                    .as_ref()
                    .map(|body| self.block(&body.stmts))
                    .transpose()?,
            }),
            _ => return Err(invalid("not a kernel source statement", Some(stmt.span()))),
        })
    }
    fn expression_statement(&mut self, expr: &s::Expr) -> Result<k::Stmt, Diagnostic> {
        if let s::Expr::Assign(assign) = expr {
            if assign.op != s::AssignOp::Assign {
                return Err(invalid("kernel assignment is explicit", Some(expr.span())));
            }
            let place = match &assign.left {
                s::AssignTarget::Simple(s::SimpleAssignTarget::Ident(name)) => {
                    k::Place::Variable(self.name(&name.id)?)
                }
                s::AssignTarget::Simple(s::SimpleAssignTarget::Member(member)) if matches!(&member.prop, s::MemberProp::Ident(name) if name.sym == "value") => {
                    k::Place::Member(self.member(&member.obj)?)
                }
                _ => {
                    return Err(invalid(
                        "expected variable or kernel field/index lvalue",
                        Some(expr.span()),
                    ));
                }
            };
            return Ok(k::Stmt::Assign {
                place,
                value: self.rhs(&assign.right)?,
            });
        }
        let (name, args) = invocation(expr)?;
        Ok(match (name.as_str(), args.as_slice()) {
            ("remove", [member]) => k::Stmt::Remove {
                member: self.member(member)?,
            },
            ("print", [value]) => k::Stmt::Print {
                value: self.expression(value)?,
            },
            ("finish", [value]) => k::Stmt::Finish {
                value: self.expression(value)?,
            },
            ("fail", [value]) => k::Stmt::Fail {
                value: self.expression(value)?,
            },
            _ => k::Stmt::Do {
                action: self.action(expr)?,
            },
        })
    }
}
