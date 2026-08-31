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
        let condition = self.lower_expr(test.expect("validated classic for condition"))?;
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

/// The root binding an expression reaches, if it names one.
///
/// `urls` and `urls[0]` and `obj.items` all root at a single identifier; a call
/// result or a literal roots at nothing, and nothing in the body can name it.
fn expression_root_binding(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Ident(name) => Some(name.as_str()),
        Expr::Member { object, .. } => expression_root_binding(object),
        _ => None,
    }
}

/// Whether an expression mentions `binding` anywhere.
fn mentions_binding(expr: &Expr, binding: &str) -> bool {
    matches!(expr, Expr::Ident(name) if name == binding)
        || matches!(expr, Expr::Assign { target, .. } | Expr::Update { target, .. }
            if assign_target_names_binding(target, binding))
        || expr
            .children()
            .any(|child| mentions_binding(child, binding))
}

fn assign_target_names_binding(target: &TsAssignTarget, binding: &str) -> bool {
    match target {
        TsAssignTarget::Ident(name) => name == binding,
        TsAssignTarget::Pattern(pattern) => pattern_names_binding(pattern, binding),
        TsAssignTarget::Member { .. } => false,
    }
}

fn pattern_names_binding(pattern: &Pattern, binding: &str) -> bool {
    match pattern {
        Pattern::Ident(name) => name == binding,
        Pattern::Rest(target) | Pattern::Assign { target, .. } => {
            pattern_names_binding(target, binding)
        }
        Pattern::Member { .. } => false,
        Pattern::Array { elements, rest } => {
            elements
                .iter()
                .flatten()
                .any(|pattern| pattern_names_binding(pattern, binding))
                || rest
                    .as_deref()
                    .is_some_and(|pattern| pattern_names_binding(pattern, binding))
        }
        Pattern::Object { properties, rest } => {
            properties
                .iter()
                .any(|property| pattern_names_binding(&property.value, binding))
                || rest
                    .as_deref()
                    .is_some_and(|pattern| pattern_names_binding(pattern, binding))
        }
    }
}

/// Whether a `for…of` body can reach the iterable it is walking.
///
/// The v1 iterator snapshots the iterable, so mutating it mid-loop would change
/// what the loop is walking. Only shapes that can actually reach it are
/// rejected: an assignment whose target roots at the iterable, a method call on
/// the iterable, or a call that passes the iterable to something that could
/// mutate it. Effects, awaits, and assignments to anything else are ordinary
/// body statements — `for (const url of urls) { const page = await
/// web.fetch({ url }); out = out + page; }` is the loop this language exists to
/// write, and suspending inside it resumes correctly.
///
/// When the iterable names no binding — a call result, a literal — nothing in
/// the body can reach it and the body is unrestricted.
pub(super) fn body_may_mutate_iterable(iterable: &Expr, body: &Stmt) -> Option<String> {
    let binding = expression_root_binding(iterable)?;
    // A body that gives the iterable a second name defeats root tracking: the
    // alias can be written through, the snapshot iterator hides it, and the
    // loop silently diverges from ECMA. Reject binding it rather than trying to
    // follow every alias.
    if let Some(reason) = body_binds_iterable_elsewhere(body, binding) {
        return Some(reason);
    }
    for expr in body.child_expressions() {
        if let Some(reason) = expression_may_mutate(expr, binding) {
            return Some(reason);
        }
    }
    None
}

/// Whether the body stores the iterable under another name — a `const alias =
/// urls`, or boxing it in a structure the loop can reach later.
fn body_binds_iterable_elsewhere(stmt: &Stmt, binding: &str) -> Option<String> {
    fn names_iterable_directly(expr: &Expr, binding: &str) -> bool {
        // `data.items` roots at `data` exactly as `data` does. The mutation
        // half of this filter tracks the root, so this half has to as well:
        // when the two disagree about what "the iterable" is, a member-rooted
        // iterable can be aliased through the gap between them and the loop
        // diverges from ECMA in silence.
        if expression_root_binding(expr) == Some(binding) {
            return true;
        }
        match expr {
            // Boxing it in a literal keeps a live reference to it.
            Expr::Array(_) | Expr::Object(_) => expr
                .children()
                .any(|child| names_iterable_directly(child, binding)),
            _ => false,
        }
    }

    fn pattern_default_binds_iterable(pattern: &Pattern, binding: &str) -> bool {
        match pattern {
            Pattern::Assign { target, default } => {
                names_iterable_directly(default, binding)
                    || pattern_default_binds_iterable(target, binding)
            }
            Pattern::Rest(target) => pattern_default_binds_iterable(target, binding),
            Pattern::Array { elements, rest } => {
                elements
                    .iter()
                    .flatten()
                    .any(|pattern| pattern_default_binds_iterable(pattern, binding))
                    || rest
                        .as_deref()
                        .is_some_and(|pattern| pattern_default_binds_iterable(pattern, binding))
            }
            Pattern::Object { properties, rest } => {
                properties
                    .iter()
                    .any(|property| pattern_default_binds_iterable(&property.value, binding))
                    || rest
                        .as_deref()
                        .is_some_and(|pattern| pattern_default_binds_iterable(pattern, binding))
            }
            Pattern::Ident(_) | Pattern::Member { .. } => false,
        }
    }

    stmt.descendants().find_map(|stmt| match stmt {
        Stmt::ForOf { iterable, .. } if names_iterable_directly(iterable, binding) => {
            Some(format!(
                "binds `{binding}`, the iterable this loop is walking, as an inner for-of iterable"
            ))
        }
        Stmt::Function { function, .. }
            if function
                .params
                .iter()
                .any(|param| pattern_default_binds_iterable(param, binding)) =>
        {
            Some(format!(
                "binds `{binding}`, the iterable this loop is walking, in a parameter default"
            ))
        }
        Stmt::Var { declarations, .. } => declarations.iter().find_map(|declaration| {
            declaration
                .init
                .as_ref()
                .filter(|init| names_iterable_directly(init, binding))
                .map(|_| {
                    format!(
                        "binds `{binding}`, the iterable this loop is walking, to `{}`",
                        single_pattern_name(&declaration.pattern).unwrap_or("a pattern")
                    )
                })
        }),
        Stmt::Expr(Expr::Assign { value, .. }) if names_iterable_directly(value, binding) => Some(
            format!("assigns `{binding}`, the iterable this loop is walking, to another binding"),
        ),
        _ => None,
    })
}

