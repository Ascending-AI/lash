use super::Value;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::borrow::Borrow;
use std::hash::{Hash, Hasher};
use std::ops::Index;
use std::sync::Arc;

const RECORD_INDEX_THRESHOLD: usize = 8;
const RECORD_INLINE_CAPACITY: usize = 4;

/// A property or binding name, identified by its text.
///
/// Two symbols are equal exactly when their text is equal, so a symbol needs
/// no table to mean anything: there is no process-wide interner for guest
/// names to accumulate in, and nothing one execution names can outlive the
/// values that hold it (FIG-4158). A constant name the runtime spells itself
/// borrows its text and allocates nothing; a name compiled into a program
/// shares one allocation with every symbol minted from it, so the common
/// comparison is a pointer check. Text equality is the fallback, never a hash.
#[derive(Clone, Debug)]
pub(crate) struct Symbol(SymbolText);

#[derive(Clone, Debug)]
enum SymbolText {
    Constant(&'static str),
    Shared(Arc<str>),
}

impl Symbol {
    pub(crate) fn new(name: &str) -> Self {
        Self(SymbolText::Shared(Arc::from(name)))
    }

    pub(crate) const fn constant(name: &'static str) -> Self {
        Self(SymbolText::Constant(name))
    }

    pub(crate) fn from_text(text: Arc<str>) -> Self {
        Self(SymbolText::Shared(text))
    }

    pub(crate) fn as_str(&self) -> &str {
        match &self.0 {
            SymbolText::Constant(text) => text,
            SymbolText::Shared(text) => text,
        }
    }
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        if let (SymbolText::Shared(left), SymbolText::Shared(right)) = (&self.0, &other.0)
            && Arc::ptr_eq(left, right)
        {
            return true;
        }
        self.as_str() == other.as_str()
    }
}

impl Eq for Symbol {}

/// Hashes exactly as the text does, so a map keyed by symbols answers a
/// lookup by `&str` through [`Borrow`].
impl Hash for Symbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl Borrow<str> for Symbol {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct RecordEntry {
    pub(super) symbol: Symbol,
    pub(super) value: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Record {
    pub(super) entries: SmallVec<[RecordEntry; RECORD_INLINE_CAPACITY]>,
    index: Option<FxHashMap<Symbol, usize>>,
}

impl Record {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: SmallVec::with_capacity(capacity),
            index: (capacity > RECORD_INDEX_THRESHOLD)
                .then(|| FxHashMap::with_capacity_and_hasher(capacity, Default::default())),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        let index = self.position_for(name)?;
        Some(&self.entries[index].value)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        let index = self.position_for(name)?;
        Some(&mut self.entries[index].value)
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        let index = self.position_for(name)?;
        Some(self.remove_at(index))
    }

    pub fn insert(&mut self, name: String, value: Value) -> Option<Value> {
        if let Some(index) = self.position_for(&name) {
            return Some(std::mem::replace(&mut self.entries[index].value, value));
        }
        self.push_new(Symbol::from_text(Arc::from(name)), value);
        None
    }

    /// Inserts under a name the runtime spells itself, allocating no key.
    pub(crate) fn insert_constant(&mut self, name: &'static str, value: Value) -> Option<Value> {
        self.insert_symbolized(&Symbol::constant(name), value)
    }

    pub fn insert_str(&mut self, name: &str, value: Value) -> Option<Value> {
        if let Some(index) = self.position_for(name) {
            return Some(std::mem::replace(&mut self.entries[index].value, value));
        }
        self.push_new(Symbol::new(name), value);
        None
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries
            .iter()
            .map(|entry| (entry.symbol.as_str(), &entry.value))
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        self.entries.iter_mut().map(|entry| &mut entry.value)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.symbol.as_str())
    }

    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|entry| &entry.value)
    }

    pub(crate) fn get_symbol(&self, symbol: &Symbol) -> Option<&Value> {
        self.get(symbol.as_str())
    }

    pub(crate) fn get_symbol_mut(&mut self, symbol: &Symbol) -> Option<&mut Value> {
        self.get_mut(symbol.as_str())
    }

    pub(crate) fn insert_symbolized(&mut self, symbol: &Symbol, value: Value) -> Option<Value> {
        if let Some(index) = self.position_for(symbol.as_str()) {
            return Some(std::mem::replace(&mut self.entries[index].value, value));
        }
        self.push_new(symbol.clone(), value);
        None
    }

    fn push_new(&mut self, symbol: Symbol, value: Value) {
        let index = self.entries.len();
        self.entries.push(RecordEntry { symbol, value });
        self.reindex_after_insert(index);
    }

    pub(super) fn remove_symbol(&mut self, symbol: &Symbol) -> Option<Value> {
        self.remove(symbol.as_str())
    }

    fn remove_at(&mut self, index: usize) -> Value {
        // Property order is observable — `Object.keys`, `JSON.stringify`, and
        // object rest all read it — so the vacated slot cannot be backfilled
        // from the end. `{ a, ...rest }` lowers to copy-then-delete, and a
        // swap_remove there rotated the last key to the front of `rest`.
        let removed = self.entries.remove(index);
        // The shift moves every entry above `index` down one slot, so the index
        // is repaired in place rather than rebuilt: a fresh map per delete
        // allocated and rehashed the whole record to learn what the shift
        // already said.
        if let Some(map) = &mut self.index {
            if self.entries.len() > RECORD_INDEX_THRESHOLD {
                map.remove(removed.symbol.as_str());
                for slot in map.values_mut() {
                    if *slot > index {
                        *slot -= 1;
                    }
                }
            } else {
                // Below the threshold the scan is the cheaper lookup, which is
                // the same shape `rebuild_index` produces at this length.
                self.index = None;
            }
        }
        removed.value
    }

    fn position_for(&self, name: &str) -> Option<usize> {
        if let Some(index) = &self.index {
            return index.get(name).copied();
        }
        self.entries
            .iter()
            .position(|entry| entry.symbol.as_str() == name)
    }

    fn rebuild_index(&mut self) {
        self.index = (self.entries.len() > RECORD_INDEX_THRESHOLD).then(|| {
            let mut index =
                FxHashMap::with_capacity_and_hasher(self.entries.len(), Default::default());
            for (slot, entry) in self.entries.iter().enumerate() {
                index.insert(entry.symbol.clone(), slot);
            }
            index
        });
    }

    fn reindex_after_insert(&mut self, index: usize) {
        if let Some(map) = &mut self.index {
            map.insert(self.entries[index].symbol.clone(), index);
            return;
        }
        if self.entries.len() > RECORD_INDEX_THRESHOLD {
            self.rebuild_index();
        }
    }
}

