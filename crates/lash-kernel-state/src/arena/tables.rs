//! Maps, sets and records in the arena: entries in insertion order as key
//! and value pairs in one resident chunk, with an index beside them.
//!
//! Each entry keeps its insertion sequence (`K-ITER-003`): a loop's cursor
//! in a table is the sequence of the entry it last visited, so a removal
//! never moves a cursor. Sequences ascend with position and are the
//! heap's row ordinals, so a projected table keeps them.

use std::collections::HashMap;

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Objects, Table};
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{NativeError, Value};

use super::Facts;
use super::keys::normal_key;

/// A resident table's key index and its entries' insertion sequences.
#[derive(Debug, Default)]
pub struct Index {
    /// Each key's normal form, to its entry's sequence. Never iterated:
    /// only looked up.
    keys: HashMap<NormalKey, u64>,
    /// The sequence of the entry at each position, ascending.
    seqs: Vec<u64>,
    /// The sequence the next new entry takes.
    next: u64,
}

impl Index {
    /// The position of the entry of sequence `seq`.
    fn position(&self, seq: u64) -> Option<usize> {
        self.seqs.binary_search(&seq).ok()
    }

    /// The position of the entry under `key`'s normal form.
    pub fn find(&self, key: &NormalKey) -> Option<usize> {
        self.keys.get(key).and_then(|seq| self.position(*seq))
    }

    /// The entries' sequences in position order.
    pub fn sequences(&self) -> &[u64] {
        &self.seqs
    }

    /// The sequence the next new entry takes.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// The position of the first entry after sequence `after`.
    pub fn after(&self, after: Option<u64>) -> usize {
        match after {
            None => 0,
            Some(seq) => self.seqs.partition_point(|at| *at <= seq),
        }
    }

    pub(super) fn resequence(&mut self, seqs: Vec<u64>, next: u64) -> Result<(), NativeError> {
        if seqs.len() != self.seqs.len()
            || seqs.windows(2).any(|pair| pair[0] >= pair[1])
            || seqs.last().is_some_and(|last| *last >= next)
        {
            return Err(NativeError::Malformed(
                "a table's sequences are not ascending below its next",
            ));
        }
        for seq in self.keys.values_mut() {
            *seq = seqs[*seq as usize];
        }
        self.seqs = seqs;
        self.next = next;
        Ok(())
    }
}

/// The values of a resident table: pairs, key then value.
pub(crate) fn pairs(table: &Table) -> Result<&[Value], NativeError> {
    match table.chunks.as_slice() {
        [] => Ok(&[]),
        [Chunk::Resident(values)] => Ok(values),
        _ => Err(NativeError::Malformed("a table is not one resident chunk")),
    }
}

fn pairs_mut(table: &mut Table) -> Result<&mut Vec<Value>, NativeError> {
    if table.chunks.is_empty() {
        table.chunks.push(Chunk::Resident(Vec::new()));
    }
    match table.chunks.as_mut_slice() {
        [Chunk::Resident(values)] => Ok(values),
        _ => Err(NativeError::Malformed("a table is not one resident chunk")),
    }
}

fn invalid_key() -> NativeError {
    NativeError::Malformed("an entry's key is not a legal key")
}

/// A table of `entries` in order. A key met again keeps its first position
/// and spelling and takes the later value (`K-KEY-003`).
pub(super) fn build(
    objects: &Objects,
    facts: &[Facts],
    entries: Vec<(Value, Value)>,
) -> Result<Table, NativeError> {
    let mut seen: HashMap<NormalKey, usize> = HashMap::with_capacity(entries.len());
    let mut values = Vec::with_capacity(entries.len() * 2);
    for (key, value) in entries {
        let normal = normal_key(objects, facts, &key)?.ok_or_else(invalid_key)?;
        match seen.get(&normal) {
            Some(at) => values[at * 2 + 1] = value,
            None => {
                seen.insert(normal, values.len() / 2);
                values.push(key);
                values.push(value);
            }
        }
    }
    Ok(Table {
        len: (values.len() / 2) as u64,
        chunks: vec![Chunk::Resident(values)],
    })
}

