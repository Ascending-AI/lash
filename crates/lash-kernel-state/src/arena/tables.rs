//! Maps, sets and records in the arena: entries in insertion order as key
//! and value pairs, with an index beside them.
//!
//! Each entry keeps its insertion sequence (`K-ITER-003`): a loop's cursor
//! in a table is the sequence of the entry it last visited, so a removal
//! never moves a cursor. Sequences ascend with position and are the
//! heap's row ordinals, so a projected table keeps them.
//!
//! A table the run built, or whose chunks are all resident, is one chunk.
//! A large table opened from the heap is chunked by sequence: chunk `k`
//! holds the entries whose sequences are `k * width ..`, each cold until a
//! read reaches it (seam D). Its index then knows the keys of its resident
//! chunks, and the keys the machine looked up in the heap's key index:
//! present ones with their sequence, absent ones as absent. A key it does
//! not know is answered by decoding the table's cold chunks.

use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Chunks, Objects, Table};
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{NativeError, Retry, Value};

use super::Facts;
use super::keys::normal_key;

/// A table's key index and its entries' insertion sequences.
#[derive(Debug, Default)]
pub struct Index {
    /// Each known key's normal form, to its entry's sequence. Never
    /// iterated: only looked up.
    keys: HashMap<NormalKey, u64>,
    /// Keys the heap's key index holds no entry for, while a chunk is
    /// cold.
    absent: HashSet<NormalKey>,
    /// The sequences each chunk spans while the table is chunked; 0 while
    /// it is one chunk.
    width: u64,
    /// Each chunk's resident entries' sequences, ascending. Empty while the
    /// table is one chunk whose entries take sequences `0..next`, at most
    /// [`DENSE`]'s, as a table the run built does until it removes one.
    seqs: Vec<Vec<u64>>,
    /// The sequence the next new entry takes.
    next: u64,
}

/// The sequences of a dense index's entries.
static DENSE: [u64; 64] = {
    let mut seqs = [0; 64];
    let mut seq = 0;
    while seq < seqs.len() {
        seqs[seq] = seq as u64;
        seq += 1;
    }
    seqs
};

/// Where an entry sits: its chunk and its position in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub chunk: usize,
    pub at: usize,
}

/// What an index knows of a key.
enum Known {
    /// The entry of this sequence holds it.
    Seq(u64),
    /// No entry holds it.
    Absent,
    /// A cold chunk may hold it.
    Unknown,
}

impl Index {
    /// The chunk that holds sequence `seq`.
    fn chunk_of(&self, seq: u64) -> usize {
        match self.width {
            0 => 0,
            width => (seq / width) as usize,
        }
    }

    /// The sequence the next new entry takes.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// The resident entries' sequences of chunk `chunk`, ascending.
    pub fn sequences(&self, chunk: usize) -> &[u64] {
        match self.seqs.get(chunk) {
            Some(seqs) => seqs,
            None if self.is_dense() && chunk == 0 => &DENSE[..self.next as usize],
            None => &[],
        }
    }

    /// Whether the index is one chunk whose entries take sequences
    /// `0..next`, which it keeps in no vector.
    fn is_dense(&self) -> bool {
        self.seqs.is_empty() && self.width == 0
    }

    /// The index of a one-chunk table whose `len` entries take sequences
    /// from zero.
    fn dense(keys: HashMap<NormalKey, u64>, len: u64) -> Self {
        Self {
            keys,
            absent: HashSet::new(),
            width: 0,
            seqs: if len as usize <= DENSE.len() {
                Vec::new()
            } else {
                vec![(0..len).collect()]
            },
            next: len,
        }
    }

    /// The sequences of chunk `chunk`, to edit.
    fn sequences_mut(&mut self, chunk: usize) -> Option<&mut Vec<u64>> {
        if self.is_dense() {
            self.seqs = vec![DENSE[..self.next as usize].to_vec()];
        }
        self.seqs.get_mut(chunk)
    }

