use super::*;

impl Lowerer {
    pub(super) fn lower_classic_for(
        &mut self,
        init: Option<&Stmt>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> Result<LashExpr, Diagnostic> {
        if continue_under_finally(body, false, 0) {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic-for continue crossing a finally boundary is not supported in v1",
                None,
            ));
        }
        let Some(Stmt::Var {
            kind: VarKind::Let,
            declarations,
        }) = init
        else {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for requires `let i = start; i < end; i++` in v1",
                None,
            ));
        };
        let [declaration] = declarations.as_slice() else {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for requires exactly one loop binding",
                None,
            ));
        };
        let Some(start) = declaration.init.as_ref() else {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for loop binding requires an initializer",
                None,
            ));
        };
        let Some(Expr::Binary {
            left,
            op: BinaryOp::Less,
            ..
        }) = test
        else {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for condition must be `i < end`",
                None,
            ));
        };
        let Expr::Ident(condition_name) = left.as_ref() else {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for condition must read its loop binding",
                None,
            ));
        };
        let Some(Expr::Update {
            name: update_name,
            delta,
            ..
        }) = update
        else {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for update must be `i++`",
                None,
            ));
        };
        if condition_name != &declaration.name || update_name != &declaration.name || *delta != 1.0
        {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for binding, condition, and update must name the same identifier",
                None,
            ));
        }

        self.scopes.push(Scope::default());
        self.declare(&declaration.name, BindingKind::Let, true, false)?;
        let internal = self.binding(&declaration.name)?.internal.clone();
        let start = self.lower_expr(start)?;
        let condition = self.lower_expr(test.expect("validated classic for condition"))?;
        let update = self.lower_update_statement(&declaration.name, *delta)?;
        self.loop_depth += 1;
        self.continue_epilogues.push(Some(update.clone()));
        let body = self.lower_stmt_block(body)?;
        self.continue_epilogues.pop();
        self.loop_depth -= 1;
        self.scopes.pop();
        Ok(LashExpr::Block(vec![
            LashExpr::Assign {
                target: AssignTarget::variable(internal.as_str().into()),
                expr: Box::new(start),
            },
            LashExpr::While {
                condition: Box::new(condition),
                body: Box::new(LashExpr::Block(vec![body, update])),
            },
        ]))
    }

    pub(super) fn lower_update_statement(
        &mut self,
        name: &str,
        delta: f64,
    ) -> Result<LashExpr, Diagnostic> {
        let target = self.lower_assign_target(&TsAssignTarget::Ident(name.to_string()))?;
        Ok(LashExpr::Assign {
            target,
            expr: Box::new(LashExpr::JavaScriptBinary {
                left: Box::new(LashExpr::Variable(self.resolve(name)?.into())),
                op: if delta > 0.0 {
                    JavaScriptBinaryOp::Add
                } else {
                    JavaScriptBinaryOp::Subtract
                },
                right: Box::new(LashExpr::Number(delta.abs())),
            }),
        })
    }
}

pub(super) fn continue_under_finally(
    stmt: &Stmt,
    protected: bool,
    nested_loop_depth: usize,
) -> bool {
    match stmt {
        Stmt::Continue => protected && nested_loop_depth == 0,
        Stmt::Block(statements) => statements
            .iter()
            .any(|stmt| continue_under_finally(stmt, protected, nested_loop_depth)),
        Stmt::If {
            consequent,
            alternate,
            ..
        } => {
            continue_under_finally(consequent, protected, nested_loop_depth)
                || alternate
                    .as_deref()
                    .is_some_and(|stmt| continue_under_finally(stmt, protected, nested_loop_depth))
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            let crosses = protected || finally.is_some();
            body.iter()
                .any(|stmt| continue_under_finally(stmt, crosses, nested_loop_depth))
                || catch.as_ref().is_some_and(|catch| {
                    catch
                        .body
                        .iter()
                        .any(|stmt| continue_under_finally(stmt, crosses, nested_loop_depth))
                })
                || finally.as_ref().is_some_and(|statements| {
                    statements
                        .iter()
                        .any(|stmt| continue_under_finally(stmt, protected, nested_loop_depth))
                })
        }
        Stmt::While { body, .. } | Stmt::For { body, .. } | Stmt::ForOf { body, .. } => {
            continue_under_finally(body, protected, nested_loop_depth + 1)
        }
        Stmt::Empty
        | Stmt::Expr(_)
        | Stmt::Var { .. }
        | Stmt::Function { .. }
        | Stmt::Return(_)
        | Stmt::Break
        | Stmt::Throw(_) => false,
    }
}

