//! Precise local bindings consume their actual operand in the accepted journal.
use super::*;
use crate::compile::room::publication::Occurrence;
use crate::compile::room::{ExitKind, Phase, Point};
use crate::machine::stream::Running;
use crate::machine::{
    Control, Halt, KernelMachine, SlotState, fault, frames::PreparedBinding,
    registers::PreparedTransfer,
};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{
    JournalEvent, PublicationValue, VirtualBody, VirtualEntity,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

struct BindingPreparation<'bank, 'slot> {
    bank: PreparedTransfer<'bank>,
    slot: PreparedBinding<'slot>,
    cell: Option<CellConstruction>,
}
impl<'bank, 'slot> StoragePreparation for BindingPreparation<'bank, 'slot> {
    type Prepared = (
        PreparedTransfer<'bank>,
        PreparedBinding<'slot>,
        Option<PreparedObjectConstruction>,
    );
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let (registration, storage, actual) = if let Some(cell) = self.cell {
            let Some(prepared) = cell.prepare(arena, actual)? else {
                return Ok(None);
            };
            (
                prepared.registration,
                Some(prepared.storage),
                prepared.actual,
            )
        } else {
            (arena.registration().map_err(native_fault)?, None, actual)
        };
        Ok(Some(PreparedStorage {
            registration,
            storage: (self.bank, self.slot, storage),
            actual,
        }))
    }
}

impl KernelMachine {
    pub(in crate::machine) fn publish_source_binding(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let op = running.stream.ops[pc as usize];
        let (Op::Bind { local, src }
        | Op::BindShared { local, src }
        | Op::LetBind { local, src }
        | Op::LetBindShared { local, src }) = op
        else {
            return Ok(false);
        };
        let shared = matches!(op, Op::BindShared { .. } | Op::LetBindShared { .. });
        let scoped = matches!(op, Op::LetBind { .. } | Op::LetBindShared { .. });
        // Compatibility eligibility until guest admission uses logical authority
        // on every path. This estimate never supplies a logical price.
        if shared && self.arena.borrow().sequence_bound(1) > self.capacity_headroom() {
            return Ok(false);
        }
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let (source, link) = {
            let frame = self.frame(running.task)?;
            // Loop headers and active departures need their own exact runtime
            // occurrence. This cohort has neither; it never invents one.
            if frame.cleaning.is_some()
                || frame.control.iter().any(|control| {
                    matches!(
                        control,
                        Control::For { .. } | Control::While { .. } | Control::Try { .. }
                    )
                })
            {
                return Ok(false);
            }
            if running
                .exe
                .code(frame.code)
                .slots_by_at
                .get(local.at as usize)
                != Some(&local.slot)
            {
                return Err(malformed("source binding has another frame slot").into());
            }
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
            )
        };
        let successor = if scoped {
            source.first_event(&site.child(1))
        } else {
            source.next_after_commit(site)
        };
        let Ok(unpaid) = successor else {
            return Ok(false);
        };
        let at = Point {
            site: (**site).clone(),
            phase: if scoped { Phase::Test } else { Phase::Commit },
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        self.logical_flush()?;
        let Some(bank) = self.regs.prepare_source_transfer(src)? else {
            return Ok(false);
        };
        let input = bank.owner();
        let value = bank.value();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.precise-binding");
        at.site.encode(&mut encoding);
        input.encode(&mut encoding);
        local.slot.encode(&mut encoding);
        let mut arena = self.arena.borrow_mut();
        let task = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            + usize::from(running.task != doc::TaskId::MAIN || !self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        let roots = frame
            .logical
            .as_ref()
            .ok_or_else(|| malformed("precise source has no entered invocation"))?;
        let output = source
            .binding_root(local.slot, running.task, roots.invocation().id())
            .ok_or_else(|| malformed("source binding has no canonical owner"))?;
        let Some(slot) = frame.slots.prepare_binding(local.at)? else {
            return Ok(false);
        };
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
                loops: vec![],
                action: None,
                departure: None,
            },
        };
        let next = Position {
            at: at.clone(),
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let entity = shared.then(|| doc::heap::EntityId {
            family: doc::heap::FamilyId::PRIMARY,
            ordinal: arena.next_ordinal(),
        });
        let safe_point = self.charged;
        let published = Context {
            admission: Admission::Exact,
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
            memory_bound: self.bounds.memory,
            logical_memory: &self.logical_memory,
        }
        .publish(
            roots.invocation(),
            &mut frame.source_frontier,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at.clone(),
                        LeafFacts {
                            dependencies: value.handle().into_iter().collect(),
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
            || {
                Program::new(vec![JournalEvent {
                    inputs: vec![input],
                    outputs: vec![(
                        output,
                        entity.map_or(PublicationValue::Existing(input), |entity| {
                            PublicationValue::Page(doc::heap::PageValue::Ref(
                                doc::heap::Identity::Object(entity),
                            ))
                        }),
                    )],
                    entities: entity
                        .into_iter()
                        .map(|entity| VirtualEntity {
                            entity,
                            body: VirtualBody::NativeVariable(value),
                        })
                        .collect(),
                    ..Default::default()
                }])
            },
            BindingPreparation {
                bank,
                slot,
                cell: entity.map(|entity| CellConstruction { entity, value }),
            },
            |arena, _, edits, (bank, slot, cell)| {
                debug_assert!(edits.is_empty());
                let state = if let Some(cell) = cell {
                    cell.batch.publish(arena);
                    SlotState::Cell(cell.handle)
                } else {
                    SlotState::Value(value)
                };
                bank.publish();
                slot.publish(state);
            },
        )?;
        drop(arena);
        if published && shared {
            self.grown();
            #[cfg(any(test, feature = "testing"))]
            crate::machine::islands::witness::cell_constructed();
        }
        #[cfg(any(test, feature = "testing"))]
        if published {
            #[cfg(test)]
            {
                self.storage.value_moves += 1;
            }
            crate::machine::islands::witness::precise_binding_published(value);
            if scoped {
                crate::machine::islands::witness::scoped_binding_published(shared);
            }
        }
        Ok(published)
    }
}
