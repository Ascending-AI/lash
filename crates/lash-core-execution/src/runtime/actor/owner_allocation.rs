//! The protocol reservation bridge to the durable node's owner ledger.
use lash_durable::owner_memory::{MemoryClass, MemoryReservation, OwnerMemory};
use lash_protocol_types::allocation::{
    AllocationBudget, AllocationError, AllocationHold, AllocationLedger, AllocationReservation,
    ReserveError,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

#[derive(Clone)]
pub struct OwnerAllocation(Arc<State>);
struct State {
    memory: OwnerMemory,
    actor: String,
    pending: Mutex<Pending>,
}
struct Pending {
    class: MemoryClass,
    holds: BTreeMap<usize, PendingCharge>,
}
struct PendingCharge {
    charge: Weak<Charge>,
    _metadata: Arc<Mutex<Option<MemoryReservation>>>,
}
struct Charge {
    payload: Mutex<Option<MemoryReservation>>,
    metadata: Arc<Mutex<Option<MemoryReservation>>>,
    registry: Weak<State>,
}
impl Drop for Charge {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut pending = registry.pending.lock().unwrap_or_else(|p| p.into_inner());
            let removed = pending.holds.remove(&(std::ptr::from_ref(self) as usize));
            if pending.holds.is_empty() {
                // BTreeMap can keep its empty root allocation after remove.
                // Free it while this charge still owns the metadata token.
                pending.holds = BTreeMap::new();
            }
            drop(pending);
            drop(removed);
        }
    }
}

struct ChargeHandle(Arc<Charge>);
impl AllocationReservation for ChargeHandle {
    fn retain(&self) {
        for cell in [&self.0.payload, &*self.0.metadata] {
            let mut token = cell.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(hold) = token.take() {
                *token = Some(hold.into_retained());
            }
        }
    }
}
impl OwnerAllocation {
    /// Admission for new work; acknowledgement promotes its live sources.
    pub fn budget(memory: OwnerMemory, actor: String) -> (AllocationBudget, Arc<Self>) {
        Self::with_class(memory, actor, MemoryClass::Dirty)
    }

    /// A coherent cold read reconstructs already committed sources. They
    /// consume the retained cap from their first allocation, rather than
    /// the host's independent allowance for new uncommitted work. The
    /// successful read bracket calls `acknowledged` before publishing the
    /// projection, so every later preparation uses the dirty cap.
    pub fn hydration_budget(memory: OwnerMemory, actor: String) -> (AllocationBudget, Arc<Self>) {
        Self::with_class(memory, actor, MemoryClass::Retained)
    }

    fn with_class(
        memory: OwnerMemory,
        actor: String,
        class: MemoryClass,
    ) -> (AllocationBudget, Arc<Self>) {
        let ledger = Self(Arc::new(State {
            memory,
            actor,
            pending: Mutex::new(Pending {
                class,
                holds: BTreeMap::new(),
            }),
        }));
        (AllocationBudget::new(ledger.clone()), Arc::new(ledger))
    }
    pub fn acknowledged(&self) {
        let mut holds = {
            let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
            pending.class = MemoryClass::Dirty;
            std::mem::take(&mut pending.holds)
        };
        // Upgrading/dropping a charge can run its registry destructor. Never
        // retain or drop those handles while the registry mutex is held.
        // Pop the registry node before its metadata token can drop, including
        // when cancellation races the drain and the weak source is dead.
        while let Some((_, pending)) = holds.pop_last() {
            if holds.is_empty() {
                // Free the final empty B-tree root before the final token.
                drop(holds);
                if let Some(hold) = pending.charge.upgrade() {
                    ChargeHandle(hold).retain();
                }
                return;
            }
            if let Some(hold) = pending.charge.upgrade() {
                ChargeHandle(hold).retain();
            }
        }
    }
    fn error(
        &self,
        error: lash_durable::owner_memory::ReserveError,
        requested: u64,
    ) -> AllocationError {
        let kind = match error {
            lash_durable::owner_memory::ReserveError::Pressure => ReserveError::Pressure,
            lash_durable::owner_memory::ReserveError::TooLarge => ReserveError::TooLarge,
        };
        AllocationError {
            kind,
            actor: self.0.actor.clone(),
            requested,
            capacity: self.0.memory.policy().max_owner_bytes.get(),
        }
    }
    fn hold(
        &self,
        reservation: MemoryReservation,
        pending: &mut Pending,
    ) -> Result<AllocationHold, AllocationError> {
        // One sparse B-tree entry plus its Charge/hold Arcs and locks.
        // The source owns this metadata obligation; its destructor removes
        // the registry entry before releasing the same obligation.
        let metadata = self
            .0
            .memory
            .reserve(2048, pending.class)
            .map_err(|error| self.error(error, 2048))?;
        let metadata = Arc::new(Mutex::new(Some(metadata)));
        let charge = Arc::new(Charge {
            payload: Mutex::new(Some(reservation)),
            metadata: Arc::clone(&metadata),
            registry: Arc::downgrade(&self.0),
        });
        pending.holds.insert(
            Arc::as_ptr(&charge) as usize,
            PendingCharge {
                charge: Arc::downgrade(&charge),
                _metadata: metadata,
            },
        );
        Ok(AllocationHold::new(ChargeHandle(charge)))
    }
}
impl AllocationLedger for OwnerAllocation {
    fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        let reservation = self
            .0
            .memory
            .reserve(bytes, pending.class)
            .map_err(|error| self.error(error, bytes))?;
        self.hold(reservation, &mut pending)
    }
    fn reserve_shared(
        &self,
        identity: usize,
        bytes: u64,
    ) -> Result<AllocationHold, AllocationError> {
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        let reservation = self
            .0
            .memory
            .reserve_shared(identity, bytes, pending.class)
            .map_err(|error| self.error(error, bytes))?;
        self.hold(reservation, &mut pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_durable::{ActorKey, Epoch, OwnerRetentionSettings};
    use std::num::NonZeroU64;

    fn memory() -> OwnerMemory {
        let limit = NonZeroU64::new(1 << 20).unwrap();
        lash_durable::owner_memory::NodeMemory::new(OwnerRetentionSettings {
            idle_evict: std::time::Duration::from_secs(60),
            max_owner_bytes: limit,
            max_node_bytes: limit,
            max_owner_dirty_bytes: limit,
            max_node_dirty_bytes: limit,
        })
        .owner(&ActorKey::session("cancelled-hydration").unwrap(), Epoch(1))
    }

    /// CANCELLED-SOURCE-ADMISSION: dropping preparation before ack releases
    /// both the source bytes and the acknowledgement registry entry.
    #[test]
    fn cancelled_source_releases_acknowledgement_metadata() {
        let memory = memory();
        let (budget, owner) = OwnerAllocation::budget(memory.clone(), "cancelled-hydration".into());
        for _ in 0..100 {
            let hold = budget.reserve(1024).unwrap();
            drop(hold);
            assert_eq!(
                memory.usage().total(),
                0,
                "a cancelled source retains no metadata"
            );
        }
        for _ in 0..100 {
            let hold = budget.reserve(1024).unwrap();
            std::thread::scope(|scope| {
                scope.spawn(|| owner.acknowledged());
                drop(hold);
            });
            assert_eq!(
                memory.usage().total(),
                0,
                "cancellation racing ack retains no metadata"
            );
        }
    }
}
