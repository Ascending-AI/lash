//! Freeze the reached graph once and translate every retained source role.
use super::super::*;
use super::Running;
use crate::machine::SlotState;
use lash_kernel_state::kernel_internals::logical::{Owner, StateLease};

impl<'m> Running<'m> {
    pub(in crate::machine::region_isa) fn reconstruct(
        mut self,
    ) -> Result<reconstruct::Finished<StateLease<'m>>, RegionError> {
        if self.faulted {
            return Err(invalid(Refused::Facts));
        }
        let stop = self.stop.ok_or_else(|| invalid(Refused::Exit))?;
        let program = &self.code.program;
        let map = program
            .recovery(self.prefix)
            .ok_or_else(|| invalid(Refused::Exit))?;
        let read = |reg| {
            self.registers.read(
                0,
                self.reached.invocation,
                super::bind::physical(program, reg)?,
            )
        };
        let frame = reconstruct::FrameImage {
            program: program.code(),
            invocation: self.reached.invocation,
            pc: self.pc.index() as u32,
            continuation: map.continuation.clone(),
            roles: map
                .roles
                .iter()
                .map(|(slot, reg)| Ok((*slot, read(*reg)?)))
                .collect::<Result<_, RegionError>>()?,
            operands: map
                .operands
                .iter()
                .map(|reg| read(*reg))
                .collect::<Result<_, _>>()?,
            pending: map.pending.map(read).transpose()?,
        };
        let graph = self.view.materialize_graph().map_err(RegionError::Heap)?;
        let translated = frame.materialize(&graph)?;
        let mut position = self
            .canonical
            .source_frontier
            .as_ref()
            .ok_or_else(|| invalid(Refused::Exit))?
            .position()
            .clone();
        position.at = translated.continuation.clone();
        position.unpaid = Some(
            program
                .step(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?
                .at
                .clone(),
        );
        self.canonical.source_frontier = Some(
            self.canonical
                .source_frontier
                .as_ref()
                .ok_or_else(|| invalid(Refused::Exit))?
                .region_successor(position)
                .map_err(RegionError::Heap)?,
        );
        let invocation = self
            .canonical
            .logical
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?
            .invocation();
        let Owner::Frame { task, .. } = invocation.frame() else {
            unreachable!()
        };
        let states = self
            .canonical
            .slot_states()
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let Owner::Value { slot, .. } = program
                    .source()
                    .binding_root(
                        // Physical binding indexes were authenticated at acquisition.
                        self.code.bindings[index],
                        task,
                        self.reached.invocation,
                    )
                    .ok_or_else(|| invalid(Refused::Source))?
                else {
                    return Err(invalid(Refused::Source));
                };
                Ok(
                    match translated.roles.iter().find(|(actual, _)| *actual == slot) {
                        None => SlotState::Empty,
                        Some((_, Value::ObjectRef(handle))) => SlotState::Cell(*handle),
                        Some((_, value)) => SlotState::Value(*value),
                    },
                )
            })
            .collect::<Result<Vec<_>, RegionError>>()?;
        for (index, state) in states.into_iter().enumerate() {
            self.canonical.restore_slot(index as u32, state);
        }
        let live = live::Live {
            frames: Box::new([frame]),
            payload: None,
            returning: None,
        };
        let registers = crate::machine::registers::Registers::region_image(
            task,
            self.reached.invocation,
            &translated.roles,
        )
        .map_err(RegionError::Heap)?;
        let (issued_invocations, invocations) = self.invocations.finish()?;
        let image = Reconstruction {
            issued_invocations,
            prefix: self.reached,
            pending: translated.pending,
            frames: Box::new([translated]),
            attempted: self.attempted,
            counters: self.counters,
            stop,
            payload: None,
            returning: None,
            source_counts: self.source.freeze(),
            entering: None,
            canonical: Some(publication::Canonical {
                frames: vec![self.canonical],
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