pub(super) fn contains_member_assignment(stmt: &Stmt) -> bool {
    fn expression_contains_member_assignment(expr: &Expr) -> bool {
        match expr {
            Expr::Assign {
                target: TsAssignTarget::Member { .. },
                ..
            } => true,
            Expr::Array(items) => items.iter().any(expression_contains_member_assignment),
            Expr::Object(entries) => entries
                .iter()
                .any(|(_, value)| expression_contains_member_assignment(value)),
            Expr::Assign { value, .. }
            | Expr::Unary { value, .. }
            | Expr::Await(value)
            | Expr::Member { object: value, .. } => expression_contains_member_assignment(value),
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                expression_contains_member_assignment(left)
                    || expression_contains_member_assignment(right)
            }
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                expression_contains_member_assignment(test)
                    || expression_contains_member_assignment(consequent)
                    || expression_contains_member_assignment(alternate)
            }
            Expr::Template { expressions, .. } => expressions
                .iter()
                .any(expression_contains_member_assignment),
            Expr::Function(function) => match &function.body {
                FunctionBody::Block(statements) => {
                    statements.iter().any(contains_member_assignment)
                }
                FunctionBody::Expression(expr) => expression_contains_member_assignment(expr),
            },
            Expr::Call { callee, args } => {
                call_may_mutate_iterable(callee)
                    || args.iter().any(expression_contains_member_assignment)
            }
            Expr::Undefined
            | Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Ident(_)
            | Expr::Update { .. } => false,
        }
    }

    fn call_may_mutate_iterable(callee: &Expr) -> bool {
        match callee {
            Expr::Ident(name) => !matches!(name.as_str(), "print" | "finish" | "wake"),
            Expr::Member {
                object,
                property: MemberProperty::Field(method),
            } => {
                let known_global = module_path(object)
                    .and_then(|path| path.first().cloned())
                    .is_some_and(|owner| {
                        matches!(
                            owner.as_str(),
                            "Object"
                                | "Array"
                                | "String"
                                | "Number"
                                | "JSON"
                                | "Math"
                                | "Date"
                                | "console"
                        )
                    });
                !(known_global || is_instance_stdlib_method(method))
            }
            _ => true,
        }
    }

    match stmt {
        Stmt::Expr(expr) | Stmt::Throw(expr) => expression_contains_member_assignment(expr),
        Stmt::Block(statements) => statements.iter().any(contains_member_assignment),
        Stmt::Var { declarations, .. } => declarations.iter().any(|declaration| {
            declaration
                .init
                .as_ref()
                .is_some_and(expression_contains_member_assignment)
        }),
        Stmt::Function { function, .. } => match &function.body {
            FunctionBody::Block(statements) => statements.iter().any(contains_member_assignment),
            FunctionBody::Expression(expr) => expression_contains_member_assignment(expr),
        },
        Stmt::Return(value) => value
            .as_ref()
            .is_some_and(expression_contains_member_assignment),
        Stmt::If {
            test,
            consequent,
            alternate,
        } => {
            expression_contains_member_assignment(test)
                || contains_member_assignment(consequent)
                || alternate.as_deref().is_some_and(contains_member_assignment)
        }
        Stmt::While { test, body } => {
            expression_contains_member_assignment(test) || contains_member_assignment(body)
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            init.as_deref().is_some_and(contains_member_assignment)
                || test
                    .as_ref()
                    .is_some_and(expression_contains_member_assignment)
                || update
                    .as_ref()
                    .is_some_and(expression_contains_member_assignment)
                || contains_member_assignment(body)
        }
        Stmt::ForOf { iterable, body, .. } => {
            expression_contains_member_assignment(iterable) || contains_member_assignment(body)
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            body.iter().any(contains_member_assignment)
                || catch
                    .as_ref()
                    .is_some_and(|catch| catch.body.iter().any(contains_member_assignment))
                || finally
                    .as_ref()
                    .is_some_and(|body| body.iter().any(contains_member_assignment))
        }
        Stmt::Empty | Stmt::Break | Stmt::Continue => false,
    }
}
