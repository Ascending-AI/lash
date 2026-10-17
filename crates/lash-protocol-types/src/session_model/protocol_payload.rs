//! A protocol event's payload, shared by every holder of the event.

use crate::derived::DerivedMemo;
use std::any::Any;
use std::sync::Arc;

/// The JSON a protocol event carries: immutable, and shared by every clone
/// of the event, so copying history copies no payload. What a reader
/// derives from it (a plugin's decoded event) is kept beside it with
/// [`Self::derived`] and reused by every later reader of the same retained
/// event. A payload that changes ([`Self::make_mut`]) is a new payload with
/// nothing derived, so a derived value never outlives what it was read
/// from. It serializes as its JSON.
///
/// ```
/// use lash_protocol_types::ProtocolPayload;
///
/// let payload = ProtocolPayload::from(serde_json::json!({"format": 1}));
/// let format = payload.derived(|value| value["format"].as_u64());
/// // The clone is the same payload: what was derived is shared.
/// let held = payload.clone();
/// assert!(std::sync::Arc::ptr_eq(&format, &held.derived(|_| None)));
/// assert_eq!(*format, Some(1));
/// ```
#[derive(Clone)]
pub struct ProtocolPayload(Arc<Inner>);

struct Inner {
    value: serde_json::Value,
    derived: DerivedMemo,
}

impl ProtocolPayload {
    pub fn new(value: serde_json::Value) -> Self {
        Self(Arc::new(Inner {
            value,
            derived: DerivedMemo::default(),
        }))
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

    /// The JSON to change: this payload becomes a new payload, with nothing
    /// derived, and other holders keep the one they hold.
    pub fn make_mut(&mut self) -> &mut serde_json::Value {
        if Arc::get_mut(&mut self.0).is_none() {
            *self = Self::new(self.0.value.clone());
        }
        let inner = Arc::get_mut(&mut self.0).unwrap_or_else(|| unreachable!());
        inner.derived = DerivedMemo::default();
        &mut inner.value
    }

    /// The `T` derived from this payload: computed by `compute` the first
    /// time a holder asks and kept as long as the payload lives.
    pub fn derived<T: Any + Send + Sync>(
        &self,
        compute: impl FnOnce(&serde_json::Value) -> T,
    ) -> Arc<T> {
        self.0.derived.get_or_compute(|| compute(&self.0.value))
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
