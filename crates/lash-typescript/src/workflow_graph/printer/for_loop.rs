//! The classic `for` loop's shapes in the lowered IR: the `var`/`let` head
//! list, the `while` it lowers to, and the statement positions a body's
//! completion wrapper marks.

use lashlang::{AssignTarget, Expr, StructuralRole};

/// The `var name = init` shape: a completion list whose one visible statement
/// assigns `name` and whose completion value reads `name` back.
pub(super) fn var_initialization(expression: &Expr) -> Option<(&AssignTarget, &Expr)> {
    let Expr::Role {
        role: StructuralRole::Completion,
        expr,
    } = expression
    else {
        return None;
    };
    let Expr::Block(items) = expr.as_ref() else {
        return None;
    };
    let [Expr::Assign { target, expr: init }, Expr::Variable(name)] = items.as_slice() else {
        return None;
    };
    (target.is_simple() && target.root.as_str() == name.as_str()).then_some((target, init))
}

/// A body position that holds statements rather than one value expression.
pub(super) fn is_statement_body(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Block(_)
            | Expr::Role {
                role: StructuralRole::Completion,
                ..
            }
    )
}

/// The parts of a classic `for` loop, from the block it lowers to: the
/// head's statements, then a `while` whose body is the loop body's statement
/// list followed by the update, when the loop has one. An authored `while`
/// is its body's statement list itself, never a block holding it, so the
/// shape is the loop's own.
pub(super) struct ClassicFor<'a> {
    pub(super) init: &'a [Expr],
    pub(super) condition: &'a Expr,
    pub(super) body: &'a Expr,
    pub(super) update: Option<&'a Expr>,
}

pub(super) fn classic_for(expression: &Expr) -> Option<ClassicFor<'_>> {
    let Expr::Block(items) = expression else {
        return None;
    };
    let (Expr::While { condition, body }, init) = items.split_last()? else {
        return None;
    };
    let Expr::Block(parts) = body.as_ref() else {
        return None;
    };
    let (body, update) = match parts.as_slice() {
        [body] => (body, None),
        [body, update] => (body, Some(update)),
        _ => return None,
    };
    matches!(
        body,
        Expr::Role {
            role: StructuralRole::Completion,
            ..
        }
    )
    .then_some(ClassicFor {
        init,
        condition,
        body,
        update,
    })
}
