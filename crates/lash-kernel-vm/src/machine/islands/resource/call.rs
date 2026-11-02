//! A statement stitches source-local caller prefixes to an authenticated pure
//! callee and its real caller continuation, before entering a physical frame.
use super::*;
use room::{placement::DenseActivation, region::calls::VerifiedCall};

pub(super) struct CallPlan {
    before: Expression,
    prefix: Option<ScalarRegion>,
    call: VerifiedCall,
    after: Expression,
}

impl CallPlan {
    pub(super) fn compile(
        exe: &Executable,
        placement: &BindingPlacement,
        id: StmtId,
        value: &expr::Expr,
        through: &Point,
    ) -> Option<(Self, ScalarRegion, ChargeProgram, CompiledExit)> {
        let (stream, statement, _) = exe.statement(id);
        let source = placement.source();
        let (mut before, pc, mut after) = Expression::split_call(exe, stream, placement, value)?;

        let call = VerifiedCall::compile(exe, placement.code(), id, pc).ok()?;
        let prefix = if before.empty() {
            None
        } else {
            let route: Vec<_> = before
                .points()
                .map(|at| Selection::Event {
                    at: at.clone(),
                    exit: ExitKind::Normal,
                })
                .collect();
            let last = before.points().last()?;
            let interval = source
                .compile_between(before.first(), last, &route, route.len())
                .ok()?;
            let region = source.compile_scalar_region(interval).ok()?;

            before.bind_facts(exe, &region)?;

            Some(region)
        };
        let first = match &prefix {
            Some(region) => region.interval().events().next()?.point.clone(),
            None => call.at().clone(),
        };
        let next = source.next_after_commit(&call.at().site).ok()?;
        let route: Vec<_> = after
            .points()
            .chain(std::iter::once(through))
            .map(|at| Selection::Event {
                at: at.clone(),
                exit: ExitKind::Normal,
            })
            .collect();
        if route
            .first()
            .is_none_or(|selected| !matches!(selected, Selection::Event { at, .. } if *at == next))
        {
            return None;
        }
        let interval = call.compile_continuation(exe, through, &route).ok()?;
        let region = source.compile_scalar_region(interval).ok()?;

        after.bind_facts(exe, &region)?;

        let start = pc.checked_add(1)?;
        let charges = ChargeProgram::compile(
            exe,
            placement.code(),
            id,
            lash_kernel_exec::OpRange {
                start,
                len: statement.ops.end().checked_sub(start)?,
            },
        )
        .ok()?;

        let exit = CompiledExit::composed(exe, &region, first, &call).ok()?;
        Some((
            Self {
                before,
                prefix,
                call,
                after,
            },
            region,
            charges,
            exit,
        ))
    }

    pub(super) fn verify(
        &self,
        stream: &Stream,
        placement: &BindingPlacement,
    ) -> Result<(), room::Refused> {
        self.before.verify(stream, placement)?;
        self.after.verify(stream, placement)?;
        if let Some(prefix) = &self.prefix {
            prefix.verify()?;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one source activation with an authenticated continuation"
    )]
    pub(super) fn evaluate<'a>(
        &'a self,
        exe: &Executable,
        activation: &DenseActivation,
        slots: &[SlotState],
        invocation: &Invocation,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        region: &'a ScalarRegion,
        charges: &'a ChargeProgram,
    ) -> Result<Option<EvaluatedStatement<'a>>, HeapFault> {
        let (call, mut initial) = if let Some(prefix) = &self.prefix {
            let mut values = entry_values(prefix, activation)?;
            if values.len() != prefix.fact_count() {
                return Ok(None);
            }
            let Some(before) = self.before.evaluate(exe, slots, arena, heap, &mut values)? else {
                return Ok(None);
            };
            let Some(bound) = prefix.bind_activation(
                ledger,
                arena,
                heap,
                invocation,
                activation,
                ScalarRegionValues {
                    values: &values,
                    calls: &before.calls,
                },
            )?
            else {
                return Ok(None);
            };
            let Some(call) =
                self.call
                    .bind(exe, self.call.callee(), ledger, arena, heap, &bound)?
            else {
                return Ok(None);
            };

            (call, before.values)
        } else {
            let position = room::publication::Position {
                at: self.call.at().clone(),
                unpaid: Some(self.call.at().clone()),
                recovery: [0; 32],
                occurrence: room::publication::Occurrence {
                    task: match invocation.frame() {
                        lash_kernel_state::kernel_internals::logical::Owner::Frame {
                            task, ..
                        } => task,
                        _ => return Ok(None),
                    },
                    invocation: invocation.id(),
                    loops: vec![],
                    action: None,
                },
            };
            let Some(call) = self.call.bind_entry(
                exe,
                self.call.callee(),
                ledger,
                arena,
                heap,
                invocation,
                activation,
                &position,
            )?
            else {
                return Ok(None);
            };
            (call, vec![])
        };

        initial.push(call.result());
        let mut values = vec![Value::Null; region.fact_count()];
        for (slot, fact) in region.entries() {
            let Some(value) = call
                .continuation_values()
                .find_map(|(candidate, value)| (candidate == slot).then_some(value))
                .or_else(|| activation.value(slot))
                .filter(|value| scalar(*value))
            else {
                return Ok(None);
            };
            values[fact.index()] = value;
        }
        let Some(after) =
            self.after
                .evaluate_from(exe, slots, arena, heap, &mut values, &initial)?
        else {
            return Ok(None);
        };

        let Some(suffix) = bind_debt(charges, &after.formulas) else {
            return Ok(None);
        };
        let Some(debt) = call.debt() else {
            return Ok(None);
        };
        let Some(bound) = region.continue_call(
            call,
            ledger,
            arena,
            heap,
            invocation,
            activation,
            ScalarRegionValues {
                values: &values,
                calls: &after.calls,
            },
        )?
        else {
            return Ok(None);
        };

        Ok(Some(EvaluatedStatement {
            result: after.result,
            bound,
            debt: StatementDebt::Call { call: debt, suffix },
            composed: true,
        }))
    }
}
