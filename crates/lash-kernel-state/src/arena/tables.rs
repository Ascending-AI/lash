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
//!
//! A table the run built is on a shared layout ([`super::layouts`]) while
//! it can be: its index is the layout, its entries take sequences from
//! zero, and an entry's sequence is its position. Any other table, and one
//! that leaves its layout, has an index of its own.

use std::rc::Rc;

use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Chunks, Objects, ShortText, Table};
use lash_kernel_doc::{NativeError, Retry, Value};

use super::keys::{Key, key_of};
use super::layouts::{Layout, Layouts, Step};
use super::{CARD, Facts};

/// A table's key index and its entries' insertion sequences: a shared
/// layout's, or its own.
#[derive(Debug)]
pub struct Index(Repr);

#[derive(Debug)]
enum Repr {
    /// Entry `i` holds the layout's key `i` and takes sequence `i`.
    Shared(Rc<Layout>),
    Own(Box<Own>),
}

/// A table's own key index and its entries' insertion sequences.
#[derive(Debug, Default)]
struct Own {
    /// Each known key, to its entry's sequence. Never iterated: only
    /// looked up.
    keys: HashMap<Key, u64>,
    /// Keys the heap's key index holds no entry for, while a chunk is
    /// cold.
    absent: HashSet<Key>,
    /// The sequences each chunk spans while the table is chunked; 0 while
    /// it is one chunk.
    width: u64,
    /// Each chunk's resident entries' sequences, ascending. Empty while the
    /// table is dense: one chunk whose entries take sequences `0..next`,
    /// as a table the run built does until it removes one, so an entry's
    /// sequence is its position and a lookup goes straight to it.
    seqs: Vec<Vec<u64>>,
    /// The sequence the next new entry takes.
    next: u64,
}

/// Where an entry sits: its chunk and its position in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub chunk: usize,
    pub at: usize,
}

/// A short key's position, or proved absence, in one object revision.
/// Process-local only; no body reference or storage address survives a read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertySlot {
    version: (u64, u64),
    place: Option<Place>,
    key: u128,
}

impl PropertySlot {
    /// Overwrites the proved entry only while its object, revision and
    /// complete key still agree. A miss has changed nothing.
    #[inline]
    pub fn write(
        self,
        arena: &mut super::Arena,
        record: ArenaHandle,
        key: &ShortText,
        value: Value,
    ) -> Result<bool, NativeError> {
        match self.place {
            Some(place) => arena.table_write_at(record, self.version, place, key, value),
            None => Ok(false),
        }
    }
}

/// The resident entries' sequences of one chunk, ascending.
#[derive(Clone, Copy, Debug)]
pub enum Sequences<'a> {
    /// A dense index's: `0..n`.
    Dense(u64),
    Held(&'a [u64]),
}

