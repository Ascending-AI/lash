//! Source member edits install their reserved storage and bank edits together.
use super::*;
use crate::compile::room::{ExitKind, Measure, Phase, Point, publication::Occurrence};
use crate::machine::{Halt, KernelMachine, fault, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{Member, MemberEdit, MemberFault, Owner};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

pub(in crate::machine) enum PreparedMember {
    Cell(lash_kernel_state::arena::PreparedCellWrite),
    Write(lash_kernel_state::arena::PreparedListWrite),
    Remove(lash_kernel_state::arena::PreparedListRemove),
    Table(lash_kernel_state::arena::PreparedTableMember),
    Field(
        lash_kernel_state::arena::PreparedBatch,
        lash_kernel_state::arena::PreparedTableMember,
    ),
}
impl PreparedMember {
    pub(in crate::machine) fn publish(self, arena: &mut Arena, edit: MemberEdit) {
        match self {
            Self::Cell(write) => write.publish(arena, edit),
            Self::Write(write) => write.publish(arena, edit),
            Self::Remove(remove) => remove.publish(arena, edit),
            Self::Table(table) => table.publish(arena, edit),
            Self::Field(batch, table) => {
                batch.publish(arena);
                table.publish(arena, edit);
            }
        }
    }
}
pub(in crate::machine) struct SourceMember<'charge> {
    pub(in crate::machine) charges: Option<&'charge room::charges::ChargeProgram>,
    pub(in crate::machine) edit: MemberEdit,
    pub(in crate::machine) inputs: Vec<Owner>,
    pub(in crate::machine) operands: Vec<doc::Value>,
}
impl BoundaryPreparation for SourceMember<'_> {
    type Prepared = PreparedMember;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, Self::Prepared)>, HeapFault> {
        use room::publication::{ExactRoots, ExternalFacts};
        let ExternalEntry {
            event,
            frontier,
            ledger,
            heap,
            mut prefix,
            resources,
        } = entry;
        let roots = ExactRoots {
            inputs: self.inputs.clone(),
            ..Default::default()
        };
        let external = ExternalFacts {
            operands: &self.operands,
            member: Some(&self.edit),
            ..Default::default()
        };
        let bound = if let Some(charges) = self.charges {
            let Some(debt) = charges.bind(&[]) else {
                return Ok(None);
            };
            let Some(allowance) = debt.native_allowance(prefix.allowance.safe_point) else {
                return Ok(None);
            };
            event.bind_external(
                frontier,
                ledger,
                arena,
                heap,
                &mut prefix.facts,
                roots,
                prefix.next,
                allowance,
                resources,
                external,
                &debt,
            )
        } else {
            event.bind_paid_external(
                frontier,
                ledger,
                arena,
                heap,
                &mut prefix.facts,
                roots,
                prefix.next,
                prefix.allowance,
                resources,
                external,
            )
        };
        let Ok(bound) = bound else {
            return Ok(None);
        };
        let edit = self.edit;
        let (registration, storage) = if let Member::Field(name) = &edit.member {
            let Some((installation, table)) =
                arena.prepare_field_member(edit.handle, Arc::clone(name), edit.next)?
            else {
                return Ok(None);
            };
            let (registration, batch) = installation.into_parts();
            (registration, PreparedMember::Field(batch, table))
        } else {
            let storage = match edit.member {
                Member::Variable => match edit.next {
                    Some(value) => arena
                        .prepare_cell_write(edit.handle, value)?
                        .map(PreparedMember::Cell),
                    None => return Ok(None),
                },
                Member::List(index) => match edit.next {
                    Some(value) => arena
                        .prepare_list_write(edit.handle, index, value)?
                        .map(PreparedMember::Write),
                    None => arena
                        .prepare_list_remove(edit.handle, index)?
                        .map(PreparedMember::Remove),
                },
                Member::Table(key) => arena
                    .prepare_table_member(edit.handle, key, edit.next)?
                    .map(PreparedMember::Table),
                Member::Field(_) => unreachable!(),
            };
            let Some(storage) = storage else {
                return Ok(None);
            };
            (arena.registration().map_err(native_fault)?, storage)
        };
        let transaction = ledger
            .prepare_source_members_boundary(registration, heap, &[edit], &self.inputs, &[])
            .map_err(|error| match error {
                MemberFault::Heap(error) => error,
                MemberFault::Native(error) => native_fault(error),
            })?;
        match bound.finish_external(frontier, ledger, transaction) {
            Ok(prepared) => Ok(Some((prepared, storage))),
            Err(PublicationError::Miss(_)) => Ok(None),
            Err(PublicationError::Heap(error)) => Err(error),
        }
    }
}
impl KernelMachine {
    pub(in crate::machine) fn publish_source_member(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let (target, removing, field) = match running.stream.ops[pc as usize] {
            Op::SetIndex { target, .. } => (target, false, None),
            Op::RemoveIndex { target } => (target, true, None),
            Op::SetField { target, field, .. } => (
                target,
                false,
                Some(Arc::<str>::from(
                    running.stream.fields[field.ix()].name.as_str(),
                )),
            ),
            Op::RemoveField { target, field } => (
                target,
                true,
                Some(Arc::<str>::from(
                    running.stream.fields[field.ix()].name.as_str(),
                )),
            ),
            _ => return Ok(false),
        };
        let target_value = self.regs[target.ix()];
        let key = field.is_none().then(|| self.regs[target.ix() + 1]);
        let (handle, member) = if let Some(name) = &field {
            let doc::Value::Record(handle) = target_value else {
                return Ok(false);
            };
            if self.arena.borrow().entries(handle).is_err() {
                return Ok(false);
            }
            (handle, Member::Field(Arc::clone(name)))
        } else {
            let Some(key) = key else {
                return Err(malformed("an indexed member operation has no evaluated key").into());
            };
            match target_value {
                doc::Value::List(handle) => {
                    let Some(index) = crate::compile::position(&key) else {
                        return Ok(false);
                    };
                    let Ok(len) = self.arena.borrow().len(handle) else {
                        return Ok(false);
                    };
                    if index > len || (removing && index == len) {
                        return Ok(false);
                    }
                    (handle, Member::List(index))
                }
                doc::Value::Map(handle) | doc::Value::Set(handle) | doc::Value::Record(handle) => {
                    if matches!(target_value, doc::Value::Record(_))
                        && !matches!(key, doc::Value::Text(_) | doc::Value::LongText(_))
                    {
                        return Ok(false);
                    }
                    let arena = self.arena.borrow();
                    if arena.key(&key).is_err() || arena.entries(handle).is_err() {
                        return Ok(false);
                    }
                    (handle, Member::Table(key))
                }
                _ => return Ok(false),
            }
        };
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let (source, link, loops) = {
            let frame = self.frame(running.task)?;
            let Some(loops) = source_loops(frame, running.exe)? else {
                return Ok(false);
            };
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                loops,
            )
        };
        let Ok(unpaid) = source.next_after_commit(site) else {
            return Ok(false);
        };
        let at = Point {
            site: (**site).clone(),
            phase: Phase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        self.publish_logical_meter()?;
        let Some(bank) = self.regs.prepare_source_inputs(running.stream, pc)? else {
            return Ok(false);
        };
        let inputs = bank.owners();
        let values = bank.values();
        let next = match (field.is_some(), values.as_slice()) {
            (true, [actual]) if removing && actual.same(&target_value) => None,
            (true, [actual, value]) if !removing && actual.same(&target_value) => Some(*value),
            (false, [actual, actual_key])
                if removing
                    && actual.same(&target_value)
                    && key.is_some_and(|key| actual_key.same(&key)) =>
            {
                None
            }
            (false, [actual, actual_key, value])
                if !removing
                    && actual.same(&target_value)
                    && key.is_some_and(|key| actual_key.same(&key)) =>
            {
                if matches!(target_value, doc::Value::Set(_)) {
                    let doc::Value::Bool(present) = value else {
                        return Ok(false);
                    };
                    present.then_some(doc::Value::Absent)
                } else {
                    Some(*value)
                }
            }
            _ => return Err(malformed("member store has another source input shape").into()),
        };
        let edit = MemberEdit {
            handle,
            member,
            next,
        };
        let store = self.logical.clone();
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("list store owners failed verification"))?;
        let invocation = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?
            .frames
            .last_mut()?
            .ok_or_else(|| fault("no frame"))?
            .invocation;
        let mut arena = self.arena.borrow_mut();
        let ledger = store.borrow();
        let measured = match ledger.prepare_source_members_boundary(
            arena.registration().map_err(native_fault)?,
            &self.heap,
            std::slice::from_ref(&edit),
            &[],
            &[],
        ) {
            Ok(member) => Ok((
                i128::from(member.balance()?.total()?) - i128::from(ledger.balance()?.total()?),
                member.release_cost(&inputs, self.heap.pages())?,
            )),
            Err(MemberFault::Native(doc::NativeError::Retry(doc::Retry::Cold {
                object,
                chunk,
            }))) => Err((object, chunk)),
            Err(MemberFault::Heap(error)) => return Err(error.into()),
            Err(MemberFault::Native(error)) => return Err(native_fault(error).into()),
        };
        let (delta, released) = match measured {
            Ok(measured) => measured,
            Err((object, chunk)) => {
                // A member the store reaches cold has done nothing a run
                // sees: the machine decodes the chunk and asks again.
                drop((ledger, arena, bank));
                self.warm(object, chunk)?;
                return self.publish_source_member(running, pc);
            }
        };
        let event = interval
            .events()
            .next()
            .ok_or_else(|| malformed("list store event disappeared"))?;
        let mut measures = std::collections::BTreeMap::new();
        for selector in &event.inputs {
            let owner = slots
                .root(selector, running.task, invocation)
                .ok_or_else(|| malformed("list store operand has no canonical owner"))?;
            if !inputs.contains(&owner) {
                return Err(malformed("list store has another source operand").into());
            }
            let holding = ledger
                .holding(owner, self.heap.pages())?
                .ok_or_else(|| malformed("list store operand is absent"))?;
            measures.insert(
                Measure::Local(selector.clone()),
                i128::from(holding.bytes()),
            );
        }
        measures.insert(Measure::MemberDelta(at.site.clone()), delta);
        measures.insert(Measure::Release(event.inputs.clone()), i128::from(released));
        drop(ledger);
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-list-store");
        at.site.encode(&mut encoding);
        inputs.encode(&mut encoding);
        let task = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            - usize::from(running.task == doc::TaskId::MAIN && self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        let roots = frame
            .logical
            .as_ref()
            .ok_or_else(|| malformed("list store has no entered invocation"))?;
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
                loops,
                action: None,
            },
        };
        let next = Position {
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
        let published = Context {
            admission: Admission::Exact(MemoryLimit::of(
                self.bounds.memory_meter,
                &mut self.bounds.memory,
                &mut self.limiters,
            )),
            source: &source,
            link,
            kernel,
            logical: &self.logical,
            heap: &self.heap,
            arena: &mut arena,
            session: &mut self.session,
            resources: SourceResources {
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
            },
            logical_memory: &self.logical_memory,
        }
        .publish_external(
            roots.invocation(),
            &mut frame.source_frontier,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at,
                        LeafFacts {
                            dependencies: std::iter::once(handle)
                                .chain(values.iter().filter_map(doc::Value::handle))
                                .collect(),
                            values: measures,
                            ..Default::default()
                        },
                    )]
                    .into(),
                ),
                start,
                next,
                allowance: Allowance {
                    occurrences: vec![],
                    before: 0,
                    units: 0,
                    close: None,
                    strict: false,
                    depth: 0,
                    safe_point,
                },
            },
            SourceMember {
                charges: None,
                edit: edit.clone(),
                inputs,
                operands: values,
            },
            |arena, _, edits, write| {
                let [edit] = edits.as_slice() else {
                    unreachable!("a checked list store changed member arity")
                };
                write.publish(arena, edit.clone());
                bank.publish();
            },
        )?;
        drop(arena);

        Ok(published)
    }
}
