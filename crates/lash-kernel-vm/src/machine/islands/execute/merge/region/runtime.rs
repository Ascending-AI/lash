//! Direct collection packets publish one authenticated source boundary.
use super::*;
use crate::admission::SourceResources;
use direct::{CandidateContext, CollectionCandidate, CollectionEntry, CollectionSite};
use room::publication::Occurrence;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_collection<S>(
        &mut self,
        frame: &mut Values<'_, 'e>,
        packet: &CollectionPacket<S>,
    ) -> Option<Result<Next, Stop>>
    where
        CollectionSite<S>: CollectionCandidate,
    {
        match self.try_collection(frame, packet) {
            Ok(false) => None,
            Ok(true) => Some(Ok(Next::Continue)),
            Err(error) => Some(Err(self.halt(error.into()))),
        }
    }
    fn try_collection<S>(
        &mut self,
        frame: &mut Values<'_, 'e>,
        packet: &CollectionPacket<S>,
    ) -> Result<bool, HeapFault>
    where
        CollectionSite<S>: CollectionCandidate,
    {
        let Some(site) = packet.proof.get().and_then(Option::as_ref) else {
            return Ok(false);
        };
        if self.deferred_end.is_some() || self.spent() || self.projection().is_some() {
            return Ok(false);
        }
        self.logical.flush(self.arena, self.heap)?;
        let Some(source) = frame.state.source.as_ref() else {
            return Ok(false);
        };
        if source.original.is_some() || source.frontier.is_none() {
            return Ok(false);
        }
        let invocation = source.invocation.clone();
        let compiled = frame.state.callee.compiled;
        let Some(placement) = &compiled.placement else {
            return Ok(false);
        };
        let store = self.logical.ledger();
        let occurrence = Occurrence {
            task: self.task,
            invocation: invocation.id(),
            loops: frame
                .state
                .control
                .iter()
                .filter_map(|control| match control {
                    Position::Loop { id, started, .. } => {
                        Some((self.exe.stmt(*id).site.clone(), started.saturating_sub(1)))
                    }
                    _ => None,
                })
                .collect(),
            action: None,
        };
        let mut candidate = None;
        {
            let ledger = store.borrow();
            let Some(activation) = placement.capture(
                frame.state.callee.plan.code,
                &invocation,
                &ledger,
                self.heap,
                frame.slots,
            )?
            else {
                return Ok(false);
            };
            for exit in site.positions(occurrence) {
                if let Some(facts) = site.candidate(
                    CandidateContext {
                        exe: self.exe,
                        compiled,
                        ledger: &ledger,
                        arena: self.arena,
                        slots: frame.slots,
                        limits: (
                            self.limits.value_depth(),
                            self.call_depth.saturating_sub(
                                (self.base + u64::from(self.depth)).saturating_sub(1),
                            ) as u32,
                        ),
                    },
                    CollectionEntry {
                        activation: &activation,
                        invocation: &invocation,
                        exit,
                    },
                ) {
                    // Guest callbacks retain their precise source frames and
                    // saved roles at this handoff.
                    if facts.transfer.continuation == CollectionContinuation::Callback {
                        continue;
                    }
                    candidate = Some(facts);
                    break;
                }
            }
        }
        let Some(mut facts) = candidate else {
            return Ok(false);
        };
        if !facts.reserve(frame)? {
            return Ok(false);
        }
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("collection activation disappeared"))?;
        let Some(frontier) = activation.frontier.as_mut() else {
            return Ok(false);
        };
        let ledger = store.borrow();
        let Some(dense) = placement.capture(
            frame.state.callee.plan.code,
            &invocation,
            &ledger,
            self.heap,
            frame.slots,
        )?
        else {
            return Ok(false);
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
        let Some((accepted, storage, transfer)) = facts.accept(
            self.exe,
            frontier,
            &dense,
            &ledger,
            self.heap,
            self.arena,
            resources,
            self.memory_bound,
        )?
        else {
            return Ok(false);
        };
        drop(ledger);
        let balance = accepted.publish_boundary(
            frontier,
            &mut store.borrow_mut(),
            |arena, edits| storage.publish(arena, edits),
            self.recorder,
        )?;
        self.logical_memory.set(balance.total()?);
        self.install_collection_transfer(frame, transfer);
        Ok(true)
    }
}