fn expression_may_mutate(expr: &Expr, binding: &str) -> Option<String> {
    let mut found = None;
    visit_expressions(expr, &mut |expr| {
        if found.is_some() {
            return;
        }
        match expr {
            Expr::Assign { target, .. } if assign_target_may_mutate_binding(target, binding) => {
                found = Some(format!(
                    "assigns through `{binding}`, the iterable this loop is walking"
                ));
            }
            Expr::Update { target, .. } if assign_target_may_mutate_binding(target, binding) => {
                found = Some(format!(
                    "updates through `{binding}`, the iterable this loop is walking"
                ));
            }
            Expr::Delete { object, .. } if expression_root_binding(object) == Some(binding) => {
                found = Some(format!(
                    "deletes through `{binding}`, the iterable this loop is walking"
                ));
            }
            Expr::Call { callee, args } => {
                if let Expr::Member {
                    object,
                    property: MemberProperty::Field(method),
                } = callee.as_ref()
                    && expression_root_binding(object) == Some(binding)
                {
                    found = Some(format!(
                        "calls `{binding}.{method}()`, which may mutate the iterable this loop is walking"
                    ));
                } else if args.iter().any(|arg| match arg {
                    CallArg::Value(value) | CallArg::Spread(value) => {
                        mentions_binding(value, binding)
                    }
                }) {
                    found = Some(format!(
                        "passes `{binding}`, the iterable this loop is walking, to a call that may mutate it"
                    ));
                }
            }
            Expr::OptionalChain { base, operations }
                if expression_root_binding(base) == Some(binding)
                    && operations
                        .iter()
                        .any(|operation| matches!(operation, OptionalOperation::Call { .. })) =>
            {
                found = Some(format!(
                    "calls an optional member through `{binding}`, which may mutate the iterable this loop is walking"
                ));
            }
            Expr::New { args, .. }
                if args.iter().any(|arg| match arg {
                    CallArg::Value(value) | CallArg::Spread(value) => {
                        mentions_binding(value, binding)
                    }
                }) =>
            {
                found = Some(format!(
                    "constructs with `{binding}`, the iterable this loop is walking"
                ));
            }
            _ => {}
        }
    });
    found
}

fn assign_target_may_mutate_binding(target: &TsAssignTarget, binding: &str) -> bool {
    match target {
        TsAssignTarget::Member { object, .. } => expression_root_binding(object) == Some(binding),
        TsAssignTarget::Pattern(pattern) => pattern_may_mutate_binding(pattern, binding),
        TsAssignTarget::Ident(_) => false,
    }
}

fn pattern_may_mutate_binding(pattern: &Pattern, binding: &str) -> bool {
    match pattern {
        Pattern::Member { object, .. } => expression_root_binding(object) == Some(binding),
        Pattern::Rest(target) | Pattern::Assign { target, .. } => {
            pattern_may_mutate_binding(target, binding)
        }
        Pattern::Array { elements, rest } => {
            elements
                .iter()
                .flatten()
                .any(|pattern| pattern_may_mutate_binding(pattern, binding))
                || rest
                    .as_deref()
                    .is_some_and(|pattern| pattern_may_mutate_binding(pattern, binding))
        }
        Pattern::Object { properties, rest } => {
            properties
                .iter()
                .any(|property| pattern_may_mutate_binding(&property.value, binding))
                || rest
                    .as_deref()
                    .is_some_and(|pattern| pattern_may_mutate_binding(pattern, binding))
        }
        Pattern::Ident(_) => false,
    }
}

/// Walk every sub-expression, outermost first.
fn visit_expressions<'a>(expr: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
    visit(expr);
    for child in expr.children() {
        visit_expressions(child, visit);
    }
}
