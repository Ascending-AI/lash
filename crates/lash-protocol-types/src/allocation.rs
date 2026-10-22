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

/// An immutable value and the obligation for its actual retained allocation.
/// Its serialized shape is exactly `T`; ownership metadata never travels.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Allocated<T> {
    value: T,
    #[serde(skip)]
    allocation: AllocationHold,
    #[serde(skip)]
    budget: AllocationBudget,
}
impl<T> Allocated<T> {
    /// Transfer an existing preallocation obligation to the materialized value.
    pub fn new(value: T, allocation: AllocationHold, budget: AllocationBudget) -> Self {
        Self {
            value,
            allocation,
            budget,
        }
    }
    /// Transfer a decode's obligation before publishing its fresh Arc.
    pub fn decoded_in(&mut self, allocation: AllocationHold, budget: AllocationBudget) {
        self.allocation = allocation;
        self.budget = budget;
    }
    pub fn allocation_budget(&self) -> &AllocationBudget {
        &self.budget
    }
}
impl<T: Clone + serde::Serialize> Allocated<T> {
    pub fn adopt_in(
        source: &Arc<Self>,
        budget: &AllocationBudget,
    ) -> Result<Arc<Self>, AllocationError> {
        if source.budget.identity() == budget.identity() {
            return Ok(Arc::clone(source));
        }
        let allocation = budget.reserve(payload_capacity(
            &source.value,
            std::mem::size_of::<Self>() + 16,
        ))?;
        Ok(Arc::new(Self::new(
            source.value.clone(),
            allocation,
            budget.clone(),
        )))
    }
}
impl<T> From<T> for Allocated<T> {
    fn from(value: T) -> Self {
        Self::new(
            value,
            AllocationHold::default(),
            AllocationBudget::unowned(),
        )
    }
}
impl<T> std::ops::Deref for Allocated<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
impl<T> AsRef<T> for Allocated<T> {
    fn as_ref(&self) -> &T {
        &self.value
    }
}
impl<T: PartialEq> PartialEq for Allocated<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl<T: Eq> Eq for Allocated<T> {}

/// A constant-space preflight of a value's encoded structure. Sparse maps and
/// empty array elements still require heap slots even when their text is tiny.
/// The bound includes strings, collection slots and transient clone/decode work.
pub fn payload_capacity(value: &impl serde::Serialize, inline: usize) -> u64 {
    let mut capacity = JsonCapacity::default();
    if serde_json::to_writer(&mut capacity, value).is_err() {
        return u64::MAX;
    }
    capacity.bound(inline)
}

