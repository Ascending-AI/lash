//! Canonical frame entry is an accepted prefix before any body suffix.
mod caller;
use super::*;
use crate::compile::room::{
    Phase, Point,
    publication::{FrameCaller, FrameEntry, Occurrence},
};
use crate::compile::{CodeId, Executable, NestedCallee, NestedKind, StmtId};
use crate::machine::{Eval, Frame, KernelMachine, SlotState, exec::ArgumentOwners, fault};
pub(in crate::machine) use caller::CallerArguments;
use lash_kernel_doc::{
    TaskId, Value,
    heap::{EntityId, FamilyId, Identity, PageValue},
};
use lash_kernel_state::kernel_internals::logical::{
    Owner, PublicationValue as PV, Slot, VirtualBody, VirtualEntity,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

pub(in crate::machine) struct EntryCall {
    pub(in crate::machine) code: CodeId,
    pub(in crate::machine) args: usize,
    pub(in crate::machine) owners: ArgumentOwners,
    pub(in crate::machine) given: usize,
    pub(in crate::machine) awaited_by: Option<StmtId>,
    pub(in crate::machine) capture: Option<lash_kernel_exec::ActIx>,
}
fn buffer<T>(count: usize) -> Result<Vec<T>, HeapFault> {
    let mut values = Vec::new();
    values
        .try_reserve(count)
        .map_err(|_| HeapFault::Exhausted {
            resource: doc::heap::HostResource::StagingBytes,
        })?;
    Ok(values)
}
impl KernelMachine {
    /// A miss returns the detached frame. It has not advanced a caller, moved
    /// an argument, granted an invocation, or installed a shared parameter.
    pub(in crate::machine) fn publish_prepared_frame_entry(
        &mut self,
        task_id: TaskId,
        exe: &Executable,
        call: EntryCall,
        mut frame: Box<Frame>,
    ) -> Eval<Option<Box<Frame>>> {
        let code = exe.code(call.code);
        let source = Arc::clone(&code.source_room);
        source
            .verify()
            .map_err(|_| malformed("frame entry source failed verification"))?;
        if frame.logical.is_some() || frame.invocation != 0 {
            return Err(malformed("prepared frame still owns an invocation").into());
        }
        if source.params.len() != code.params.len() || call.given > code.params.len() {
            return Err(malformed("frame parameters differ from their source").into());
        }
        for (index, param) in code.params.iter().enumerate() {
            let slot = code
                .slots
                .get(param.slot as usize)
                .ok_or_else(|| malformed("entry parameter slot is absent"))?;
            if slot.name != source.params[index]
                || slot.declared != source.site
                || slot.shared != param.shared
                || param.shared
                    != source
                        .shared
                        .get(&(source.site.clone(), source.params[index].clone()))
                        .copied()
                        .unwrap_or(false)
                || code.slots_by_at.get(param.at as usize) != Some(&param.slot)
            {
                return Err(malformed(
                    "entry parameter mapping differs from its source declaration",
                )
                .into());
            }
        }
        let mut encoding = TokenWriter::new();
        encoding.seq(6);
        encoding.text("lash.source.frame-entry");
        frame.link.encode(&mut encoding);
        source.site.encode(&mut encoding);
        encoding.unsigned(call.given as u64);
        let rooted = self.task(task_id)?.frames.is_empty();

        let Some((caller, ordinal)) =
            self.prepare_frame_caller(task_id, exe, &CallerArguments::from(&call), &mut encoding)?
        else {
            return Ok(Some(frame));
        };
        let callee = caller.callee;
        let document = exe
            .instances
            .get(&frame.link)
            .ok_or_else(|| malformed("frame entry has no admitted defining link"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("frame entry kernel is unsupported"))?;
        let at = Point {
            site: source.site.clone(),
            phase: Phase::Enter,
        };
        let unpaid = if source.body.is_empty() {
            Point {
                site: source.site.clone(),
                phase: Phase::Fallthrough,
            }
        } else {
            source
                .first_event(&source.site.child(0))
                .map_err(|_| malformed("frame body has no first source event"))?
        };
        let frame_entry = FrameEntry::compile(exe, call.code)
            .map_err(|_| malformed("frame entry descriptor failed verification"))?;
        let frame_caller = if !matches!(call.owners, ArgumentOwners::Control)
            && let Some(statement) = call.awaited_by
        {
            let outer = self.frame(task_id)?;
            if let Some(act) = call.capture.or(outer.nested) {
                let entry = &exe.statement(statement).0.nested[act.ix()];
                Some(FrameCaller {
                    source: Arc::clone(&exe.code(outer.code).source_room),
                    invocation: outer
                        .logical
                        .as_ref()
                        .ok_or_else(|| malformed("entry caller has no invocation"))?
                        .invocation()
                        .clone(),
                    entry,
                    callable: callee.map(|reg| self.regs[reg.ix()]),
                    save: call.capture.is_some(),
                })
            } else {
                None
            }
        } else {
            None
        };
        let Some(bank) = self.regs.prepare_frame_entry(
            call.args,
            call.given,
            callee,
            matches!(call.owners, ArgumentOwners::Control),
            caller.roles(),
        )?
        else {
            return Ok(Some(frame));
        };
        caller.authenticate(&bank)?;
        let capture = caller.pending(&bank, ordinal)?;

        let mut arguments = buffer(call.given)?;
        arguments.extend(bank.arguments());
        let mut inputs = buffer(call.given + usize::from(callee.is_some()) + caller.roles().len())?;
        inputs.extend(bank.owners());
        let mut shared_values = buffer(code.params.len())?;
        for (index, param) in code.params.iter().enumerate() {
            let value = arguments
                .get(index)
                .map_or(Value::Absent, |(_, value)| *value);
            if param.shared {
                shared_values.push(value);
            } else {
                frame.restore_slot(param.at, SlotState::Value(value));
            }
        }

        let store = self.logical.clone();
        let ledger = store.borrow();
        let invocation = ledger.prospective_invocation(task_id);
        let mut arena = self.arena.borrow_mut();
        let first = arena.next_provisional().ordinal;
        let (handles, mut installation) =
            arena.prepare_cells(&shared_values).map_err(native_fault)?;
        let mut entities = buffer(handles.len())?;
        let mut shared_ids = buffer(handles.len())?;
        let mut cells = handles.iter().zip(&shared_values).enumerate();
        for param in &code.params {
            if param.shared {
                let (index, (handle, value)) = cells
                    .next()
                    .ok_or_else(|| malformed("a shared parameter has no prepared cell"))?;
                let entity = EntityId {
                    family: FamilyId::PROVISIONAL,
                    ordinal: first
                        .checked_add(index as u64)
                        .ok_or_else(|| malformed("parameter identity overflow"))?,
                };
                installation
                    .bind_entity(*handle, entity)
                    .map_err(native_fault)?;
                entities.push(VirtualEntity {
                    entity,
                    body: VirtualBody::NativeVariable(*value),
                });
                shared_ids.push((*handle, entity));
                frame.restore_slot(param.at, SlotState::Cell(*handle));
            }
        }
        let mut outputs = buffer(code.slots_by_at.len() + caller.roles().len())?;
        for (index, state) in frame.slot_states().iter().enumerate() {
            if matches!(state, SlotState::Empty) {
                continue;
            }
            let owner = source
                .binding_root(code.slots_by_at[index], task_id, invocation.id())
                .ok_or_else(|| malformed("entry binding has no canonical source owner"))?;
            let Owner::Value { slot, .. } = owner else {
                return Err(malformed("entry binding names another owner kind").into());
            };
            let value = match state {
                SlotState::Empty => unreachable!(),
                SlotState::Cell(handle) if matches!(slot, Slot::Capture(_)) => PV::Capture(*handle),
                SlotState::Cell(handle) => {
                    match shared_ids.iter().find(|(cell, _)| cell == handle) {
                        Some((_, entity)) => PV::Page(PageValue::Ref(Identity::Object(*entity))),
                        None => PV::Native(Value::ObjectRef(*handle)),
                    }
                }
                SlotState::Value(value) => {
                    let parameter = code
                        .params
                        .iter()
                        .position(|param| param.at as usize == index)
                        .ok_or_else(|| malformed("entry value is not a source parameter"))?;
                    match arguments.get(parameter).and_then(|(owner, _)| *owner) {
                        Some(owner) => PV::Existing(owner),
                        None => PV::Native(*value),
                    }
                }
            };
            outputs.push((owner, value));
        }
        for (_, _, _, input, output) in bank.saved() {
            if ledger.holding(output, self.heap.pages())?.is_some() {
                return Err(malformed("entry capture would overwrite a continuation owner").into());
            }
            outputs.push((output, PV::Existing(input)));
        }
        let roots = crate::machine::logical_held::FrameRoots::prepared(
            self.logical.clone(),
            task_id,
            invocation.clone(),
            Arc::clone(&source),
            Arc::clone(&code.slots_by_at),
        )?;
        let position = Position {
            at,
            unpaid: Some(unpaid),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: task_id,
                invocation: invocation.id(),
                loops: vec![],
                action: None,
            },
        };
        let frontier = source
            .frontier(
                &ledger,
                &self.heap,
                &invocation,
                frame.link,
                kernel,
                position,
            )
            .map_err(|_| malformed("entry frontier failed authentication"))?;
        let dependencies: Vec<_> = bank.dependencies().collect();
        let Some((prepared, batch)) = frame_entry.prepare(
            &source,
            frame.link,
            &frontier,
            &invocation,
            &ledger,
            installation,
            &self.heap,
            &arguments,
            bank.public_arguments(),
            frame.slot_states(),
            frame_caller.as_ref(),
            &inputs,
            &outputs,
            &entities,
            &dependencies,
        )?
        else {
            return Ok(Some(frame));
        };
        let mut memory = MemoryLimit::of(
            self.bounds.memory_meter,
            &mut self.bounds.memory,
            &mut self.limiters,
        );
        let prepared = if memory.exceeded(prepared.peak()) {
            prepared.collect(&ledger, self.heap.pages())?
        } else {
            prepared
        };
        // A root entry has no step to refuse at: it enters, and its first
        // step's admission asks the limiter and ends the run at that step,
        // as a charge bound does.
        let limit = if rooted {
            u64::MAX
        } else {
            memory.admit(prepared.peak())?
        };
        let after = prepared.balance()?.total()?;
        let task = self
            .tasks
            .get_mut(task_id.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth =
            (task.frames.len() + usize::from(task_id != TaskId::MAIN || !self.session_cell)) as u64;
        let push = task.frames.prepare_push(call.awaited_by, capture)?;
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
            counters: &mut task.counters,
            bound: if self.charging {
                self.bounds.charge_limit
            } else {
                u64::MAX
            },
            slice: self.slice_end,
            depth,
            limit: u64::from(self.bounds.call_depth),
            safe_point,
        };
        let facts = resources
            .facts(&allowance)
            .ok_or_else(|| malformed("entry resource facts are absent"))?;
        let accepted = prepared
            .admit(limit)
            .map_err(|_| malformed("entry peak passed its granted memory limit"))?;
        let Some(accepted) = resources.admit_interval(&facts, &allowance, accepted)? else {
            return Ok(Some(frame));
        };
        drop(ledger);
        accepted.commit(|accepted| {
            accepted.publish(&mut store.borrow_mut(), |arena, edits| {
                assert!(edits.is_empty());
                batch.publish(arena);
                bank.publish();
                frame.invocation = invocation.id();
                frame.logical = Some(roots);
                frame.source_frontier = Some(frontier);
                push.publish(frame);
            })
        })?;
        self.logical_memory.set(after);
        drop(arena);
        #[cfg(test)]
        {
            self.storage.value_moves += call.given;
        }

        self.observe(crate::observe::Event::Push {
            task: task_id,
            code: call.code,
        });
        Ok(None)
    }
}
