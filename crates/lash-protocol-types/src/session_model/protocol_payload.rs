//! A protocol event's payload, shared by every holder of the event.

use crate::allocation::{Allocated, AllocationBudget, AllocationError, AllocationHold};
use crate::derived::DerivedMemo;
use std::any::Any;
use std::sync::Arc;

/// The JSON a protocol event carries: immutable, and shared by every clone
/// of the event, so copying history copies no payload. What a reader
/// derives from it (a plugin's decoded event) is kept beside it with
/// [`Self::derived`] and reused by every later reader of the same retained
/// event. A payload that changes ([`Self::replace`]) is a new payload with
/// nothing derived, so a derived value never outlives what it was read
/// from. It serializes as its JSON.
///
/// ```
/// use lash_protocol_types::ProtocolPayload;
///
/// let payload = ProtocolPayload::from(serde_json::json!({"format": 1}));
/// let format = payload.derived(0, |value, _| value["format"].as_u64()).unwrap();
/// // The clone is the same payload: what was derived is shared.
/// let held = payload.clone();
/// assert!(std::sync::Arc::ptr_eq(&format, &held.derived(0, |_, _| None::<u64>).unwrap()));
/// assert_eq!(**format, Some(1));
/// ```
#[derive(Clone)]
pub struct ProtocolPayload(Arc<Inner>);

struct Inner {
    value: serde_json::Value,
    derived: DerivedMemo,
    allocation: AllocationHold,
    budget: AllocationBudget,
}

impl ProtocolPayload {
    pub fn new(value: serde_json::Value) -> Self {
        Self(Arc::new(Inner {
            value,
            derived: DerivedMemo::default(),
            allocation: Default::default(),
            budget: AllocationBudget::unowned(),
        }))
    }

    /// Transfer a source decode's admission to the Arc readers retain.
    pub fn decoded_in(
        &mut self,
        allocation: AllocationHold,
        budget: AllocationBudget,
    ) -> Result<(), crate::StoredDataCorruption> {
        let inner = Arc::get_mut(&mut self.0).ok_or_else(|| crate::StoredDataCorruption {
            record_kind: "protocol payload".into(),
            message: "a fresh decode unexpectedly shares its payload before admission".into(),
        })?;
        inner.allocation = allocation;
        inner.budget = budget;
        Ok(())
    }

    pub fn allocation_budget(&self) -> &AllocationBudget {
        &self.0.budget
    }

    pub fn value(&self) -> &serde_json::Value {
        &self.0.value
    }

    /// The JSON, copied only when another holder shares it.
    pub fn into_value(self) -> serde_json::Value {
        match Arc::try_unwrap(self.0) {
            Ok(inner) => inner.value,
            Err(shared) => shared.value.clone(),
        }
    }

    /// Adopt a cold or foreign payload before an owner retains it. Immutable
    /// payloads already admitted by this ledger retain their memo and identity.
    pub fn adopt_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        let allocation = budget.reserve(crate::allocation::payload_capacity(
            self.value(),
            std::mem::size_of::<Inner>() + 16,
        ))?;
        Ok(Self(Arc::new(Inner {
            value: self.value().clone(),
            derived: DerivedMemo::default(),
            allocation,
            budget: budget.clone(),
        })))
    }

    /// Replace with independent JSON only after admitting its complete clone.
    /// A refusal preserves this payload and every value derived from it.
    pub fn replace(&mut self, value: &serde_json::Value) -> Result<(), AllocationError> {
        let allocation = self.0.budget.reserve(crate::allocation::payload_capacity(
            value,
            std::mem::size_of::<Inner>() + 16,
        ))?;
        *self = Self(Arc::new(Inner {
            value: value.clone(),
            derived: DerivedMemo::default(),
            allocation,
            budget: self.0.budget.clone(),
        }));
        Ok(())
    }

    /// Derive once from this payload. Admission precedes `compute`, includes
    /// source-sized typed materialization, the Arc and amortized memo slots.
    /// `additional_bytes` bounds any output beyond that source-shaped decode.
    /// The callback can transfer a clone of its hold to nested retained Arcs.
    pub fn derived<T: Any + Send + Sync>(
        &self,
        additional_bytes: u64,
        compute: impl FnOnce(&serde_json::Value, AllocationHold) -> T,
    ) -> Result<Arc<Allocated<T>>, AllocationError> {
        self.0.derived.get_or_try_compute(|| {
            let allocation = self.0.budget.reserve(
                crate::allocation::payload_capacity(
                    self.value(),
                    std::mem::size_of::<Allocated<T>>() + 16 + 256,
                )
                .saturating_add(additional_bytes),
            )?;
            let value = compute(self.value(), allocation.clone());
            Ok(Arc::new(Allocated::new(
                value,
                allocation,
                self.0.budget.clone(),
            )))
        })
    }

    /// Whether `left` and `right` are one retained payload, not merely
    /// equal ones.
    pub fn same(left: &Self, right: &Self) -> bool {
        Arc::ptr_eq(&left.0, &right.0)
    }
}

impl std::ops::Deref for ProtocolPayload {
    type Target = serde_json::Value;

    fn deref(&self) -> &serde_json::Value {
        &self.0.value
    }
}

impl From<serde_json::Value> for ProtocolPayload {
    fn from(value: serde_json::Value) -> Self {
        Self::new(value)
    }
}

impl PartialEq for ProtocolPayload {
    fn eq(&self, other: &Self) -> bool {
        Self::same(self, other) || self.0.value == other.0.value
    }
}

impl PartialEq<serde_json::Value> for ProtocolPayload {
    fn eq(&self, other: &serde_json::Value) -> bool {
        self.0.value == *other
    }
}

impl std::fmt::Debug for ProtocolPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.value.fmt(f)
    }
}

/// The payload's JSON, as [`serde_json::Value`] displays it.
impl std::fmt::Display for ProtocolPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.value.fmt(f)
    }
}

impl serde::Serialize for ProtocolPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.value.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ProtocolPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Self::new)
    }
}
