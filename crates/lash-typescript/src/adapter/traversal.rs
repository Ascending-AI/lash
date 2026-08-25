use super::*;

impl Pattern {
    pub(crate) fn child_expressions(&self) -> Box<dyn Iterator<Item = &Expr> + '_> {
        let mut children = Vec::new();
        match self {
            Pattern::Ident(_) => {}
            Pattern::Rest(target) => children.extend(target.child_expressions()),
            Pattern::Member { object, property } => {
                children.push(object.as_ref());
                if let MemberProperty::Index(index) = property {
                    children.push(index);
                }
            }
            Pattern::Assign { target, default } => {
                children.extend(target.child_expressions());
                children.push(default);
            }
            Pattern::Array { elements, rest } => {
                children.extend(
                    elements
                        .iter()
                        .flatten()
                        .flat_map(Pattern::child_expressions),
                );
                children.extend(rest.iter().flat_map(|rest| rest.child_expressions()));
            }
            Pattern::Object { properties, rest } => {
                for property in properties {
                    if let PropertyKey::Computed(key) = &property.key {
                        children.push(key);
                    }
                    children.extend(property.value.child_expressions());
                }
                children.extend(rest.iter().flat_map(|rest| rest.child_expressions()));
            }
        }
        Box::new(children.into_iter())
    }
}

impl AssignTarget {
    fn child_expressions(&self) -> Box<dyn Iterator<Item = &Expr> + '_> {
        let mut children = Vec::new();
        match self {
            AssignTarget::Ident(_) => {}
            AssignTarget::Member { object, property } => {
                children.push(object.as_ref());
                if let MemberProperty::Index(index) = property {
                    children.push(index);
                }
            }
            AssignTarget::Pattern(pattern) => children.extend(pattern.child_expressions()),
        }
        Box::new(children.into_iter())
    }
}

impl Expr {
    pub(crate) fn children(&self) -> Box<dyn Iterator<Item = &Expr> + '_> {
        let mut children = Vec::new();
        match self {
            Expr::Array(items) => children.extend(items.iter().map(|item| match item {
                ArrayElement::Value(value) | ArrayElement::Spread(value) => value,
            })),
            Expr::Object(properties) => {
                for property in properties {
                    match property {
                        ObjectProperty::KeyValue(key, value) => {
                            if let PropertyKey::Computed(key) = key {
                                children.push(key.as_ref());
                            }
                            children.push(value);
                        }
                        ObjectProperty::Spread(value) => children.push(value),
                    }
                }
            }
            Expr::Assign { target, value, .. } => {
                children.push(value);
                children.extend(target.child_expressions());
            }
            Expr::Member { object, property } | Expr::Delete { object, property } => {
                children.push(object);
                if let MemberProperty::Index(index) = property {
                    children.push(index);
                }
            }
            Expr::Unary { value, .. } | Expr::Await(value) => children.push(value),
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                children.push(left);
                children.push(right);
            }
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                children.push(test);
                children.push(consequent);
                children.push(alternate);
            }
            Expr::Template { expressions, .. } => children.extend(expressions),
            Expr::Function(function) => {
                children.extend(function.params.iter().flat_map(Pattern::child_expressions));
                match &function.body {
                    FunctionBody::Block(statements) => {
                        children.extend(statements.iter().flat_map(Stmt::child_expressions));
                    }
                    FunctionBody::Expression(expression) => children.push(expression),
                }
            }
            Expr::Call { callee, args } => {
                children.push(callee);
                children.extend(args.iter().map(|arg| match arg {
                    CallArg::Value(value) | CallArg::Spread(value) => value,
                }));
            }
            Expr::New { args, .. } => children.extend(args.iter().map(|arg| match arg {
                CallArg::Value(value) | CallArg::Spread(value) => value,
            })),
            Expr::OptionalChain { base, operations } => {
                children.push(base);
                for operation in operations {
                    match operation {
                        OptionalOperation::Member {
                            property: MemberProperty::Index(index),
                            ..
                        } => children.push(index),
                        OptionalOperation::Member { .. } => {}
                        OptionalOperation::Call { args, .. } => {
                            children.extend(args.iter().map(|arg| match arg {
                                CallArg::Value(value) | CallArg::Spread(value) => value,
                            }));
                        }
                    }
                }
            }
            Expr::Update { target, .. } => children.extend(target.child_expressions()),
            Expr::Undefined
            | Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::RegExp { .. }
            | Expr::Ident(_)
            | Expr::This
            | Expr::LoneSurrogateString => {}
        }
        Box::new(children.into_iter())
    }
}