impl Index<&str> for Record {
    type Output = Value;

    fn index(&self, name: &str) -> &Self::Output {
        self.get(name)
            .unwrap_or_else(|| panic!("missing record key `{name}`"))
    }
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        self.entries.iter().all(|entry| {
            other
                .get_symbol(&entry.symbol)
                .is_some_and(|value| value == &entry.value)
        })
    }
}

impl FromIterator<(String, Value)> for Record {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        let iter = iter.into_iter();
        let (lower, _) = iter.size_hint();
        let mut record = Record::with_capacity(lower);
        for (name, value) in iter {
            record.insert(name, value);
        }
        record
    }
}

impl Serialize for Record {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for entry in &self.entries {
            map.serialize_entry(entry.symbol.as_str(), &entry.value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Record {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let map = FxHashMap::<String, Value>::deserialize(deserializer)?;
        Ok(map.into_iter().collect())
    }
}

pub(crate) fn record_with_capacity(capacity: usize) -> Record {
    Record::with_capacity(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_of(len: usize) -> Record {
        let mut record = Record::new();
        for key in 0..len {
            record.insert(format!("k{key}"), Value::Number(key as f64));
        }
        record
    }

    fn keys_of(record: &Record) -> Vec<String> {
        record.keys().map(str::to_string).collect()
    }

    /// Removal shifts, so every entry above the vacated slot moves down one.
    /// The symbol index has to learn that; the edge is that it is repaired in
    /// place now rather than rebuilt from scratch, so a stale slot would read
    /// back the wrong value rather than simply being slower.
    #[test]
    fn removing_a_key_keeps_order_and_lookups_above_the_vacated_slot() {
        // Indexed: ten keys is past `RECORD_INDEX_THRESHOLD`.
        let mut record = record_of(10);
        assert_eq!(record.remove("k3"), Some(Value::Number(3.0)));
        assert_eq!(
            keys_of(&record),
            ["k0", "k1", "k2", "k4", "k5", "k6", "k7", "k8", "k9"]
        );
        assert!(record.index.is_some(), "nine keys still index");
        for key in [0, 1, 2, 4, 5, 6, 7, 8, 9] {
            assert_eq!(
                record.get(&format!("k{key}")),
                Some(&Value::Number(key as f64)),
                "k{key} after removing a key below it"
            );
        }

        // The last key: nothing shifts, and nothing may be left pointing past
        // the end.
        assert_eq!(record.remove("k9"), Some(Value::Number(9.0)));
        assert_eq!(record.get("k8"), Some(&Value::Number(8.0)));
        assert_eq!(record.get("k9"), None);

        // Crossing back under the threshold drops the index, which is what
        // `rebuild_index` produced at this length before.
        assert_eq!(record.len(), 8);
        assert!(record.index.is_none(), "eight keys stop indexing");
        assert_eq!(record.get("k7"), Some(&Value::Number(7.0)));
        record.insert("k10".to_string(), Value::Number(10.0));
        assert!(record.index.is_some(), "nine keys index again");
        assert_eq!(record.get("k10"), Some(&Value::Number(10.0)));
        assert_eq!(record.get("k0"), Some(&Value::Number(0.0)));
    }

    /// The un-indexed path takes the same shift, and a record that never
    /// reaches the threshold must not grow an index on the way out.
    #[test]
    fn removing_a_key_from_a_small_record_keeps_order() {
        let mut record = record_of(4);
        assert_eq!(record.remove("k1"), Some(Value::Number(1.0)));
        assert_eq!(keys_of(&record), ["k0", "k2", "k3"]);
        assert!(record.index.is_none());
        assert_eq!(record.remove("missing"), None);
        assert_eq!(record.get("k3"), Some(&Value::Number(3.0)));
    }
}
