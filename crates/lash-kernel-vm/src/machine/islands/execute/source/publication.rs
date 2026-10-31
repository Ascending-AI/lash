//! The resource owner and canonical journal commit at the same source frontier.
use super::*;
use crate::admission::{Allowance, SourceResources};
use doc::heap::EntityId;
use lash_kernel_state::{
    arena::{PreparedBatch, Registration},
    kernel_internals::logical::{MemberEdit, Program, VirtualBody},
};

pub(super) struct PreparedStorage<'a, S> {
    pub(super) registration: Registration<'a>,
    pub(super) storage: S,
    pub(super) actual: Program,
}

pub(super) trait StoragePreparation {
    type Prepared;
    const MATERIALIZES: bool = false;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault>;
    fn allowance(_: &Self::Prepared, _: &mut Allowance) {}
    fn facts(_: &Self::Prepared, _: &mut LeafExecution) -> Result<(), HeapFault> {
        Ok(())
    }
}
pub(super) struct Reserve<F>(pub(super) F);
impl<F, S> StoragePreparation for Reserve<F>
where
    F: FnOnce(&mut Arena) -> Result<Option<S>, HeapFault>,
{
    type Prepared = S;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, S>>, HeapFault> {
        let Some(storage) = (self.0)(arena)? else {
            return Ok(None);
        };
        let registration = arena.registration().map_err(native_fault)?;
        Ok(Some(PreparedStorage {
            registration,
            storage,
            actual,
        }))
    }
}
pub(super) struct ListConstruction {
    pub(super) entity: EntityId,
    pub(super) values: Vec<Value>,
}
pub(super) struct PreparedListConstruction {
    pub(super) batch: PreparedBatch,
    pub(super) value: Value,
}
impl StoragePreparation for ListConstruction {
    type Prepared = PreparedListConstruction;
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let [event] = actual.events() else {
            return Ok(None);
        };
        let [entity] = event.entities.as_slice() else {
            return Ok(None);
        };
        let VirtualBody::NativeList(values) = &entity.body else {
            return Ok(None);
        };
        if !matches!(event.outputs.as_slice(), [(_, lash_kernel_state::kernel_internals::logical::PublicationValue::Page(doc::heap::PageValue::List(id)))] if *id == self.entity)
            || event.member.is_some()
            || event.frame.is_some()
            || !event.scratch.is_empty()
            || entity.entity != self.entity
            || values.len() != self.values.len()
            || !values.iter().zip(&self.values).all(|(a, b)| a.same(b))
        {
            return Ok(None);
        }
        // No handle reservation survives a declined logical admission.
        let mut nursery = doc::Nursery::unaccounted(arena.objects());
        let value = nursery.allocate(doc::NewObject::List(self.values));
        let mut installation = arena
            .prepare_install(nursery.into_new())
            .map_err(native_fault)?;
        installation
            .bind_entity(
                value
                    .handle()
                    .ok_or_else(|| malformed("list construction lost its handle"))?,
                self.entity,
            )
            .map_err(native_fault)?;
        let (registration, batch) = installation.into_parts();
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedListConstruction { batch, value },
            actual,
        }))
    }
}
pub(super) struct CellConstruction {
    pub(super) entity: EntityId,
    pub(super) value: Value,
}
pub(super) struct PreparedObjectConstruction {
    pub(super) batch: PreparedBatch,
    pub(super) handle: ArenaHandle,
}
impl StoragePreparation for CellConstruction {
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
        if !matches!(&entity.body, VirtualBody::NativeVariable(value) if value.same(&self.value))
            || entity.entity != self.entity
            || !matches!(event.outputs.as_slice(), [(_, PV::Page(doc::heap::PageValue::Ref(doc::heap::Identity::Object(id))))] if *id == self.entity)
            || event.member.is_some()
            || event.frame.is_some()
            || !event.scratch.is_empty()
        {
            return Ok(None);
        }
        let (cell, mut installation) = arena.prepare_cell(self.value).map_err(native_fault)?;
        installation
            .bind_entity(cell, self.entity)
            .map_err(native_fault)?;
        let (registration, batch) = installation.into_parts();
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedObjectConstruction {
                batch,
                handle: cell,
            },
            actual,
        }))
    }
}
pub(super) struct ClosureConstruction {
    pub(super) entity: EntityId,
    pub(super) identity: Arc<doc::arena::ClosureIdentity>,
    pub(super) captures: Vec<ArenaHandle>,
    pub(super) entities: Vec<EntityId>,
    pub(super) code: doc::arena::LocalCode,
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
pub(super) fn native_fault(error: doc::NativeError) -> HeapFault {
    match error {
        doc::NativeError::Host(error) => error,
        error => malformed(&error.to_string()),
    }
}
use room::publication::{Position, PublicationError};
use std::sync::Arc;

pub(super) struct Prefix {
    pub(super) interval: Arc<room::CompiledInterval>,
    pub(super) facts: LeafExecution,
    pub(super) start: Position,
    pub(super) next: Position,
    pub(super) allowance: Allowance,
}
impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn publish_source(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        prefix: Prefix,
        make: impl Fn() -> Program,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<bool, HeapFault> {
        self.publish_source_prepared(
            callee,
            activation,
            prefix,
            make,
            Reserve(|_: &mut Arena| Ok(Some(()))),
            |arena, _, edits, ()| semantic(arena, edits),
        )
    }

    pub(super) fn publish_source_prepared<S: StoragePreparation>(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        mut prefix: Prefix,
        make: impl Fn() -> Program,
        prepare: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, Vec<MemberEdit>, S::Prepared),
    ) -> Result<bool, HeapFault> {
        let source = &self.exe.code(callee.plan.code).source_room;
        let store = self.logical.ledger();
        if let Some(frontier) = &mut activation.frontier
            && frontier.position() != &prefix.start
        {
            frontier.handoff(&mut store.borrow_mut(), prefix.start.clone())?;
        }
        let ledger = store.borrow();
        let document = self
            .exe
            .instances
            .get(&callee.link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        if activation.frontier.is_none() {
            activation.frontier = Some(
                source
                    .frontier(
                        &ledger,
                        self.heap,
                        &activation.invocation,
                        callee.link,
                        kernel,
                        prefix.start,
                    )
                    .map_err(|_| malformed("source frontier failed authentication"))?,
            );
        }
        let frontier = activation
            .frontier
            .as_mut()
            .ok_or_else(|| malformed("source frontier disappeared"))?;
        // Detached bodies and bookkeeping are complete before acceptance.
        // Their registration returns every reserved slot on any later miss.
        let Some(PreparedStorage {
            mut registration,
            storage,
            actual,
        }) = prepare.prepare(self.arena, make())?
        else {
            return Ok(false);
        };
        S::allowance(&storage, &mut prefix.allowance);
        S::facts(&storage, &mut prefix.facts)?;
        let safe_point = self.charged;
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: self.base + u64::from(self.depth),
            limit: self.call_depth,
            safe_point,
        };
        let Some(resource_facts) = resources.facts(&prefix.allowance) else {
            return Ok(false);
        };
        let Ok(evaluated) = frontier.evaluate_interval(
            prefix.interval.clone(),
            &mut prefix.facts,
            &ledger,
            registration.arena(),
            self.heap,
            actual.clone(),
            prefix.next.clone(),
            prefix.allowance.clone(),
            resource_facts,
        ) else {
            return Ok(false);
        };
        let Some(accepted) = evaluated.admit_interval(
            frontier,
            &ledger,
            registration.arena(),
            resources,
            &prefix.allowance,
            self.memory_bound,
        )?
        else {
            return Ok(false);
        };
        let prepared = match accepted.prepare_publication(
            frontier,
            &ledger,
            registration,
            self.heap,
            actual,
            prefix.interval.exit(),
            &prefix.next,
        ) {
            Ok(prepared) => prepared,
            Err(PublicationError::Miss(_)) => return Ok(false),
            Err(PublicationError::Heap(error)) => return Err(error),
        };
        drop(ledger);
        let committed = if S::MATERIALIZES {
            prepared.publish_materialized(
                frontier,
                &mut store.borrow_mut(),
                |arena, edits, _entities| semantic(arena, self.session, edits, storage),
            )
        } else {
            prepared.publish(frontier, &mut store.borrow_mut(), |arena, edits| {
                semantic(arena, self.session, edits, storage)
            })
        }
        .map_err(|error| match error {
            PublicationError::Heap(error) => error,
            PublicationError::Miss(reason) => malformed(reason),
        })?;
        debug_assert!(committed.materializations.entities().is_empty());
        self.logical_memory.set(committed.balance.total()?);
        Ok(true)
    }
}
