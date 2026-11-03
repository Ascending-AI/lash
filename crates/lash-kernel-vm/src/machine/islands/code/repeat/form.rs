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
        let args = arguments(expr)?;
        let [a, b] = args else {
            return None;
        };
        Some(Binary {
            scalar: Scalar::of(unpin(expr), self.exe, comparison)?,
            inputs: [self.input(a)?, self.input(b)?],
        })
    }

    fn copy(&self, statement: &Statement, from: u32) -> Option<Copy> {
        let (at, bind, value) = write(statement)?;
        let unary = if local(value) == Some(from) {
            None
        } else {
            let args = arguments(value)?;
            if args.len() != 1 || local(&args[0]) != Some(from) {
                return None;
            }
            Some(Scalar::of(value, self.exe, false)?)
        };
        Some(Copy {
            at,
            keep: !bind,
            unary,
        })
    }
}

impl Repeat {
    pub(in crate::machine::islands) fn of(
        statement: &Statement,
        shared: &[bool],
        exe: &Executable,
        placement: std::sync::Arc<crate::compile::room::placement::BindingPlacement>,
    ) -> Option<Self> {
        let StatementKind::While { cond, body, .. } = &statement.kind else {
            return None;
        };
        if body.statements.len() > 4 {
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
        let args = arguments(cond)?;
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
        if writes[update_at].1 || body.declares.contains(&counter) {
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
        // Lowerings may preserve the induction value either before the
        // reduction or immediately before its write. Match the dependency
        // graph while retaining that source order in the charge schedule.
        let mut before = if update_at > 0 {
            cx.copy(&body.statements[0], counter)
        } else {
            None
        };
        let copy_after_reduction = before.is_none() && update_at == 2;
        if copy_after_reduction {
            before = Some(cx.copy(&body.statements[1], counter)?);
        }
        if !copy_after_reduction && let Some(copy) = &before {
            cx.previous = Some(copy.at);
        }
        let reduction_at = usize::from(before.is_some() && !copy_after_reduction);
        let reduction_end = update_at - usize::from(copy_after_reduction);
        let reduction = if reduction_at < reduction_end {
            if reduction_at + 1 != reduction_end || writes[reduction_at].1 {
                return None;
            }
            let (at, _, expr) = writes[reduction_at];
            cx.accumulator = Some(at);
            let args = arguments(expr)?;
            let [a, b] = args else {
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
                scale,
            })
        } else {
            None
        };
        // An update cannot read the reduced accumulator: that would make the
        // induction dependent on another graph position outside this template.
        cx.accumulator = None;
        if let Some(copy) = &before {
            cx.previous = Some(copy.at);
        }
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
        };
        let after = match body.statements.get(update_at + 1) {
            Some(statement) => {
                if update_at + 2 != body.statements.len() {
                    return None;
                }
                Some(cx.copy(statement, counter)?)
            }
            None => None,
        };
        let proof = proof::Proof::compile(
            exe,
            std::sync::Arc::clone(&placement),
            statement,
            proof::Targets {
                counter,
                before: before.as_ref().map(|copy| copy.at),
                reduced: reduction.as_ref().map(|reduce| reduce.at),
                after: after.as_ref().map(|copy| copy.at),
            },
        )?;
        let run = Self {
            source_room: std::sync::Arc::clone(placement.source()),
            source_site: exe.stmt(statement.id).site.clone(),
            proof,
            test,
            before,
            copy_after_reduction,
            reduction,
            update,
            after,
        };
        (run.verify(statement, exe, shared) && run.proof.verify(exe).is_ok()).then_some(run)
    }
}
