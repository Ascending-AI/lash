//! Match only the finite induction/copy/reduction graph. The source AST and
//! source charge summaries are separately verified before this projection.

use super::*;

struct Match<'a> {
    counter: u32,
    previous: Option<u32>,
    accumulator: Option<u32>,
    written: &'a [u32],
    shared: &'a [bool],
    exe: &'a Executable,
}

impl Match<'_> {
    fn input(&self, expr: &Expr) -> Option<Input> {
        match unpin(expr).kind {
            Expression::Const(value @ (Value::Int(_) | Value::Float(_))) => {
                Some(Input::Literal(value))
            }
            _ => {
                let at = local(expr)?;
                if *self.shared.get(at as usize)? {
                    return None;
                }
                if at == self.counter {
                    Some(Input::Counter)
                } else if Some(at) == self.previous {
                    Some(Input::Previous)
                } else if Some(at) == self.accumulator {
                    Some(Input::Accumulator)
                } else {
                    (!self.written.contains(&at)).then_some(Input::Invariant(at))
                }
            }
        }
    }

    fn binary(&self, expr: &Expr, comparison: bool) -> Option<Binary> {
        let Expression::Operator { args, .. } = &unpin(expr).kind else {
            return None;
        };
        let [a, b] = &**args else {
            return None;
        };
        Some(Binary {
            scalar: Scalar::of(unpin(expr), self.exe, comparison)?,
            inputs: [self.input(a)?, self.input(b)?],
        })
    }

    fn copy(&self, statement: &Statement, source: &Summary, from: u32) -> Option<Copy> {
        let (at, bind, value) = write(statement)?;
        let unary = if local(value) == Some(from) {
            None
        } else {
            let Expression::Operator { args, .. } = &value.kind else {
                return None;
            };
            if args.len() != 1 || local(&args[0]) != Some(from) {
                return None;
            }
            Some(Scalar::of(value, self.exe, false)?)
        };
        Some(Copy {
            at,
            keep: !bind,
            charge: Charge::of(source, usize::from(unary.is_some()))?,
            unary,
        })
    }
}

impl Repeat {
    pub(in crate::machine::islands) fn of(
        statement: &Statement,
        plan: &LoopPlan,
        shared: &[bool],
        exe: &Executable,
        charged: bool,
    ) -> Option<Self> {
        let StatementKind::While { cond, body, .. } = &statement.kind else {
            return None;
        };
        let Head::While(test_source) = &plan.head else {
            return None;
        };
        if body.statements.len() != plan.body.steps.len() || body.statements.len() > 4 {
            return None;
        }
        let writes = body
            .statements
            .iter()
            .map(write)
            .collect::<Option<Vec<_>>>()?;
        let written: Vec<_> = writes.iter().map(|(at, _, _)| *at).collect();
        if written
            .iter()
            .enumerate()
            .any(|(i, at)| written[..i].contains(at) || shared[*at as usize])
        {
            return None;
        }
        let Expression::Operator { args, .. } = &unpin(cond).kind else {
            return None;
        };
        let candidates: Vec<_> = args
            .iter()
            .filter_map(local)
            .filter(|at| written.contains(at))
            .collect();
        let [counter] = &*candidates else {
            return None;
        };
        let counter = *counter;
        let update_at = writes.iter().position(|(at, _, _)| *at == counter)?;
        if writes[update_at].1 || plan.body.declares.contains(&counter) {
            return None;
        }
        let sources = plan
            .body
            .steps
            .iter()
            .map(summary)
            .collect::<Option<Vec<_>>>()?;
        if sources
            .iter()
            .zip(&writes)
            .any(|((source, bind), (at, binding, _))| {
                bind != binding || source.write != Some(RegionSlot::Frame(*at))
            })
        {
            return None;
        }
        let mut cx = Match {
            counter,
            previous: None,
            accumulator: None,
            written: &written,
            shared,
            exe,
        };
        let test = cx.binary(unpin(cond), true)?;
        let before = if update_at > 0 {
            cx.copy(&body.statements[0], sources[0].0, counter)
        } else {
            None
        };
        if let Some(copy) = &before {
            cx.previous = Some(copy.at);
        }
        let reduction_at = usize::from(before.is_some());
        let reduction = if reduction_at < update_at {
            if reduction_at + 1 != update_at || writes[reduction_at].1 {
                return None;
            }
            let (at, _, expr) = writes[reduction_at];
            cx.accumulator = Some(at);
            let Expression::Operator { args, .. } = &expr.kind else {
                return None;
            };
            let [a, b] = &**args else {
                return None;
            };
            let reverse = local(b) == Some(at);
            let term = if local(a) == Some(at) {
                b
            } else if reverse {
                a
            } else {
                return None;
            };
            let (scale, term) = match cx.input(term) {
                Some(input @ (Input::Counter | Input::Previous)) => (None, input),
                _ => {
                    let scale = cx.binary(unpin(term), false)?;
                    if !scale
                        .inputs
                        .iter()
                        .any(|input| matches!(input, Input::Counter | Input::Previous))
                    {
                        return None;
                    }
                    (Some(scale), Input::Counter)
                }
            };
            Some(Reduction {
                at,
                fold: Scalar::of(expr, exe, false)?,
                term,
                reverse,
                charge: Charge::of(sources[reduction_at].0, 1 + usize::from(scale.is_some()))?,
                scale,
            })
        } else {
            None
        };
        // An update cannot read the reduced accumulator: that would make the
        // induction dependent on another graph position outside this template.
        cx.accumulator = None;
        let operation = cx.binary(writes[update_at].2, false)?;
        if !operation
            .inputs
            .iter()
            .any(|input| matches!(input, Input::Counter | Input::Previous))
        {
            return None;
        }
        let update = Update {
            at: counter,
            operation,
            charge: Charge::of(sources[update_at].0, 1)?,
        };
        let after = match body.statements.get(update_at + 1) {
            Some(statement) => {
                if update_at + 2 != body.statements.len() {
                    return None;
                }
                Some(cx.copy(statement, sources[update_at + 1].0, counter)?)
            }
            None => None,
        };
        let run = Self {
            source_room: std::sync::Arc::clone(&plan.source_room),
            source_site: plan.source_site.clone(),
            charged,
            test,
            test_charge: Charge::of(test_source, 1)?,
            before,
            reduction,
            update,
            after,
        };
        run.verify(plan, exe, shared).then_some(run)
    }
}
