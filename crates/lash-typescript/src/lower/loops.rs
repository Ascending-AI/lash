use super::*;

impl Lowerer {
    /// Lowers ECMA-262's ForStatement: any head (`let`, `const`, `var`, an
    /// expression, or none), any condition (none reads as `true`) and any
    /// update (none runs nothing).
    ///
    /// The loop lowers to `Block([init.., While(test, Block([body, update?]))])`.
    /// A `continue` runs the update before it continues, and a `continue`
    /// that would cross a `finally` to reach the update refuses, since the
    /// update would otherwise run before the `finally` body (register entry
    /// 23).
    ///
    /// A closure copies what it captures, so ECMA-262's per-iteration `let`
    /// copies (CreatePerIterationEnvironment) are the capture ledger's to
    /// judge. The head runs once, outside the loop, in the copy the first
    /// iteration's copy is taken from, so the head's `let` bindings enter the
    /// ledger again, inside the loop, once the head has lowered: a closure the
    /// head creates keeps the head's values, and one the loop creates keeps
    /// its iteration's. The update runs in the next iteration's copy, before
    /// that iteration's test and body, so it is lowered ahead of both.
    pub(super) fn lower_classic_for(
        &mut self,
        init: Option<&Stmt>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> Result<LashExpr, Diagnostic> {
        if update.is_some() && continue_under_finally(body, false, 0) {
            return Err(Diagnostic::new(
                DiagnosticCode::ForUnsupported,
                "a classic-for `continue` that crosses a `finally` is not supported: the loop's update would run before the `finally` body",
                None,
            ));
        }
        self.scopes.push(Scope::default());
        let result = self.lower_classic_for_in_scope(init, test, update, body);
        self.scopes.pop();
        result
    }

    fn lower_classic_for_in_scope(
        &mut self,
        init: Option<&Stmt>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> Result<LashExpr, Diagnostic> {
        let mut per_iteration = Vec::new();
        let mut output = match init {
            Some(init) => {
                if let Stmt::Var {
                    kind: kind @ (VarKind::Let | VarKind::Const),
                    declarations,
                } = init.unlabeled()
                {
                    self.predeclare(std::slice::from_ref(init), false)?;
                    if *kind == VarKind::Let {
                        for declaration in declarations {
                            pattern_names(&declaration.pattern, &mut per_iteration);
                        }
                    }
                }
                self.lower_stmt(init)?
            }
            None => Vec::new(),
        };
        let lowered_loop = self.in_loop_statement(|lowerer| {
            for name in &per_iteration {
                lowerer.begin_iteration_copy(name);
            }
            let update = update
                .map(|update| lowerer.lower_for_update(update))
                .transpose()?;
            let condition = test
                .map(|test| lowerer.lower_expr(test))
                .transpose()?
                .unwrap_or(LashExpr::Bool(true));
            let body = lowerer.with_loop(|lowerer| {
                lowerer.continue_epilogues.push(update.clone());
                let body = lowerer.lower_stmt_block(body);
                lowerer.continue_epilogues.pop();
                body
            })?;
            let mut iteration = vec![body];
            iteration.extend(update);
            Ok(LashExpr::While {
                condition: Box::new(condition),
                body: Box::new(LashExpr::Block(iteration)),
            })
        })?;
        output.push(lowered_loop);
        Ok(LashExpr::Block(output))
    }

    /// Re-enters a head `let` binding into the capture ledger as the copy the
    /// loop's iterations are taken from: ECMA-262 copies the binding before
    /// the first test, so nothing the loop assigns reaches a closure the head
    /// created.
    fn begin_iteration_copy(&mut self, name: &str) {
        let id = self.declare_in_ledger(name, BindingKind::Let);
        #[expect(
            clippy::expect_used,
            reason = "the head's scope is the innermost one, and it declared this name"
        )]
        let binding = self
            .scopes
            .last_mut()
            .and_then(|scope| scope.bindings.get_mut(name))
            .expect("a head binding lives in the loop's scope");
        binding.id = id;
    }

    /// The update expression, run for its effect. `x++`, `++x`, `x--` and
    /// `--x` on a binding lower to the one assignment `x = x - -1` (or
    /// `x = x - 1`): a subtraction converts its operand with ToNumeric exactly
    /// as the update operator does, and `x - -1` is `x + 1` for every Number,
    /// so the old value needs no temporary. Any other update lowers as the
    /// expression statement it is.
    fn lower_for_update(&mut self, update: &Expr) -> Result<LashExpr, Diagnostic> {
        match update {
            Expr::Update {
                target: TsAssignTarget::Ident(name),
                delta,
                ..
            } => self.lower_update_statement(name, *delta),
            update => self.lower_expr(update),
        }
    }

    pub(super) fn lower_update_statement(
        &mut self,
        name: &str,
        delta: f64,
    ) -> Result<LashExpr, Diagnostic> {
        let target = self.lower_assign_target(&TsAssignTarget::Ident(name.to_string()))?;
        self.clear_process_handle_role(name)?;
        Ok(LashExpr::Assign {
            target,
            expr: Box::new(LashExpr::JavaScriptBinary {
                left: Box::new(LashExpr::Variable(self.resolve(name)?.into())),
                op: JavaScriptBinaryOp::Subtract,
                right: Box::new(LashExpr::Number(-delta)),
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
