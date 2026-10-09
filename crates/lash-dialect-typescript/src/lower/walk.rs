//! Questions about the adapter's tree that lowering asks ahead of time.

use crate::adapter::{
    ArrayElement, AssignTarget, CallArg, Expr, MemberProperty, ObjectProperty, OptionalOperation,
    Pattern, PropertyKey, Stmt, VarKind,
};

/// Whether evaluating `expr` runs no code at all, so that nothing read
/// before it can have changed after it.
pub(super) fn is_inert(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Ident(..)
            | Expr::This
            | Expr::Function(_)
    ) || matches!(expr, Expr::As { value, .. } if is_inert(value))
}

/// Whether evaluating `expr` neither runs code nor reads a binding: a
/// literal, a function, or an array or object of such.
fn is_constant(expr: &Expr) -> bool {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::String(_) => true,
        Expr::Function(function) => !function.is_async,
        Expr::Unary { value, .. } => matches!(value.as_ref(), Expr::Number(_)),
        Expr::Template { expressions, .. } => expressions.is_empty(),
        Expr::Array(elements) => elements
            .iter()
            .all(|element| matches!(element, ArrayElement::Value(value) if is_constant(value))),
        Expr::Object(properties) => properties.iter().all(|property| {
            matches!(
                property,
                ObjectProperty::KeyValue(PropertyKey::Static(_), value) if is_constant(value)
            )
        }),
        _ => false,
    }
}

/// Whether a statement only declares names with constants, or declares a
/// function, so that nothing can be called while it runs.
pub(super) fn declares_constants(statement: &Stmt) -> bool {
    match statement {
        Stmt::Spanned(_, inner) | Stmt::Labeled { stmt: inner, .. } => declares_constants(inner),
        Stmt::Empty | Stmt::TypeAlias { .. } | Stmt::Function { .. } => true,
        Stmt::Var { declarations, .. } => declarations.iter().all(|declaration| {
            matches!(declaration.pattern, Pattern::Ident(..))
                && declaration.init.as_ref().is_none_or(is_constant)
        }),
        _ => false,
    }
}

/// Whether a statement never runs to its end: it returns, throws, or
/// leaves or restarts a loop.
pub(super) fn always_leaves(statement: &Stmt) -> bool {
    match statement.unlabeled() {
        Stmt::Return(_) | Stmt::Throw(_) | Stmt::Break | Stmt::Continue => true,
        Stmt::Block(body) => body.last().is_some_and(always_leaves),
        _ => false,
    }
}

/// The names a pattern binds, in source order.
pub(super) fn pattern_names(pattern: &Pattern, names: &mut Vec<String>) {
    match pattern {
        Pattern::Ident(name, _) => names.push(name.clone()),
        Pattern::Rest(inner) => pattern_names(inner, names),
        Pattern::Member { .. } => {}
        Pattern::Assign { target, .. } => pattern_names(target, names),
        Pattern::Array { elements, rest } => {
            for element in elements.iter().flatten() {
                pattern_names(element, names);
            }
            if let Some(rest) = rest {
                pattern_names(rest, names);
            }
        }
        Pattern::Object { properties, rest } => {
            for property in properties {
                pattern_names(&property.value, names);
            }
            if let Some(rest) = rest {
                pattern_names(rest, names);
            }
        }
    }
}

/// The names `var` declares anywhere in `statements`, outside nested
/// functions: they belong to the enclosing function.
pub(super) fn var_names(statements: &[Stmt], names: &mut Vec<String>) {
    for statement in statements {
        var_names_of(statement, names);
    }
}

