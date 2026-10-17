//! The definition's logical operand file, independent of executable layout.
//!
//! Operands evaluate left to right into successive slots; a child reuses
//! its result slot for its own operands. Statements reuse the same file.
//! Closure bodies own a different activation and do not grow this one.

use lash_kernel_doc::{
    ActKind, Action, AssignOrder, Block, Callee, Expr, Member, Place, Rhs, Stmt,
};

pub(super) fn canonical_registers(body: &Block) -> u32 {
    body.iter().map(statement).max().unwrap_or(0)
}

fn operands<'a>(values: impl Iterator<Item = &'a Expr>) -> u32 {
    values
        .enumerate()
        .map(|(index, value)| (index as u32).saturating_add(expression(value)))
        .max()
        .unwrap_or(1)
}

fn expression(value: &Expr) -> u32 {
    match value {
        Expr::Literal(_) | Expr::Variable(_) | Expr::Closure(_) | Expr::Clock | Expr::Random => 1,
        Expr::Tuple(items)
        | Expr::List(items)
        | Expr::Set(items)
        | Expr::Call { args: items, .. } => operands(items.iter()),
        Expr::Map(items) => operands(items.iter().flat_map(|item| [&item.key, &item.value])),
        Expr::Record(items) => operands(items.iter().map(|item| &item.value)),
        Expr::Member(value) => member(value),
        Expr::If { cond, then, r#else } => expression(cond)
            .max(expression(then))
            .max(expression(r#else)),
        Expr::Let { value, body, .. } => expression(value).max(expression(body)),
        Expr::Read(read) => expression(&read.handle).max(1 + expression(&read.request)),
        Expr::Act(act) => {
            let callee = matches!(
                &act.kind,
                ActKind::Call(Callee::Value(_)) | ActKind::Spawn(Callee::Value(_))
            );
            if callee {
                act.args
                    .iter()
                    .enumerate()
                    .map(|(index, value)| 1 + index as u32 + expression(value))
                    .max()
                    .unwrap_or(1)
            } else {
                operands(act.args.iter())
            }
        }
    }
}

fn member(value: &Member) -> u32 {
    match value {
        Member::Field { target, .. } => expression(target),
        Member::Index { target, index } => expression(target).max(1 + expression(index)),
    }
}

fn rhs(value: &Rhs) -> u32 {
    match value {
        Rhs::Expr(value) => expression(value),
        Rhs::Action(action) => match action {
            Action::Call { callee, args } | Action::Spawn { callee, args } => {
                (args.len() as u32 + u32::from(matches!(callee, Callee::Value(_)))).max(1)
            }
            Action::Perform { args, .. } => (args.len() as u32).max(1),
            _ => 1,
        },
    }
}

fn statement(stmt: &Stmt) -> u32 {
    match stmt {
        Stmt::Let { value, .. } => rhs(value),
        Stmt::Assign {
            place: Place::Variable(_),
            value,
            ..
        } => rhs(value),
        Stmt::Assign {
            place: Place::Member(place),
            value,
            order,
        } => {
            let place_registers = member(place);
            match order {
                AssignOrder::ValueFirst => rhs(value).max(1 + place_registers),
                AssignOrder::PlaceFirst => {
                    let next = match place {
                        Member::Field { .. } => 1,
                        Member::Index { .. } => 2,
                    };
                    place_registers.max(next + rhs(value))
                }
            }
        }
        Stmt::Remove { member: place } => member(place),
        Stmt::Do { value }
        | Stmt::Return { value }
        | Stmt::ReturnVia { value, .. }
        | Stmt::Throw { value }
        | Stmt::Print { value }
        | Stmt::Finish { value }
        | Stmt::Fail { value } => expression(value),
        Stmt::If {
            condition,
            then_block,
            else_block,
        } => expression(condition)
            .max(canonical_registers(then_block))
            .max(canonical_registers(else_block)),
        Stmt::For {
            iterable,
            via,
            body,
            else_block,
            ..
        } => {
            // K-ITER-005: check receives walked, size, remaining and more;
            // item receives walked and raw; open/next each receive one.
            let protocol = via.as_ref().map_or(0, |via| {
                if via.check.is_some() {
                    4
                } else if via.item.is_some() {
                    2
                } else {
                    1
                }
            });
            expression(iterable)
                .max(protocol)
                .max(canonical_registers(body))
                .max(canonical_registers(else_block))
        }
        Stmt::While {
            condition,
            body,
            else_block,
        } => expression(condition)
            .max(canonical_registers(body))
            .max(canonical_registers(else_block)),
        Stmt::Try(scope) => canonical_registers(&scope.body)
            .max(
                scope
                    .catch
                    .as_ref()
                    .map_or(0, |catch| canonical_registers(&catch.body)),
            )
            .max(scope.finally.as_ref().map_or(0, canonical_registers)),
        Stmt::Break | Stmt::Continue => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal() -> Expr {
        Expr::Literal(lash_kernel_doc::Literal::Null)
    }

    #[test]
    fn place_first_assignment_keeps_place_registers_live_for_rhs() {
        let body = vec![Stmt::Assign {
            place: Place::Member(Member::Index {
                target: literal(),
                index: literal(),
            }),
            value: Rhs::Expr(Expr::List(vec![literal(), literal()])),
            order: AssignOrder::PlaceFirst,
        }];

        assert_eq!(canonical_registers(&body), 4);
    }
}
