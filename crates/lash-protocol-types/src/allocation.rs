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
    let mut in_string = false;
    let mut escaped = false;
    let mut structure = 0u64;
    for byte in source.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b':' => structure = structure.saturating_add(JSON_MAP_ENTRY_BYTES),
                b'[' => structure = structure.saturating_add(256),
                b',' => structure = structure.saturating_add(64),
                _ => {}
            }
        }
    }
    structure
        .saturating_mul(4)
        .saturating_add((source.len() as u64).saturating_mul(8))
        .saturating_add(inline as u64)
        .saturating_add(512)
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
}

impl<T: ?Sized> SourceCapacity for Arc<T> {
    fn owned_capacity(&self) -> u64 {
        0
    }
}
macro_rules! inline_source {
    ($($ty:ty),* $(,)?) => { $(impl SourceCapacity for $ty {
        fn owned_capacity(&self) -> u64 { 0 }
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
    fn owned_capacity(&self) -> u64 {
        (self.id.capacity() as u64)
            .saturating_add(payload_capacity(&(&self.origin, &self.reply_marker), 0))
    }
}
impl SourceCapacity for crate::Message {
    fn owned_capacity(&self) -> u64 {
        (self.id.capacity() as u64)
            .saturating_add(payload_capacity(&(&self.origin, &self.reply_marker), 0))
    }
}
impl SourceCapacity for crate::ProtocolEvent {
    fn owned_capacity(&self) -> u64 {
        self.plugin_id.capacity() as u64 + 32
    }
}
impl<E: SourceCapacity> SourceCapacity for crate::SessionHistoryRecord<E> {
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
