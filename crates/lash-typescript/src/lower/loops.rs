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
        let Expr::Ident(condition_name, _) = left.as_ref() else {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for condition must read its loop binding",
                None,
            ));
        };
        let Some(Expr::Update {
            target: TsAssignTarget::Ident(update_name),
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
        let Some(declaration_name) = single_pattern_name(&declaration.pattern) else {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for requires one identifier binding",
                None,
            ));
        };
        if condition_name != declaration_name || update_name != declaration_name || *delta != 1.0 {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "classic for binding, condition, and update must name the same identifier",
                None,
            ));
        }

        self.scopes.push(Scope::default());
        self.declare(declaration_name, BindingKind::Let, true, false)?;
        let internal = self.binding(declaration_name)?.internal.clone();
        let start = self.lower_expr(start)?;
        #[expect(
            clippy::expect_used,
            reason = "the classic-for validation above refuses a loop without a condition"
        )]
        let condition = self.lower_expr(test.expect("validated classic for condition"))?;
        // The increment is lowered ahead of the body on purpose: it writes the
        // next iteration's copy of the binding (ECMA-262's
        // CreatePerIterationEnvironment runs before it), so for the capture
        // ledger it precedes every closure the body creates.
        let update = self.lower_update_statement(declaration_name, *delta)?;
        let body = self.with_loop(|lowerer| {
            lowerer.continue_epilogues.push(Some(update.clone()));
            let body = lowerer.lower_stmt_block(body);
            lowerer.continue_epilogues.pop();
            body
        })?;
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

fn continue_under_finally(stmt: &Stmt, protected: bool, nested_loop_depth: usize) -> bool {
    match stmt {
        Stmt::Spanned(_, stmt) | Stmt::Labeled { stmt, .. } => {
            continue_under_finally(stmt, protected, nested_loop_depth)
        }
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
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForOf { body, .. }
        | Stmt::ForIn { body, .. } => {
            continue_under_finally(body, protected, nested_loop_depth + 1)
        }
        Stmt::Switch { cases, .. } => cases.iter().any(|case| {
            case.consequent
                .iter()
                .any(|stmt| continue_under_finally(stmt, protected, nested_loop_depth))
        }),
        Stmt::Empty
        | Stmt::Expr(_)
        | Stmt::Enum { .. }
        | Stmt::Var { .. }
        | Stmt::Function { .. }
        | Stmt::Return(_)
        | Stmt::Break
        | Stmt::Throw(_) => false,
    }
}
