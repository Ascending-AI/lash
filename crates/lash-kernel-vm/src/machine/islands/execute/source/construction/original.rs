//! Constructor graph publication issues the original source charge receipt.
use super::*;
use crate::admission::SourceResources;
use crate::machine::source_publication::ExternalEntry;
use room::publication::{ExactEvent, PublicationError, Successor};

pub(in crate::machine::islands::execute) struct OriginalConstruction<'a> {
    pub(in crate::machine::islands::execute) original:
        &'a std::sync::Arc<room::charges::OriginalPrefix>,
    pub(in crate::machine::islands::execute) registers: Vec<Value>,
    pub(in crate::machine::islands::execute) construction: storage::Construction<'a>,
}
impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn defer_original_close(
        &mut self,
        frame: &Values<'_, '_>,
        close: &crate::compile::expr::Close,
    ) -> Result<bool, Stop> {
        let Some(source) = frame.state.source.as_ref() else {
            return Ok(false);
        };
        let Some(original) = source.original.as_ref() else {
            return Ok(false);
        };
        let Some(frontier) = source.frontier.as_ref() else {
            return Err(self.at(close.head));
        };
        if original
            .defer_close(
                frontier,
                &self.logical.ledger().borrow(),
                close,
                self.charged,
            )
            .is_err()
        {
            return Err(self.at(close.head));
        }
        Ok(true)
    }
    pub(in crate::machine::islands::execute) fn publish_original_construction(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        prefix: publication::Prefix,
        candidate: OriginalConstruction<'_>,
    ) -> Result<Option<Value>, HeapFault> {
        self.logical.flush(self.arena, self.heap)?;
        let source = &self.exe.code(callee.plan.code).source_room;
        let store = self.logical.ledger();
        if let Some(frontier) = activation.frontier.as_mut()
            && frontier.departs(&prefix.start)
        {
            if let Some(original) = activation.original.as_ref() {
                if !original.enter_event(
                    frontier,
                    &mut store.borrow_mut(),
                    prefix.start.clone(),
                    self.charged,
                )? {
                    return Ok(None);
                }
            } else {
                frontier.handoff(&mut store.borrow_mut(), prefix.start.clone())?;
            }
        }
        let ledger = store.borrow();
        if activation.frontier.is_none() {
            let document = self
                .exe
                .instances
                .get(&callee.link)
                .ok_or_else(|| malformed("constructor source document is absent"))?;
            let kernel = doc::KernelVersion::of(document.kernel())
                .ok_or_else(|| malformed("constructor source kernel is unsupported"))?;
            activation.frontier = Some(
                source
                    .frontier(
                        &ledger,
                        self.heap,
                        &activation.invocation,
                        callee.link,
                        kernel,
                        prefix.start.clone(),
                    )
                    .map_err(|_| malformed("constructor source frontier failed authentication"))?,
            );
        }
        let frontier = activation
            .frontier
            .as_mut()
            .ok_or_else(|| malformed("constructor frontier disappeared"))?;
        let Ok(event) = ExactEvent::compile(source, prefix.interval.clone(), Successor::Normal)
        else {
            return Ok(None);
        };
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
        if prefix.allowance.safe_point != safe_point {
            return Ok(None);
        }
        let Some(facts) = resources.facts(&prefix.allowance) else {
            return Ok(None);
        };
        let Some((prepared, storage)) = candidate.construction.prepare_original(
            self.arena,
            ExternalEntry {
                event: &event,
                frontier,
                ledger: &ledger,
                heap: self.heap,
                prefix,
                resources: facts,
            },
            (
                candidate.original,
                &candidate.registers,
                activation.original.as_ref(),
            ),
        )?
        else {
            return Ok(None);
        };
        let Some(accepted) = prepared.admit(frontier, &ledger, resources, self.memory_bound)?
        else {
            return Ok(None);
        };
        drop(ledger);
        let value = storage.value;
        let balance = accepted
            .publish_original_event(
                frontier,
                &mut store.borrow_mut(),
                &mut activation.original,
                |arena, edits| {
                    debug_assert!(edits.is_empty());
                    storage.batch.publish(arena);
                },
                self.recorder,
            )
            .map_err(|error| match error {
                PublicationError::Heap(error) => error,
                PublicationError::Miss(reason) => malformed(reason),
            })?;
        self.logical_memory.set(balance.total()?);
        #[cfg(test)]
        super::super::super::super::compiled::original_paid(self.charged - safe_point);
        Ok(Some(value))
    }
}
