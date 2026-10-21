//! A method of an array, called on a value the front end checks is one:
//!
//! ```text
//! let t = same(kind(x), "list")
//! if t {} else { if invoke ts.array.is(x) {} else { throw ... } }
//! let r = absent
//! if t { set r = <the list's own method> } else {
//!   let m = invoke ts.member.method(x)
//!   ...
//!   set r = let(f = m, let(a = [args], let(c = ts.callable(f), apply c(x, a))))
//! }
//! ```
//!
//! is `x.method(args)`, read from the general arm. Only reading the print
//! back shows the list's arm is the one the front end gives it.

use lash_kernel_doc::{self as k};

use lash_vocabulary::Name;
use lash_vocabulary::sites::Site;

use super::lift::{Lift, Lifter};
use super::source::Expr;

impl Lifter<'_> {
    /// The method call `block`, whose first statement is child `first` of
    /// the block at `site`, opens with, with the temporary it is bound to
    /// and how many statements it takes.
    pub(super) fn method_ladder(
        &mut self,
        block: &[k::Stmt],
        site: &Site,
        first: usize,
    ) -> Lift<Option<(Name, Expr, usize)>> {
        let [
            k::Stmt::Let {
                name: test,
                value: k::Rhs::Expr(tested),
            },
            k::Stmt::If {
                condition: k::Expr::Variable(checked),
                then_block: passes,
                else_block: checks,
            },
            k::Stmt::Let {
                name: result,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
            },
            k::Stmt::If {
                condition: k::Expr::Variable(dispatched),
                else_block: general,
                ..
            },
            ..,
        ] = block
        else {
            return Ok(None);
        };
        let Some(target) = self.kind_test(tested, "list") else {
            return Ok(None);
        };
        if checked != test
            || dispatched != test
            || !passes.is_empty()
            || !self.array_check(checks, target)
        {
            return Ok(None);
        }
        let [
            k::Stmt::Let {
                name: member,
                value: selected,
            },
            middle @ ..,
            k::Stmt::Assign {
                place: k::Place::Variable(assigned),
                value: k::Rhs::Expr(call),
                ..
            },
        ] = general.as_slice()
        else {
            return Ok(None);
        };
        let act = match selected {
            k::Rhs::Expr(k::Expr::Act(act)) => Some((**act).clone()),
            k::Rhs::Action(action) => Some(super::statements::act_of(action)),
            k::Rhs::Expr(_) => None,
        };
        let method = act.and_then(|act| match &act.kind {
            k::ActKind::Call(k::Callee::Import(id)) if act.args.as_slice() == [target.clone()] => {
                self.library(id)
                    .and_then(|name| name.strip_prefix("ts.member.").map(str::to_owned))
            }
            _ => None,
        });
        let Some(method) = method else {
            return Ok(None);
        };
        if assigned != result {
            return Ok(None);
        }
        let Some(args) = self.method_arguments(member, target, call) else {
            return Ok(None);
        };
        // The general arm's own temporaries: the function values it passes.
        let arm = site.child((first + 3) as u32).child(2);
        let saved = self.enter();
        let mut bound = Vec::new();
        let mut lifted = Ok(());
        for (index, statement) in middle.iter().enumerate() {
            let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(value),
            } = statement
            else {
                lifted = Err("a statement among a method's arguments".to_owned());
                break;
            };
            let at = arm.child((index + 1) as u32).child(0);
            let value = match self.closure(value, &at) {
                Ok(Some(function)) => Ok(Expr::arrow(function)),
                Ok(None) => self.expr(value),
                Err(why) => Err(why),
            };
            match value {
                Ok(value) => bound.push((name.clone(), value)),
                Err(why) => {
                    lifted = Err(why);
                    break;
                }
            }
        }
        self.leave(saved);
        lifted?;
        let target = self.expr(target)?;
        let mut written = Vec::with_capacity(args.len());
        for arg in args {
            let value = match arg {
                k::Expr::Variable(name) => bound
                    .iter()
                    .position(|(bound, _)| bound == name)
                    .map(|at| bound.remove(at).1),
                _ => None,
            };
            written.push(match value {
                Some(value) => value,
                None => self.expr(arg)?,
            });
        }
        if !bound.is_empty() {
            return Ok(None);
        }
        Ok(Some((
            result.clone(),
            Expr::Call(Box::new(Expr::Field(Box::new(target), method)), written),
            4,
        )))
    }

    /// `if invoke ts.array.is(x) {} else { throw ... }`.
    fn array_check(&self, checks: &[k::Stmt], target: &k::Expr) -> bool {
        matches!(checks, [k::Stmt::If { condition: k::Expr::Act(act), then_block, else_block }]
            if then_block.is_empty()
                && matches!(else_block.as_slice(), [k::Stmt::Throw { .. }])
                && matches!(&act.kind, k::ActKind::Call(k::Callee::Import(id)) if self.is(id, "ts.array.is"))
                && act.args.as_slice() == [target.clone()])
    }

    /// The arguments `call` passes the method `member` holds, with `target`
    /// its receiver: `let(f = member, let(a = [args], let(c =
    /// ts.callable(f), apply c(target, a))))`.
    fn method_arguments<'e>(
        &self,
        member: &Name,
        target: &k::Expr,
        call: &'e k::Expr,
    ) -> Option<&'e [k::Expr]> {
        let k::Expr::Let {
            name: held,
            value: holds,
            body,
        } = call
        else {
            return None;
        };
        let k::Expr::Let {
            name: list,
            value: args,
            body,
        } = &**body
        else {
            return None;
        };
        let k::Expr::Let {
            name: callable,
            value: selects,
            body: applied,
        } = &**body
        else {
            return None;
        };
        let k::Expr::List(args) = &**args else {
            return None;
        };
        let selects = matches!(&**selects, k::Expr::Call { function, args }
            if self.is(function, "ts.callable") && args.as_slice() == [k::Expr::Variable(held.clone())]);
        let applies = matches!(&**applied, k::Expr::Act(act)
            if matches!(&act.kind, k::ActKind::Call(k::Callee::Value(called)) if called == callable)
                && act.args.as_slice() == [target.clone(), k::Expr::Variable(list.clone())]);
        (**holds == k::Expr::Variable(member.clone()) && selects && applies)
            .then_some(args.as_slice())
    }
}
