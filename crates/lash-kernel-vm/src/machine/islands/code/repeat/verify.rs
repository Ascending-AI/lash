//! Check graph roles against the separately verified source summaries.

use super::*;

struct Roles<'a> {
    counter: u32,
    previous: Option<u32>,
    accumulator: Option<u32>,
    written: &'a [u32],
    shared: &'a [bool],
}

impl Roles<'_> {
    fn input(&self, input: Input, source: &Expr) -> bool {
        match input {
            Input::Literal(value) => match (value, &unpin(source).kind) {
                (Value::Int(a), Expression::Const(Value::Int(b))) => a == *b,
                (Value::Float(a), Expression::Const(Value::Float(b))) => a == *b,
                _ => false,
            },
            _ => {
                let Some(at) = local(source) else {
                    return false;
                };
                if self.shared.get(at as usize) != Some(&false) {
                    return false;
                }
                match input {
                    Input::Counter => at == self.counter,
                    Input::Previous => Some(at) == self.previous,
                    Input::Accumulator => Some(at) == self.accumulator,
                    Input::Invariant(slot) => at == slot && !self.written.contains(&at),
                    Input::Literal(_) => unreachable!(),
                }
            }
        }
    }

    fn binary(&self, binary: &Binary, source: &Expr, exe: &Executable, comparison: bool) -> bool {
        let Expression::Operator { args, .. } = &unpin(source).kind else {
            return false;
        };
        let [left, right] = &**args else {
            return false;
        };
        binary.scalar.verifies(source, exe, comparison)
            && self.input(binary.inputs[0], left)
            && self.input(binary.inputs[1], right)
    }

    fn copy(&self, copy: &Copy, source: &Expr, exe: &Executable) -> bool {
        match &copy.unary {
            None => self.input(Input::Counter, source),
            Some(unary) => {
                let Expression::Operator { args, .. } = &unpin(source).kind else {
                    return false;
                };
                args.len() == 1
                    && unary.verifies(source, exe, false)
                    && self.input(Input::Counter, &args[0])
            }
        }
    }
}

impl Scalar {
    fn verifies(&self, source: &Expr, exe: &Executable, comparison: bool) -> bool {
        let Some(canonical) = Self::of(unpin(source), exe, comparison) else {
            return false;
        };
        let (a, b) = (&self.site, &canonical.site);
        a.lib == b.lib
            && a.checked == b.checked
            && a.pc == b.pc
            && a.reg == b.reg
            && a.close == b.close
            && a.fixed == b.fixed
            && a.counted.is_none()
            && a.act.is_none()
            && a.root.is_none()
    }
}

fn next<'a>(
    statements: &mut impl Iterator<Item = &'a Step>,
    at: u32,
    bind: bool,
    charge: &Charge,
) -> Option<&'a Expr> {
    let (source, binding) = summary(statements.next()?)?;
    if source.write != Some(RegionSlot::Frame(at)) || bind != binding || !charge.verifies(source) {
        return None;
    }
    source.scalar_root()
}

impl Repeat {
    pub(in crate::machine::islands) fn verify(
        &self,
        plan: &LoopPlan,
        exe: &Executable,
        shared: &[bool],
    ) -> bool {
        let Head::While(test) = &plan.head else {
            return false;
        };
        if !std::sync::Arc::ptr_eq(&self.source_room, &plan.source_room)
            || self.source_site != plan.source_site
            || !self.test_charge.verifies(test)
        {
            return false;
        }
        let written: Vec<_> = plan
            .body
            .steps
            .iter()
            .filter_map(|step| match summary(step)?.0.write? {
                RegionSlot::Frame(at) => Some(at),
                _ => None,
            })
            .collect();
        if written.len() != plan.body.steps.len()
            || written.iter().enumerate().any(|(i, at)| {
                written[..i].contains(at) || shared.get(*at as usize) != Some(&false)
            })
        {
            return false;
        }
        let mut roles = Roles {
            counter: self.update.at,
            previous: None,
            accumulator: None,
            written: &written,
            shared,
        };
        let Some(source) = test.scalar_root() else {
            return false;
        };
        if !roles.binary(&self.test, source, exe, true) {
            return false;
        }
        let mut statements = plan.body.steps.iter();
        if let Some(copy) = &self.before {
            let Some(source) = next(&mut statements, copy.at, !copy.keep, &copy.charge) else {
                return false;
            };
            if !roles.copy(copy, source, exe) {
                return false;
            }
            roles.previous = Some(copy.at);
        }
        if let Some(reduce) = &self.reduction {
            let Some(source) = next(&mut statements, reduce.at, false, &reduce.charge) else {
                return false;
            };
            let Expression::Operator { args, .. } = &unpin(source).kind else {
                return false;
            };
            let [left, right] = &**args else {
                return false;
            };
            roles.accumulator = Some(reduce.at);
            let (accumulator, term) = if reduce.reverse {
                (right, left)
            } else {
                (left, right)
            };
            if !reduce.fold.verifies(source, exe, false)
                || !roles.input(Input::Accumulator, accumulator)
                || !match &reduce.scale {
                    Some(scale) => roles.binary(scale, term, exe, false),
                    None => roles.input(reduce.term, term),
                }
            {
                return false;
            }
            roles.accumulator = None;
        }
        let Some(source) = next(&mut statements, self.update.at, false, &self.update.charge) else {
            return false;
        };
        if !roles.binary(&self.update.operation, source, exe, false) {
            return false;
        }
        if let Some(copy) = &self.after {
            let Some(source) = next(&mut statements, copy.at, !copy.keep, &copy.charge) else {
                return false;
            };
            if !roles.copy(copy, source, exe) {
                return false;
            }
        }
        statements.next().is_none()
    }
}
