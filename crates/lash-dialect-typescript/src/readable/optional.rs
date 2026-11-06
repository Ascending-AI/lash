//! Recover optional references before their value/receiver/key tuple is projected.
use lash_kernel_doc as k;
use lash_kernel_value::{Name, sites::Site};

use super::lift::{Lift, Lifter, refuse};
use super::source::Expr;

impl Lifter<'_> {
    pub(super) fn optional_reference(
        &mut self,
        block: &[k::Stmt],
        site: &Site,
        at: usize,
    ) -> Lift<Option<(Name, Expr, usize)>> {
        let Some(k::Stmt::Let {
            name: pack,
            value: k::Rhs::Expr(value),
        }) = block.first()
        else {
            return Ok(None);
        };
        let statements = matches!(value, k::Expr::Tuple(initial) if initial.len() == 3)
            && matches!(block.get(1), Some(k::Stmt::If { condition, .. }) if nullish_name(self, condition).is_some());
        let first_projection = if statements { 2 } else { 1 };
        if !statements
            && !matches!(value, k::Expr::Let { body, .. } if matches!(body.as_ref(), k::Expr::If { cond, .. } if nullish_name(self, cond).is_some()))
        {
            return Ok(None);
        }
        let rest = &block[first_projection..];
        let mut projections = Vec::new();
        for (index, stmt) in rest.iter().take(3).enumerate() {
            let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Member(member)),
            } = stmt
            else {
                return Ok(None);
            };
            if !matches!(member.as_ref(), k::Member::Index { target: k::Expr::Variable(target), index: k::Expr::Literal(k::Literal::Float(n)) } if target == pack && n.get() == index as f64)
            {
                return Ok(None);
            }
            projections.push(name.clone());
        }
        if projections.len() != 3 {
            return Ok(None);
        }
        let saved = self.substitutions.clone();
        let result = if statements {
            self.optional_statements(&block[..2], site, at)
                .and_then(|()| {
                    self.substitutions
                        .get(pack)
                        .cloned()
                        .ok_or_else(|| "an optional chain's result".into())
                })
        } else {
            self.optional_value(value)
        };
        self.substitutions = saved;
        Ok(Some((projections.remove(0), result?, first_projection + 3)))
    }

    fn optional_statements(&mut self, block: &[k::Stmt], site: &Site, first: usize) -> Lift<()> {
        let mut index = 0;
        while index < block.len() {
            if let Some((name, value, taken)) = self.read_ladder(&block[index..])? {
                self.substitutions.insert(name, value);
                index += taken;
                continue;
            }
            match &block[index] {
                k::Stmt::Let {
                    name,
                    value: k::Rhs::Expr(value),
                }
                | k::Stmt::Assign {
                    place: k::Place::Variable(name),
                    value: k::Rhs::Expr(value),
                    ..
                } => {
                    let value = self.optional_value(value)?;
                    self.substitutions.insert(name.clone(), value);
                }
                k::Stmt::If {
                    condition,
                    then_block,
                    else_block,
                } if then_block.is_empty() => {
                    let Some(name) = nullish_name(self, condition).cloned() else {
                        return refuse("an optional reference's nullish guard");
                    };
                    let value = self.expr(&k::Expr::Variable(name.clone()))?;
                    let previous = self
                        .substitutions
                        .insert(name.clone(), Expr::Optional(Box::new(value)));
                    self.optional_statements(
                        else_block,
                        &site.child((first + index) as u32).child(2),
                        0,
                    )?;
                    match previous {
                        Some(value) => {
                            self.substitutions.insert(name, value);
                        }
                        None => {
                            self.substitutions.remove(&name);
                        }
                    }
                }
                _ => return refuse("an optional reference's evaluation"),
            }
            index += 1;
        }
        Ok(())
    }

    fn optional_value(&mut self, value: &k::Expr) -> Lift<Expr> {
        match value {
            k::Expr::Tuple(parts) if parts.len() == 3 => self.optional_value(&parts[0]),
            k::Expr::Let { name, value, body } => {
                if let Some(call) = self.call(name, value, body)? {
                    return Ok(call);
                }
                let value = self.optional_value(value)?;
                let previous = self.substitutions.insert(name.clone(), value);
                let result = self.optional_value(body);
                match previous {
                    Some(value) => {
                        self.substitutions.insert(name.clone(), value);
                    }
                    None => {
                        self.substitutions.remove(name);
                    }
                }
                result
            }
            k::Expr::If { cond, r#else, .. } if nullish_name(self, cond).is_some() => {
                let Some(name) = nullish_name(self, cond).cloned() else {
                    return refuse("a nullish guard");
                };
                let value = self.expr(&k::Expr::Variable(name.clone()))?;
                let previous = self
                    .substitutions
                    .insert(name.clone(), Expr::Optional(Box::new(value)));
                let result = self.optional_value(r#else);
                match previous {
                    Some(value) => {
                        self.substitutions.insert(name, value);
                    }
                    None => {
                        self.substitutions.remove(&name);
                    }
                }
                result
            }
            _ => self.expr(value),
        }
    }
}

fn nullish_name<'a>(lifter: &Lifter<'_>, condition: &'a k::Expr) -> Option<&'a Name> {
    let k::Expr::If { cond, .. } = condition else {
        return None;
    };
    let k::Expr::Call { args, .. } = cond.as_ref() else {
        return None;
    };
    let [k::Expr::Variable(name), _] = args.as_slice() else {
        return None;
    };
    lifter.nullish(condition, name).then_some(name)
}