fn var_names_of(statement: &Stmt, names: &mut Vec<String>) {
    match statement {
        Stmt::Spanned(_, inner) | Stmt::Labeled { stmt: inner, .. } => var_names_of(inner, names),
        Stmt::Var {
            kind: VarKind::Var,
            declarations,
        } => {
            for declaration in declarations {
                pattern_names(&declaration.pattern, names);
            }
        }
        Stmt::Block(body) => var_names(body, names),
        Stmt::If {
            consequent,
            alternate,
            ..
        } => {
            var_names_of(consequent, names);
            if let Some(alternate) = alternate {
                var_names_of(alternate, names);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => var_names_of(body, names),
        Stmt::For { init, body, .. } => {
            if let Some(init) = init {
                var_names_of(init, names);
            }
            var_names_of(body, names);
        }
        Stmt::ForOf {
            pattern,
            kind,
            body,
            ..
        }
        | Stmt::ForIn {
            pattern,
            kind,
            body,
            ..
        } => {
            if *kind == Some(VarKind::Var) {
                pattern_names(pattern, names);
            }
            var_names_of(body, names);
        }
        Stmt::Switch { cases, .. } => {
            for case in cases {
                var_names(&case.consequent, names);
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            var_names(body, names);
            if let Some(catch) = catch {
                var_names(&catch.body, names);
            }
            if let Some(finally) = finally {
                var_names(finally, names);
            }
        }
        _ => {}
    }
}

/// Whether a function is written anywhere in a statement: a closure made in
/// a loop's body captures that iteration's bindings.
pub(super) fn statement_makes_function(statement: &Stmt) -> bool {
    match statement {
        Stmt::Spanned(_, inner) | Stmt::Labeled { stmt: inner, .. } => {
            statement_makes_function(inner)
        }
        Stmt::Empty | Stmt::TypeAlias { .. } | Stmt::Break | Stmt::Continue => false,
        Stmt::Function { .. } => true,
        Stmt::Expr(expr) | Stmt::Throw(expr) => makes_function(expr),
        Stmt::Block(body) => body.iter().any(statement_makes_function),
        Stmt::Var { declarations, .. } => declarations.iter().any(|declaration| {
            pattern_makes_function(&declaration.pattern)
                || declaration.init.as_ref().is_some_and(makes_function)
        }),
        Stmt::Enum { members, .. } => members.iter().any(|member| makes_function(&member.value)),
        Stmt::Return(value) => value.as_ref().is_some_and(makes_function),
        Stmt::If {
            test,
            consequent,
            alternate,
        } => {
            makes_function(test)
                || statement_makes_function(consequent)
                || alternate
                    .as_ref()
                    .is_some_and(|alternate| statement_makes_function(alternate))
        }
        Stmt::While { test, body } | Stmt::DoWhile { body, test, .. } => {
            makes_function(test) || statement_makes_function(body)
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            init.as_ref()
                .is_some_and(|init| statement_makes_function(init))
                || test.as_ref().is_some_and(makes_function)
                || update.as_ref().is_some_and(makes_function)
                || statement_makes_function(body)
        }
        Stmt::ForOf {
            pattern,
            iterable: subject,
            body,
            ..
        }
        | Stmt::ForIn {
            pattern,
            object: subject,
            body,
            ..
        } => {
            pattern_makes_function(pattern)
                || makes_function(subject)
                || statement_makes_function(body)
        }
        Stmt::Switch {
            discriminant,
            cases,
        } => {
            makes_function(discriminant)
                || cases.iter().any(|case| {
                    case.test.as_ref().is_some_and(makes_function)
                        || case.consequent.iter().any(statement_makes_function)
                })
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            body.iter().any(statement_makes_function)
                || catch.as_ref().is_some_and(|catch| {
                    catch.binding.as_ref().is_some_and(pattern_makes_function)
                        || catch.body.iter().any(statement_makes_function)
                })
                || finally
                    .as_ref()
                    .is_some_and(|finally| finally.iter().any(statement_makes_function))
        }
    }
}

fn pattern_makes_function(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Ident(..) => false,
        Pattern::Rest(inner) => pattern_makes_function(inner),
        Pattern::Member { object, property } => {
            makes_function(object) || property_makes_function(property)
        }
        Pattern::Assign { target, default } => {
            pattern_makes_function(target) || makes_function(default)
        }
        Pattern::Array { elements, rest } => {
            elements.iter().flatten().any(pattern_makes_function)
                || rest
                    .as_ref()
                    .is_some_and(|rest| pattern_makes_function(rest))
        }
        Pattern::Object { properties, rest } => {
            properties.iter().any(|property| {
                key_makes_function(&property.key) || pattern_makes_function(&property.value)
            }) || rest
                .as_ref()
                .is_some_and(|rest| pattern_makes_function(rest))
        }
    }
}

fn property_makes_function(property: &MemberProperty) -> bool {
    match property {
        MemberProperty::Field(_) => false,
        MemberProperty::Index(index) => makes_function(index),
    }
}

fn key_makes_function(key: &PropertyKey) -> bool {
    match key {
        PropertyKey::Static(_) => false,
        PropertyKey::Computed(key) => makes_function(key),
    }
}

fn target_makes_function(target: &AssignTarget) -> bool {
    match target {
        AssignTarget::Ident(_) | AssignTarget::ParenIdent(_) => false,
        AssignTarget::Member { object, property } => {
            makes_function(object) || property_makes_function(property)
        }
        AssignTarget::Pattern(pattern) => pattern_makes_function(pattern),
    }
}

fn args_make_function(args: &[CallArg]) -> bool {
    args.iter().any(|arg| match arg {
        CallArg::Value(value) | CallArg::Spread(value) => makes_function(value),
    })
}

fn makes_function(expr: &Expr) -> bool {
    match expr {
        Expr::Function(_) => true,
        Expr::Null
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::RegExp { .. }
        | Expr::Ident(..)
        | Expr::This
        | Expr::LoneSurrogateString => false,
        Expr::Array(elements) => elements.iter().any(|element| match element {
            ArrayElement::Value(value) | ArrayElement::Spread(value) => makes_function(value),
            ArrayElement::Hole => false,
        }),
        Expr::Object(properties) => properties.iter().any(|property| match property {
            ObjectProperty::KeyValue(key, value) => {
                key_makes_function(key) || makes_function(value)
            }
            ObjectProperty::Spread(value) => makes_function(value),
        }),
        Expr::Assign { target, value, .. } => {
            target_makes_function(target) || makes_function(value)
        }
        Expr::Member {
            object, property, ..
        }
        | Expr::Delete { object, property } => {
            makes_function(object) || property_makes_function(property)
        }
        Expr::Unary { value, .. } | Expr::Await { value, .. } | Expr::As { value, .. } => {
            makes_function(value)
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            makes_function(left) || makes_function(right)
        }
        Expr::Conditional {
            test,
            consequent,
            alternate,
        } => makes_function(test) || makes_function(consequent) || makes_function(alternate),
        Expr::Template { expressions, .. } => expressions.iter().any(makes_function),
        Expr::Call { callee, args, .. } => makes_function(callee) || args_make_function(args),
        Expr::New { args, .. } => args_make_function(args),
        Expr::OptionalChain { base, operations } => {
            makes_function(base)
                || operations.iter().any(|operation| match operation {
                    OptionalOperation::Member { property, .. } => property_makes_function(property),
                    OptionalOperation::Call { args, .. } => args_make_function(args),
                })
        }
        Expr::Update { target, .. } => target_makes_function(target),
    }
}
