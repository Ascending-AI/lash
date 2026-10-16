use lash_kernel_doc as k;
use swc_common::Spanned;
use swc_ecma_ast as s;

use super::{Reader, invalid, invocation};
use crate::Diagnostic;

impl Reader {
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
    /// What a `for (const x of ...)` walks and its protocol: the base's
    /// `ts.for_of`, or the hooks `k.via(iterable, open, next, item, check)`
    /// names, `undefined` for one it omits.
    fn protocol(&mut self, right: &s::Expr) -> Result<(k::Expr, k::Protocol), Diagnostic> {
        let args = match invocation(right) {
            Ok((name, args)) if name == "via" => args,
            _ => {
                let via = self.operators.for_of().ok_or_else(|| {
                    invalid(
                        "the base document holds no ts.for_of protocol",
                        Some(right.span()),
                    )
                })?;
                return Ok((self.expression(right)?, via));
            }
        };
        let [iterable, open, next, item, check] = args[..] else {
            return Err(invalid("k.via takes five arguments", Some(right.span())));
        };
        let hook = |expr: &s::Expr| match expr {
            s::Expr::Ident(ident) if ident.sym.as_ref() == "undefined" => Ok(None),
            expr => self.id(expr).map(Some),
        };
        let via = k::Protocol {
            open: hook(open)?.ok_or_else(|| invalid("k.via needs open", Some(right.span())))?,
            next: hook(next)?,
            item: hook(item)?,
            check: hook(check)?,
        };
        Ok((self.expression(iterable)?, via))
    }
    pub(super) fn statement(&mut self, stmt: &s::Stmt) -> Result<k::Stmt, Diagnostic> {
        let stmt = self.read_statement(stmt)?;
        self.types.statement(&stmt, self.operators.functions());
        Ok(stmt)
    }
    fn read_statement(&mut self, stmt: &s::Stmt) -> Result<k::Stmt, Diagnostic> {
        Ok(match stmt {
            s::Stmt::Decl(s::Decl::Var(var))
                if matches!(var.kind, s::VarDeclKind::Let | s::VarDeclKind::Const)
                    && !var.declare =>
            {
                let [binding] = var.decls.as_slice() else {
                    return Err(invalid(
                        "one binding per kernel statement",
                        Some(stmt.span()),
                    ));
                };
                let s::Pat::Ident(name) = &binding.name else {
                    return Err(invalid("kernel bindings are names", Some(stmt.span())));
                };
                if name.type_ann.is_some() || name.id.optional || binding.definite {
                    return Err(invalid(
                        "kernel bindings have no type or optional marker",
                        Some(stmt.span()),
                    ));
                }
                let value = binding.init.as_ref().ok_or_else(|| {
                    invalid("kernel bindings need an initializer", Some(stmt.span()))
                })?;
                k::Stmt::Let {
                    name: self.name(&name.id)?,
                    value: self.rhs(value)?,
                }
            }
            s::Stmt::Expr(statement) => return self.expression_statement(&statement.expr),
            s::Stmt::If(statement) => {
                let condition = self.expression(&statement.test)?;
                let saved = self.types.narrow(&condition, self.operators.functions());
                let cons = self.body(&statement.cons);
                self.types.restore(saved);
                k::Stmt::If {
                    condition,
                    then_block: cons?,
                    else_block: statement
                        .alt
                        .as_ref()
                        .map(|body| self.body(body))
                        .transpose()?
                        .unwrap_or_default(),
                }
            }
            s::Stmt::ForOf(statement) if !statement.is_await => {
                let s::ForHead::VarDecl(var) = &statement.left else {
                    return Err(invalid(
                        "kernel iteration declares a name",
                        Some(stmt.span()),
                    ));
                };
                if var.declare {
                    return Err(invalid("kernel iteration uses let", Some(stmt.span())));
                }
                let protocol = match var.kind {
                    s::VarDeclKind::Let => false,
                    s::VarDeclKind::Const => true,
                    s::VarDeclKind::Var => {
                        return Err(invalid(
                            "kernel iteration uses let or const",
                            Some(stmt.span()),
                        ));
                    }
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
                if name.type_ann.is_some()
                    || name.id.optional
                    || binding.init.is_some()
                    || binding.definite
                {
                    return Err(invalid(
                        "kernel iteration binds an untyped name",
                        Some(stmt.span()),
                    ));
                }
                let binding = self.name(&name.id)?;
                let (iterable, via) = if protocol {
                    let (iterable, via) = self.protocol(&statement.right)?;
                    (iterable, Some(via))
                } else {
                    (self.expression(&statement.right)?, None)
                };
                k::Stmt::For {
                    binding,
                    iterable,
                    via,
                    body: self.body(&statement.body)?,
                    else_block: Vec::new(),
                }
            }
            s::Stmt::While(statement) => k::Stmt::While {
                condition: self.expression(&statement.test)?,
                body: self.body(&statement.body)?,
                else_block: Vec::new(),
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
                s::AssignTarget::Simple(s::SimpleAssignTarget::Member(member)) => {
                    k::Place::Member(self.native_member(member)?)
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
        if let Some(native) = self.native_statement(expr) {
            return native;
        }
        let (name, args) = match invocation(expr) {
            Ok(call) => call,
            Err(_) => {
                return Ok(k::Stmt::Do {
                    value: self.expression(expr)?,
                });
            }
        };
        Ok(match (name.as_str(), args.as_slice()) {
            ("try", [body]) => {
                let function = self.function(body)?;
                if !function.params.is_empty() {
                    return Err(invalid(
                        "kernel try body has no parameters",
                        Some(expr.span()),
                    ));
                }
                k::Stmt::Try(k::TryStmt {
                    body: function.body,
                    catch: None,
                    finally: None,
                })
            }
            _ => k::Stmt::Do {
                value: self.expression(expr)?,
            },
        })
    }
}
