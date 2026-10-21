//! Explicit allocation admission shared by persistent prompt values.
//!
//! The durable owner supplies the ledger implementation. This module owns
//! no limits, defaults, registry or allocator; immutable allocations carry
//! the returned obligations for as long as any reader holds them.

use std::sync::Arc;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum ReserveError {
    Pressure,
    TooLarge,
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
#[error("owner {actor} allocation of {requested} bytes refused ({kind:?}; capacity {capacity})")]
pub struct AllocationError {
    pub kind: ReserveError,
    pub actor: String,
    pub requested: u64,
    pub capacity: u64,
}

/// Implemented by the durable node's reservation handle, without a reverse
/// dependency from protocol values to the durable actor runtime.
pub trait AllocationReservation: Send + Sync {
    fn retain(&self);
}

#[derive(Clone, Default)]
pub struct AllocationHold(Option<Arc<dyn AllocationReservation>>);
impl std::fmt::Debug for AllocationHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AllocationHold")
            .field(&self.0.is_some())
            .finish()
    }
}
impl AllocationHold {
    pub fn new(reservation: impl AllocationReservation + 'static) -> Self {
        Self(Some(Arc::new(reservation)))
    }
    pub fn into_retained(self) -> Self {
        if let Some(hold) = &self.0 {
            hold.retain();
        }
        self
    }
}

pub trait AllocationLedger: Send + Sync {
    fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError>;
    fn reserve_shared(
        &self,
        identity: usize,
        bytes: u64,
    ) -> Result<AllocationHold, AllocationError>;
}

#[derive(Clone, Default)]
pub struct AllocationBudget(Option<Arc<dyn AllocationLedger>>);
impl std::fmt::Debug for AllocationBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AllocationBudget")
            .field(&self.0.is_some())
            .finish()
    }
}
impl AllocationBudget {
    pub fn new(ledger: impl AllocationLedger + 'static) -> Self {
        Self(Some(Arc::new(ledger)))
    }
    /// Cold, independently owned flat values have no actor attribution.
    /// Serving owners must supply their node ledger instead.
    pub fn identity(&self) -> usize {
        self.0
            .as_ref()
            .map_or(0, |ledger| Arc::as_ptr(ledger) as *const () as usize)
    }
    pub fn unowned() -> Self {
        Self::default()
    }
    pub fn reserve_shared(
        &self,
        identity: usize,
        bytes: u64,
    ) -> Result<AllocationHold, AllocationError> {
        self.0.as_ref().map_or_else(
            || Ok(AllocationHold::default()),
            |ledger| ledger.reserve_shared(identity, bytes),
        )
    }
    pub fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
        self.0.as_ref().map_or_else(
            || Ok(AllocationHold::default()),
            |ledger| ledger.reserve(bytes),
        )
    }
}

/// A conservative capacity bound including inline objects, owned strings,
/// collection capacity and allocator/Arc headers. The serde walk visits only
/// the changed value and never retains a second representation.
pub fn payload_capacity(value: &impl serde::Serialize, inline: usize) -> u64 {
    let mut length = crate::core_support::EncodedLength::default();
    if serde_json::to_writer(&mut length, value).is_err() {
        return u64::MAX;
    }
    (length.len() as u64)
        .saturating_mul(4)
        .saturating_add(inline as u64)
        .saturating_add(128)
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error(transparent)]
    Stored(#[from] crate::StoredDataCorruption),
    #[error(transparent)]
    Allocation(#[from] AllocationError),
}
