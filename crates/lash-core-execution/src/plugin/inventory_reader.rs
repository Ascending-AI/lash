//! Weak observations of the canonical frozen inventory. This index owns
//! neither a plugin instance nor a second contribution roster.
use super::InstalledInventory;
use lash_vocabulary::identity::RuntimeOwner;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

type Observations = Mutex<BTreeMap<RuntimeOwner, Vec<Weak<InstalledInventory>>>>;

/// Read metadata of an existing materialization. Its weak entries cannot keep
/// a plugin, a callback, or an execution composition alive.
#[derive(Clone, Default)]
pub struct MaterializedInventoryReader(Arc<Observations>);
#[derive(Clone)]
pub struct WeakMaterializedInventoryReader(Weak<Observations>);

#[must_use = "the observation lives for this guard's lifetime"]
pub(super) struct InventoryObservation {
    reader: WeakMaterializedInventoryReader,
    owner: RuntimeOwner,
    inventory: Weak<InstalledInventory>,
}

impl Drop for InventoryObservation {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.upgrade() {
            reader.forget_weak(&self.owner, &self.inventory);
        }
    }
}

impl MaterializedInventoryReader {
    #[cfg(test)]
    pub(super) fn retained_slots_for_testing(&self) -> (usize, usize) {
        let owners = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (owners.len(), owners.values().map(Vec::len).sum())
    }

    pub fn downgrade(&self) -> WeakMaterializedInventoryReader {
        WeakMaterializedInventoryReader(Arc::downgrade(&self.0))
    }
    pub fn get(&self, owner: &RuntimeOwner) -> Option<InstalledInventory> {
        let instances = {
            let mut owners = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let instances = owners.get_mut(owner)?;
            instances.retain(|instance| instance.strong_count() > 0);
            instances.clone()
        };
        instances
            .iter()
            .rev()
            .find_map(|instance| instance.upgrade().map(|inventory| (*inventory).clone()))
    }
    pub(super) fn observe(&self, owner: &RuntimeOwner, inventory: &Arc<InstalledInventory>) {
        let mut owners = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        owners.retain(|_, instances| {
            instances.retain(|instance| instance.strong_count() > 0);
            !instances.is_empty()
        });
        owners
            .entry(owner.clone())
            .or_default()
            .push(Arc::downgrade(inventory));
    }
    pub(super) fn observe_scoped(
        &self,
        owner: &RuntimeOwner,
        inventory: &Arc<InstalledInventory>,
    ) -> InventoryObservation {
        let observation = InventoryObservation {
            reader: self.downgrade(),
            owner: owner.clone(),
            inventory: Arc::downgrade(inventory),
        };
        self.observe(owner, inventory);
        observation
    }
    pub(super) fn forget(&self, owner: &RuntimeOwner, inventory: &Arc<InstalledInventory>) {
        self.forget_weak(owner, &Arc::downgrade(inventory));
    }
    fn forget_weak(&self, owner: &RuntimeOwner, weak: &Weak<InstalledInventory>) {
        let mut owners = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(instances) = owners.get_mut(owner) {
            instances
                .retain(|instance| instance.strong_count() > 0 && !Weak::ptr_eq(instance, weak));
            if instances.is_empty() {
                owners.remove(owner);
            }
        }
    }
}
impl WeakMaterializedInventoryReader {
    pub fn upgrade(&self) -> Option<MaterializedInventoryReader> {
        self.0.upgrade().map(MaterializedInventoryReader)
    }
}
