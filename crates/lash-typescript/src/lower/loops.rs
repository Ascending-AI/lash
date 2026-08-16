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
    fn any(exprs: &[Expr], binding: &str) -> bool {
        exprs.iter().any(|expr| mentions_binding(expr, binding))
    }
    match expr {
        Expr::Ident(name) => name.as_str() == binding,
        Expr::Array(items) => items.iter().any(|item| match item {
            ArrayElement::Value(value) | ArrayElement::Spread(value) => {
                mentions_binding(value, binding)
            }
        }),
        Expr::Object(entries) => entries.iter().any(|property| match property {
            ObjectProperty::KeyValue(key, value) => {
                matches!(key, PropertyKey::Computed(key) if mentions_binding(key, binding))
                    || mentions_binding(value, binding)
            }
            ObjectProperty::Spread(value) => mentions_binding(value, binding),
        }),
        Expr::Assign { value, target, .. } => {
            mentions_binding(value, binding)
                || match target {
                    TsAssignTarget::Member { object, property } => {
                        mentions_binding(object, binding)
                            || match property {
                                MemberProperty::Index(index) => mentions_binding(index, binding),
                                MemberProperty::Field(_) => false,
                            }
                    }
                    TsAssignTarget::Ident(name) => name.as_str() == binding,
                    TsAssignTarget::Pattern(pattern) => pattern_mentions_binding(pattern, binding),
                }
        }
        Expr::Unary { value, .. } | Expr::Await(value) => mentions_binding(value, binding),
        Expr::Member { object, property } => {
            mentions_binding(object, binding)
                || match property {
                    MemberProperty::Index(index) => mentions_binding(index, binding),
                    MemberProperty::Field(_) => false,
                }
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            mentions_binding(left, binding) || mentions_binding(right, binding)
        }
        Expr::Conditional {
            test,
            consequent,
            alternate,
        } => {
            mentions_binding(test, binding)
                || mentions_binding(consequent, binding)
                || mentions_binding(alternate, binding)
        }
        Expr::Template { expressions, .. } => any(expressions, binding),
        Expr::Call { callee, args } => {
            mentions_binding(callee, binding)
                || args.iter().any(|arg| match arg {
                    CallArg::Value(value) | CallArg::Spread(value) => {
                        mentions_binding(value, binding)
                    }
                })
        }
        Expr::New { args, .. } => args.iter().any(|arg| match arg {
            CallArg::Value(value) | CallArg::Spread(value) => mentions_binding(value, binding),
        }),
        Expr::OptionalChain { base, operations } => {
            mentions_binding(base, binding)
                || operations.iter().any(|operation| match operation {
                    OptionalOperation::Member { property, .. } => {
                        matches!(property, MemberProperty::Index(index) if mentions_binding(index, binding))
                    }
                    OptionalOperation::Call { args, .. } => args.iter().any(|arg| match arg {
                        CallArg::Value(value) | CallArg::Spread(value) => {
                            mentions_binding(value, binding)
                        }
                    }),
                })
        }
        Expr::Function(function) => match &function.body {
            FunctionBody::Block(statements) => statements
                .iter()
                .any(|stmt| statement_mentions_binding(stmt, binding)),
            FunctionBody::Expression(expr) => mentions_binding(expr, binding),
        },
        Expr::Undefined
        | Expr::Null
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::RegExp { .. } => false,
        Expr::This => false,
        Expr::LoneSurrogateString => false,
        Expr::Update { target, .. } => assign_target_mentions_binding(target, binding),
        Expr::Delete { object, property } => {
            mentions_binding(object, binding)
                || matches!(property, MemberProperty::Index(index) if mentions_binding(index, binding))
        }
    }
}

fn assign_target_mentions_binding(target: &TsAssignTarget, binding: &str) -> bool {
    match target {
        TsAssignTarget::Ident(name) => name == binding,
        TsAssignTarget::Member { object, property } => {
            mentions_binding(object, binding)
                || matches!(property, MemberProperty::Index(index) if mentions_binding(index, binding))
        }
        TsAssignTarget::Pattern(pattern) => pattern_mentions_binding(pattern, binding),
    }
}

