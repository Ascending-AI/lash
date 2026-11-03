//! Fresh shared bindings use the same external boundary as other constructors.
use super::construction::storage::Construction;
use super::*;
use lash_kernel_state::arena::PreparedBatch;

pub(in crate::machine) struct CellConstruction<'a> {
    pub(in crate::machine) construction: Construction<'a>,
}
pub(in crate::machine) struct PreparedObjectConstruction {
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) handle: ArenaHandle,
}
impl BoundaryPreparation for CellConstruction<'_> {
    type Prepared = super::PreparedObjectConstruction;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, Self::Prepared)>, HeapFault> {
        Ok(self
            .construction
            .prepare(arena, entry)?
            .map(|(event, storage)| {
                let doc::Value::ObjectRef(handle) = storage.value else {
                    unreachable!("cell constructor changed its result kind");
                };
                (
                    event,
                    PreparedObjectConstruction {
                        batch: storage.batch,
                        handle,
                    },
                )
            }))
    }
    fn prepare_remaining<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
        original: &room::charges::AcceptedOriginalPrefix,
    ) -> Result<Option<(room::publication::OriginalPrepared<'a>, Self::Prepared)>, HeapFault> {
        Ok(self
            .construction
            .prepare_remaining(arena, entry, original)?
            .map(|(event, storage)| {
                let doc::Value::ObjectRef(handle) = storage.value else {
                    unreachable!("cell constructor changed its result kind");
                };
                (
                    event,
                    PreparedObjectConstruction {
                        batch: storage.batch,
                        handle,
                    },
                )
            }))
    }
}