    /// Appends entry `seq`, the index's next, to chunk `chunk`.
    fn push(&mut self, chunk: usize, seq: u64) -> Result<(), NativeError> {
        if !(self.is_dense() && chunk == 0 && seq == self.next && seq < DENSE.len() as u64) {
            self.sequences_mut(chunk)
                .ok_or(NativeError::Malformed("a table's index names no chunk"))?
                .push(seq);
        }
        Ok(())
    }

    /// The sequences chunk `chunk` spans: everything while the table is one
    /// chunk.
    pub fn span(&self, chunk: usize) -> std::ops::Range<u64> {
        match self.width {
            0 => 0..u64::MAX,
            width => chunk as u64 * width..(chunk as u64 + 1) * width,
        }
    }

    fn known(&self, key: &NormalKey, cold: bool) -> Known {
        match self.keys.get(key) {
            Some(seq) => Known::Seq(*seq),
            None if !cold || self.absent.contains(key) => Known::Absent,
            None => Known::Unknown,
        }
    }

    /// Whether the index must ask the heap's key index for `key`: the
    /// table has a cold chunk and the index does not know the key.
    pub fn unknown(&self, key: &NormalKey, cold: bool) -> bool {
        cold && matches!(self.known(key, true), Known::Unknown)
    }

    /// Records what the heap's key index holds for `key`.
    pub(super) fn resolved(&mut self, key: NormalKey, seq: Option<u64>) {
        match seq {
            Some(seq) => {
                self.keys.insert(key, seq);
            }
            None => {
                self.absent.insert(key);
            }
        }
    }

    pub(super) fn resequence(&mut self, seqs: Vec<u64>, next: u64) -> Result<(), NativeError> {
        let held = self.sequences(0).len();
        if self.width != 0
            || self.seqs.len() > 1
            || seqs.len() != held
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
        self.seqs = vec![seqs];
        self.next = next;
        Ok(())
    }

    /// Chunk `chunk` decoded: the sequences of its entries, ascending.
    pub(super) fn decoded(&mut self, chunk: usize, seqs: Vec<u64>) -> Result<(), NativeError> {
        let span = self.span(chunk);
        if seqs.windows(2).any(|pair| pair[0] >= pair[1])
            || seqs.iter().any(|seq| !span.contains(seq))
        {
            return Err(NativeError::Malformed(
                "a table chunk's sequences are outside its span",
            ));
        }
        let held = self
            .seqs
            .get_mut(chunk)
            .ok_or(NativeError::Malformed("a decoded table chunk is no chunk"))?;
        *held = seqs;
        Ok(())
    }

    /// The table's chunks were joined into one: its index knows every key.
    pub(super) fn join(&mut self) {
        let all: Vec<u64> = std::mem::take(&mut self.seqs)
            .into_iter()
            .flatten()
            .collect();
        self.seqs = vec![all];
        self.width = 0;
        self.absent.clear();
    }

    /// The index of a large table opened from the heap: `chunks` cold
    /// chunks of `width` sequences each, below `next`.
    pub(super) fn chunked(width: u64, chunks: usize, next: u64) -> Self {
        Self {
            keys: HashMap::new(),
            absent: HashSet::new(),
            width,
            seqs: vec![Vec::new(); chunks],
            next,
        }
    }
}

/// The chunk width of a large table of `len` entries below `next`: a power
/// of two of at least 64 sequences, wide enough that a chunk holds 64
/// entries on average however many entries were removed.
pub(super) fn width(len: u64, next: u64) -> u64 {
    let spread = next
        .saturating_mul(64)
        .checked_div(len.max(1))
        .unwrap_or(64);
    spread
        .max(64)
        .checked_next_power_of_two()
        .unwrap_or(1 << 63)
}

