//! Detached canonical frames use the one frozen graph identity mapping.
use super::super::*;
use super::Running;
use crate::machine::{
    Frame, SlotState,
    frames::{Continuation, Slots},
};
use lash_kernel_state::kernel_internals::logical::{Invocation, LeaseGraphExit, Owner};

impl Running<'_> {
    pub(super) fn canonical_frame(
        &mut self,
        depth: usize,
        code: &super::Code,
        image: &reconstruct::FrameImage,
        graph: &LeaseGraphExit,
    ) -> Result<Box<Frame>, RegionError> {
        let roots = self
            .canonical
            .logical
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?;
        let bank = roots.bank();
        let Owner::Frame { task, .. } = roots.invocation().frame() else {
            unreachable!()
        };
        let invocation: &Invocation = if depth == 0 {
            roots.invocation()
        } else {
            &self
                .parked
                .get(depth - 1)
                .ok_or_else(|| invalid(Refused::Facts))?
                .callee
        };
        if invocation.id() != image.invocation {
            return Err(invalid(Refused::Facts));
        }
        let pc = if depth == self.parked.len() {
            self.pc
        } else {
            self.parked[depth].pc
        };
        let mut frame = if depth == 0 {
            self.canonical
                .region_image(Rc::clone(&bank.ledger), task, &code.exe)
                .map_err(RegionError::Heap)?
        } else if let Some(saved) = self
            .canonical_callees
            .get(depth - 1)
            .filter(|frame| frame.invocation == image.invocation && frame.code == image.program)
        {
            saved
                .region_image(Rc::clone(&bank.ledger), task, &code.exe)
                .map_err(RegionError::Heap)?
        } else {
            let compiled = code.exe.code(code.program.code());
            let mut slots = Slots::default();
            slots
                .prepare_fresh(code.bindings.len())
                .map_err(RegionError::Heap)?;
            slots.fresh(code.bindings.len());
            let parked = &self.parked[depth - 1];
            let call = parked
                .code
                .calls
                .get(parked.site as usize)
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let plan = call
                .plan
                .target(&parked.code.exe, parked.target)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let library = plan
                .library()
                .filter(|_| !compiled.charged)
                .map(|lib| {
                    let mut args: Vec<_> = call
                        .arguments
                        .iter()
                        .map(|reg| {
                            self.registers
                                .read(depth - 1, parked.reached.invocation, *reg)?
                                .materialize(graph)
                                .map_err(RegionError::Heap)
                        })
                        .collect::<Result<_, RegionError>>()?;
                    if let Some(guard) = self.hook_guard(depth - 1, &parked.code, call)? {
                        args.extend(guard.values());
                    }
                    args.resize(code.exe.lib(lib).arity, Value::Absent);
                    Ok::<_, RegionError>(crate::machine::LibraryCall {
                        lib,
                        args: args.into_boxed_slice(),
                    })
                })
                .transpose()?;
            Box::new(Frame {
                source_action: None,
                texts: Vec::new(),
                source_frontier: None,
                invocation: image.invocation,
                logical: Some(
                    crate::machine::logical_held::FrameRoots::prepared(
                        Rc::clone(&bank.ledger),
                        task,
                        invocation.clone(),
                        Arc::clone(code.program.source()),
                        Arc::clone(&compiled.slots_by_at),
                    )
                    .map_err(RegionError::Heap)?,
                ),
                convention: None,
                public_result: None,
                link: self.parked[depth - 1].link,
                code: image.program,
                charging: compiled.charging,
                paced: compiled.paced,
                slots,
                statement: self.canonical.statement.clone(),
                pending: Continuation::default(),
                control: Default::default(),
                awaiting: None,
                nested: None,
                library,
                cleaning: None,
                held: Vec::new(),
                owned: false,
            })
        };
        let suspensions = self.suspensions(depth, &image.roles)?;
        frame.control = self.controls_for(code, pc, depth, &image.roles, &suspensions)?;
        // A suspended exit's value is its cleanup's, recorded with it.
        let dynamic: Vec<_> = self
            .suspended_in(depth)
            .filter_map(|entry| entry.value.map(|(slot, _)| slot))
            .collect();
        for (index, binding) in code.bindings.iter().enumerate() {
            let Owner::Value { slot, .. } = code
                .program
                .source()
                .binding_root(*binding, task, image.invocation)
                .ok_or_else(|| invalid(Refused::Source))?
            else {
                return Err(invalid(Refused::Source));
            };
            let state = match image.roles.iter().find(|(actual, _)| *actual == slot) {
                None => SlotState::Empty,
                Some((_, Value::ObjectRef(handle))) => SlotState::Cell(*handle),
                Some((_, value)) => SlotState::Value(*value),
            };
            frame.restore_slot(index as u32, state);
        }
        // One canonical continuation carries every resumable semantic phase.
        // Binding and capture values already have named frame fields.
        let (prefix, reached) = if depth == self.parked.len() {
            (self.prefix, self.reached)
        } else {
            (self.parked[depth].prefix, self.parked[depth].reached)
        };
        let departure = (depth == self.parked.len())
            .then_some(self.departing.as_ref())
            .flatten();
        let incoming = (depth == self.parked.len())
            .then_some(self.returning.as_ref())
            .flatten();
        let unpaid = code
            .program
            .step(pc)
            .ok_or_else(|| invalid(Refused::Path))?
            .at
            .clone();
        frame.statement = lash_kernel_state::paged::SiteRef::new(
            &code.program.source().site,
            self.view.heap().pages(),
        )
        .map_err(RegionError::Heap)?;
        frame.pending.clear();
        frame.pending.source = Some(crate::machine::frames::source::SourceContinuation {
            at: incoming
                .map(|incoming| {
                    code.calls
                        .get(incoming.site as usize)
                        .and_then(Option::as_ref)
                        .map(|call| call.plan.resume().clone())
                        .ok_or_else(|| invalid(Refused::Facts))
                })
                .transpose()?
                .unwrap_or_else(|| image.continuation.clone()),
            unpaid,
            progress: if depth < self.parked.len() {
                3
            } else if incoming.is_some() {
                4
            } else {
                code.program
                    .source_progress(prefix)
                    .ok_or_else(|| invalid(Refused::Exit))?
            },
            occurrence: reached.occurrence,
            roles: image
                .roles
                .iter()
                .filter(|(slot, _)| {
                    !matches!(
                        slot,
                        lash_kernel_state::kernel_internals::logical::Slot::Binding(_)
                            | lash_kernel_state::kernel_internals::logical::Slot::Capture(_)
                    ) && !dynamic.contains(slot)
                })
                .copied()
                .collect(),
            pending: incoming
                .map(|incoming| graph.value(incoming.value).map_err(RegionError::Heap))
                .transpose()?
                .or(image.pending),
            departure: departure
                .map(|departure| departure.origin.clone())
                .or_else(|| {
                    (depth == self.parked.len())
                        .then(|| self.looping.clone())
                        .flatten()
                }),
            suspended: self
                .suspended_in(depth)
                .map(|entry| {
                    (
                        entry.origin.clone(),
                        entry
                            .departure
                            .as_ref()
                            .map_or(0, |departure| departure.completion),
                    )
                })
                .collect(),
            ordinal: reached.ordinal,
            debt: lash_kernel_state::paged::ChargeDebtRecord {
                cursor: reached.debt,
                paid: self.counters.progress,
                completion: departure.map_or_else(
                    || incoming.map_or(0, |incoming| incoming.completion),
                    |departure| departure.completion,
                ),
                completion_paid: incoming.is_some_and(|incoming| incoming.paid),
            },
            root: code.program.source().site.clone(),
            refusal: if depth == self.parked.len() {
                self.refusal.as_ref().map(super::super::refusal_tag)
            } else {
                None
            },
        });
        frame.awaiting = None;
        frame.nested = None;
        let base = self
            .canonical
            .source_frontier
            .as_ref()
            .ok_or_else(|| invalid(Refused::Exit))?;
        let mut position = base.position().clone();
        position.at = image.continuation.clone();
        position.occurrence.invocation = image.invocation;
        position.occurrence.action = None;
        position.occurrence.loops =
            crate::machine::source_publication::source_loops(&frame, &code.exe)
                .map_err(RegionError::Heap)?
                .ok_or_else(|| invalid(Refused::Exit))?;
        position.unpaid = Some(
            code.program
                .step(pc)
                .ok_or_else(|| invalid(Refused::Path))?
                .at
                .clone(),
        );
        frame.source_frontier = Some(
            if depth == 0 {
                base.region_successor(position)
            } else {
                let document = code
                    .exe
                    .instances
                    .get(&frame.link)
                    .ok_or_else(|| invalid(Refused::Source))?;
                let kernel = lash_kernel_doc::KernelVersion::of(document.kernel())
                    .ok_or_else(|| invalid(Refused::Source))?;
                base.region_callee(
                    code.program.source(),
                    invocation,
                    frame.link,
                    kernel,
                    position,
                )
            }
            .map_err(RegionError::Heap)?,
        );
        Ok(frame)
    }
}
