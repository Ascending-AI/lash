//! Explicit allocation admission shared by persistent prompt values.
//!
//! The durable owner supplies the ledger implementation. This module owns
//! no limits, defaults, registry or allocator; immutable allocations carry
//! the returned obligations for as long as any reader holds them.

use std::sync::Arc;

pub use lash_kernel_doc::allocation::{
    AllocationBudget, AllocationError, AllocationHold, AllocationLedger, AllocationReservation,
    ReserveError, SharedBytes,
};

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
impl SourceCapacity for crate::NodeId {
    fn owned_capacity(&self) -> u64 {
        self.len() as u64
    }
}
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
/// Conservative typed MessagePack materialization bound, scanned in constant
/// space before serde allocates. Container hints are validated against actual
/// tokens, so a truncated large-count header never sizes a collection.
/// Every scalar covers sparse map slots/array growth; encoded text and bytes
/// cover parser copies. Returns None for malformed or trailing input.
pub fn msgpack_decode_capacity(source: &[u8], inline: usize) -> Option<u64> {
    fn take<'a>(source: &'a [u8], pos: &mut usize, len: usize) -> Option<&'a [u8]> {
        let end = pos.checked_add(len)?;
        let value = source.get(*pos..end)?;
        *pos = end;
        Some(value)
    }
    fn number(source: &[u8], pos: &mut usize, width: usize) -> Option<usize> {
        take(source, pos, width)?.iter().try_fold(0usize, |n, b| {
            n.checked_mul(256)?.checked_add(usize::from(*b))
        })
    }
    let mut pos = 0usize;
    let mut pending = 1u64;
    let mut capacity = (inline as u64).saturating_add(512);
    while pending != 0 {
        let marker = *take(source, &mut pos, 1)?.first()?;
        let mut children = 0u64;
        let mut payload = 0usize;
        let mut byte_array = false;
        match marker {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => {}
            0x80..=0x8f => children = u64::from(marker & 15) * 2,
            0x90..=0x9f => children = u64::from(marker & 15),
            0xa0..=0xbf => payload = usize::from(marker & 31),
            0xc4..=0xc6 => {
                payload = number(source, &mut pos, 1 << (marker - 0xc4))?;
                byte_array = true;
            }
            0xc7..=0xc9 => {
                payload = number(source, &mut pos, 1 << (marker - 0xc7))?.checked_add(1)?
            }
            0xca => payload = 4,
            0xcb => payload = 8,
            0xcc..=0xcf => payload = 1 << (marker - 0xcc),
            0xd0..=0xd3 => payload = 1 << (marker - 0xd0),
            0xd4..=0xd8 => payload = (1usize << (marker - 0xd4)).checked_add(1)?,
            0xd9..=0xdb => payload = number(source, &mut pos, 1 << (marker - 0xd9))?,
            0xdc..=0xdd => children = number(source, &mut pos, 2 << (marker - 0xdc))? as u64,
            0xde..=0xdf => {
                children =
                    (number(source, &mut pos, 2 << (marker - 0xde))? as u64).checked_mul(2)?
            }
            _ => return None,
        }
        take(source, &mut pos, payload)?;
        pending = (pending - 1).checked_add(children)?;
        capacity = capacity
            .saturating_add(2048)
            .saturating_add((payload as u64).saturating_mul(if byte_array { 128 } else { 8 }));
    }
    (pos == source.len()).then_some(capacity)
}