/// The values of a resident one-chunk table: pairs, key then value. A
/// chunked table answers [`Retry::Cold`] for its first cold chunk, or for
/// chunk 0 when every chunk is resident: the machine decodes, and joins
/// the chunks into one.
pub(crate) fn pairs(object: ArenaHandle, table: &Table) -> Result<&[Value], NativeError> {
    table.chunks.values().ok_or_else(|| {
        Retry::Cold {
            object,
            chunk: table.chunks.first_cold().unwrap_or(0),
        }
        .into()
    })
}

fn invalid_key() -> NativeError {
    NativeError::Malformed("an entry's key is not a legal key")
}

/// A table of `entries` in order. A key met again keeps its first position
/// and spelling and takes the later value (`K-KEY-003`).
pub(super) fn build(
    objects: &Objects,
    facts: &[Facts],
    entries: impl ExactSizeIterator<Item = (Value, Value)>,
) -> Result<(Table, Index), NativeError> {
    // The deduplication map is the table's final index. Its capacity stays
    // with its owner, rather than being discarded and rebuilt from tokens.
    let mut keys = HashMap::with_capacity(entries.len());
    let mut values = Vec::with_capacity(entries.len() * 2);
    for (key, value) in entries {
        let normal = normal_key(objects, facts, &key)?.ok_or_else(invalid_key)?;
        match keys.entry(normal) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                values[*entry.get() as usize * 2 + 1] = value;
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((values.len() / 2) as u64);
                values.push(key);
                values.push(value);
            }
        }
    }
    // Duplicate inputs must not leave a larger retained key buffer than
    // constructing the index from the final unique rows would have used.
    keys.shrink_to_fit();
    let table = Table::whole(values);
    let index = Index::dense(keys, table.len);
    Ok((table, index))
}

/// The index of a resident one-chunk table whose entries take sequences
/// from zero.
pub(super) fn index_of(
    objects: &Objects,
    facts: &[Facts],
    object: ArenaHandle,
    table: &Table,
) -> Result<Index, NativeError> {
    let values = pairs(object, table)?;
    let mut keys = HashMap::with_capacity(values.len() / 2);
    for (at, key) in values.iter().step_by(2).enumerate() {
        let normal = normal_key(objects, facts, key)?.ok_or_else(invalid_key)?;
        keys.insert(normal, at as u64);
    }
    Ok(Index::dense(keys, (values.len() / 2) as u64))
}

fn table_of(body: &Body) -> Result<&Table, NativeError> {
    match body {
        Body::Map(table) | Body::Set(table) | Body::Record(table) => Ok(table),
        _ => Err(NativeError::Malformed("a table handle names another body")),
    }
}

fn table_mut(body: &mut Body) -> Result<&mut Table, NativeError> {
    match body {
        Body::Map(table) | Body::Set(table) | Body::Record(table) => Ok(table),
        _ => Err(NativeError::Malformed("a table handle names another body")),
    }
}

fn resident(
    object: ArenaHandle,
    table: &Table,
    chunk: usize,
) -> Result<Option<&[Value]>, NativeError> {
    match table.chunks.get(chunk) {
        Some(Chunk::Resident(values)) => Ok(Some(values)),
        Some(Chunk::Cold { .. }) => Err(Retry::Cold {
            object,
            chunk: chunk as u32,
        }
        .into()),
        None => Ok(None),
    }
}

/// Chunk `chunk` of a table, to grow or shrink it.
fn resident_mut(
    object: ArenaHandle,
    table: &mut Table,
    chunk: usize,
) -> Result<&mut Vec<Value>, NativeError> {
    match table.chunks.vec_mut(chunk) {
        Some(Chunk::Resident(values)) => Ok(values),
        Some(Chunk::Cold { .. }) => Err(Retry::Cold {
            object,
            chunk: chunk as u32,
        }
        .into()),
        None => Err(NativeError::Malformed("a table's index names no chunk")),
    }
}