fn pattern_mentions_binding(pattern: &Pattern, binding: &str) -> bool {
    match pattern {
        Pattern::Ident(name) => name == binding,
        Pattern::Rest(target) => pattern_mentions_binding(target, binding),
        Pattern::Member { object, property } => {
            mentions_binding(object, binding)
                || matches!(property, MemberProperty::Index(index) if mentions_binding(index, binding))
        }
        Pattern::Assign { target, default } => {
            pattern_mentions_binding(target, binding) || mentions_binding(default, binding)
        }
        Pattern::Array { elements, rest } => {
            elements
                .iter()
                .flatten()
                .any(|pattern| pattern_mentions_binding(pattern, binding))
                || rest
                    .as_deref()
                    .is_some_and(|pattern| pattern_mentions_binding(pattern, binding))
        }
        Pattern::Object { properties, rest } => properties.iter().any(|property| {
            matches!(&property.key, PropertyKey::Computed(key) if mentions_binding(key, binding))
                || pattern_mentions_binding(&property.value, binding)
        }) || rest
            .as_deref()
            .is_some_and(|pattern| pattern_mentions_binding(pattern, binding)),
    }
}

fn statement_mentions_binding(stmt: &Stmt, binding: &str) -> bool {
    statement_expressions(stmt).any(|expr| mentions_binding(expr, binding))
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
    for expr in statement_expressions(body) {
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
            Expr::Array(items) => items.iter().any(|item| match item {
                ArrayElement::Value(value) | ArrayElement::Spread(value) => {
                    names_iterable_directly(value, binding)
                }
            }),
            Expr::Object(entries) => entries.iter().any(|property| match property {
                ObjectProperty::KeyValue(_, value) | ObjectProperty::Spread(value) => {
                    names_iterable_directly(value, binding)
                }
            }),
            _ => false,
        }
    }
    fn walk(stmt: &Stmt, binding: &str) -> Option<String> {
        match stmt {
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
            Stmt::Expr(Expr::Assign { value, .. }) if names_iterable_directly(value, binding) => {
                Some(format!(
                    "assigns `{binding}`, the iterable this loop is walking, to another binding"
                ))
            }
            Stmt::Block(statements) => statements.iter().find_map(|stmt| walk(stmt, binding)),
            Stmt::If {
                consequent,
                alternate,
                ..
            } => walk(consequent, binding)
                .or_else(|| alternate.as_deref().and_then(|stmt| walk(stmt, binding))),
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::For { body, .. }
            | Stmt::ForOf { body, .. }
            | Stmt::ForIn { body, .. } => walk(body, binding),
            Stmt::Switch { cases, .. } => cases
                .iter()
                .flat_map(|case| &case.consequent)
                .find_map(|stmt| walk(stmt, binding)),
            Stmt::Try {
                body,
                catch,
                finally,
            } => body
                .iter()
                .find_map(|stmt| walk(stmt, binding))
                .or_else(|| {
                    catch
                        .iter()
                        .find_map(|catch| catch.body.iter().find_map(|stmt| walk(stmt, binding)))
                })
                .or_else(|| {
                    finally
                        .iter()
                        .find_map(|stmts| stmts.iter().find_map(|stmt| walk(stmt, binding)))
                }),
            _ => None,
        }
    }
    walk(stmt, binding)
}

