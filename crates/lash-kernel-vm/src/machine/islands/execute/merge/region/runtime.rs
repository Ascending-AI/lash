//! Ordinary source and grouped entries share one affine publication.
use super::*;
use crate::admission::SourceResources;
use direct::{CandidateContext, CollectionCandidate, CollectionEntry, CollectionSite};
use room::publication::Occurrence;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_collection<S>(
        &mut self,
        frame: &mut Values<'_, 'e>,
        packet: &CollectionPacket<S>,
    ) -> Option<Result<(Next, CollectionContinuation), Stop>>
    where
        CollectionSite<S>: CollectionCandidate,
    {
        match self.try_source_publication(frame, packet) {
            Ok(None) => None,
            Ok(Some(exit)) => Some(Ok((Next::Continue, exit))),
            Err(error) => Some(Err(self.halt(error.into()))),
        }
    }
    pub(in crate::machine::islands::execute) fn try_source_publication<S>(
        &mut self,
        frame: &mut Values<'_, 'e>,
        packet: &CollectionPacket<S>,
    ) -> Result<Option<CollectionContinuation>, HeapFault>
    where
        CollectionSite<S>: CollectionCandidate,
    {
        let Some(site) = packet.proof.get().and_then(Option::as_ref) else {
            return Ok(None);
        };
        if self.deferred_end.is_some() || self.spent() || self.projection().is_some() {
            return Ok(None);
        }
        self.logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[])?;
        let Some(source) = frame.state.source.as_ref() else {
            return Ok(None);
        };
        if source.original.is_some() || source.frontier.is_none() {
            return Ok(None);
        }
        let invocation = source.invocation.clone();
        let compiled = frame.state.callee.compiled;
        let Some(placement) = &compiled.placement else {
            return Ok(None);
        };
        let store = self.logical.clone();
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
                return Ok(None);
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
                    // A callback exit publishes only the verified prefix. The
                    // original call and its saved roles execute at transfer.pc.
                    candidate = Some(facts);
                    break;
                }
            }
        }
        let Some(mut facts) = candidate else {
            return Ok(None);
        };
        if !facts.reserve(frame)? {
            return Ok(None);
        }
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("collection activation disappeared"))?;
        let Some(frontier) = activation.frontier.as_mut() else {
            return Ok(None);
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
            return Ok(None);
        };
        drop(ledger);
        let balance = accepted.publish_boundary(
            frontier,
            &mut store.borrow_mut(),
            |arena, edits| storage.publish(arena, edits),
            self.recorder,
        )?;
        self.logical_memory.set(balance.total()?);
        let continuation = transfer.continuation;
        self.install_collection_transfer(frame, transfer);
        Ok(Some(continuation))
    }
}

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(in crate::machine::islands::execute) fn callback_statement(
        &mut self,
        frame: &mut Values<'_, 'e>,
        statement: crate::compile::StmtId,
    ) -> Result<bool, HeapFault> {
        let sites = &frame.state.callee.compiled.code.sites;
        for packet in &sites.collection_reduce {
            let shape = &packet.shape;
            if shape.callback_site.statement() != statement {
                continue;
            }
            let values = [shape.accumulator, shape.item].map(|at| match frame.slots.get(at) {
                Some(SlotState::Value(value)) => Some(*value),
                _ => None,
            });
            let [Some(a), Some(b)] = values else {
                return Ok(false);
            };
            let expected = if shape.reverse { [b, a] } else { [a, b] };
            return self.publish_callback(frame, &shape.callback_site, &expected);
        }
        for packet in &sites.collection_merge {
            let shape = &packet.shape;
            if shape.callback_site.statement() != statement {
                continue;
            }
            let values = shape.items.map(|at| match frame.slots.get(at) {
                Some(SlotState::Value(value)) => Some(*value),
                _ => None,
            });
            let [Some(a), Some(b)] = values else {
                return Ok(false);
            };
            return self.publish_callback(frame, &shape.callback_site, &[a, b]);
        }
        Ok(false)
    }
    pub(in crate::machine::islands::execute) fn publish_callback(
        &mut self,
        frame: &mut Values<'_, 'e>,
        callback: &crate::machine::islands::compiled::CollectionCallback,
        expected: &[Value],
    ) -> Result<bool, HeapFault> {
        use room::publication::sort::{CallbackProgram, CallbackTarget};
        if self.deferred_end.is_some() || self.spent() || self.projection().is_some() {
            return Ok(false);
        }
        let Some(source) = frame.state.source.as_ref() else {
            return Ok(false);
        };
        let Some(frontier) = source.frontier.as_ref() else {
            return Ok(false);
        };
        if source.original.is_some() {
            return Ok(false);
        }
        let compiled = frame.state.callee.compiled;
        let Some(placement) = &compiled.placement else {
            return Ok(false);
        };
        let code = frame.state.callee.plan.code;
        let (callee, target) = if let Some(at) = callback.binding() {
            let Some(SlotState::Value(value)) = frame.slots.get(at) else {
                return Ok(false);
            };
            let Some(callee) = CallbackProgram::resolve(self.exe, self.arena, *value) else {
                return Ok(false);
            };
            (callee, CallbackTarget::Value(*value))
        } else if let Some(callee) = callback.body() {
            (callee, CallbackTarget::Declared(callee))
        } else {
            return Ok(false);
        };
        let Some(program) = callback.program(self.exe, code, callee) else {
            return Ok(false);
        };
        let Some(first) = frontier.position().unpaid.clone() else {
            return Ok(false);
        };
        let through = room::Point {
            site: self.exe.stmt(callback.statement()).site.clone(),
            phase: room::Phase::Commit,
        };
        let Ok(caller) = program.caller_program(self.exe, first, through, &[]) else {
            return Ok(false);
        };
        let invocation = source.invocation.clone();
        let statement = callback.statement();
        let Some((block, index)) = compiled
            .blocks
            .iter()
            .enumerate()
            .find_map(|(block, info)| {
                self.exe
                    .block(info.id)
                    .stmts
                    .iter()
                    .position(|id| *id == statement)
                    .map(|index| (block, index))
            })
        else {
            return Ok(false);
        };
        let info = &compiled.blocks[block];
        let next_pc = info.starts.get(index + 1).copied().unwrap_or(info.end);
        let open =
            !matches!(frame.state.control.last(), Some(Position::Block(at, _)) if *at == block);
        if open {
            if index != 0 || frame.state.pc != info.starts[0].saturating_sub(1) {
                return Ok(false);
            }
            frame
                .state
                .control
                .try_reserve(1)
                .map_err(|_| HeapFault::Exhausted {
                    resource: lash_kernel_doc::heap::HostResource::StagingBytes,
                })?;
        }
        let store = self.logical.clone();
        let ledger = store.borrow();
        let Some(dense) = placement.capture(code, &invocation, &ledger, self.heap, frame.slots)?
        else {
            return Ok(false);
        };
        let safe_point = self.charged;
        let (balance, writes, clears) = {
            let resources = SourceResources {
                charged: &mut self.charged,
                counters: self.counters,
                bound: self.bound,
                slice: self.slice_end,
                depth: (self.base + u64::from(self.depth)).saturating_sub(1),
                limit: self.call_depth,
                safe_point,
            };
            let Some(publication) = caller.prepare(
                &program,
                self.exe,
                target,
                expected,
                frontier,
                &dense,
                &invocation,
                &ledger,
                self.heap,
                self.arena,
                resources,
                self.memory_bound,
            )?
            else {
                return Ok(false);
            };
            let room::publication::sort::CallbackPublication {
                accepted,
                storage,
                values,
                clears,
            } = publication;
            let writes: Option<Vec<_>> = values
                .into_iter()
                .map(|(slot, value)| placement.physical(slot).map(|at| (at as usize, value)))
                .collect();
            let Some(writes) = writes else {
                return Ok(false);
            };
            let clears: Option<Vec<_>> = clears
                .into_iter()
                .map(|slot| placement.physical(slot).map(|at| at as usize))
                .collect();
            let Some(clears) = clears else {
                return Ok(false);
            };
            drop(ledger);
            let frontier = frame
                .state
                .source
                .as_mut()
                .and_then(|source| source.frontier.as_mut())
                .ok_or_else(|| {
                    HeapFault::Malformed("accepted callback lost its source Frontier".into())
                })?;
            let balance = accepted.publish_boundary(
                frontier,
                &mut store.borrow_mut(),
                |arena, edits| storage.publish(arena, edits),
                self.recorder,
            )?;
            (balance, writes, clears)
        };
        self.logical_memory.set(balance.total()?);
        self.end_statement(frame);
        for at in clears {
            frame.slots[at] = SlotState::Empty;
        }
        for (at, value) in writes {
            frame.slots[at] = SlotState::Value(value);
        }
        if open {
            frame.state.control.push(Position::Block(block, index + 1));
        } else if let Some(Position::Block(_, next)) = frame.state.control.last_mut() {
            *next = index + 1;
        }
        frame.state.statement = None;
        frame.state.pc = next_pc;
        #[cfg(test)]
        super::super::callback_accepted();
        Ok(true)
    }
}