impl<'a> Sequences<'a> {
    pub fn len(&self) -> usize {
        match self {
            Self::Dense(len) => *len as usize,
            Self::Held(seqs) => seqs.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The sequence at position `at`.
    pub fn get(&self, at: usize) -> Option<u64> {
        match self {
            Self::Dense(len) => ((at as u64) < *len).then_some(at as u64),
            Self::Held(seqs) => seqs.get(at).copied(),
        }
    }

    /// How many sequences come before the first for which `pred` fails.
    pub fn partition_point(&self, pred: impl Fn(u64) -> bool) -> usize {
        match self {
            Self::Dense(len) => {
                // `pred` holds on a prefix of `0..len`: search it.
                let (mut low, mut high) = (0u64, *len);
                while low < high {
                    let mid = low + (high - low) / 2;
                    if pred(mid) {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                low as usize
            }
            Self::Held(seqs) => seqs.partition_point(|seq| pred(*seq)),
        }
    }

    pub fn iter(self) -> impl Iterator<Item = u64> + 'a {
        (0..self.len()).filter_map(move |at| self.get(at))
    }
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

impl Own {
    /// The chunk that holds sequence `seq`.
    #[inline]
    fn chunk_of(&self, seq: u64) -> usize {
        match self.width {
            0 => 0,
            width => (seq / width) as usize,
        }
    }

    /// The resident entries' sequences of chunk `chunk`, ascending.
    fn sequences(&self, chunk: usize) -> Sequences<'_> {
        match self.seqs.get(chunk) {
            Some(seqs) => Sequences::Held(seqs),
            None if self.is_dense() && chunk == 0 => Sequences::Dense(self.next),
            None => Sequences::Held(&[]),
        }
    }

    /// Whether the index is one chunk whose entries take sequences
    /// `0..next`, which it keeps in no vector.
    #[inline]
    fn is_dense(&self) -> bool {
        self.seqs.is_empty() && self.width == 0
    }

    /// The index of a one-chunk table whose `len` entries take sequences
    /// from zero.
    fn dense(keys: HashMap<Key, u64>, len: u64) -> Self {
        Self {
            keys,
            absent: HashSet::new(),
            width: 0,
            seqs: Vec::new(),
            next: len,
        }
    }

    /// The sequences of chunk `chunk`, to edit: a dense index holds them
    /// from then on.
    fn sequences_mut(&mut self, chunk: usize) -> Option<&mut Vec<u64>> {
        if self.is_dense() {
            self.seqs = vec![(0..self.next).collect()];
        }
        self.seqs.get_mut(chunk)
    }

    /// Appends entry `seq`, the index's next, to chunk `chunk`.
    fn push(&mut self, chunk: usize, seq: u64) -> Result<(), NativeError> {
        if !(self.is_dense() && chunk == 0 && seq == self.next) {
            self.sequences_mut(chunk)
                .ok_or(NativeError::Malformed("a table's index names no chunk"))?
                .push(seq);
        }
        Ok(())
    }

    /// The sequences chunk `chunk` spans: everything while the table is one
    /// chunk.
    fn span(&self, chunk: usize) -> std::ops::Range<u64> {
        match self.width {
            0 => 0..u64::MAX,
            width => chunk as u64 * width..(chunk as u64 + 1) * width,
        }
    }

    #[inline]
    fn known(&self, key: &Key, cold: bool) -> Known {
        match self.keys.get(key) {
            Some(seq) => Known::Seq(*seq),
            None if !cold || self.absent.contains(key) => Known::Absent,
            None => Known::Unknown,
        }
    }

    /// Records what the heap's key index holds for `key`.
    fn resolved(&mut self, key: Key, seq: Option<u64>) {
        match seq {
            Some(seq) => {
                self.keys.insert(key, seq);
            }
            None => {
                self.absent.insert(key);
            }
        }
    }

    fn resequence(&mut self, seqs: Vec<u64>, next: u64) -> Result<(), NativeError> {
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
        let current: Vec<u64> = self.sequences(0).iter().collect();
        for seq in self.keys.values_mut() {
            *seq = seqs[current.partition_point(|held| held < seq)];
        }
        self.seqs = vec![seqs];
        self.next = next;
        Ok(())
    }

    /// Chunk `chunk` decoded: the sequences of its entries, ascending.
    fn decoded(&mut self, chunk: usize, seqs: Vec<u64>) -> Result<(), NativeError> {
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
    fn join(&mut self) {
        let all: Vec<u64> = std::mem::take(&mut self.seqs)
            .into_iter()
            .flatten()
            .collect();
        self.seqs = vec![all];
        self.width = 0;
        self.absent.clear();
    }
}

impl Index {
    /// The index of a large table opened from the heap: `chunks` cold
    /// chunks of `width` sequences each, below `next`.
    pub(super) fn chunked(width: u64, chunks: usize, next: u64) -> Self {
        Self(Repr::Own(Box::new(Own {
            keys: HashMap::new(),
            absent: HashSet::new(),
            width,
            seqs: vec![Vec::new(); chunks],
            next,
        })))
    }

    /// The table's own index, for an edit a layout does not make: a table
    /// on a layout leaves it for good, its sequences dense.
    fn own_mut(&mut self) -> &mut Own {
        if let Repr::Shared(layout) = &self.0 {
            self.0 = Repr::Own(Box::new(Own::dense(layout.own(), u64::from(layout.len()))));
        }
        match &mut self.0 {
            Repr::Own(own) => own,
            Repr::Shared(_) => unreachable!("a shared index was just made the table's own"),
        }
    }

    /// The sequence the next new entry takes.
    pub fn next(&self) -> u64 {
        match &self.0 {
            Repr::Shared(layout) => u64::from(layout.len()),
            Repr::Own(own) => own.next,
        }
    }

    /// The resident entries' sequences of chunk `chunk`, ascending.
    pub fn sequences(&self, chunk: usize) -> Sequences<'_> {
        match &self.0 {
            Repr::Shared(layout) if chunk == 0 => Sequences::Dense(u64::from(layout.len())),
            Repr::Shared(_) => Sequences::Held(&[]),
            Repr::Own(own) => own.sequences(chunk),
        }
    }

    /// The sequences chunk `chunk` spans: everything while the table is one
    /// chunk.
    pub fn span(&self, chunk: usize) -> std::ops::Range<u64> {
        match &self.0 {
            Repr::Shared(_) => 0..u64::MAX,
            Repr::Own(own) => own.span(chunk),
        }
    }

    /// The chunk that holds sequence `seq`.
    #[inline]
    fn chunk_of(&self, seq: u64) -> usize {
        match &self.0 {
            Repr::Shared(_) => 0,
            Repr::Own(own) => own.chunk_of(seq),
        }
    }

    /// Whether the index must ask the heap's key index for `key`: the
    /// table has a cold chunk and the index does not know the key.
    pub fn unknown(&self, key: &Key, cold: bool) -> bool {
        match &self.0 {
            Repr::Shared(_) => false,
            Repr::Own(own) => cold && matches!(own.known(key, true), Known::Unknown),
        }
    }

    /// Records what the heap's key index holds for `key`.
    pub(super) fn resolved(&mut self, key: Key, seq: Option<u64>) {
        self.own_mut().resolved(key, seq);
    }

    /// Sets the insertion sequences a table's page form keeps.
    pub(super) fn resequence(&mut self, seqs: Vec<u64>, next: u64) -> Result<(), NativeError> {
        if matches!(&self.0, Repr::Shared(layout) if next == u64::from(layout.len()))
            && seqs.len() as u64 == next
            && seqs.iter().copied().eq(0..next)
        {
            return Ok(());
        }
        self.own_mut().resequence(seqs, next)
    }

    /// Chunk `chunk` decoded: the sequences of its entries, ascending.
    pub(super) fn decoded(&mut self, chunk: usize, seqs: Vec<u64>) -> Result<(), NativeError> {
        self.own_mut().decoded(chunk, seqs)
    }

    /// The table's chunks were joined into one: its index knows every key.
    pub(super) fn join(&mut self) {
        self.own_mut().join();
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

/// A table of `entries` in order, on the layout their keys lead to. A key
/// met again keeps its first position and spelling and takes the later
/// value (`K-KEY-003`).
pub(super) fn build(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    layouts: &Layouts,
    mut entries: impl ExactSizeIterator<Item = (Value, Value)>,
) -> Result<(Table, Index), NativeError> {
    let mut values = Vec::with_capacity(entries.len() * 2);
    let mut layout = layouts.empty();
    while let Some((key, value)) = entries.next() {
        let resident = key_of(objects, facts, &key)?.ok_or_else(invalid_key)?;
        match layout.step(&resident) {
            Step::Next(next) => {
                layout = next;
                values.push(key);
                values.push(value);
            }
            Step::Held(at) => values[at as usize * 2 + 1] = value,
            Step::Full => {
                // The rest of the table is keyed by its own index, which
                // starts from the layout's keys.
                let mut keys = layout.own();
                keys.reserve(entries.len() + 1);
                let mut add = |resident: Key, key: Value, value: Value| match keys.entry(resident) {
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        values[*entry.get() as usize * 2 + 1] = value;
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert((values.len() / 2) as u64);
                        values.push(key);
                        values.push(value);
                    }
                };
                add(resident, key, value);
                for (key, value) in entries.by_ref() {
                    add(
                        key_of(objects, facts, &key)?.ok_or_else(invalid_key)?,
                        key,
                        value,
                    );
                }
                // Duplicate inputs must not leave a larger retained key
                // buffer than the final unique rows need.
                keys.shrink_to_fit();
                let table = Table::whole(values);
                let index = Index(Repr::Own(Box::new(Own::dense(keys, table.len))));
                return Ok((table, index));
            }
        }
    }
    let table = Table::whole(values);
    Ok((table, Index(Repr::Shared(layout.taken()))))
}

/// The index of a resident one-chunk table whose entries take sequences
/// from zero: the layout its keys lead to, or its own.
pub(super) fn index_of(
    objects: &Objects,
    facts: &[Facts],
    layouts: &Layouts,
    object: ArenaHandle,
    table: &Table,
) -> Result<Index, NativeError> {
    let values = pairs(object, table)?;
    let mut layout = Some(layouts.empty());
    let mut resident = Vec::with_capacity(values.len() / 2);
    for key in values.iter().step_by(2) {
        let key = key_of(objects, facts, key)?.ok_or_else(invalid_key)?;
        layout = layout.and_then(|layout| match layout.step(&key) {
            Step::Next(next) => Some(next),
            Step::Held(_) | Step::Full => None,
        });
        resident.push(key);
    }
    if let Some(layout) = layout {
        return Ok(Index(Repr::Shared(layout.taken())));
    }
    let mut keys = HashMap::with_capacity(resident.len());
    for (at, key) in resident.into_iter().enumerate() {
        keys.insert(key, at as u64);
    }
    Ok(Index(Repr::Own(Box::new(Own::dense(
        keys,
        (values.len() / 2) as u64,
    )))))
}

#[inline]
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

/// The entry under `key` of a table, by its index: where it sits, or
/// `None`. On a layout, or on a dense index of its own, an entry's
/// sequence is its position. A key held by a cold chunk, or that only a
/// cold chunk may hold, answers [`Retry::Cold`].
/// Keep the dense lookup with its caller: outlining the hash lookup in
/// one downstream copy adds a second call on every resident key read.
#[inline(always)]
pub(crate) fn find(
    object: ArenaHandle,
    table: &Table,
    index: &Index,
    key: &Key,
) -> Result<Option<Place>, NativeError> {
    let own = match &index.0 {
        Repr::Shared(layout) => {
            return Ok(layout.position(key).map(|at| Place {
                chunk: 0,
                at: at as usize,
            }));
        }
        Repr::Own(own) => own,
    };
    if own.is_dense() {
        return Ok(own.keys.get(key).map(|seq| Place {
            chunk: 0,
            at: *seq as usize,
        }));
    }
    let cold = table.chunks.first_cold();
    match own.known(key, cold.is_some()) {
        Known::Seq(seq) => {
            let chunk = own.chunk_of(seq);
            resident(object, table, chunk)?;
            let seqs = own.sequences(chunk);
            let at = seqs.partition_point(|held| held < seq);
            if seqs.get(at) != Some(seq) {
                return Err(NativeError::Malformed("a table's index names no entry"));
            }
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
#[inline(always)]
pub(crate) fn pair(table: &Table, place: Place) -> Result<&[Value], NativeError> {
    table
        .chunks
        .resident(place.chunk)
        .and_then(|values| values.get(place.at * 2..place.at * 2 + 2))
        .ok_or(NativeError::Malformed("a table's index names no entry"))
}

impl super::Arena {
    /// The body and index of `handle`, for a structural edit: one that
    /// adds, removes or moves an entry, which revokes every position proof
    /// of the table ([`Self::table_version`]).
    fn table_parts(
        &mut self,
        handle: ArenaHandle,
    ) -> Result<(&mut Table, &mut Index), NativeError> {
        self.warm(handle)?;
        let (body, facts) = self.parts_mut(handle)?;
        let index = facts
            .index
            .as_mut()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        Ok((table_mut(body)?, index))
    }

    /// The body and index of the resident table `handle`, and its facts.
    #[inline]
    fn table_facts(&self, handle: ArenaHandle) -> Result<(&Table, &Index, &Facts), NativeError> {
        let (body, facts) = self.table_body(handle)?;
        let index = facts
            .index
            .as_ref()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        Ok((table_of(body)?, index, facts))
    }

    #[inline]
    fn table_body(&self, handle: ArenaHandle) -> Result<(&Body, &Facts), NativeError> {
        let (Some(body), Some(facts)) = (
            self.objects.get(handle),
            self.facts.get(handle.index() as usize),
        ) else {
            return Err(NativeError::Malformed("a stale handle reached the machine"));
        };
        if facts.cold {
            return Err(Retry::Cold {
                object: handle,
                chunk: 0,
            }
            .into());
        }
        Ok((body, facts))
    }

    #[inline]
    pub(super) fn table_view(&self, handle: ArenaHandle) -> Result<(&Table, &Index), NativeError> {
        let (table, index, _) = self.table_facts(handle)?;
        Ok((table, index))
    }

    pub(crate) fn logical_entry(
        &self,
        handle: ArenaHandle,
        key: &Value,
    ) -> Result<Option<(Value, Value)>, NativeError> {
        let key = self.key(key)?;
        let (table, index) = self.table_view(handle)?;
        find(handle, table, index, &key)?
            .map(|place| pair(table, place).map(|pair| (pair[0], pair[1])))
            .transpose()
    }

    /// Writes `value` over the entry under `key`, if the table holds one:
    /// the key keeps its first spelling and position (`K-KEY-003`). A
    /// value write moves no entry, so the table's position proofs stand.
    /// Gives the entry's sequence, or `None`, having written nothing,
    /// where the table holds no `key`.
    pub fn table_overwrite(
        &mut self,
        handle: ArenaHandle,
        key: &Key,
        value: Value,
    ) -> Result<Option<u64>, NativeError> {
        let (Some(body), Some(facts)) = (
            self.objects.get_mut(handle),
            self.facts.get(handle.index() as usize),
        ) else {
            return Err(NativeError::Malformed("a stale handle reached the machine"));
        };
        if facts.cold {
            return Err(Retry::Cold {
                object: handle,
                chunk: 0,
            }
            .into());
        }
        let index = facts
            .index
            .as_ref()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        let set = matches!(body, Body::Set(_));
        let table = table_mut(body)?;
        let Some(place) = find(handle, table, index, key)? else {
            return Ok(None);
        };
        let seq = index
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
        let held = values
            .get_mut(place.at * 2 + 1)
            .ok_or(NativeError::Malformed("a table's index names no entry"))?;
        let old = std::mem::replace(held, value);
        let facts = self.facts_mut(handle)?;
        facts.content_revision = facts.content_revision.saturating_add(1);
        if !set {
            self.update_flat(handle, Some(&old), Some(&value));
        }
        self.hold(&value);
        self.unhold(&old);
        self.note_member(handle, Some(old), Some(value))?;
        Ok(Some(seq))
    }

    /// Writes `value` over the entry at `place` where the short text `key`
    /// keys it in the table's `version` ([`Self::table_version`]), as
    /// [`Self::table_write`] writes over that entry, without finding
    /// it. Gives `false`, having written nothing, where the table is cold
    /// or no longer that version, or no resident entry at `place` is keyed
    /// `key`.
    #[inline(always)]
    pub fn table_write_at(
        &mut self,
        handle: ArenaHandle,
        version: (u64, u64),
        place: Place,
        key: &ShortText,
        value: Value,
    ) -> Result<bool, NativeError> {
        let (Some(body), Some(facts)) = (
            self.objects.get_mut(handle),
            self.facts.get_mut(handle.index() as usize),
        ) else {
            return Err(NativeError::Malformed("a stale handle reached the machine"));
        };
        if facts.cold || !Self::proves(facts, version) {
            return Ok(false);
        }
        let Some(seq) = facts
            .index
            .as_ref()
            .and_then(|index| index.sequences(place.chunk).get(place.at))
        else {
            return Ok(false);
        };
        let set = matches!(body, Body::Set(_));
        let Some(Chunk::Resident(values)) = table_mut(body)?.chunks.get_mut(place.chunk) else {
            return Ok(false);
        };
        let Some([Value::Text(held_key), held]) = values.get_mut(place.at * 2..place.at * 2 + 2)
        else {
            return Ok(false);
        };
        if held_key != key {
            return Ok(false);
        }
        let old = std::mem::replace(held, value);
        // Equal-size scalar replacements change neither the flat size nor
        // holding edges. The dirty card still records the new value.
        let unchanged = match (old, value) {
            (Value::Float(_), Value::Float(_)) => true,
            (Value::Int(old), Value::Int(new)) => (old == 0) == (new == 0),
            _ => false,
        };
        if unchanged {
            Self::mark_resident(facts, &mut self.dirty, handle, Some(seq / CARD));
            self.note_member(handle, Some(old), Some(value))?;
            return Ok(true);
        }
        if !set {
            self.update_flat(handle, Some(&old), Some(&value));
        }
        self.hold(&value);
        self.unhold(&old);
        self.mark(handle, Some(seq / CARD))?;
        self.note_member(handle, Some(old), Some(value))?;
        Ok(true)
    }

    /// Writes `value` under `key`, whose resident key is `resident` and
    /// which the table does not hold, as its last entry: a table on a
    /// layout moves to the layout `resident` leads to, when a table built
    /// one. Gives the entry's sequence.
    pub fn table_append(
        &mut self,
        handle: ArenaHandle,
        resident: Key,
        key: Value,
        value: Value,
    ) -> Result<u64, NativeError> {
        let record = matches!(self.body(handle)?, Body::Record(_));
        let flat = self.facts(handle)?.flat;
        let (table, index) = self.table_parts(handle)?;
        let seq = index.next();
        let chunk = index.chunk_of(seq);
        let joined = match &index.0 {
            Repr::Shared(layout) => layout.next(&resident),
            Repr::Own(_) => None,
        };
        let own = match joined {
            Some(next) => {
                index.0 = Repr::Shared(next);
                None
            }
            None => Some(index.own_mut()),
        };
        if let Some(own) = own {
            if let Chunks::Split(chunks) = &mut table.chunks
                && chunk >= chunks.len()
            {
                chunks.resize_with(chunk + 1, || Chunk::Resident(Vec::new()));
                own.seqs.resize_with(chunk + 1, Vec::new);
            }
            let next = seq
                .checked_add(1)
                .ok_or(NativeError::Malformed("a table's sequence overflows"))?;
            own.push(chunk, seq)?;
            own.next = next;
            if own.width != 0 {
                own.absent.remove(&resident);
            }
            own.keys.insert(resident, seq);
        }
        if !table.chunks.push_inline(key) {
            let values = resident_mut(handle, table, chunk)?;
            values.push(key);
            values.push(value);
        } else if !table.chunks.push_inline(value) {
            resident_mut(handle, table, chunk)?.push(value);
        }
        table.len += 1;
        self.facts_mut(handle)?.flat = flat;
        self.update_table_flat(handle, None, Some((&key, &value)));
        self.hold(&key);
        self.hold(&value);
        self.note_entry(
            handle,
            None,
            Some(super::member_events::MemberEntry::Pair { key, value, record }),
        )?;
        Ok(seq)
    }

    /// Removes the entry under `key`, if any, and gives its sequence and
    /// value. A table on a layout leaves it.
    pub fn table_remove(
        &mut self,
        handle: ArenaHandle,
        key: &Key,
    ) -> Result<Option<(u64, Value)>, NativeError> {
        let record = matches!(self.body(handle)?, Body::Record(_));
        let flat = self.facts(handle)?.flat;
        let (table, index) = self.table_parts(handle)?;
        let Some(place) = find(handle, table, index, key)? else {
            return Ok(None);
        };
        let own = index.own_mut();
        let values = resident_mut(handle, table, place.chunk)?;
        let value = values.remove(place.at * 2 + 1);
        let removed = values.remove(place.at * 2);
        let seq = own
            .sequences_mut(place.chunk)
            .ok_or(NativeError::Malformed("a table's index names no chunk"))?
            .remove(place.at);
        own.keys.remove(key);
        if own.width != 0 {
            own.absent.insert(key.clone());
        }
        table.len -= 1;
        self.facts_mut(handle)?.flat = flat;
        self.update_table_flat(handle, Some((&removed, &value)), None);
        self.unhold(&removed);
        self.unhold(&value);
        self.note_entry(
            handle,
            Some(super::member_events::MemberEntry::Pair {
                key: removed,
                value,
                record,
            }),
            None,
        )?;
        Ok(Some((seq, value)))
    }

    /// The entry under `key`: where it sits and its value.
    #[inline]
    pub fn table_find(
        &self,
        handle: ArenaHandle,
        key: &Key,
    ) -> Result<Option<(Place, Value)>, NativeError> {
        let (table, index) = self.table_view(handle)?;
        let Some(place) = find(handle, table, index, key)? else {
            return Ok(None);
        };
        Ok(Some((place, pair(table, place)?[1])))
    }

    /// The value the table `handle` holds under the short text `text`,
    /// read as a native's call reads it through the table's view and the
    /// arena's lookup (`K-KEY-002`): a short text is its own key, so the
    /// read normalizes and hashes nothing. A table with a cold chunk
    /// answers [`Retry::Cold`], as its view does.
    #[inline]
    pub fn table_get_text(
        &self,
        handle: ArenaHandle,
        text: &ShortText,
    ) -> Result<Option<Value>, NativeError> {
        let table = table_of(
            self.objects
                .get(handle)
                .ok_or(NativeError::Malformed("a stale handle reached the arena"))?,
        )?;
        if let Some(chunk) = table.chunks.first_cold() {
            return Err(Retry::Cold {
                object: handle,
                chunk,
            }
            .into());
        }
        let index = self
            .facts
            .get(handle.index() as usize)
            .and_then(|facts| facts.index.as_ref())
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        let Some(place) = find(handle, table, index, &Key::text(text))? else {
            return Ok(None);
        };
        Ok(Some(pair(table, place)?[1]))
    }

    /// A process-local object identity and layout revision: what a position
    /// proof is guarded by. Every structural edit of the body revokes it,
    /// including changes of kind, chunk layout and key positions; a value
    /// written over an entry keeps it. Saturation disables caching rather
    /// than reusing a revision.
    #[inline]
    pub fn table_version(&self, handle: ArenaHandle) -> Result<Option<(u64, u64)>, NativeError> {
        let (_, _, facts) = self.table_facts(handle)?;
        Ok((facts.revision != u64::MAX).then_some((facts.token, facts.revision)))
    }

    /// Whether `handle` is still the resident table of `version`
    /// ([`Self::table_version`]), so that the position proofs `version`
    /// guards still hold: the facts alone answer, without the body.
    #[inline]
    pub fn table_is(&self, handle: ArenaHandle, version: (u64, u64)) -> bool {
        self.facts
            .get(handle.index() as usize)
            .is_some_and(|facts| !facts.cold && Self::proves(facts, version))
    }

    /// Whether `facts` are of the object and revision `version` names,
    /// unsaturated.
    #[inline(always)]
    fn proves(facts: &Facts, version: (u64, u64)) -> bool {
        facts.revision != u64::MAX && (facts.token, facts.revision) == version
    }

    /// Reads a short key through its guarded position or absence. One
    /// generation-checked resident view supplies both the proof check and
    /// the index fallback. A changed object or revision must match the
    /// complete entry key before reusing a position; equal lengths prove
    /// nothing. Cold chunks take the same resolver as an uncached read.
    #[inline]
    pub fn table_read(
        &self,
        handle: ArenaHandle,
        slot: Option<PropertySlot>,
        key: &Key,
    ) -> Result<(Option<Value>, Option<PropertySlot>), NativeError> {
        let (body, facts) = self.table_body(handle)?;
        let version = (facts.revision != u64::MAX).then_some((facts.token, facts.revision));
        let short = match key {
            Key::Text(low, high) => Some(u128::from(*high) << 64 | u128::from(*low)),
            _ => None,
        };
        // The proof's object generation and structural revision still
        // agree. A proved absence needs neither the table nor its index.
        if let (Some(slot), Some(word)) = (slot, short)
            && slot.place.is_none()
            && slot.key == word
            && version == Some(slot.version)
        {
            return Ok((None, Some(slot)));
        }
        let index = facts
            .index
            .as_ref()
            .ok_or(NativeError::Malformed("a resident table has no index"))?;
        let table = table_of(body)?;
        if let (Some(slot), Some(word)) = (slot, short)
            && slot.key == word
        {
            let current = version == Some(slot.version);
            match slot.place {
                None => {}
                Some(place) => {
                    if let Some(pair) = table
                        .chunks
                        .resident(place.chunk)
                        .and_then(|values| values.get(place.at * 2..place.at * 2 + 2))
                        && (current || matches!(pair[0], Value::Text(key) if key.word() == word))
                    {
                        return Ok((
                            Some(pair[1]),
                            version.map(|version| PropertySlot { version, ..slot }),
                        ));
                    }
                }
            }
        }
        let place = find(handle, table, index, key)?;
        let value = place
            .map(|place| pair(table, place).map(|pair| pair[1]))
            .transpose()?;
        let slot = short.zip(version).map(|(key, version)| PropertySlot {
            version,
            place,
            key,
        });
        Ok((value, slot))
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
            let at = seqs.partition_point(|seq| seq < start);
            if let Some(seq) = seqs.get(at) {
                return Ok(Some((seq, values[at * 2])));
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
                seqs.partition_point(|seq| seq <= last)
            } else {
                seqs.len()
            } as u64;
        }
        Ok(passed)
    }
}

#[cfg(test)]
mod tests {
    use lash_kernel_doc::{Float, NewObject, Value};

    use super::super::Arena;
    use super::super::layouts::MAX_KEYS;
    use super::*;

    fn map(arena: &mut Arena, entries: Vec<(Value, Value)>) -> ArenaHandle {
        match arena.create(NewObject::Map(entries)).unwrap() {
            Value::Map(handle) => handle,
            _ => panic!("a map"),
        }
    }

    fn layout(arena: &Arena, handle: ArenaHandle) -> Option<*const Layout> {
        match &arena.facts(handle).unwrap().index.as_ref().unwrap().0 {
            Repr::Shared(layout) => Some(Rc::as_ptr(layout)),
            Repr::Own(_) => None,
        }
    }

    fn keys(arena: &Arena, handle: ArenaHandle) -> Vec<String> {
        let mut keys = Vec::new();
        let mut after = None;
        while let Some((seq, key)) = arena.table_after(handle, after).unwrap() {
            keys.push(match key {
                Value::Int(word) => format!("{seq}:{word}"),
                Value::Float(float) => format!("{seq}:{:.1}", float.get()),
                other => format!("{seq}:{other:?}"),
            });
            after = Some(seq);
        }
        keys
    }

    /// K-KEY-002, K-KEY-003: a read by a short text, and the general
    /// lookup, answer the value written under each key, and none for a
    /// key the table does not hold, on every index: a layout that
    /// compares its keys, one that hashes them, a table's own past the
    /// layouts' bound and after a removal, and a set's.
    #[test]
    fn short_text_reads_find_each_key_on_every_index() {
        let mut arena = Arena::new();
        let names: Vec<String> = (0..=MAX_KEYS).map(|at| format!("key-{at}")).collect();
        let texts: Vec<Value> = names.iter().map(|name| arena.text(name)).collect();
        let mut tables = Vec::new();
        for len in 1..=texts.len() {
            let entries = (0..len)
                .map(|at| (texts[at], Value::Int(at as i64)))
                .collect();
            let table = map(&mut arena, entries);
            assert_eq!(layout(&arena, table).is_some(), len <= MAX_KEYS as usize);
            tables.push((table, (0..len).collect::<Vec<_>>()));
        }
        let (removed, held) = &mut tables[2];
        arena.table_delete(*removed, &texts[0]).unwrap();
        held.remove(0);
        let set = match arena.create(NewObject::Set(vec![texts[1]])).unwrap() {
            Value::Set(handle) => handle,
            _ => panic!("a set"),
        };
        let absent = Value::Text(ShortText::new("absent").unwrap());
        for (table, held) in &tables {
            for (at, text) in texts.iter().chain([&absent]).enumerate() {
                let Value::Text(short) = text else {
                    panic!("a short text")
                };
                let expected = held.contains(&at).then_some(at as i64);
                for found in [
                    arena.table_get_text(*table, short).unwrap(),
                    arena.table_get(*table, text).unwrap(),
                ] {
                    let found = found.map(|value| match value {
                        Value::Int(word) => word,
                        other => panic!("{other:?}"),
                    });
                    assert_eq!(found, expected, "key {at} of {}", held.len());
                }
            }
        }
        for (at, text) in texts.iter().chain([&absent]).enumerate() {
            let Value::Text(short) = text else {
                panic!("a short text")
            };
            let found = arena.table_get_text(set, short).unwrap();
            assert_eq!(found.is_some(), at == 1);
            assert_eq!(arena.table_get(set, text).unwrap().is_some(), at == 1);
        }
    }

    /// K-KEY-003, K-ITER-003: tables on one layout keep their own first
    /// spellings and later values; one that removes an entry leaves the
    /// layout with its cursors' sequences, and a key written again takes
    /// the next sequence. An append joins the layout a built table made,
    /// and a value write keeps the layout.
    #[test]
    fn tables_on_a_layout_keep_spellings_values_and_cursors() {
        let mut arena = Arena::new();
        let float = |float: f64| Value::Float(Float::new(float));
        let words = map(
            &mut arena,
            vec![
                (Value::Int(1), Value::Int(10)),
                (Value::Int(2), Value::Int(20)),
            ],
        );
        let floats = map(
            &mut arena,
            vec![
                (float(1.0), Value::Int(11)),
                (float(2.0), Value::Int(21)),
                (Value::Int(1), Value::Int(12)),
            ],
        );
        assert!(layout(&arena, words).is_some());
        assert_eq!(layout(&arena, words), layout(&arena, floats));
        // A reload with unchanged dense sequences keeps the shared layout.
        let loaded = map(
            &mut arena,
            vec![(float(1.0), Value::Int(11)), (float(2.0), Value::Int(12))],
        );
        arena.set_sequences(loaded, vec![0, 1], 2).unwrap();
        assert_eq!(layout(&arena, loaded), layout(&arena, floats));
        assert_eq!(keys(&arena, loaded), ["0:1.0", "1:2.0"]);
        // A deletion gap must keep its stored cursor sequences instead.
        arena.set_sequences(loaded, vec![1, 3], 4).unwrap();
        assert_eq!(layout(&arena, loaded), None);
        assert_eq!(keys(&arena, loaded), ["1:1.0", "3:2.0"]);
        assert_eq!(keys(&arena, floats), ["0:1.0", "1:2.0"]);
        assert!(matches!(
            arena.table_get(floats, &Value::Int(1)).unwrap(),
            Some(Value::Int(12))
        ));
        let position_version = arena.table_version(words).unwrap();
        let content_version = arena.content_version(words).unwrap();
        assert!(arena.content_is(&content_version));
        let mut foreign = Arena::new();
        map(
            &mut foreign,
            vec![
                (Value::Int(1), Value::Int(10)),
                (Value::Int(2), Value::Int(20)),
            ],
        );
        assert!(
            !foreign.content_is(&content_version),
            "facts cannot cross arenas"
        );
        arena
            .table_write(words, float(2.0), Value::Int(22))
            .unwrap();
        assert_eq!(
            arena.table_version(words).unwrap(),
            position_version,
            "replacing a value preserves entry positions"
        );
        assert_ne!(
            arena.content_version(words).unwrap(),
            content_version,
            "ledger facts expire when a value changes at the same position"
        );
        assert!(!arena.content_is(&content_version));
        assert_eq!(layout(&arena, words), layout(&arena, floats));
        assert_eq!(keys(&arena, words), ["0:1", "1:2"]);
        assert!(matches!(
            arena.table_get(words, &Value::Int(2)).unwrap(),
            Some(Value::Int(22))
        ));

        arena.table_delete(words, &Value::Int(1)).unwrap();
        assert_eq!(layout(&arena, words), None);
        assert_eq!(keys(&arena, words), ["1:2"]);
        arena
            .table_write(words, Value::Int(1), Value::Int(13))
            .unwrap();
        assert_eq!(keys(&arena, words), ["1:2", "2:1"]);
        assert!(matches!(
            arena.table_get(floats, &float(2.0)).unwrap(),
            Some(Value::Int(21))
        ));

        let one = map(&mut arena, vec![(Value::Int(1), Value::Null)]);
        arena.table_write(one, Value::Int(2), Value::Null).unwrap();
        assert_eq!(layout(&arena, one), layout(&arena, floats));
        let other = map(&mut arena, vec![(Value::Int(1), Value::Null)]);
        arena
            .table_write(other, Value::Int(3), Value::Null)
            .unwrap();
        assert_eq!(layout(&arena, other), None, "an append makes no layout");
        assert_eq!(keys(&arena, other), ["0:1", "1:3"]);
    }
}
