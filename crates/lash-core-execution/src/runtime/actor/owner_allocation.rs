//! The protocol reservation bridge to the durable node's owner ledger.
use lash_durable::owner_memory::{MemoryClass, MemoryReservation, OwnerMemory};
use lash_protocol_types::allocation::{
    AllocationBudget, AllocationError, AllocationHold, AllocationLedger, AllocationReservation,
    ReserveError,
};
use std::sync::{Arc, Mutex, Weak};

#[derive(Clone)]
pub struct OwnerAllocation(Arc<State>);
struct State {
    memory: OwnerMemory,
    actor: String,
    pending: Mutex<Vec<Pending>>,
}
struct Pending {
    charge: Weak<Charge>,
    _metadata: Arc<Mutex<Option<MemoryReservation>>>,
}
struct Charge {
    payload: Mutex<Option<MemoryReservation>>,
    metadata: Arc<Mutex<Option<MemoryReservation>>>,
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
    pub fn budget(memory: OwnerMemory, actor: String) -> (AllocationBudget, Arc<Self>) {
        let ledger = Self(Arc::new(State {
            memory,
            actor,
            pending: Mutex::default(),
        }));
        (AllocationBudget::new(ledger.clone()), Arc::new(ledger))
    }
    pub fn acknowledged(&self) {
        let pending =
            std::mem::take(&mut *self.0.pending.lock().unwrap_or_else(|p| p.into_inner()));
        for hold in pending.into_iter().filter_map(|hold| hold.charge.upgrade()) {
            ChargeHandle(hold).retain();
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
    fn hold(&self, reservation: MemoryReservation) -> Result<AllocationHold, AllocationError> {
        // The pending weak handle and its queue capacity outlive a dropped
        // payload token until acknowledgement or owner invalidation.
        let metadata = self
            .0
            .memory
            .reserve(256, MemoryClass::Dirty)
            .map_err(|error| self.error(error, 256))?;
        let metadata = Arc::new(Mutex::new(Some(metadata)));
        let charge = Arc::new(Charge {
            payload: Mutex::new(Some(reservation)),
            metadata: metadata.clone(),
        });
        self.0
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Pending {
                charge: Arc::downgrade(&charge),
                _metadata: metadata,
            });
        Ok(AllocationHold::new(ChargeHandle(charge)))
    }
}
impl AllocationLedger for OwnerAllocation {
    fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
        let reservation = self
            .0
            .memory
            .reserve(bytes, MemoryClass::Dirty)
            .map_err(|error| self.error(error, bytes))?;
        self.hold(reservation)
    }
    fn reserve_shared(
        &self,
        identity: usize,
        bytes: u64,
    ) -> Result<AllocationHold, AllocationError> {
        let reservation = self
            .0
            .memory
            .reserve_shared(identity, bytes, MemoryClass::Dirty)
            .map_err(|error| self.error(error, bytes))?;
        self.hold(reservation)
    }
}
