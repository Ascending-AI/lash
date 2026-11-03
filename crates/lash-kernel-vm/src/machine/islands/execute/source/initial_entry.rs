//! An island's first frame uses the same detached parameters as nested calls.
use super::*;
use crate::admission::{Allowance, SourceResources};

use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    Phase, Point,
    publication::{FrameEntry, Occurrence, Position as SourcePosition},
};

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_initial_frame(
        &mut self,
        target: Callee<'e>,
        args: &[Value],
        inputs: &[Owner],
    ) -> Result<Option<Frame<'e>>, HeapFault> {
        if !inputs.is_empty() && inputs.len() != args.len() {
            return Ok(None);
        }
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let invocation = ledger.prospective_invocation(self.task);
        let arguments = args
            .iter()
            .enumerate()
            .map(|(index, value)| (inputs.get(index).copied(), *value))
            .collect::<Vec<_>>();
        self.scratch.slab.reserve_call(target)?;
        let window = self.scratch.windows.pop().unwrap_or_default();
        let start = target.captured as usize;
        let captured = self
            .scratch
            .captured
            .get(start..start + target.plan.captures.len())
            .ok_or_else(|| malformed("packet entry has no complete capture window"))?;
        let dependencies = args
            .iter()
            .filter_map(Value::handle)
            .chain(captured.iter().map(|(_, cell)| *cell))
            .collect::<Vec<_>>();
        let frame_entry = FrameEntry::compile(self.exe, target.plan.code)
            .map_err(|_| malformed("initial frame descriptor failed verification"))?;
        let parameters::Parameters {
            installation,
            bindings,
            outputs,
            entities,
            shared,
        } = (parameters::EntryParameters {
            target,
            exe: self.exe,
            task: self.task,
            invocation: invocation.id(),
        })
        .prepare(self.arena, &arguments, captured)?;
        let source = &self.exe.code(target.plan.code).source_room;
        let unpaid = if source.body.is_empty() {
            Point {
                site: source.site.clone(),
                phase: Phase::Fallthrough,
            }
        } else {
            source
                .first_event(&source.site.child(0))
                .map_err(|_| malformed("initial packet frame has no first source event"))?
        };
        let document = self
            .exe
            .instances
            .get(&target.link)
            .ok_or_else(|| malformed("initial packet frame has no admitted defining link"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("initial packet frame kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.initial-packet-frame");
        source.site.encode(&mut encoding);
        target.link.encode(&mut encoding);
        let frontier = source
            .frontier(
                &ledger,
                self.heap,
                &invocation,
                target.link,
                kernel,
                SourcePosition {
                    at: Point {
                        site: source.site.clone(),
                        phase: Phase::Enter,
                    },
                    unpaid: Some(unpaid),
                    recovery: logical_digest(&encoding.into_bytes()),
                    occurrence: Occurrence {
                        task: self.task,
                        invocation: invocation.id(),
                        loops: vec![],
                        action: None,
                    },
                },
            )
            .map_err(|_| malformed("initial packet frame frontier failed authentication"))?;
        let Some((prepared, batch)) = frame_entry.prepare(
            source,
            target.link,
            &frontier,
            &invocation,
            &ledger,
            installation,
            self.heap,
            &arguments,
            None,
            &bindings,
            None,
            inputs,
            &outputs,
            &entities,
            &dependencies,
        )?
        else {
            return Ok(None);
        };
        let after = prepared.balance()?.total()?;
        let safe_point = self.charged;
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: None,
            strict: false,
            depth: 0,
            safe_point,
        };
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: self.base,
            limit: self.call_depth,
            safe_point,
        };
        let facts = resources
            .facts(&allowance)
            .ok_or_else(|| malformed("initial packet frame resource facts are unavailable"))?;
        let Ok(accepted) = prepared.admit(self.memory_bound) else {
            return Ok(None);
        };
        let Some(accepted) = resources.admit_interval(&facts, &allowance, accepted)? else {
            return Ok(None);
        };
        drop(ledger);
        let slab = &mut self.scratch.slab;
        let mut active = None;
        accepted.commit(|accepted| {
            accepted.publish(&mut store.borrow_mut(), |arena, edits| {
                assert!(edits.is_empty());
                batch.publish(arena);
                let ranges = slab.open(
                    target.plan.slots,
                    target.compiled.registers,
                    target.compiled.pending_slots(),
                );
                let mut frame = Frame::new(target, ranges, window);
                slab.view(&mut frame).slots.copy_from_slice(&bindings);
                frame.source = Some(Activation {
                    invocation,
                    frontier: Some(frontier),
                    original: None,
                    loop_input: None,
                    cleaning: None,
                });
                active = Some(frame);
            })
        })?;
        self.logical_memory.set(after);
        self.register_bytes += target.compiled.register_bytes();
        #[cfg(test)]
        self.scratch.slab.entered_call();
        #[cfg(not(any(test, feature = "testing")))]
        let _ = shared;
        #[cfg(any(test, feature = "testing"))]
        crate::machine::islands::witness::packet_frame_entered(shared);
        Ok(active)
    }
}