/// The entry under `normal` of a table, by its index: where it sits, or
/// `None`. A key held by a cold chunk, or that only a cold chunk may hold,
/// answers [`Retry::Cold`].
pub(crate) fn find(
    object: ArenaHandle,
    table: &Table,
    index: &Index,
    normal: &NormalKey,
) -> Result<Option<Place>, NativeError> {
    let cold = table.chunks.first_cold();
    match index.known(normal, cold.is_some()) {
        Known::Seq(seq) => {
            let chunk = index.chunk_of(seq);
            resident(object, table, chunk)?;
            let at = index
                .sequences(chunk)
                .binary_search(&seq)
                .map_err(|_| NativeError::Malformed("a table's index names no entry"))?;
            Ok(Some(Place { chunk, at }))
        }
        Known::Absent => Ok(None),
        Known::Unknown => Err(Retry::Cold {
            object,
            chunk: cold.unwrap_or(0),
        }
        .into()),
    }
}

/// The pair at `place` of a table.
pub(crate) fn pair(table: &Table, place: Place) -> Result<&[Value], NativeError> {
    match table.chunks.get(place.chunk) {
        Some(Chunk::Resident(values)) => values
            .get(place.at * 2..place.at * 2 + 2)
            .ok_or(NativeError::Malformed("a table's index names no entry")),
        _ => Err(NativeError::Malformed("a table's index names no entry")),
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

    pub(super) fn table_view(&self, handle: ArenaHandle) -> Result<(&Table, &Index), NativeError> {
        self.warm(handle)?;
        let index = self
            .facts(handle)?
            .index
            .as_deref()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        Ok((table_of(self.body(handle)?)?, index))
    }

    /// Writes `value` over the entry at `place`, where [`Self::table_find`]
    /// found its key: the key keeps its first spelling and position
    /// (`K-KEY-003`). Gives the entry's sequence.
    pub fn table_replace(
        &mut self,
        handle: ArenaHandle,
        place: Place,
        value: Value,
    ) -> Result<u64, NativeError> {
        let (table, index) = self.table_parts(handle)?;
        let seq = *index
            .sequences(place.chunk)
            .get(place.at)
            .ok_or(NativeError::Malformed("a table's index names no entry"))?;
        let values = match table.chunks.get_mut(place.chunk) {
            Some(Chunk::Resident(values)) => values,
            Some(Chunk::Cold { .. }) => {
                return Err(Retry::Cold {
                    object: handle,
                    chunk: place.chunk as u32,
                }
                .into());
            }
            None => return Err(NativeError::Malformed("a table's index names no chunk")),
        };
        let old = std::mem::replace(&mut values[place.at * 2 + 1], value);
        self.hold(&value);
        self.unhold(&old);
        Ok(seq)
    }

    /// Writes `value` under `key`, whose normal form is `normal` and which
    /// the table does not hold, as its last entry. Gives the entry's
    /// sequence.
    pub fn table_append(
        &mut self,
        handle: ArenaHandle,
        normal: NormalKey,
        key: Value,
        value: Value,
    ) -> Result<u64, NativeError> {
        let (table, index) = self.table_parts(handle)?;
        let seq = index.next;
        let chunk = index.chunk_of(seq);
        if let Chunks::Split(chunks) = &mut table.chunks
            && chunk >= chunks.len()
        {
            chunks.resize_with(chunk + 1, || Chunk::Resident(Vec::new()));
            index.seqs.resize_with(chunk + 1, Vec::new);
        }
        let next = seq
            .checked_add(1)
            .ok_or(NativeError::Malformed("a table's sequence overflows"))?;
        if !table.chunks.push_inline(key) {
            let values = resident_mut(handle, table, chunk)?;
            values.push(key);
            values.push(value);
        } else if !table.chunks.push_inline(value) {
            resident_mut(handle, table, chunk)?.push(value);
        }
        index.push(chunk, seq)?;
        index.next = next;
        index.absent.remove(&normal);
        index.keys.insert(normal, seq);
        table.len += 1;
        self.hold(&key);
        self.hold(&value);
        Ok(seq)
    }

    /// Removes the entry under `normal`, if any, and gives its sequence and
    /// value.
    pub fn table_remove(
        &mut self,
        handle: ArenaHandle,
        normal: &NormalKey,
    ) -> Result<Option<(u64, Value)>, NativeError> {
        let (table, index) = self.table_parts(handle)?;
        let Some(place) = find(handle, table, index, normal)? else {
            return Ok(None);
        };
        let values = resident_mut(handle, table, place.chunk)?;
        let value = values.remove(place.at * 2 + 1);
        let key = values.remove(place.at * 2);
        let seq = index
            .sequences_mut(place.chunk)
            .ok_or(NativeError::Malformed("a table's index names no chunk"))?
            .remove(place.at);
        index.keys.remove(normal);
        if index.width != 0 {
            index.absent.insert(normal.clone());
        }
        table.len -= 1;
        self.unhold(&key);
        self.unhold(&value);
        Ok(Some((seq, value)))
    }

    /// The entry under `normal`: where it sits and its value.
    pub fn table_find(
        &self,
        handle: ArenaHandle,
        normal: &NormalKey,
    ) -> Result<Option<(Place, Value)>, NativeError> {
        let (table, index) = self.table_view(handle)?;
        let Some(place) = find(handle, table, index, normal)? else {
            return Ok(None);
        };
        Ok(Some((place, pair(table, place)?[1])))
    }

    /// A process-local object identity and body revision. Every mutable body
    /// borrow invalidates it, including changes of kind, chunk layout and key
    /// positions. Saturation disables caching rather than reusing a revision.
    #[inline]
    pub fn table_version(&self, handle: ArenaHandle) -> Result<Option<(u64, u64)>, NativeError> {
        self.table_view(handle)?;
        let facts = self.facts(handle)?;
        Ok((facts.revision != u64::MAX).then_some((facts.token, facts.revision)))
    }

    /// The entry at `place`, if the table holds one there: what a read
    /// hint checks before it trusts a position.
    pub fn table_at(
        &self,
        handle: ArenaHandle,
        place: Place,
    ) -> Result<Option<&[Value]>, NativeError> {
        let (table, _) = self.table_view(handle)?;
        Ok(match table.chunks.get(place.chunk) {
            Some(Chunk::Resident(values)) => values.get(place.at * 2..place.at * 2 + 2),
            _ => None,
        })
    }

    /// The first entry after cursor `after`: its sequence and key
    /// (`K-ITER-003`).
    pub fn table_after(
        &self,
        handle: ArenaHandle,
        after: Option<u64>,
    ) -> Result<Option<(u64, Value)>, NativeError> {
        let (table, index) = self.table_view(handle)?;
        let start = after.map_or(0, |after| after.saturating_add(1));
        let mut chunk = index.chunk_of(start);
        while let Some(values) = resident(handle, table, chunk)? {
            let seqs = index.sequences(chunk);
            let at = seqs.partition_point(|seq| *seq < start);
            if let Some(seq) = seqs.get(at) {
                return Ok(Some((*seq, values[at * 2])));
            }
            chunk += 1;
        }
        Ok(None)
    }

    /// How many entries sit at or before cursor `last` (`K-ITER-002`).
    pub fn table_passed(&self, handle: ArenaHandle, last: Option<u64>) -> Result<u64, NativeError> {
        let (table, index) = self.table_view(handle)?;
        let Some(last) = last else {
            return Ok(0);
        };
        let end = index.chunk_of(last);
        let mut passed = 0u64;
        for chunk in 0..=end {
            if resident(handle, table, chunk)?.is_none() {
                break;
            }
            let seqs = index.sequences(chunk);
            passed += if chunk == end {
                seqs.partition_point(|seq| *seq <= last)
            } else {
                seqs.len()
            } as u64;
        }
        Ok(passed)
    }
}
