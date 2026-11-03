//! Check graph roles against the resolved source statements and loop plan.

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
        let Some(args) = arguments(source) else {
            return false;
        };
        let [left, right] = args else {
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
                let Some(args) = arguments(source) else {
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
        self.pair.map(|p| (p.ints, p.floats)) == canonical.pair.map(|p| (p.ints, p.floats))
            && self.site.as_ref().map(|s| (s.lib, s.pc, s.reg, s.checked))
                == canonical
                    .site
                    .as_ref()
                    .map(|s| (s.lib, s.pc, s.reg, s.checked))
    }
}

fn next<'a>(
    statements: &mut impl Iterator<Item = &'a syntax::Write>,
    at: u32,
    bind: bool,
) -> Option<&'a Expr> {
    let (actual, binding, value) = write(statements.next()?);
    (actual == at && binding == bind).then_some(value)
}

impl Repeat {
    pub(in crate::machine::islands) fn verify(
        &self,
        statement: &Statement,
        exe: &Executable,
        shared: &[bool],
    ) -> bool {
        let Some(original) = syntax::LoopSyntax::read(exe, statement.id) else {
            return false;
        };
        let cond = &original.cond;
        let body = &original.body;
        if !std::sync::Arc::ptr_eq(&self.source_room, self.proof.recurrence.iteration.source())
            || self.source_site != exe.stmt(statement.id).site
        {
            return false;
        }
        let written: Vec<_> = body.iter().map(|s| write(s).0).collect();
        if written
            .iter()
            .enumerate()
            .any(|(i, at)| written[..i].contains(at) || shared.get(*at as usize) != Some(&false))
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
        if !roles.binary(&self.test, cond, exe, true) {
            return false;
        }
        let mut statements = body.iter();
        if !self.copy_after_reduction
            && let Some(copy) = &self.before
        {
            let Some(source) = next(&mut statements, copy.at, !copy.keep) else {
                return false;
            };
            if !roles.copy(copy, source, exe) {
                return false;
            }
            roles.previous = Some(copy.at);
        }
        if let Some(reduce) = &self.reduction {
            let Some(source) = next(&mut statements, reduce.at, false) else {
                return false;
            };
            let Some(args) = arguments(source) else {
                return false;
            };
            let [left, right] = args else {
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
        if self.copy_after_reduction {
            let Some(copy) = &self.before else {
                return false;
            };
            let Some(source) = next(&mut statements, copy.at, !copy.keep) else {
                return false;
            };
            if self.reduction.is_none() || !roles.copy(copy, source, exe) {
                return false;
            }
            roles.previous = Some(copy.at);
        }
        let Some(source) = next(&mut statements, self.update.at, false) else {
            return false;
        };
        if !roles.binary(&self.update.operation, source, exe, false) {
            return false;
        }
        if let Some(copy) = &self.after {
            let Some(source) = next(&mut statements, copy.at, !copy.keep) else {
                return false;
            };
            if !roles.copy(copy, source, exe) {
                return false;
            }
        }
        statements.next().is_none()
    }
}
