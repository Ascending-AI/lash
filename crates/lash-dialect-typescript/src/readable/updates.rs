//! Reconstruct assignments whose value is held before writing the place.
use super::lift::{Lift, Lifter};
use super::source::{Expr, StmtKind};
use lash_kernel_doc as k;

impl Lifter<'_> {
    pub(super) fn enum_statement(&mut self, block: &[k::Stmt]) -> Lift<Option<(StmtKind, usize)>> {
        let Some(k::Stmt::Let {
            name,
            value: k::Rhs::Expr(k::Expr::Record(empty)),
        }) = block.first()
        else {
            return Ok(None);
        };
        if !empty.is_empty()
            || !self.style.operators
            || (super::lift::temporary(name) && self.document.private_bindings.contains(name))
        {
            return Ok(None);
        }
        let mut index = 1;
        let mut members = Vec::new();
        while let Some(k::Stmt::Assign {
            place: k::Place::Member(k::Member::Field { target, field }),
            value: k::Rhs::Expr(value),
            order: k::AssignOrder::ValueFirst,
        }) = block.get(index)
        {
            if *target != k::Expr::Variable(name.clone()) {
                break;
            }
            members.push((field.clone(), self.expr(value)?));
            index += 1;
            if let Some(k::Stmt::Do {
                value: k::Expr::Act(act),
            }) = block.get(index)
                && matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if self.is(id, "ts.set"))
                && act.args
                    == [
                        target.clone(),
                        value.clone(),
                        k::Expr::Literal(k::Literal::Text(field.clone())),
                    ]
            {
                index += 1;
            }
        }
        if members.is_empty() {
            return Ok(None);
        }
        Ok(Some((StmtKind::Enum(self.binding(name)?, members), index)))
    }

    pub(super) fn logical_assignment(
        &mut self,
        block: &[k::Stmt],
    ) -> Lift<Option<(StmtKind, usize)>> {
        let [
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(initial),
            },
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
            ..,
        ] = block
        else {
            return Ok(None);
        };
        if !super::lift::temporary(name) {
            return Ok(None);
        }
        let boolean = matches!(condition, k::Expr::Call { function, args } if self.is(function, "ts.to_boolean") && args.as_slice() == [k::Expr::Variable(name.clone())])
            || *condition == k::Expr::Variable(name.clone());
        let (operator, branch) = if boolean && else_block.is_empty() {
            ("&&=", then_block)
        } else if boolean && then_block.is_empty() {
            ("||=", else_block)
        } else if self.nullish(condition, name) && else_block.is_empty() {
            ("??=", then_block)
        } else {
            return Ok(None);
        };
        let [
            write,
            k::Stmt::Assign {
                place: k::Place::Variable(result),
                value,
                order: k::AssignOrder::ValueFirst,
            },
        ] = branch.as_slice()
        else {
            return Ok(None);
        };
        if result != name {
            return Ok(None);
        }
        let rhs = match write {
            k::Stmt::Assign {
                place: k::Place::Variable(target),
                value: written,
                ..
            } if *initial == k::Expr::Variable(target.clone())
                && *value == k::Rhs::Expr(k::Expr::Variable(target.clone())) =>
            {
                written
            }
            k::Stmt::Assign { value: written, .. } if written == value => value,
            k::Stmt::If { .. } => value,
            _ => return Ok(None),
        };
        let target = self.expr(initial)?;
        if !matches!(target, Expr::Ident(_) | Expr::Field(..) | Expr::Index(..)) {
            return Ok(None);
        }
        let value = self.rhs(rhs)?;
        Ok(Some((StmtKind::Assign(target, operator, value), 2)))
    }

    pub(super) fn compound_assignment(
        &mut self,
        block: &[k::Stmt],
    ) -> Lift<Option<(StmtKind, usize)>> {
        let [
            k::Stmt::Let { name, value },
            k::Stmt::Assign {
                place,
                value: k::Rhs::Expr(k::Expr::Variable(read)),
                order: k::AssignOrder::ValueFirst,
            },
            ..,
        ] = block
        else {
            return Ok(None);
        };
        if name != read || !super::lift::temporary(name) {
            return Ok(None);
        }
        let expression = match value {
            k::Rhs::Expr(value) => value.clone(),
            k::Rhs::Action(action) => k::Expr::Act(Box::new(super::statements::act_of(action))),
        };
        let (function, args) = match &expression {
            k::Expr::Call { function, args } => (function, args),
            k::Expr::Act(act) => match &act.kind {
                k::ActKind::Call(k::Callee::Local(id)) => (id, &act.args),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        let Some(helper) = self.library(function) else {
            return Ok(None);
        };
        let operator = crate::constructs::operators::BINARY
            .iter()
            .find(|op| op.helper == helper)
            .map(|op| op.token)
            .or_else(|| crate::constructs::operators::native_inverse(&helper));
        let Some(operator) = operator else {
            return Ok(None);
        };
        let [left, right] = args.as_slice() else {
            return Ok(None);
        };
        let read = match place {
            k::Place::Variable(name) => k::Expr::Variable(name.clone()),
            k::Place::Member(member) => k::Expr::Member(Box::new(member.clone())),
        };
        if *left != read {
            return Ok(None);
        }
        let token = match operator {
            "+" => "+=",
            "-" => "-=",
            "*" => "*=",
            "/" => "/=",
            "%" => "%=",
            "**" => "**=",
            "<<" => "<<=",
            ">>" => ">>=",
            ">>>" => ">>>=",
            "&" => "&=",
            "|" => "|=",
            "^" => "^=",
            _ => return Ok(None),
        };
        let StmtKind::Assign(target, _, _) =
            self.assign(place, &k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)))?
        else {
            return Ok(None);
        };
        let value = self.expr(right)?;
        Ok(Some((StmtKind::Assign(target, token, value), 2)))
    }
}
