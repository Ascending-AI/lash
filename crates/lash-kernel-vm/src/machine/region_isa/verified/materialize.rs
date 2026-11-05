//! Freeze the reached graph once and translate every retained source role.
use super::super::*;
use super::Running;
use lash_kernel_state::kernel_internals::logical::StateLease;

impl<'m> Running<'m> {
    pub(in crate::machine::region_isa) fn reconstruct(
        mut self,
    ) -> Result<reconstruct::Finished<StateLease<'m>>, RegionError> {
        // These new execution phases still need their canonical incoming/debt
        // reconstruction. Refuse before freezing or publishing a partial image.
        if self.faulted || self.departing.is_some() || self.returning.is_some() || self.complete {
            return Err(invalid(Refused::Facts));
        }
        let stop = self.stop.ok_or_else(|| invalid(Refused::Exit))?;
        if (stop == RegionStop::Refusal) != self.refusal.is_some() {
            return Err(invalid(Refused::Facts));
        }
        let refusal = self.refusal.as_ref().map(|bound| {
            let limit = match bound {
                crate::Bound::Charge => self.limits.charge,
                crate::Bound::Memory => self.limits.memory,
                crate::Bound::CallDepth => u64::from(self.limits.depth),
                _ => unreachable!("only execution limits can refuse a region phase"),
            };
            Box::new(crate::BoundExceeded {
                bound: bound.clone(),
                limit,
                function: None,
                site: None,
            })
        });
        let mut frames = Vec::new();
        for depth in 0..=self.parked.len() {
            let (code, pc, prefix, reached) = if depth == self.parked.len() {
                (&self.code, self.pc, self.prefix, self.reached)
            } else {
                let parked = &self.parked[depth];
                (&parked.code, parked.pc, parked.prefix, parked.reached)
            };
            let program = &code.program;
            let map = program
                .recovery(prefix)
                .ok_or_else(|| invalid(Refused::Exit))?;
            let read = |reg| {
                self.registers.read(
                    depth,
                    reached.invocation,
                    super::bind::physical(program, reg)?,
                )
            };
            let parked_callable = if depth < self.parked.len() {
                code.calls[self.parked[depth].site as usize]
                    .as_ref()
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .callable
            } else {
                None
            };
            let moved = if depth < self.parked.len() {
                code.calls
                    .get(self.parked[depth].site as usize)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .arguments
                    .as_ref()
            } else {
                &[]
            };
            let moved = |reg: crate::compile::region_isa::Reg<
                lash_kernel_state::kernel_internals::logical::LeaseValue,
            >| {
                moved
                    .iter()
                    .chain(parked_callable.iter())
                    .any(|physical| match physical {
                        code::Register::Value(index)
                        | code::Register::Int(index)
                        | code::Register::Float(index)
                        | code::Register::Bool(index) => *index == reg.index(),
                    })
            };
            frames.push(reconstruct::FrameImage {
                program: program.code(),
                invocation: reached.invocation,
                pc: pc.index() as u32,
                continuation: if depth < self.parked.len() {
                    code.calls[self.parked[depth].site as usize]
                        .as_ref()
                        .ok_or_else(|| invalid(Refused::Facts))?
                        .plan
                        .point()
                        .clone()
                } else {
                    map.continuation.clone()
                },
                roles: map
                    .roles
                    .iter()
                    .filter(|(_, reg)| !moved(*reg))
                    .map(|(slot, reg)| {
                        let slot = if depth < self.parked.len() {
                            code.calls[self.parked[depth].site as usize]
                                .as_ref()
                                .ok_or_else(|| invalid(Refused::Facts))?
                                .saved
                                .iter()
                                .find(|saved| saved.source == *slot)
                                .map_or(*slot, |saved| saved.parked)
                        } else {
                            *slot
                        };
                        Ok((slot, read(*reg)?))
                    })
                    .collect::<Result<_, RegionError>>()?,
                operands: map
                    .operands
                    .iter()
                    .filter(|reg| !moved(**reg))
                    .map(|reg| read(*reg))
                    .collect::<Result<_, _>>()?,
                pending: map.pending.map(read).transpose()?,
            });
        }
        let graph = self.view.materialize_graph().map_err(RegionError::Heap)?;
        let translated = frames
            .iter()
            .map(|frame| frame.materialize(&graph))
            .collect::<Result<Vec<_>, RegionError>>()?;
        let canonical = translated
            .iter()
            .enumerate()
            .map(|(depth, frame)| {
                let code = Arc::clone(if depth == self.parked.len() {
                    &self.code
                } else {
                    &self.parked[depth].code
                });
                self.canonical_frame(depth, &code, frame, &graph)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let current = translated.last().ok_or_else(|| invalid(Refused::Exit))?;
        let lash_kernel_state::kernel_internals::logical::Owner::Frame { task, .. } = self
            .canonical
            .logical
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?
            .invocation()
            .frame()
        else {
            unreachable!()
        };
        let registers = crate::machine::registers::Registers::region_frames_image(
            task,
            &translated,
            crate::machine::registers::RegionCursor::new(
                Arc::clone(&self.code.program),
                self.reached.invocation,
                self.pc,
                self.prefix,
                refusal,
            ),
        )
        .map_err(RegionError::Heap)?;
        let pending = current.pending;
        let live = live::Live {
            frames: frames.into_boxed_slice(),
            payload: None,
            returning: None,
        };
        let (issued_invocations, invocations) = self.invocations.finish()?;
        let image = Reconstruction {
            issued_invocations,
            prefix: self.reached,
            pending,
            frames: translated.into_boxed_slice(),
            attempted: self.attempted,
            counters: self.counters,
            stop,
            payload: None,
            returning: None,
            source_counts: self.source.freeze(),
            entering: None,
            canonical: Some(publication::Canonical {
                frames: canonical,
                registers: Some(registers),
                crossing: self.crossing,
            }),
        };
        Ok(reconstruct::Finished {
            image,
            live,
            view: self.view,
            graph,
            invocations,
        })
    }
}