#[derive(Default)]
struct JsonCapacity {
    in_string: bool,
    escaped: bool,
    structure: u64,
    bytes: u64,
}
impl JsonCapacity {
    fn bound(&self, inline: usize) -> u64 {
        self.structure
            .saturating_mul(4)
            .saturating_add(self.bytes.saturating_mul(8))
            .saturating_add(inline as u64)
            .saturating_add(512)
    }
}
impl std::io::Write for JsonCapacity {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        for &byte in bytes {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
            } else {
                match byte {
                    b'"' => self.in_string = true,
                    b':' => self.structure = self.structure.saturating_add(JSON_MAP_ENTRY_BYTES),
                    b'[' => self.structure = self.structure.saturating_add(256),
                    b',' => self.structure = self.structure.saturating_add(64),
                    _ => {}
                }
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// A sparse serde_json B-tree may allocate a full eleven-entry root for
// one pair. Include its edges, allocator headers and the inline Value slots.
const JSON_MAP_ENTRY_BYTES: u64 = 2048;

/// Admission bound for decoding JSON into a typed source value. This scans
/// borrowed bytes with constant space before the JSON parser allocates.
/// Punctuation inside strings is ignored; escapes never expand beyond their
/// encoded byte length. Malformed input still receives a conservative bound
/// before the real decoder reports its own error.
///
/// Every object entry admits a full sparse B-tree node. Arrays admit minimum
/// capacity plus geometric growth per separator (object commas only enlarge
/// the bound). Four such representations cover the JSON value, serde's
/// flattened content, the typed materialization and transient reallocations;
/// encoded bytes separately cover strings and parser scratch. `inline` is
/// the typed root, including its Arc and allocation headers.
pub fn json_decode_capacity(source: &str, inline: usize) -> u64 {
    use std::io::Write as _;
    let mut capacity = JsonCapacity::default();
    // The structural writer is infallible and retains no source bytes.
    let _ = capacity.write_all(source.as_bytes());
    capacity.bound(inline)
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error(transparent)]
    Stored(#[from] crate::StoredDataCorruption),
    #[error(transparent)]
    Allocation(#[from] AllocationError),
}

/// Heap fields a source value copies when its containing buffer detaches.
/// Shared immutable allocations carry their own holds and are excluded.
pub trait SourceCapacity {
    fn owned_capacity(&self) -> u64;
    /// Whether immutable allocations excluded from `owned_capacity` already
    /// belong to this ledger. Unknown sources conservatively require adoption.
    fn sources_admitted_in(&self, _budget: &AllocationBudget) -> bool {
        false
    }
}

impl<T: ?Sized> SourceCapacity for Arc<T> {
    fn owned_capacity(&self) -> u64 {
        0
    }
}
macro_rules! inline_source {
    ($($ty:ty),* $(,)?) => { $(impl SourceCapacity for $ty {
        fn owned_capacity(&self) -> u64 { 0 }
        fn sources_admitted_in(&self, _: &AllocationBudget) -> bool { true }
    })* };
}
inline_source!((), usize, u64, u32, u16, u8, i64, i32, bool);
impl SourceCapacity for String {
    fn owned_capacity(&self) -> u64 {
        self.capacity() as u64 + 32
    }
}
impl SourceCapacity for serde_json::Value {
    fn owned_capacity(&self) -> u64 {
        match self {
            Self::String(text) => text.capacity() as u64,
            Self::Array(values) => values.iter().fold(
                (values.capacity() as u64).saturating_mul(std::mem::size_of::<Self>() as u64),
                |bytes, value| bytes.saturating_add(value.owned_capacity()),
            ),
            Self::Object(values) => values.owned_capacity(),
            Self::Null | Self::Bool(_) | Self::Number(_) => 0,
        }
    }
}
impl SourceCapacity for serde_json::Map<String, serde_json::Value> {
    fn owned_capacity(&self) -> u64 {
        self.iter().fold(
            (self.len() as u64).saturating_mul(JSON_MAP_ENTRY_BYTES),
            |bytes, (key, value)| {
                bytes
                    .saturating_add(key.owned_capacity())
                    .saturating_add(value.owned_capacity())
            },
        )
    }
}

impl SourceCapacity for crate::ConversationRecord {
    fn sources_admitted_in(&self, budget: &AllocationBudget) -> bool {
        self.parts.allocation_budget().identity() == budget.identity()
    }
    fn owned_capacity(&self) -> u64 {
        (self.id.capacity() as u64)
            .saturating_add(payload_capacity(&(&self.origin, &self.reply_marker), 0))
    }
}
impl SourceCapacity for crate::Message {
    fn sources_admitted_in(&self, budget: &AllocationBudget) -> bool {
        self.parts.allocation_budget().identity() == budget.identity()
    }
    fn owned_capacity(&self) -> u64 {
        (self.id.capacity() as u64)
            .saturating_add(payload_capacity(&(&self.origin, &self.reply_marker), 0))
    }
}
impl SourceCapacity for crate::ProtocolEvent {
    fn sources_admitted_in(&self, budget: &AllocationBudget) -> bool {
        self.payload.allocation_budget().identity() == budget.identity()
    }
    fn owned_capacity(&self) -> u64 {
        self.plugin_id.capacity() as u64 + 32
    }
}
impl<E: SourceCapacity> SourceCapacity for crate::SessionHistoryRecord<E> {
    fn sources_admitted_in(&self, budget: &AllocationBudget) -> bool {
        match self {
            Self::Conversation(message) => message.sources_admitted_in(budget),
            Self::Protocol(event) => event.sources_admitted_in(budget),
        }
    }
    fn owned_capacity(&self) -> u64 {
        match self {
            Self::Conversation(message) => message.owned_capacity(),
            Self::Protocol(event) => event.owned_capacity(),
        }
    }
}
impl SourceCapacity for crate::llm::types::LlmMessage {
    fn owned_capacity(&self) -> u64 {
        0
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone, Default)]
    pub(crate) struct Ledger {
        pub used: Arc<AtomicU64>,
        pub limit: Arc<AtomicU64>,
    }
    struct Hold(Ledger, u64);
    impl AllocationReservation for Hold {
        fn retain(&self) {}
    }
    impl Drop for Hold {
        fn drop(&mut self) {
            self.0.used.fetch_sub(self.1, Ordering::SeqCst);
        }
    }
    impl Ledger {
        pub fn budget(&self) -> AllocationBudget {
            AllocationBudget::new(self.clone())
        }
        pub fn used(&self) -> u64 {
            self.used.load(Ordering::SeqCst)
        }
        pub fn limit(&self, limit: u64) {
            self.limit.store(limit, Ordering::SeqCst);
        }
    }
    impl AllocationLedger for Ledger {
        fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
            let limit = self.limit.load(Ordering::SeqCst);
            self.used
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                    used.checked_add(bytes).filter(|next| *next <= limit)
                })
                .map_err(|_| AllocationError {
                    kind: ReserveError::Pressure,
                    actor: "payload-law".into(),
                    requested: bytes,
                    capacity: limit,
                })?;
            Ok(AllocationHold::new(Hold(self.clone(), bytes)))
        }
        fn reserve_shared(&self, _: usize, bytes: u64) -> Result<AllocationHold, AllocationError> {
            self.reserve(bytes)
        }
    }
}
