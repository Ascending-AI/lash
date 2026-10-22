//! Immutable source maps whose allocation obligation follows every reader.
//!
//! A clone shares the source. A replacement admits its complete map and key
//! storage before copying, and adopts each value through its source owner.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;

use lash_protocol_types::allocation::{AllocationBudget, AllocationError, AllocationHold};

/// A map key's separately allocated fields, copied with the containing map.
pub trait MapKey: Ord + Clone {
    fn owned_capacity(&self) -> u64;
}
impl MapKey for String {
    fn owned_capacity(&self) -> u64 {
        self.capacity() as u64 + 32
    }
}
impl MapKey for crate::checkpoint_graph::NodeId {
    fn owned_capacity(&self) -> u64 {
        0
    }
}

/// A value's source owner admits foreign copies before materializing them.
/// Same-ledger clones share their immutable allocations. A returned value
/// carries its own obligations, including when removed from the map.
pub trait MapValue: Clone {
    fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError>;
}

/// A complete immutable map. The serde representation remains a flat map.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
pub struct SharedSourceMap<K, V>(Arc<Storage<K, V>>);
struct Storage<K, V> {
    entries: BTreeMap<K, V>,
    allocation: AllocationHold,
    budget: AllocationBudget,
}
impl<K, V> Clone for SharedSourceMap<K, V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}
impl<K, V> Default for SharedSourceMap<K, V> {
    fn default() -> Self {
        Self::from(BTreeMap::new())
    }
}
impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for SharedSourceMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.entries.fmt(f)
    }
}
impl<K: PartialEq, V: PartialEq> PartialEq for SharedSourceMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.0.entries == other.0.entries
    }
}
impl<K: Eq, V: Eq> Eq for SharedSourceMap<K, V> {}
impl<K, V> Deref for SharedSourceMap<K, V> {
    type Target = BTreeMap<K, V>;
    fn deref(&self) -> &Self::Target {
        &self.0.entries
    }
}
impl<'a, K, V> IntoIterator for &'a SharedSourceMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::collections::btree_map::Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.entries.iter()
    }
}
impl<K: Ord, V> FromIterator<(K, V)> for SharedSourceMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        Self::from(iter.into_iter().collect::<BTreeMap<_, _>>())
    }
}
impl<K, V> From<BTreeMap<K, V>> for SharedSourceMap<K, V> {
    /// Independently owned cold input. Serving decoders must pre-admit their
    /// materialization and transfer that obligation with `from_admitted`.
    fn from(entries: BTreeMap<K, V>) -> Self {
        Self::from_admitted(
            entries,
            AllocationHold::default(),
            AllocationBudget::unowned(),
        )
    }
}
impl<K: serde::Serialize, V: serde::Serialize> serde::Serialize for SharedSourceMap<K, V> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.entries.serialize(serializer)
    }
}
impl<'de, K: Ord + serde::Deserialize<'de>, V: serde::Deserialize<'de>> serde::Deserialize<'de>
    for SharedSourceMap<K, V>
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BTreeMap::deserialize(deserializer).map(Self::from)
    }
}
impl<K, V> SharedSourceMap<K, V> {
    /// Transfer a whole-source reservation acquired before the map and its
    /// values were decoded. Values with escaping readers must already carry
    /// that same obligation on their own immutable sources.
    pub fn from_admitted(
        entries: BTreeMap<K, V>,
        allocation: AllocationHold,
        budget: AllocationBudget,
    ) -> Self {
        Self(Arc::new(Storage {
            entries,
            allocation,
            budget,
        }))
    }
    pub fn len(&self) -> usize {
        self.0.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.entries.is_empty()
    }
    pub fn allocation_budget(&self) -> &AllocationBudget {
        &self.0.budget
    }
    /// Attach a pre-admitted fresh decode and transfer that same source
    /// obligation into every escaping value before exposing a reader.
    pub fn decoded_in_with(
        &mut self,
        allocation: AllocationHold,
        budget: AllocationBudget,
        mut adopt: impl FnMut(&mut V, AllocationHold, AllocationBudget) -> bool,
    ) -> bool {
        let Some(storage) = Arc::get_mut(&mut self.0) else {
            return false;
        };
        for value in storage.entries.values_mut() {
            if !adopt(value, allocation.clone(), budget.clone()) {
                return false;
            }
        }
        storage.allocation = allocation;
        storage.budget = budget;
        true
    }
    /// Fresh decoders attach their already acquired source obligation before
    /// sharing the map. An unexpectedly shared decode cannot be adopted.
    pub fn decoded_in(&mut self, allocation: AllocationHold, budget: AllocationBudget) -> bool {
        let Some(storage) = Arc::get_mut(&mut self.0) else {
            return false;
        };
        storage.allocation = allocation;
        storage.budget = budget;
        true
    }
}
impl<K: MapKey, V: MapValue> SharedSourceMap<K, V> {
    fn entry_capacity(key: &K) -> u64 {
        // One entry may occupy a complete sparse eleven-slot B-tree node.
        (11 * (std::mem::size_of::<(K, V)>() + std::mem::size_of::<usize>()) + 128) as u64
            + key.owned_capacity()
    }
    fn capacity(entries: &BTreeMap<K, V>) -> u64 {
        entries.keys().fold(
            (std::mem::size_of::<Storage<K, V>>() + 128) as u64,
            |bytes, key| bytes.saturating_add(Self::entry_capacity(key)),
        )
    }
    pub fn empty_in(budget: &AllocationBudget) -> Result<Self, AllocationError> {
        let entries = BTreeMap::new();
        let hold = budget.reserve(Self::capacity(&entries))?;
        Ok(Self::from_admitted(entries, hold, budget.clone()))
    }
    pub fn from_map_in(
        entries: &BTreeMap<K, V>,
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        let hold = budget.reserve(Self::capacity(entries))?;
        let entries = entries
            .iter()
            .map(|(key, value)| Ok((key.clone(), MapValue::clone_in(value, budget)?)))
            .collect::<Result<_, AllocationError>>()?;
        Ok(Self::from_admitted(entries, hold, budget.clone()))
    }
    pub fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            Ok(self.clone())
        } else {
            Self::from_map_in(&self.0.entries, budget)
        }
    }
    /// Publish a complete replacement, or leave this map unchanged.
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<V>, AllocationError> {
        self.insert_ref(&key, value)
    }
    /// Admit a borrowed key before cloning any retained key or map entry.
    pub fn insert_ref(&mut self, key: &K, value: V) -> Result<Option<V>, AllocationError> {
        let budget = &self.0.budget;
        let capacity = self
            .0
            .entries
            .keys()
            .filter(|stored| *stored != key)
            .fold(
                (std::mem::size_of::<Storage<K, V>>() + 128) as u64,
                |bytes, stored| bytes.saturating_add(Self::entry_capacity(stored)),
            )
            .saturating_add(Self::entry_capacity(key));
        let hold = budget.reserve(capacity)?;
        let mut entries = self
            .0
            .entries
            .iter()
            .filter(|(stored, _)| *stored != key)
            .map(|(stored, value)| Ok((stored.clone(), MapValue::clone_in(value, budget)?)))
            .collect::<Result<BTreeMap<_, _>, AllocationError>>()?;
        let previous = self
            .0
            .entries
            .get(key)
            .map(|value| MapValue::clone_in(value, budget))
            .transpose()?;
        entries.insert(key.clone(), MapValue::clone_in(&value, budget)?);
        *self = Self::from_admitted(entries, hold, budget.clone());
        Ok(previous)
    }
    pub fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Result<Option<V>, AllocationError>
    where
        K: Borrow<Q>,
    {
        if !self.0.entries.contains_key(key) {
            return Ok(None);
        }
        let budget = &self.0.budget;
        let hold = budget.reserve(Self::capacity(&self.0.entries))?;
        let mut entries = self
            .0
            .entries
            .iter()
            .map(|(key, value)| Ok((key.clone(), MapValue::clone_in(value, budget)?)))
            .collect::<Result<BTreeMap<_, _>, AllocationError>>()?;
        let removed = entries.remove(key);
        *self = Self::from_admitted(entries, hold, budget.clone());
        Ok(removed)
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) -> Result<(), AllocationError> {
        let budget = &self.0.budget;
        let hold = budget.reserve(Self::capacity(&self.0.entries))?;
        let mut entries = BTreeMap::new();
        for (key, value) in &self.0.entries {
            if keep(key, value) {
                entries.insert(key.clone(), MapValue::clone_in(value, budget)?);
            }
        }
        *self = Self::from_admitted(entries, hold, budget.clone());
        Ok(())
    }
    pub fn extend(
        &mut self,
        entries: impl IntoIterator<Item = (K, V)>,
    ) -> Result<(), AllocationError> {
        let mut draft = self.clone();
        for (key, value) in entries {
            draft.insert(key, value)?;
        }
        *self = draft;
        Ok(())
    }
    /// Replace a value by its key. The callback constructs an admitted
    /// immutable value; no mutable view of map storage escapes.
    pub fn update<Q: Ord + ?Sized>(
        &mut self,
        key: &Q,
        update: impl FnOnce(&V, &AllocationBudget) -> Result<V, AllocationError>,
    ) -> Result<bool, AllocationError>
    where
        K: Borrow<Q>,
    {
        if !self.0.entries.contains_key(key) {
            return Ok(false);
        }
        let budget = &self.0.budget;
        let hold = budget.reserve(Self::capacity(&self.0.entries))?;
        let mut entries = self
            .0
            .entries
            .iter()
            .map(|(key, value)| Ok((key.clone(), MapValue::clone_in(value, budget)?)))
            .collect::<Result<BTreeMap<_, _>, AllocationError>>()?;
        if let Some(value) = entries.get_mut(key) {
            *value = update(value, budget)?;
        }
        *self = Self::from_admitted(entries, hold, budget.clone());
        Ok(true)
    }
}