fn expression_may_mutate(expr: &Expr, binding: &str) -> Option<String> {
    let mut found = None;
    visit_expressions(expr, &mut |expr| {
        if found.is_some() {
            return;
        }
        match expr {
            Expr::Assign {
                target: TsAssignTarget::Member { object, .. },
                ..
            } if expression_root_binding(object) == Some(binding) => {
                found = Some(format!(
                    "assigns through `{binding}`, the iterable this loop is walking"
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
            _ => {}
        }
    });
    found
}

/// Every expression a statement contains, including nested bodies.
fn statement_expressions(stmt: &Stmt) -> Box<dyn Iterator<Item = &Expr> + '_> {
    match stmt {
        Stmt::Expr(expr) | Stmt::Throw(expr) => Box::new(std::iter::once(expr)),
        Stmt::Return(value) => Box::new(value.iter()),
        Stmt::Block(statements) => Box::new(statements.iter().flat_map(statement_expressions)),
        Stmt::Var { declarations, .. } => {
            Box::new(declarations.iter().filter_map(|d| d.init.as_ref()))
        }
        Stmt::Enum { members, .. } => Box::new(members.iter().map(|member| &member.value)),
        Stmt::If {
            test,
            consequent,
            alternate,
        } => Box::new(
            std::iter::once(test)
                .chain(statement_expressions(consequent))
                .chain(
                    alternate
                        .iter()
                        .flat_map(|stmt| statement_expressions(stmt)),
                ),
        ),
        Stmt::While { test, body } => {
            Box::new(std::iter::once(test).chain(statement_expressions(body)))
        }
        Stmt::DoWhile { body, test } => {
            Box::new(statement_expressions(body).chain(std::iter::once(test)))
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => Box::new(
            init.iter()
                .flat_map(|stmt| statement_expressions(stmt))
                .chain(test.iter())
                .chain(update.iter())
                .chain(statement_expressions(body)),
        ),
        Stmt::ForOf { iterable, body, .. } => {
            Box::new(std::iter::once(iterable).chain(statement_expressions(body)))
        }
        Stmt::ForIn { object, body, .. } => {
            Box::new(std::iter::once(object).chain(statement_expressions(body)))
        }
        Stmt::Switch {
            discriminant,
            cases,
        } => Box::new(
            std::iter::once(discriminant)
                .chain(cases.iter().filter_map(|case| case.test.as_ref()))
                .chain(
                    cases
                        .iter()
                        .flat_map(|case| case.consequent.iter().flat_map(statement_expressions)),
                ),
        ),
        Stmt::Try {
            body,
            catch,
            finally,
        } => Box::new(
            body.iter()
                .flat_map(statement_expressions)
                .chain(
                    catch
                        .iter()
                        .flat_map(|catch| catch.body.iter().flat_map(statement_expressions)),
                )
                .chain(
                    finally
                        .iter()
                        .flat_map(|stmts| stmts.iter().flat_map(statement_expressions)),
                ),
        ),
        Stmt::Function { function, .. } => match &function.body {
            FunctionBody::Block(statements) => {
                Box::new(statements.iter().flat_map(statement_expressions))
            }
            FunctionBody::Expression(expr) => Box::new(std::iter::once(expr.as_ref())),
        },
        Stmt::Empty | Stmt::Break | Stmt::Continue => Box::new(std::iter::empty()),
    }
}

/// Walk every sub-expression, outermost first.
fn visit_expressions<'a>(expr: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
    visit(expr);
    let mut walk = |expr: &'a Expr| visit_expressions(expr, visit);
    match expr {
        Expr::Array(items) => items.iter().for_each(|item| match item {
            ArrayElement::Value(value) | ArrayElement::Spread(value) => walk(value),
        }),
        Expr::Object(entries) => entries.iter().for_each(|property| match property {
            ObjectProperty::KeyValue(key, value) => {
                if let PropertyKey::Computed(key) = key {
                    walk(key);
                }
                walk(value);
            }
            ObjectProperty::Spread(value) => walk(value),
        }),
        Expr::Assign { value, target, .. } => {
            walk(value);
            if let TsAssignTarget::Member { object, property } = target {
                walk(object);
                if let MemberProperty::Index(index) = property {
                    walk(index);
                }
            }
        }
        Expr::Unary { value, .. } | Expr::Await(value) => walk(value),
        Expr::Member { object, property } => {
            walk(object);
            if let MemberProperty::Index(index) = property {
                walk(index);
            }
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            walk(left);
            walk(right);
        }
        Expr::Conditional {
            test,
            consequent,
            alternate,
        } => {
            walk(test);
            walk(consequent);
            walk(alternate);
        }
        Expr::Template { expressions, .. } => expressions.iter().for_each(walk),
        Expr::Call { callee, args } => {
            walk(callee);
            args.iter().for_each(|arg| match arg {
                CallArg::Value(value) | CallArg::Spread(value) => walk(value),
            });
        }
        Expr::New { args, .. } => args.iter().for_each(|arg| match arg {
            CallArg::Value(value) | CallArg::Spread(value) => walk(value),
        }),
        Expr::OptionalChain { base, operations } => {
            walk(base);
            for operation in operations {
                match operation {
                    OptionalOperation::Member {
                        property: MemberProperty::Index(index),
                        ..
                    } => walk(index),
                    OptionalOperation::Member { .. } => {}
                    OptionalOperation::Call { args, .. } => {
                        for arg in args {
                            match arg {
                                CallArg::Value(value) | CallArg::Spread(value) => walk(value),
                            }
                        }
                    }
                }
            }
        }
        Expr::Function(function) => match &function.body {
            FunctionBody::Block(statements) => statements
                .iter()
                .flat_map(statement_expressions)
                .for_each(walk),
            FunctionBody::Expression(expr) => walk(expr),
        },
        Expr::Undefined
        | Expr::Null
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Ident(_)
        | Expr::RegExp { .. }
        | Expr::This
        | Expr::LoneSurrogateString => {}
        Expr::Update { target, .. } => {
            if let TsAssignTarget::Member { object, property } = target {
                walk(object);
                if let MemberProperty::Index(index) = property {
                    walk(index);
                }
            }
        }
        Expr::Delete { object, property } => {
            walk(object);
            if let MemberProperty::Index(index) = property {
                walk(index);
            }
        }
    }
}
