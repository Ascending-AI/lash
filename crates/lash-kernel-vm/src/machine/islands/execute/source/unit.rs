//! The empty-tuple comparison installs its nursery only after shared acceptance.
use super::*;
use crate::admission::SourceResources;
use room::publication::Occurrence;
impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_unit_comparison(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &UnitSite,
        value: Value,
    ) -> Result<bool, HeapFault> {
        let Some(selected) = &site.source else {
            return Ok(false);
        };
        self.logical.flush(self.arena, self.heap)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("unit comparison has no source activation"))?;
        if activation.original.is_some() {
            return Ok(false);
        }
        let store = self.logical.ledger();
        let occurrence = Occurrence {
            task: self.task,
            invocation: activation.invocation.id(),
            loops: frame
                .state
                .control
                .iter()
                .filter_map(|control| match control {
                    Position::Loop { id, started, .. } => Some((
                        self.exe.stmt(*id).site.clone(),
                        if *id == selected.statement {
                            *started
                        } else {
                            started.saturating_sub(1)
                        },
                    )),
                    _ => None,
                })
                .collect(),
            action: None,
        };
        let start = selected
            .compiled
            .start(&store.borrow(), self.heap, occurrence)?;
        if let Some(frontier) = activation.frontier.as_mut()
            && frontier.departs(&start)
        {
            frontier.handoff(&mut store.borrow_mut(), start.clone())?;
        }
        if activation.frontier.is_none() {
            let source = &self.exe.code(frame.state.callee.plan.code).source_room;
            let document = self
                .exe
                .instances
                .get(&frame.state.callee.link)
                .ok_or_else(|| malformed("unit source has no defining document"))?;
            let kernel = doc::KernelVersion::of(document.kernel())
                .ok_or_else(|| malformed("unit source kernel is unsupported"))?;
            activation.frontier = Some(
                source
                    .frontier(
                        &store.borrow(),
                        self.heap,
                        &activation.invocation,
                        frame.state.callee.link,
                        kernel,
                        start,
                    )
                    .map_err(|_| malformed("unit source frontier failed verification"))?,
            );
        }
        let frontier = activation
            .frontier
            .as_mut()
            .ok_or_else(|| malformed("unit source frontier disappeared"))?;
        let ledger = store.borrow();
        let safe_point = self.charged;
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: (self.base + u64::from(self.depth)).saturating_sub(1),
            limit: self.call_depth,
            safe_point,
        };
        let Some((accepted, debt)) = selected.compiled.prepare(
            self.exe,
            frame.state.callee.plan.code,
            frontier,
            &activation.invocation,
            &ledger,
            self.heap,
            self.arena,
            frame.slots,
            value,
            resources,
            self.memory_bound,
        )?
        else {
            return Ok(false);
        };
        drop(ledger);
        let balance = accepted.publish_unit(frontier, &mut store.borrow_mut())?;
        self.logical_memory.set(balance.total()?);
        if let Some(tape) = self.recorder {
            debt.record(tape);
        }
        witnessed!(SourceUnit, Commit);
        Ok(true)
    }
}
pub(super) fn member_fault(
    error: lash_kernel_state::kernel_internals::logical::MemberFault,
) -> HeapFault {
    match error {
        lash_kernel_state::kernel_internals::logical::MemberFault::Heap(error) => error,
        lash_kernel_state::kernel_internals::logical::MemberFault::Native(error) => {
            publication::native_fault(error)
        }
    }
}
