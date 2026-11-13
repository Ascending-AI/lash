//! Invert the clause-selection and fallthrough blocks emitted for a switch.
use lash_kernel_doc as k;
use lash_kernel_value::{Name, sites::Site};

use super::lift::{Lift, Lifter};
use super::source::{Expr, StmtKind};

fn number(expr: &k::Expr) -> Option<f64> {
    match expr {
        k::Expr::Literal(k::Literal::Float(value)) => Some(value.get()),
        _ => None,
    }
}

fn assigned_position(block: &[k::Stmt], chosen: &Name) -> Option<usize> {
    let [
        k::Stmt::Assign {
            place: k::Place::Variable(name),
            value: k::Rhs::Expr(value),
            order: k::AssignOrder::ValueFirst,
        },
    ] = block
    else {
        return None;
    };
    if name != chosen {
        return None;
    }
    let value = number(value)?;
    (value >= 0.0 && value.fract() == 0.0).then_some(value as usize)
}

impl Lifter<'_> {
    pub(super) fn for_continuation_update<'s>(
        &self,
        body: &'s [k::Stmt],
    ) -> Option<(&'s Name, &'static str)> {
        let k::Stmt::Assign {
            place: k::Place::Variable(target),
            value: k::Rhs::Expr(k::Expr::Call { function, args }),
            ..
        } = body.last()?
        else {
            return None;
        };
        let [
            k::Expr::Variable(read),
            k::Expr::Literal(k::Literal::Float(delta)),
        ] = args.as_slice()
        else {
            return None;
        };
        if target != read || !self.is(function, "num.add") {
            return None;
        }
        let operator = match delta.get() {
            1.0 => "++",
            -1.0 => "--",
            _ => return None,
        };
        let has_forwarded_update = body.iter().any(|statement| matches!(statement, k::Stmt::If { then_block, else_block, .. } if else_block.is_empty() && then_block.len() > 1 && matches!(then_block.last(), Some(k::Stmt::Continue))));
        has_forwarded_update.then_some((target, operator))
    }

    pub(super) fn switch_statement(
        &mut self,
        block: &[k::Stmt],
        site: &Site,
    ) -> Lift<Option<(StmtKind, usize)>> {
        let Some(k::Stmt::Let {
            name: chosen,
            value: k::Rhs::Expr(value),
        }) = block.first()
        else {
            return Ok(None);
        };
        if number(value) != Some(-1.0) || !super::lift::temporary(chosen) {
            return Ok(None);
        }
        let mut tests = std::collections::BTreeMap::new();
        let mut discriminant = None;
        let mut index = 1;
        while let Some(k::Stmt::If {
            condition: k::Expr::Call { function, args },
            then_block,
            else_block,
        }) = block.get(index)
        {
            if !self.is(function, "same")
                || args.as_slice() != [k::Expr::Variable(chosen.clone()), value.clone()]
                || !else_block.is_empty()
            {
                break;
            }
            if let Some(position) = assigned_position(then_block, chosen) {
                tests.insert(position, None);
            } else {
                let [
                    k::Stmt::If {
                        condition: k::Expr::Act(equal),
                        then_block,
                        else_block,
                    },
                ] = then_block.as_slice()
                else {
                    return Ok(None);
                };
                let k::ActKind::Call(k::Callee::Local(id)) = equal.kind else {
                    return Ok(None);
                };
                if !self.is(&id, "ts.strict_equals") || !else_block.is_empty() {
                    return Ok(None);
                }
                let [subject, test] = equal.args.as_slice() else {
                    return Ok(None);
                };
                if discriminant
                    .as_ref()
                    .is_some_and(|previous| previous != subject)
                {
                    return Ok(None);
                }
                discriminant = Some(subject.clone());
                let Some(position) = assigned_position(then_block, chosen) else {
                    return Ok(None);
                };
                tests.insert(position, Some(test.clone()));
            }
            index += 1;
        }
        let Some(k::Stmt::Let {
            name: running,
            value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Bool(false))),
        }) = block.get(index)
        else {
            return Ok(None);
        };
        index += 1;
        let continuation = match block.get(index) {
            Some(k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Bool(false))),
            }) => {
                index += 1;
                Some(name.clone())
            }
            _ => None,
        };
        let Some(k::Stmt::While {
            metered: 0,
            condition: k::Expr::Literal(k::Literal::Bool(true)),
            body,
            else_block,
        }) = block.get(index)
        else {
            return Ok(None);
        };
        if !else_block.is_empty() || !matches!(body.last(), Some(k::Stmt::Break)) {
            return Ok(None);
        }
        let clauses = &body[..body.len() - 1];
        if clauses.len() % 2 != 0 || clauses.len() / 2 != tests.len() {
            return Ok(None);
        }
        let mut bodies = Vec::new();
        for (position, pair) in clauses.as_chunks::<2>().0.iter().enumerate() {
            let [
                k::Stmt::If {
                    condition: k::Expr::Call { function, args },
                    then_block,
                    else_block,
                },
                k::Stmt::If {
                    condition: k::Expr::Variable(test),
                    then_block: body,
                    else_block: otherwise,
                },
            ] = pair
            else {
                return Ok(None);
            };
            if !self.is(function, "same")
                || args.len() != 2
                || args[0] != k::Expr::Variable(chosen.clone())
                || number(&args[1]) != Some(position as f64)
                || !else_block.is_empty()
                || test != running
                || !otherwise.is_empty()
                || !matches!(then_block.as_slice(), [k::Stmt::Assign { place: k::Place::Variable(name), value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Bool(true))), order: k::AssignOrder::ValueFirst }] if name == running)
            {
                return Ok(None);
            }
            bodies.push(body);
        }
        let discriminant = match discriminant {
            Some(value) => self.expr(&value)?,
            None => Expr::Undefined,
        };
        let mut taken = index + 1;
        if let Some(flag) = &continuation {
            if !matches!(block.get(taken), Some(k::Stmt::If { condition: k::Expr::Variable(name), then_block, else_block }) if name == flag && else_block.is_empty() && matches!(then_block.last(), Some(k::Stmt::Continue)))
            {
                return Ok(None);
            }
            taken += 1;
            self.switch_continuations.insert(flag.clone());
        }
        let mut cases = Vec::new();
        for (position, body) in bodies.into_iter().enumerate() {
            let Some(test) = tests.remove(&position) else {
                return Ok(None);
            };
            let test = test.map(|test| self.expr(&test)).transpose()?;
            let body = self.block(
                body,
                &site
                    .child(index as u32)
                    .child(1)
                    .child((2 * position + 1) as u32)
                    .child(0),
            )?;
            cases.push((test, body));
        }
        if let Some(flag) = continuation {
            self.switch_continuations.remove(&flag);
        }
        Ok(Some((
            StmtKind::Switch {
                discriminant,
                cases,
            },
            taken,
        )))
    }
}
