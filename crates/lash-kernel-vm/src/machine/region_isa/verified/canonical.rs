//! Detached canonical frames use the one frozen graph identity mapping.
use super::super::*;
use super::Running;
use crate::machine::{
    Frame, SlotState,
    frames::{Continuation, Pending, Slots},
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
        let mut statement = code
            .program
            .step(pc)
            .ok_or_else(|| invalid(Refused::Path))?
            .at
            .site
            .clone();
        while statement.path.len() > code.program.source().site.path.len()
            && code.exe.stmt_at(code.program.code(), &statement).is_none()
        {
            statement.path.pop();
        }
        let mut frame = if depth == 0 {
            self.canonical
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
                .targets()
                .iter()
                .find(|plan| plan.code() == parked.target)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let library = plan
                .library()
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
                link: self.canonical.link,
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
        frame.control = self.controls_for(code, pc, depth)?;
        frame.statement =
            lash_kernel_state::paged::SiteRef::new(&statement, self.view.heap().pages())
                .map_err(RegionError::Heap)?;
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
        if depth < self.parked.len() {
            let parked = &self.parked[depth];
            let call = parked
                .code
                .calls
                .get(parked.site as usize)
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let stmt = code
                .exe
                .stmt_at(image.program, &statement)
                .ok_or_else(|| invalid(Refused::Path))?;
            let (stream, _, _) = code.exe.statement(stmt);
            if let Some(act) = stream
                .nested_at
                .get(&(stmt, call.plan.point().site.clone()))
            {
                let entry = &stream.nested[act.ix()];
                frame.stand_at(
                    *act,
                    Pending {
                        site: Arc::clone(&entry.suffix),
                        phase: lash_kernel_state::paged::PhaseRecord::Called,
                        occurrence: parked.reached.occurrence,
                        roles: call.saved.iter().map(|saved| {
                            let value = self.registers.read(depth, image.invocation, saved.register)?;
                            let holding = match value {
                                operands::Operand::Dynamic(value) => self.view.recovery_holding(graph, value).map_err(RegionError::Heap)?,
                                operands::Operand::Immediate(value) => lash_kernel_state::kernel_internals::logical::Holding::scalar(self.view.arena().logical_local(&value).map_err(RegionError::Heap)?),
                            };
                            Ok(crate::machine::frames::Role { site: Arc::clone(&saved.suffix), value: value.materialize(graph).map_err(RegionError::Heap)?, holding })
                        }).collect::<Result<_, RegionError>>()?,
                    },
                );
            }
            frame.awaiting = Some(stmt);
            if let Some(crate::machine::Control::Block { next, .. }) = frame.control.last_mut() {
                *next += 1;
            }
        }
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