impl<K: MapKey, V: MapValue> MapValue for SharedSourceMap<K, V> {
    fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Self::clone_in(self, budget)
    }
}

impl MapValue for u64 {
    fn clone_in(&self, _: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(*self)
    }
}
impl MapValue for () {
    fn clone_in(&self, _: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(())
    }
}

impl MapValue for crate::checkpoint_graph::EncodedNode {
    fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(Self {
            id: self.id,
            bytes: self.bytes.clone_in(budget)?,
        })
    }
}
impl crate::checkpoint_graph::NodeSource
    for SharedSourceMap<crate::checkpoint_graph::NodeId, crate::checkpoint_graph::EncodedNode>
{
    fn fetch(
        &self,
        id: &crate::checkpoint_graph::NodeId,
    ) -> Option<crate::checkpoint_graph::StoredNode<'_>> {
        self.get(id)
            .map(|node| crate::checkpoint_graph::StoredNode {
                encoding: crate::checkpoint_graph::BlobEncoding::Graph,
                bytes: &node.bytes,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_protocol_types::allocation::{AllocationLedger, AllocationReservation, ReserveError};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone)]
    struct Ledger(Arc<AtomicU64>);
    struct Hold(Ledger, u64);
    impl AllocationReservation for Hold {
        fn retain(&self) {}
    }
    impl Drop for Hold {
        fn drop(&mut self) {
            self.0.0.fetch_sub(self.1, Ordering::SeqCst);
        }
    }
    impl AllocationLedger for Ledger {
        fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
            self.0
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                    used.checked_add(bytes).filter(|sum| *sum <= 8192)
                })
                .map_err(|_| AllocationError {
                    kind: ReserveError::TooLarge,
                    actor: "map".into(),
                    requested: bytes,
                    capacity: 8192,
                })?;
            Ok(AllocationHold::new(Hold(self.clone(), bytes)))
        }
        fn reserve_shared(&self, _: usize, bytes: u64) -> Result<AllocationHold, AllocationError> {
            self.reserve(bytes)
        }
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Value(u64);
    impl MapValue for Value {
        fn clone_in(&self, _: &AllocationBudget) -> Result<Self, AllocationError> {
            Ok(self.clone())
        }
    }

    /// SOURCE-MAP-REPLACEMENT: refusal precedes copying and publication;
    /// immutable readers retain the original map and its charge until drop.
    #[test]
    fn replacement_admits_before_copy_and_keeps_held_readers() {
        let used = Arc::new(AtomicU64::new(0));
        let budget = AllocationBudget::new(Ledger(used.clone()));
        let mut source: SharedSourceMap<String, Value> =
            SharedSourceMap::empty_in(&budget).unwrap();
        source.insert("first".into(), Value(1)).unwrap();
        let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
        let reader = source.clone();
        let cloned = measurement.finish();
        assert_eq!(cloned.allocated, 0);
        let blocker = budget.reserve(8192 - used.load(Ordering::SeqCst)).unwrap();
        let key = "second".to_string();
        let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
        assert!(source.insert(key, Value(2)).is_err());
        let refused = measurement.finish();
        assert!(
            refused.allocated < 64,
            "refusal preceded key/map copying: {refused:?}"
        );
        assert_eq!(source, reader);
        drop(blocker);
        source.insert("second".into(), Value(2)).unwrap();
        assert_eq!(reader.len(), 1);
        assert_eq!(source.len(), 2);
        drop(source);
        assert!(used.load(Ordering::SeqCst) > 0);
        drop(reader);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }
}
