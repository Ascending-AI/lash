//! Compose a verified call's virtual survivors with its caller continuation.
//! Only entry roots and final survivors enter the boundary transaction.
use super::calls::BoundCall;
use super::*;

pub(super) struct Transfer {
    pub(super) inputs: Vec<Slot>,
    pub(super) outputs: Vec<(Slot, Value, Option<Slot>)>,
    pub(super) reservation: logical::InvocationRange,
    callee: Arc<Source>,
    occurrences: Vec<usize>,
    depth: u64,
}

impl ScalarRegion {
    #[expect(
        clippy::too_many_arguments,
        reason = "actual source activation and composed call authority"
    )]
    pub(crate) fn continue_call<'a>(
        &'a self,
        call: BoundCall<'_>,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        invocation: &logical::Invocation,
        activation: &super::super::placement::DenseActivation,
        values: ScalarRegionValues<'_>,
    ) -> Result<Option<BoundScalarRegion<'a>>, doc::heap::HeapFault> {
        if !call.current(ledger, invocation)
            || !Arc::ptr_eq(call.caller_source(), &self.source)
            || !activation.current(&self.source, ledger, invocation)
            || self.interval.events().next().map(|event| &event.point)
                != self
                    .source
                    .next_after_commit(&call.point().site)
                    .ok()
                    .as_ref()
            || values.values.len() != self.facts
        {
            return Ok(None);
        }
        let mut inputs = call.inputs.clone();
        let mut prices = vec![0_i128; self.prices];
        for entry in &self.entries {
            let value = values.values[entry.live.fact.0];
            if let Some((_, expected, _)) =
                call.live.iter().find(|(slot, _, _)| *slot == entry.slot)
            {
                if !value.same(expected) {
                    return Ok(None);
                }
            } else {
                if !activation
                    .value(entry.slot)
                    .is_some_and(|expected| value.same(&expected))
                {
                    return Ok(None);
                }
                let Some(holding) = ledger.holding(invocation.owner(entry.slot), heap.pages())?
                else {
                    return Ok(None);
                };
                let mut scalar = true;
                holding.visit_edges(heap.pages(), &mut |_, _| {
                    scalar = false;
                    Ok(())
                })?;
                if !scalar
                    || holding.bytes() != Holding::from_native(arena, heap.pages(), &value)?.bytes()
                {
                    return Ok(None);
                }
                if !inputs.contains(&entry.slot) {
                    inputs.push(entry.slot)
                }
            }
            prices[entry.live.price] =
                i128::from(Holding::from_native(arena, heap.pages(), &value)?.bytes());
        }
        for slot in &self.vacant {
            if ledger
                .holding(invocation.owner(*slot), heap.pages())?
                .is_some()
                || call.live.iter().any(|(live, _, _)| live == slot)
            {
                return Ok(None);
            }
        }
        let Some(mut bound) = self.valuate(ledger, arena, heap, invocation, values, prices)? else {
            return Ok(None);
        };
        let mut outputs: Vec<_> = self
            .survivors
            .iter()
            .map(|(slot, live)| {
                let origin = live.entry.and_then(|entry| {
                    call.live
                        .iter()
                        .find(|(slot, _, _)| *slot == entry)
                        .map_or(Some(entry), |(_, _, origin)| *origin)
                });
                (*slot, bound.values[live.fact.0], origin)
            })
            .collect();
        for &(slot, value, origin) in &call.live {
            if !self.entries.iter().any(|entry| entry.slot == slot) {
                if outputs.iter().any(|(output, _, _)| *output == slot) {
                    return Ok(None);
                }
                outputs.push((slot, value, origin));
            }
        }
        let Some(peak) = call
            .delta()
            .checked_add(i128::from(bound.peak))
            .map(|peak| peak.max(i128::from(call.peak())))
            .and_then(|peak| u64::try_from(peak).ok())
        else {
            return Ok(None);
        };
        let Some(delta) = call.delta().checked_add(bound.delta) else {
            return Ok(None);
        };
        let callee = Arc::clone(call.callee_source());
        let occurrences = call.occurrences().to_vec();
        let depth = call.depth();
        let Some(reservation) = call.finish(ledger, invocation) else {
            return Ok(None);
        };
        bound.peak = peak;
        bound.delta = delta;
        bound.transfer = Some(Transfer {
            inputs,
            outputs,
            reservation,
            callee,
            occurrences,
            depth,
        });
        Ok(Some(bound))
    }
}

impl BoundScalarRegion<'_> {
    pub(crate) fn composed_source(&self) -> Option<&Arc<Source>> {
        self.transfer.as_ref().map(|transfer| &transfer.callee)
    }
    pub(crate) fn occurrences(&self) -> &[usize] {
        self.transfer
            .as_ref()
            .map_or(&[], |transfer| &transfer.occurrences)
    }
    pub(crate) fn depth(&self) -> u64 {
        self.transfer.as_ref().map_or(0, |transfer| transfer.depth)
    }
}
