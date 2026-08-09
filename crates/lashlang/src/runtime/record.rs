use super::Value;
use lash_sansio::sync::RwLockExt;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::ops::Index;
use std::sync::{Arc, OnceLock, RwLock};

const RECORD_INDEX_THRESHOLD: usize = 8;
const RECORD_INLINE_CAPACITY: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Symbol(u32);

#[derive(Default)]
struct SymbolTable {
    lookup: FxHashMap<Arc<str>, Symbol>,
    names: Vec<Arc<str>>,
}

fn symbol_table() -> &'static RwLock<SymbolTable> {
    static TABLE: OnceLock<RwLock<SymbolTable>> = OnceLock::new();
    TABLE.get_or_init(|| RwLock::new(SymbolTable::default()))
}

pub(crate) fn lookup_symbol(name: &str) -> Option<Symbol> {
    symbol_table().read_recover().lookup.get(name).copied()
}

pub(crate) fn intern_symbol(name: &str) -> Symbol {
    intern_symbol_with_name(name).0
}

pub(crate) fn intern_symbol_with_name(name: &str) -> (Symbol, Arc<str>) {
    {
        let table = symbol_table().read_recover();
        if let Some(symbol) = table.lookup.get(name) {
            return (*symbol, table.names[symbol.0 as usize].clone());
        }
    }

    let mut table = symbol_table().write_recover();
    if let Some(symbol) = table.lookup.get(name) {
        return (*symbol, table.names[symbol.0 as usize].clone());
    }

    let symbol = Symbol(table.names.len() as u32);
    let text: Arc<str> = Arc::<str>::from(name);
    table.names.push(text.clone());
    table.lookup.insert(text.clone(), symbol);
    (symbol, text)
}

pub(crate) fn symbol_name(symbol: Symbol) -> Arc<str> {
    symbol_table().read_recover().names[symbol.0 as usize].clone()
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct RecordEntry {
    pub(super) symbol: Symbol,
    pub(super) name: Arc<str>,
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
        self.get_symbol(lookup_symbol(name)?)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        let symbol = lookup_symbol(name)?;
        let index = self.position_for(symbol)?;
        Some(&mut self.entries[index].value)
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        let symbol = lookup_symbol(name)?;
        self.remove_symbol(symbol)
    }

    pub fn insert(&mut self, name: String, value: Value) -> Option<Value> {
        let (symbol, name) = intern_symbol_with_name(&name);
        self.insert_symbolized(symbol, name, value)
    }

    pub fn insert_str(&mut self, name: &str, value: Value) -> Option<Value> {
        let (symbol, name) = intern_symbol_with_name(name);
        self.insert_symbolized(symbol, name, value)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries
            .iter()
            .map(|entry| (entry.name.as_ref(), &entry.value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_ref())
    }

    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|entry| &entry.value)
    }

    pub(crate) fn get_symbol(&self, symbol: Symbol) -> Option<&Value> {
        let index = self.position_for(symbol)?;
        Some(&self.entries[index].value)
    }

    pub(crate) fn get_symbol_mut(&mut self, symbol: Symbol) -> Option<&mut Value> {
        let index = self.position_for(symbol)?;
        Some(&mut self.entries[index].value)
    }

    pub(crate) fn insert_symbolized(
        &mut self,
        symbol: Symbol,
        name: Arc<str>,
        value: Value,
    ) -> Option<Value> {
        if let Some(index) = self.position_for(symbol) {
            return Some(std::mem::replace(&mut self.entries[index].value, value));
        }

        let index = self.entries.len();
        self.entries.push(RecordEntry {
            symbol,
            name,
            value,
        });
        self.reindex_after_insert(index);
        None
    }

    pub(super) fn remove_symbol(&mut self, symbol: Symbol) -> Option<Value> {
        let index = self.position_for(symbol)?;
        let removed = self.entries.swap_remove(index);
        self.reindex_after_remove(symbol, index);
        Some(removed.value)
    }

    fn position_for(&self, symbol: Symbol) -> Option<usize> {
        if let Some(index) = &self.index {
            return index.get(&symbol).copied();
        }
        self.entries.iter().position(|entry| entry.symbol == symbol)
    }

    fn rebuild_index(&mut self) {
        self.index = (self.entries.len() > RECORD_INDEX_THRESHOLD).then(|| {
            let mut index =
                FxHashMap::with_capacity_and_hasher(self.entries.len(), Default::default());
            for (slot, entry) in self.entries.iter().enumerate() {
                index.insert(entry.symbol, slot);
            }
            index
        });
    }

    fn reindex_after_insert(&mut self, index: usize) {
        if let Some(map) = &mut self.index {
            map.insert(self.entries[index].symbol, index);
            return;
        }
        if self.entries.len() > RECORD_INDEX_THRESHOLD {
            self.rebuild_index();
        }
    }

    fn reindex_after_remove(&mut self, removed: Symbol, index: usize) {
        if self.entries.len() <= RECORD_INDEX_THRESHOLD {
            self.index = None;
            return;
        }

        let Some(map) = &mut self.index else {
            self.rebuild_index();
            return;
        };
        map.remove(&removed);
        if let Some(moved) = self.entries.get(index) {
            map.insert(moved.symbol, index);
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
                .get_symbol(entry.symbol)
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
            map.serialize_entry(entry.name.as_ref(), &entry.value)?;
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