impl Stmt {
    pub(crate) fn child_expressions(&self) -> Box<dyn Iterator<Item = &Expr> + '_> {
        match self {
            Stmt::Expr(expr) | Stmt::Throw(expr) => Box::new(std::iter::once(expr)),
            Stmt::Return(value) => Box::new(value.iter()),
            Stmt::Block(statements) => {
                Box::new(statements.iter().flat_map(Stmt::child_expressions))
            }
            Stmt::Var { declarations, .. } => {
                Box::new(declarations.iter().flat_map(|declaration| {
                    declaration
                        .init
                        .iter()
                        .chain(declaration.pattern.child_expressions())
                }))
            }
            Stmt::Enum { members, .. } => Box::new(members.iter().map(|member| &member.value)),
            Stmt::If {
                test,
                consequent,
                alternate,
            } => Box::new(
                std::iter::once(test)
                    .chain(consequent.child_expressions())
                    .chain(alternate.iter().flat_map(|stmt| stmt.child_expressions())),
            ),
            Stmt::While { test, body } => {
                Box::new(std::iter::once(test).chain(body.child_expressions()))
            }
            Stmt::DoWhile { body, test } => {
                Box::new(body.child_expressions().chain(std::iter::once(test)))
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => Box::new(
                init.iter()
                    .flat_map(|stmt| stmt.child_expressions())
                    .chain(test.iter())
                    .chain(update.iter())
                    .chain(body.child_expressions()),
            ),
            Stmt::ForOf {
                pattern,
                iterable,
                body,
                ..
            } => Box::new(
                std::iter::once(iterable)
                    .chain(pattern.child_expressions())
                    .chain(body.child_expressions()),
            ),
            Stmt::ForIn {
                pattern,
                object,
                body,
                ..
            } => Box::new(
                std::iter::once(object)
                    .chain(pattern.child_expressions())
                    .chain(body.child_expressions()),
            ),
            Stmt::Switch {
                discriminant,
                cases,
            } => Box::new(
                std::iter::once(discriminant)
                    .chain(cases.iter().filter_map(|case| case.test.as_ref()))
                    .chain(
                        cases
                            .iter()
                            .flat_map(|case| case.consequent.iter())
                            .flat_map(Stmt::child_expressions),
                    ),
            ),
            Stmt::Try {
                body,
                catch,
                finally,
            } => Box::new(
                body.iter()
                    .flat_map(Stmt::child_expressions)
                    .chain(
                        catch
                            .iter()
                            .flat_map(|catch| catch.binding.iter())
                            .flat_map(Pattern::child_expressions),
                    )
                    .chain(
                        catch
                            .iter()
                            .flat_map(|catch| catch.body.iter())
                            .flat_map(Stmt::child_expressions),
                    )
                    .chain(finally.iter().flatten().flat_map(Stmt::child_expressions)),
            ),
            Stmt::Function { function, .. } => Box::new(
                function
                    .params
                    .iter()
                    .flat_map(Pattern::child_expressions)
                    .chain(match &function.body {
                        FunctionBody::Block(statements) => {
                            Box::new(statements.iter().flat_map(Stmt::child_expressions))
                                as Box<dyn Iterator<Item = &Expr>>
                        }
                        FunctionBody::Expression(expression) => {
                            Box::new(std::iter::once(expression.as_ref()))
                        }
                    }),
            ),
            Stmt::Empty | Stmt::Break | Stmt::Continue => Box::new(std::iter::empty()),
        }
    }

    pub(crate) fn descendants(&self) -> Box<dyn Iterator<Item = &Stmt> + '_> {
        let mut children = Vec::new();
        match self {
            Stmt::Block(statements) => children.extend(statements),
            Stmt::If {
                consequent,
                alternate,
                ..
            } => {
                children.push(consequent.as_ref());
                children.extend(alternate.iter().map(AsRef::as_ref));
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::ForOf { body, .. }
            | Stmt::ForIn { body, .. } => children.push(body),
            Stmt::For { init, body, .. } => {
                children.extend(init.iter().map(AsRef::as_ref));
                children.push(body);
            }
            Stmt::Switch { cases, .. } => {
                children.extend(cases.iter().flat_map(|case| &case.consequent));
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                children.extend(body);
                children.extend(catch.iter().flat_map(|catch| &catch.body));
                children.extend(finally.iter().flatten());
            }
            Stmt::Function { function, .. } => {
                if let FunctionBody::Block(statements) = &function.body {
                    children.extend(statements);
                }
            }
            Stmt::Empty
            | Stmt::Expr(_)
            | Stmt::Var { .. }
            | Stmt::Enum { .. }
            | Stmt::Return(_)
            | Stmt::Break
            | Stmt::Continue
            | Stmt::Throw(_) => {}
        }
        Box::new(std::iter::once(self).chain(children.into_iter().flat_map(Stmt::descendants)))
    }
}
