//! Bind the actual resident entry before taking the exclusive graph view.
use super::super::*;
use super::{Code, Running};
use crate::compile::room::Owner as Selector;
use crate::machine::{
    SlotState,
    controls::{self, Roles},
};
use lash_kernel_state::kernel_internals::logical::{Owner, StateLease};
use std::collections::BTreeMap;

impl<'m> RegionLease<'m> {
    pub(in crate::machine::region_isa) fn acquire_verified(
        machine: &'m mut KernelMachine,
        code: Arc<Code>,
        limits: Limits,
    ) -> Result<Self, RegionError> {
        let task_id = machine.current.ok_or_else(|| invalid(Refused::Facts))?;
        let exe = Arc::clone(&machine.exe);
        let task = machine
            .tasks
            .get_mut(task_id.0 as usize)
            .map_err(RegionError::Heap)?
            .ok_or_else(|| invalid(Refused::Facts))?;
        let frame = task
            .frames
            .last_mut()
            .map_err(RegionError::Heap)?
            .ok_or_else(|| invalid(Refused::Facts))?;
        frame
            .reconstruct_region_frontier(&exe, &machine.heap, &machine.logical, task_id)
            .map_err(RegionError::Heap)?;
        if frame.code != code.program.code() {
            return Err(invalid(Refused::Source));
        }
        let invocation = frame
            .logical
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?
            .invocation()
            .clone();
        let document = exe
            .instances
            .get(&frame.link)
            .ok_or_else(|| invalid(Refused::Source))?;
        let kernel = lash_kernel_doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| invalid(Refused::Source))?;
        let frontier = frame
            .source_frontier
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?;
        if !frontier.region_binding_matches(code.program.source(), frame.link, kernel, &invocation)
            || frontier.position().occurrence.task != task_id
        {
            return Err(invalid(Refused::Source));
        }
        let unpaid = frontier
            .position()
            .unpaid
            .as_ref()
            .ok_or_else(|| invalid(Refused::Exit))?;
        let (pc, prefix) = match machine
            .regs
            .region_position(&code.program, invocation.id())
            .map_err(RegionError::Heap)?
        {
            Some((pc, prefix)) => {
                if code.program.step(pc).is_none_or(|step| &step.at != unpaid)
                    || code
                        .program
                        .recovery(prefix)
                        .is_none_or(|map| map.continuation != frontier.position().at)
                {
                    return Err(invalid(Refused::Exit));
                }
                (pc, prefix)
            }
            None => {
                let mut pc = code
                    .program
                    .at(unpaid)
                    .ok_or_else(|| invalid(Refused::Path))?;
                let mut index = 0;
                while let Some(info) = code.program.loop_info(index) {
                    if info.while_loop && unpaid == &info.entry {
                        let active = frame.control.iter().any(|control| matches!(control, crate::machine::Control::While { stmt, .. } if *stmt == info.statement));
                        pc = code
                            .program
                            .stage(crate::compile::region_isa::LoopStage {
                                info: index,
                                phase: if active {
                                    crate::compile::region_isa::LoopPhase::Test
                                } else {
                                    crate::compile::region_isa::LoopPhase::Enter
                                },
                            })
                            .ok_or_else(|| invalid(Refused::Path))?;
                        break;
                    }
                    index += 1;
                }
                let prefix = code
                    .program
                    .step(pc)
                    .ok_or_else(|| invalid(Refused::Path))?
                    .before;
                (pc, prefix)
            }
        };
        let mut actual = BTreeMap::new();
        let source = code.program.source();
        for (index, state) in frame.slot_states().iter().enumerate() {
            let Some(Owner::Value { slot, .. }) = source.binding_root(
                exe.code(frame.code).slots_by_at[index],
                task_id,
                invocation.id(),
            ) else {
                continue;
            };
            let value = match state {
                SlotState::Empty => continue,
                SlotState::Value(value) => *value,
                SlotState::Cell(handle) => Value::ObjectRef(*handle),
            };
            actual.insert(slot, value);
        }
        for (owner, value) in machine.regs.region_values() {
            if let Owner::Value {
                task,
                invocation: id,
                slot,
            } = owner
                && task == task_id
                && id == invocation.id()
            {
                actual.insert(slot, value);
            }
        }
        let slots = source.owner_slots().map_err(invalid)?;
        if let Some(pending) = frame.pending.peek() {
            let statement = frame
                .statement
                .resolve(machine.heap.pages())
                .map_err(RegionError::Heap)?;
            for role in &pending.roles {
                let mut site = (*statement).clone();
                site.path.extend_from_slice(&role.site);
                if let Some(Owner::Value { slot, .. }) =
                    slots.continuation(&site, task_id, invocation.id())
                {
                    actual.insert(slot, role.value);
                }
            }
        }
        for control in frame.control.iter() {
            if let Some(Roles::For {
                stmt,
                cursor,
                header,
            }) = controls::roles(control)
            {
                let at = &exe.stmt(stmt).site;
                for (selector, value) in [
                    (Selector::LoopCursor(at.clone()), cursor),
                    (Selector::LoopHeader(at.clone()), header),
                ] {
                    if let (Some(Owner::Value { slot, .. }), Some(value)) =
                        (slots.root(&selector, task_id, invocation.id()), value)
                    {
                        actual.insert(slot, value);
                    }
                }
            }
        }
        let map = code
            .program
            .recovery(prefix)
            .ok_or_else(|| invalid(Refused::Exit))?;
        let values = map
            .roles
            .iter()
            .map(|(slot, register)| {
                Ok((
                    *register,
                    *actual.get(slot).ok_or_else(|| invalid(Refused::Facts))?,
                ))
            })
            .collect::<Result<Vec<_>, RegionError>>()?;
        let mut dependencies = Vec::new();
        frame.each_value(&mut |value| dependencies.push(*value));
        dependencies.extend(machine.regs.region_values().map(|(_, value)| value));
        let image = frame
            .region_image(Rc::clone(&machine.logical), task_id, &exe)
            .map_err(RegionError::Heap)?;
        let inputs = machine
            .logical
            .borrow()
            .frame_owners(task_id, invocation.id(), machine.heap.pages())
            .map_err(RegionError::Heap)?
            .into_boxed_slice();
        let next = machine
            .logical
            .borrow()
            .prospective_invocation(task_id)
            .id();
        if machine
            .logical
            .borrow()
            .balance()
            .and_then(|balance| balance.total())
            .map_err(RegionError::Heap)?
            != limits.entry_balance
        {
            return Err(invalid(Refused::Facts));
        }
        let indexes: Vec<_> = (0..exe.occurrence_count()).collect();
        let counters = task
            .counters
            .lease_counters(&indexes)
            .map_err(RegionError::Heap)?;
        let source = source_counters::SourceCounters::bind(counters.entry())?;
        let mut lease = StateLease::acquire_shared(&mut machine.heap, &machine.arena)
            .map_err(RegionError::Heap)?;
        for value in dependencies {
            lease.import(&value).map_err(RegionError::Heap)?;
        }
        let range = lease
            .reserve_invocations(&invocation, u64::MAX - next)
            .map_err(RegionError::Heap)?;
        let invocations = invocations::Invocations::bind(range)?;
        let code_id = code.program.code();
        let execution = Running::acquire(
            code,
            pc,
            prefix,
            invocation.id(),
            values,
            source,
            limits,
            invocations,
            lease,
            image,
        )?;
        Ok(Self {
            authority: Authority::Machine(publication::Authority {
                frames: &mut task.frames,
                counters,
                registers: &mut machine.regs,
                charged: &mut machine.charged,
                logical_memory: &machine.logical_memory,
                task: task_id,
                exe,
                entry_code: code_id,
                entry_invocation: invocation.id(),
                inputs,
                limits,
            }),
            execution: Executing::Machine(execution),
        })
    }
}