/// The index of a resident table whose entries take sequences from zero.
pub(super) fn index_of(
    objects: &Objects,
    facts: &[Facts],
    table: &Table,
) -> Result<Index, NativeError> {
    let values = pairs(table)?;
    let mut keys = HashMap::with_capacity(values.len() / 2);
    for (at, key) in values.iter().step_by(2).enumerate() {
        let normal = normal_key(objects, facts, key)?.ok_or_else(invalid_key)?;
        keys.insert(normal, at as u64);
    }
    let len = (values.len() / 2) as u64;
    Ok(Index {
        keys,
        seqs: (0..len).collect(),
        next: len,
    })
}

fn table_mut(body: &mut Body) -> Result<&mut Table, NativeError> {
    match body {
        Body::Map(table) | Body::Set(table) | Body::Record(table) => Ok(table),
        _ => Err(NativeError::Malformed("a table handle names another body")),
    }
}

impl super::Arena {
    fn table_parts(
        &mut self,
        handle: ArenaHandle,
    ) -> Result<(&mut Table, &mut Index), NativeError> {
        self.warm(handle)?;
        let (body, facts) = self.parts_mut(handle)?;
        let index = facts
            .index
            .as_deref_mut()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        Ok((table_mut(body)?, index))
    }

    /// Writes `value` under `key`, whose normal form is `normal`: an
    /// existing key keeps its first spelling and position (`K-KEY-003`).
    pub fn table_insert(
        &mut self,
        handle: ArenaHandle,
        normal: NormalKey,
        key: Value,
        value: Value,
    ) -> Result<(), NativeError> {
        let (table, index) = self.table_parts(handle)?;
        let values = pairs_mut(table)?;
        match index.find(&normal) {
            Some(at) => values[at * 2 + 1] = value,
            None => {
                let seq = index.next;
                index.next = seq
                    .checked_add(1)
                    .ok_or(NativeError::Malformed("a table's sequence overflows"))?;
                index.keys.insert(normal, seq);
                index.seqs.push(seq);
                values.push(key);
                values.push(value);
                table.len += 1;
            }
        }
        Ok(())
    }

    /// Removes the entry under `normal`, if any, and gives its value.
    pub fn table_remove(
        &mut self,
        handle: ArenaHandle,
        normal: &NormalKey,
    ) -> Result<Option<Value>, NativeError> {
        let (table, index) = self.table_parts(handle)?;
        let Some(seq) = index.keys.remove(normal) else {
            return Ok(None);
        };
        let at = index
            .position(seq)
            .ok_or(NativeError::Malformed("a table's index names no entry"))?;
        index.seqs.remove(at);
        let values = pairs_mut(table)?;
        let value = values.remove(at * 2 + 1);
        values.remove(at * 2);
        table.len -= 1;
        Ok(Some(value))
    }

    /// The entry under `normal`: its position and value.
    pub fn table_find(
        &self,
        handle: ArenaHandle,
        normal: &NormalKey,
    ) -> Result<Option<(usize, Value)>, NativeError> {
        self.warm(handle)?;
        let facts = self.facts(handle)?;
        let index = facts
            .index
            .as_deref()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        let Some(at) = index.find(normal) else {
            return Ok(None);
        };
        let table = match self.body(handle)? {
            Body::Map(table) | Body::Set(table) | Body::Record(table) => table,
            _ => return Err(NativeError::Malformed("a table handle names another body")),
        };
        Ok(Some((at, pairs(table)?[at * 2 + 1])))
    }

    /// The first entry after cursor `after`: its sequence and key
    /// (`K-ITER-003`).
    pub fn table_after(
        &self,
        handle: ArenaHandle,
        after: Option<u64>,
    ) -> Result<Option<(u64, Value)>, NativeError> {
        self.warm(handle)?;
        let index = self
            .facts(handle)?
            .index
            .as_deref()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        let at = index.after(after);
        let Some(seq) = index.seqs.get(at) else {
            return Ok(None);
        };
        let table = match self.body(handle)? {
            Body::Map(table) | Body::Set(table) | Body::Record(table) => table,
            _ => return Err(NativeError::Malformed("a table handle names another body")),
        };
        Ok(Some((*seq, pairs(table)?[at * 2])))
    }

    /// How many entries sit at or before cursor `last` (`K-ITER-002`).
    pub fn table_passed(&self, handle: ArenaHandle, last: Option<u64>) -> Result<u64, NativeError> {
        self.warm(handle)?;
        let index = self
            .facts(handle)?
            .index
            .as_deref()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        Ok(index.after(last) as u64)
    }
}
