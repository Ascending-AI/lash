//! Final region graph access through the canonical detached proof.
use super::*;

impl PreparedGraphProof {
    pub(crate) fn region_cell(&self, handle: ArenaHandle) -> Option<EntityId> {
        let entity = self.region_entity(handle)?;
        self.0
            .entities
            .iter()
            .any(|held| {
                held.entity == entity && matches!(held.body, VirtualBody::NativeVariable(_))
            })
            .then_some(entity)
    }
    pub(crate) fn region_entity(&self, handle: ArenaHandle) -> Option<EntityId> {
        self.0
            .values
            .iter()
            .find(|value| value.value.handle() == Some(handle))
            .and_then(|value| value.native.entity)
    }
    pub(crate) fn contains_handle(&self, handle: ArenaHandle) -> bool {
        self.0.installed.iter().any(|entry| entry.handle == handle)
    }
    pub(crate) fn holding_for(&self, value: &Value) -> Option<Holding> {
        self.0
            .values
            .iter()
            .find(|fact| fact.value.same(value))
            .map(|fact| fact.holding.clone())
    }
}

impl PreparedBatch {
    pub(crate) fn region_len(&self) -> usize {
        self.entries.len()
    }
}

impl<'a> PreparedInstallation<'a> {
    pub(in crate::arena) fn from_region_parts(
        registration: Registration<'a>,
        batch: PreparedBatch,
    ) -> Self {
        Self {
            holdings: HashMap::default(),
            observed: Vec::new(),
            registration,
            batch,
        }
    }
}
