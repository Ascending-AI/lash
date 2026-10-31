//! The resource owner and canonical journal commit at the same source frontier.
use super::*;
use crate::admission::SourceResources;
use doc::heap::EntityId;
use lash_kernel_state::{
    arena::PreparedBatch,
    kernel_internals::logical::{MemberEdit, Program, VirtualBody},
};
use std::sync::Arc;

use crate::machine::source_publication::{Admission, Context};
pub(super) use crate::machine::source_publication::{
    Prefix, PreparedStorage, Reserve, StoragePreparation, native_fault,
};

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
        prefix: Prefix,
        make: impl Fn() -> Program,
        prepare: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, Vec<MemberEdit>, S::Prepared),
    ) -> Result<bool, HeapFault> {
        let source = &self.exe.code(callee.plan.code).source_room;
        let document = self
            .exe
            .instances
            .get(&callee.link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let safe_point = self.charged;
        Context {
            admission: Admission::Envelope,
            source,
            link: callee.link,
            kernel,
            logical: &self.logical,
            heap: self.heap,
            arena: self.arena,
            session: self.session,
            resources: SourceResources {
                charged: &mut self.charged,
                counters: self.counters,
                bound: self.bound,
                slice: self.slice_end,
                depth: self.base + u64::from(self.depth),
                limit: self.call_depth,
                safe_point,
            },
            memory_bound: self.memory_bound,
            logical_memory: self.logical_memory,
        }
        .publish(
            &activation.invocation,
            &mut activation.frontier,
            prefix,
            make,
            prepare,
            semantic,
        )
    }
}
