//! A precise closure publishes its prepared body and captured cells atomically.
use super::*;
use crate::compile::{
    CodeId,
    room::{ExitKind, Measure, Phase, Point, publication::Occurrence},
};
use crate::machine::{
    Control, Halt, KernelMachine, SlotState, fault, registers::PreparedConstruction,
    stream::Running,
};
use doc::heap::EntityId;
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{
    Holding, JournalEvent, PublicationValue as PV, VirtualBody, VirtualEntity,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

pub(in crate::machine) struct ClosureConstruction {
    pub(in crate::machine) entity: EntityId,
    pub(in crate::machine) identity: Arc<doc::arena::ClosureIdentity>,
    pub(in crate::machine) captures: Vec<ArenaHandle>,
    pub(in crate::machine) entities: Vec<EntityId>,
    pub(in crate::machine) code: doc::arena::LocalCode,
}
impl StoragePreparation for ClosureConstruction {
    type Prepared = PreparedObjectConstruction;
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        use lash_kernel_state::kernel_internals::logical::PublicationValue as PV;
        let [event] = actual.events() else {
            return Ok(None);
        };
        let [entity] = event.entities.as_slice() else {
            return Ok(None);
        };
        if !matches!(&entity.body, VirtualBody::Closure(captures) if captures == &self.entities)
            || entity.entity != self.entity
            || !matches!(event.outputs.as_slice(), [(_, PV::Page(doc::heap::PageValue::Closure(id)))] if *id == self.entity)
            || !event.inputs.is_empty()
            || event.member.is_some()
            || event.frame.is_some()
            || !event.scratch.is_empty()
            || self.captures.len() != self.entities.len()
            || !self
                .captures
                .iter()
                .zip(&self.entities)
                .all(|(handle, entity)| {
                    arena
                        .facts(*handle)
                        .is_ok_and(|facts| facts.entity == Some(*entity))
                })
        {
            return Ok(None);
        }
        let (handle, mut installation) = arena
            .prepare_closure(self.identity, self.captures, self.code)
            .map_err(native_fault)?;
        installation
            .bind_entity(handle, self.entity)
            .map_err(native_fault)?;
        let (registration, batch) = installation.into_parts();
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedObjectConstruction { batch, handle },
            actual,
        }))
    }
}

struct SourceClosure<'bank> {
    recipe: ClosureConstruction,
    bank: PreparedConstruction<'bank>,
    holding: Holding,
}
impl<'bank> StoragePreparation for SourceClosure<'bank> {
    type Prepared = (
        PreparedObjectConstruction,
        PreparedConstruction<'bank>,
        Holding,
    );
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let Some(prepared) = self.recipe.prepare(arena, actual)? else {
            return Ok(None);
        };
        Ok(Some(PreparedStorage {
            registration: prepared.registration,
            storage: (prepared.storage, self.bank, self.holding),
            actual: prepared.actual,
        }))
    }
}
impl KernelMachine {
    pub(in crate::machine) fn publish_source_closure(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let Op::Closure { code, .. } = running.stream.ops[pc as usize] else {
            return Ok(false);
        };
        let target = running.exe.code(CodeId(code.0));
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        if target.created.as_ref() != Some(site)
            || self.arena.borrow().sequence_bound(target.captures.len()) > self.capacity_headroom()
        {
            return Ok(false);
        }
        let lash_kernel_value::sites::Unit::Library(function) = target.site.unit else {
            return Ok(false);
        };
        let (source, link, captures) = {
            let frame = self.frame(running.task)?;
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
            let mut captures = Vec::new();
            captures
                .try_reserve(target.captures.len())
                .map_err(|_| HeapFault::Exhausted {
                    resource: doc::heap::HostResource::StagingBytes,
                })?;
            for capture in &target.captures {
                let SlotState::Cell(cell) = frame.slot(capture.outer_at) else {
                    return Ok(false);
                };
                captures.push(*cell);
            }
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                captures,
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
        self.logical_flush()?;
        let mut entities = Vec::new();
        entities
            .try_reserve(captures.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        for handle in &captures {
            let Some(entity) = self
                .arena
                .borrow()
                .facts(*handle)
                .ok()
                .and_then(|facts| facts.entity)
            else {
                return Ok(false);
            };
            entities.push(entity);
        }
        let Some(bank) = self.regs.prepare_source_construction(running.stream, pc)? else {
            return Ok(false);
        };
        let output = bank.owner();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let entity = EntityId {
            family: doc::heap::FamilyId::PRIMARY,
            ordinal: self.arena.borrow().next_ordinal(),
        };
        let holding =
            Holding::from_page(self.heap.pages(), &doc::heap::PageValue::Closure(entity))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.precise-closure");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        entities.encode(&mut encoding);
        let identity = Arc::new(doc::arena::ClosureIdentity {
            capture_names: target.capture_names.clone(),
            callable: lash_kernel_value::names::CallableId { link, function },
            site: doc::linked_site::LinkedSite {
                link,
                site: (**site).clone(),
            },
        });
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
        let safe_point = self.charged;
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: captures.clone(),
                    values: [(Measure::CaptureEdges(at.site), captures.len() as i128)].into(),
                    ..Default::default()
                },
            )]
            .into(),
        );
        let event = JournalEvent {
            outputs: vec![(output, PV::Page(doc::heap::PageValue::Closure(entity)))],
            entities: vec![VirtualEntity {
                entity,
                body: VirtualBody::Closure(entities.clone()),
            }],
            ..Default::default()
        };
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
                facts,
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
            || Program::new(vec![event.clone()]),
            SourceClosure {
                recipe: ClosureConstruction {
                    entity,
                    identity,
                    captures,
                    entities,
                    code: doc::arena::LocalCode {
                        executable: running.exe.local(),
                        code: code.0,
                    },
                },
                bank,
                holding,
            },
            |arena, _, edits, (storage, bank, holding)| {
                debug_assert!(edits.is_empty());
                storage.batch.publish(arena);
                bank.publish(doc::Value::Closure(storage.handle), holding);
            },
        )?;
        drop(arena);
        if published {
            self.grown();
            #[cfg(any(test, feature = "testing"))]
            crate::machine::islands::witness::closure_constructed();
        }
        Ok(published)
    }
}
