//! Recover the lazy branch recipes without evaluating an arm early.
use super::{Lift, Lifter, refuse, source, temporary};
use lash_kernel_doc as k;
use lash_kernel_value::{Name, sites::Site};

impl Lifter<'_> {
    /// Recover the branch buffers the conditional declaration emits lazily.
    pub(super) fn conditional(
        &mut self,
        block: &[k::Stmt],
        site: &Site,
        at: usize,
    ) -> Lift<Option<(Name, source::Expr)>> {
        let Some(choice) = crate::constructs::conditional::CONDITIONAL.recognize(block) else {
            return Ok(None);
        };
        if !temporary(choice.name) || self.style.preserve_temporaries {
            return Ok(None);
        }
        let condition = self.condition(choice.condition)?;
        let mut arm = |arm: crate::constructs::conditional::Arm<'_>, child| {
            let outer = self.pending.len();
            let arm_site = site.child((at + 1) as u32).child(child);
            let mut index = 0;
            while index < arm.prelude.len() {
                let taken = self.pend(&arm.prelude[index..], &arm_site, index)?;
                if taken == 0 {
                    return refuse("an authored statement in a conditional operand");
                }
                index += taken;
            }
            let value = self.rhs(arm.value)?;
            if self.pending.len() > outer {
                return refuse("an unconsumed conditional operand temporary");
            }
            Ok(value)
        };
        let then = arm(choice.then, 1)?;
        let otherwise = arm(choice.otherwise, 2)?;
        Ok(Some((
            choice.name.clone(),
            source::Expr::Conditional(Box::new(condition), Box::new(then), Box::new(otherwise)),
        )))
    }

    /// `a || b` (`a && b`, `a ?? b`) whose right operand needs statements of its own:
    /// `let t = a; if t {} else { …; set t = b }`, the arm's statements
    /// all temporaries `b` reads.
    pub(super) fn short_circuit(
        &mut self,
        block: &[k::Stmt],
        site: &Site,
        at: usize,
    ) -> Lift<Option<(Name, source::Expr)>> {
        let [
            k::Stmt::Let { name, value: left },
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
        let tested = match condition {
            k::Expr::Call { function, args } if self.is(function, "ts.to_boolean") => {
                args.as_slice()
            }
            condition => std::slice::from_ref(condition),
        };
        if !temporary(name) || self.style.preserve_temporaries {
            return Ok(None);
        }
        let (operator, arm, child) = if self.nullish(condition, name) && else_block.is_empty() {
            ("??", then_block, 1)
        } else if tested == [k::Expr::Variable(name.clone())] {
            match (then_block.is_empty(), else_block.is_empty()) {
                (true, false) => ("||", else_block, 2),
                (false, true) => ("&&", then_block, 1),
                _ => return Ok(None),
            }
        } else {
            return Ok(None);
        };
        let [
            prelude @ ..,
            k::Stmt::Assign {
                place: k::Place::Variable(set),
                value: right,
                order: k::AssignOrder::ValueFirst,
            },
        ] = arm.as_slice()
        else {
            return Ok(None);
        };
        if set != name {
            return Ok(None);
        }
        let left = self.rhs(left)?;
        let outer = self.pending.len();
        let arm_site = site.child((at + 1) as u32).child(child);
        let mut index = 0;
        while index < prelude.len() {
            match self.pend(&prelude[index..], &arm_site, index)? {
                0 => return refuse("a statement in a short circuit's right operand"),
                taken => index += taken,
            }
        }
        let right = self.rhs(right)?;
        if self.pending.len() > outer {
            return refuse("a temporary a short circuit's right operand does not read");
        }
        Ok(Some((
            name.clone(),
            source::Expr::Binary(operator, Box::new(left), Box::new(right)),
        )))
    }
}
