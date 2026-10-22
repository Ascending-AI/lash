//! Owner-supplied allocation admission for immutable retained sources.

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

/// Immutable encoded bytes with their admission on the allocation readers
/// actually retain. Borrowing never reveals an uncharged inner Arc.
#[derive(Clone)]
pub struct SharedBytes(Arc<ByteStorage>);
struct ByteStorage {
    bytes: Arc<[u8]>,
    _allocation: AllocationHold,
    budget: AllocationBudget,
}
impl SharedBytes {
    /// Cold independently owned bytes. Serving adoption uses `clone_in`.
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self(Arc::new(ByteStorage {
            bytes: bytes.into(),
            _allocation: AllocationHold::default(),
            budget: AllocationBudget::unowned(),
        }))
    }
    /// Reserve before copying a borrowed encoded source into retained storage.
    pub fn copy_from_slice_in(
        bytes: &[u8],
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        let allocation = budget.reserve(
            (bytes.len() as u64).saturating_add(std::mem::size_of::<ByteStorage>() as u64 + 96),
        )?;
        Ok(Self(Arc::new(ByteStorage {
            bytes: Arc::from(bytes),
            _allocation: allocation,
            budget: budget.clone(),
        })))
    }
    /// Consume a reservation acquired before allocating `bytes`. The caller
    /// must cover the Vec capacity, final shared slice, and storage headers
    /// together, since conversion keeps both byte allocations alive briefly.
    pub fn from_admitted(
        bytes: Vec<u8>,
        allocation: AllocationHold,
        budget: AllocationBudget,
    ) -> Self {
        Self(Arc::new(ByteStorage {
            bytes: bytes.into(),
            _allocation: allocation,
            budget,
        }))
    }
    /// Transfer a whole-source decode reservation before publishing readers.
    /// Shared storage is refused instead of changing another reader's ledger.
    pub fn decoded_in(&mut self, allocation: AllocationHold, budget: AllocationBudget) -> bool {
        let Some(storage) = Arc::get_mut(&mut self.0) else {
            return false;
        };
        if Arc::get_mut(&mut storage.bytes).is_none() {
            return false;
        }
        storage._allocation = allocation;
        storage.budget = budget;
        true
    }

    /// Reuse exactly this ledger's admitted source; a foreign source gets an
    /// independently charged copy before the new owner retains any bytes.
    pub fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        Self::copy_from_slice_in(self, budget)
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Default for SharedBytes {
    fn default() -> Self {
        Self::new(Arc::<[u8]>::default())
    }
}
impl From<Vec<u8>> for SharedBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}
impl From<Arc<[u8]>> for SharedBytes {
    fn from(bytes: Arc<[u8]>) -> Self {
        Self::new(bytes)
    }
}
impl From<&[u8]> for SharedBytes {
    fn from(bytes: &[u8]) -> Self {
        Self::new(bytes)
    }
}
impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.bytes
    }
}
impl std::ops::Deref for SharedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}
impl PartialEq for SharedBytes {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || self.as_ref() == other.as_ref()
    }
}
impl Eq for SharedBytes {}
impl std::fmt::Debug for SharedBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_ref().fmt(f)
    }
}

impl serde::Serialize for SharedBytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(self.as_ref(), serializer)
    }
}
impl<'de> serde::Deserialize<'de> for SharedBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Vec<u8> as serde::Deserialize>::deserialize(deserializer).map(Self::from)
    }
}
