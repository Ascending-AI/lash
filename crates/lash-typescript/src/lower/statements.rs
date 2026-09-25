use super::*;

/// How a lowered statement list scopes its declarations.
pub(super) enum StatementScope {
    /// A function body or the cell itself: its `var`s hoist to this scope.
    Root,
    /// A block or another nested statement list: its own declarative scope.
    Nested,
    /// A switch consequent: the case block's scope, which `lower_switch`
    /// pushes once and predeclares over every case — ECMA-262 gives the
    /// whole case block one declarative environment.
    Case,
}

impl Lowerer {
    pub(super) fn lower_statements(
        &mut self,
        statements: &[Stmt],
        scope: StatementScope,
    ) -> Result<Vec<LashExpr>, Diagnostic> {
        if matches!(scope, StatementScope::Nested) {
            self.scopes.push(Scope::default());
        }
        let hoisted_vars = if matches!(scope, StatementScope::Root) {
            let mut hoisted = Vec::new();
            for name in function_var_names(statements) {
                let existing = self
                    .scopes
                    .last()
                    .and_then(|scope| scope.bindings.get(&name));
                if let Some(binding) = existing {
                    if !matches!(
                        binding.kind,
                        BindingKind::Var | BindingKind::Function | BindingKind::Parameter
                    ) {
                        return Err(Diagnostic::new(
                            DiagnosticCode::DuplicateBinding,
                            format!("var `{name}` conflicts with a lexical binding"),
                            None,
                        ));
                    }
                    if binding.kind == BindingKind::Parameter {
                        let parameter = binding.internal.clone();
                        hoisted.extend(self.separate_parameter_var(&name, &parameter));
                    }
                    continue;
                }
                self.declare(&name, BindingKind::Var, true, true)?;
                let binding = self.binding(&name)?.clone();
                hoisted.push(LashExpr::Assign {
                    target: AssignTarget::variable(binding.internal.as_str().into()),
                    expr: Box::new(self.binding_initial_value(&binding, LashExpr::Undefined)),
                });
            }
            hoisted
        } else {
            Vec::new()
        };
        if !matches!(scope, StatementScope::Case) {
            self.predeclare(statements, matches!(scope, StatementScope::Root))?;
        }
        if matches!(scope, StatementScope::Root) && self.current_function() == 0 {
            self.declare_global_this_slots()?;
        }

        let local_function_internals = statements
            .iter()
            .filter_map(|statement| match statement.unlabeled() {
                Stmt::Function { name, .. } => {
                    Some(self.binding(name).map(|binding| binding.internal.clone()))
                }
                _ => None,
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let current_function = self.current_function();
        let mut available = self
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .filter(|binding| {
                // A binding owned by an enclosing function reaches this frame as
                // a capture or a parameter, so it already holds a value whenever
                // these statements run. Only bindings this frame declares itself
                // have to be ordered against the declarations that fill them.
                (binding.initialized || binding.owner_function != current_function)
                    && !local_function_internals.contains(&binding.internal)
            })
            .map(|binding| binding.internal.clone())
            .collect::<BTreeSet<_>>();
        let mut pending = Vec::with_capacity(local_function_internals.len());
        let previous_capture_mode = self.allow_uninitialized_declaration_capture;
        self.allow_uninitialized_declaration_capture = true;
        for statement in statements {
            if let Stmt::Function { name, function } = statement.unlabeled() {
                let binding = self.binding(name)?.clone();
                if function.is_async {
                    self.set_role(name, BindingRole::AsyncHelper)?;
                }
                let expression = self.lower_function(function, Some(binding.internal.clone()))?;
                let definition = match &expression {
                    LashExpr::Function(definition) => definition.as_ref(),
                    LashExpr::BuiltinCall { name, args } => {
                        let [LashExpr::Function(definition), ..] = args.as_slice() else {
                            unreachable!("closure intrinsic starts with a function literal")
                        };
                        debug_assert_eq!(name.as_str(), "__typescript_closure");
                        definition
                    }
                    _ => unreachable!("function lowering returns a function expression"),
                };
                let captures = definition
                    .captures
                    .iter()
                    .map(|capture| capture.as_str().to_string())
                    .collect();
                pending.push(PendingFunction {
                    internal: binding.internal.clone(),
                    captures,
                    expression: self.binding_initial_value(&binding, expression),
                });
            }
        }
        self.allow_uninitialized_declaration_capture = previous_capture_mode;
        reject_mutual_recursion(&pending, statements, self)?;
        let mut pending = pending
            .into_iter()
            .map(|function| PendingBinding {
                internal: function.internal.clone(),
                captures: function.captures,
                assignment: LashExpr::Assign {
                    target: AssignTarget::variable(function.internal.into()),
                    expr: Box::new(function.expression),
                },
            })
            .collect::<Vec<_>>();

        // A function declaration holds its value only once it is emitted,
        // which waits until every binding it captures holds one. Until then
        // a read of it is a read before initialization, refused by name like
        // any other, rather than of a name that has no assignment yet.
        for binding in &pending {
            self.set_local_initialized(&binding.internal, false);
        }
        let flush_ready = |pending: &mut Vec<PendingBinding>,
                           available: &mut BTreeSet<String>,
                           output: &mut Vec<LashExpr>| {
            let mut flushed = Vec::new();
            while let Some(index) = pending.iter().position(|binding| {
                binding
                    .captures
                    .iter()
                    .all(|capture| available.contains(capture))
            }) {
                let binding = pending.remove(index);
                available.insert(binding.internal.clone());
                flushed.push(binding.internal);
                output.push(binding.assignment);
            }
            flushed
        };
        let mut output = hoisted_vars;
        // A `break` that exits a switch lowers to a flag assignment rather
        // than a jump, so the statements that follow one — directly or
        // through a nested block or branch — must stop running once it
        // fires. Everything after the first statement that may assign the
        // flag is emitted gated on it (FIG-3714).
        let mut dead = Vec::new();
        let mut switch_broken = None;
        for statement in statements {
            for internal in flush_ready(&mut pending, &mut available, &mut output) {
                self.set_local_initialized(&internal, true);
            }
            if !matches!(statement.unlabeled(), Stmt::Function { .. }) {
                let lowered = self.lower_stmt(statement)?;
                if switch_broken.is_some() {
                    dead.extend(lowered);
                } else {
                    output.extend(lowered);
                }
            }
            if let Stmt::Var { declarations, .. } = statement.unlabeled() {
                for declaration in declarations {
                    let mut names = Vec::new();
                    pattern_names(&declaration.pattern, &mut names);
                    for name in names {
                        available.insert(self.binding(&name)?.internal.clone());
                    }
                }
            }
            if switch_broken.is_none()
                && Self::may_break_switch(statement)
                && let Some(entry) = self.switch_breaks.last()
                && !entry.abrupt
                && entry.loop_depth == self.position.loop_depth
            {
                switch_broken = Some(entry.flag.clone());
            }
        }
        for internal in flush_ready(&mut pending, &mut available, &mut output) {
            self.set_local_initialized(&internal, true);
        }
        if let Some(function) = pending.first() {
            return Err(Diagnostic::new(
                DiagnosticCode::TemporalDeadZone,
                format!(
                    "function `{}` captures a binding that is unavailable at declaration time",
                    function.internal
                ),
                None,
            ));
        }
        if let Some(flag) = switch_broken
            && !dead.is_empty()
        {
            output.push(LashExpr::If {
                condition: Box::new(js_unary(JavaScriptUnaryOp::Not, Self::variable(&flag))),
                then_block: Box::new(LashExpr::Block(dead)),
                else_block: Box::new(LashExpr::Undefined),
            });
        }
        if matches!(scope, StatementScope::Nested) {
            self.scopes.pop();
        }
        Ok(output)
    }

    /// Whether `statement` can assign the enclosing switch's break flag: a
    /// `break` it contains, short of a nested `switch`, a loop body, or a
    /// function — each binds `break` to a target of its own.
    fn may_break_switch(statement: &Stmt) -> bool {
        match statement.unlabeled() {
            Stmt::Break => true,
            Stmt::Block(statements) => statements.iter().any(Self::may_break_switch),
            Stmt::If {
                consequent,
                alternate,
                ..
            } => {
                Self::may_break_switch(consequent)
                    || alternate.as_deref().is_some_and(Self::may_break_switch)
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                body.iter().any(Self::may_break_switch)
                    || catch
                        .iter()
                        .any(|catch| catch.body.iter().any(Self::may_break_switch))
                    || finally
                        .iter()
                        .any(|finally| finally.iter().any(Self::may_break_switch))
            }
            // A `for` head's init is the only part of a loop that runs at
            // the enclosing switch's depth.
            Stmt::For { init, .. } => init.as_deref().is_some_and(Self::may_break_switch),
            _ => false,
        }
    }

    /// Whether a `break` bound to the enclosing switch — one no nested loop,
    /// switch or function binds instead — sits under a `finally` clause,
    /// where it can run while another completion is still unwinding.
    pub(super) fn break_through_finally(statement: &Stmt, in_finally: bool) -> bool {
        match statement.unlabeled() {
            Stmt::Break => in_finally,
            Stmt::Block(statements) => statements
                .iter()
                .any(|statement| Self::break_through_finally(statement, in_finally)),
            Stmt::If {
                consequent,
                alternate,
                ..
            } => {
                Self::break_through_finally(consequent, in_finally)
                    || alternate
                        .as_deref()
                        .is_some_and(|statement| Self::break_through_finally(statement, in_finally))
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                body.iter()
                    .any(|statement| Self::break_through_finally(statement, in_finally))
                    || catch.iter().any(|catch| {
                        catch
                            .body
                            .iter()
                            .any(|statement| Self::break_through_finally(statement, in_finally))
                    })
                    || finally.iter().any(|finally| {
                        finally
                            .iter()
                            .any(|statement| Self::break_through_finally(statement, true))
                    })
            }
            _ => false,
        }
    }
}
