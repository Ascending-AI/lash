//! Shared bindings install one detached cell under their canonical journal.
use super::*;
use doc::{Value, heap::EntityId};
use lash_kernel_state::{arena::PreparedBatch, kernel_internals::logical::VirtualBody};

pub(in crate::machine) struct CellConstruction {
    pub(in crate::machine) entity: EntityId,
    pub(in crate::machine) value: Value,
}
pub(in crate::machine) struct PreparedObjectConstruction {
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) handle: ArenaHandle,
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
