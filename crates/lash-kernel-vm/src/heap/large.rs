//! Large lists, tuples and tables (cutover contract §6, cold access): a
//! list, tuple or table past [`LARGE`] members, or whose row would pass
//! [`ROW_BYTES`], is written as
//! a [`Chunked`] row over index pages and chunk pages, opened by their row
//! alone, and read one chunk at a time.
//!
//! A point read of a cold large object loads its row, the index pages on
//! the chunk's path ([`lash_kernel_state::paged::run::index`]; one up to
//! 589,824 members) and one chunk. A point edit dirties one chunk: the
//! seal re-encodes the object's resident chunks, writes only those whose
//! encoding changed, rewrites the index pages on their paths and the row,
//! and adjusts the row's edge multiset by the changed chunks' references
//! alone. A table's new
//! entries wait in an overflow chunk until the seal places them by key;
//! a chunk past [`CHUNK`] entries splits by placement, and an object whose
//! chunks moved goes back to its row as a proxy, so the arena's chunks are
//! always the row's.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Chunks, Sequence, Table};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::tree::Access;
use lash_kernel_state::paged::PageKind;
use lash_kernel_state::paged::identity::chunk_leaf;
use lash_kernel_state::paged::run::chunked::{
    CHUNK, ChunkPage, ChunkRef, Chunked, EDGES, Entry, LARGE, Members, Placement, ROW_BYTES,
    check_tuple_chunk, chunk_page, placement, rewrite_edges,
};
use lash_kernel_state::paged::run::directory::Row;
use lash_kernel_state::paged::run::ids::ObjectId;
use lash_kernel_state::paged::run::index::{Index, Seek};
use lash_kernel_state::paged::run::objects::{Heads, ObjectKind, Record, RunValue};
use lash_kernel_value::tokens::{encode_body, logical_digest};

use super::seal::{fault, heads, named_immutable, run_value};
use crate::paged::SealError;

/// The entries a table's chunk holds when it is first written: room for a
/// quarter more before it splits.
const FILL: usize = CHUNK * 3 / 4;

/// What the heap knows of a chunk it read or wrote: its encoding's digest
/// and the ids its members reference, once per reference.
#[derive(Clone, Debug)]
struct Seen {
    digest: [u8; 32],
    edges: Vec<ObjectId>,
}

/// A large object the heap opened or wrote: its row and what it knows of
/// its chunks.
#[derive(Debug)]
struct Opened {
    row: Chunked,
    seen: HashMap<u32, Seen>,
    /// Each chunk's first placement, when the heap wrote every chunk.
    firsts: Option<Vec<Option<Placement>>>,
}

/// The large objects the heap opened or wrote, by id.
#[derive(Debug, Default)]
pub(crate) struct Large {
    opened: HashMap<ObjectId, Opened>,
}

/// Whether `body` is written chunked: a list, tuple or table past
/// [`LARGE`] members, one opened from its chunks, or one whose row the
/// heads of the immutables it names would make wider than [`ROW_BYTES`].
/// The seal and a loop's saved cursor both ask it, so a table's cursor is
/// saved the way the table is written.
pub(crate) fn is_large(arena: &Arena, body: &Body) -> bool {
    let (len, chunks) = match body {
        Body::List(sequence) | Body::Tuple(sequence) => (sequence.len, &sequence.chunks),
        Body::Map(table) | Body::Set(table) | Body::Record(table) => (table.len, &table.chunks),
        _ => return false,
    };
    if len > LARGE as u64 || matches!(chunks, Chunks::Split(_)) {
        return true;
    }
    chunks
        .resident(0)
        .is_some_and(|values| row_bytes(arena, values) > ROW_BYTES)
}

/// The most bytes one member value takes in a row.
const VALUE_BYTES: usize = 16;

/// The most bytes a row takes besides its values and heads.
const ROW_FRAME_BYTES: usize = 64;

/// An upper bound of the bytes the row of an object holding `values`
/// takes, from its values and the heads of the immutables they name, once
/// each; stops counting past [`ROW_BYTES`].
fn row_bytes(arena: &Arena, values: &[Value]) -> usize {
    let mut bytes = ROW_FRAME_BYTES + values.len() * VALUE_BYTES;
    let mut named = HashSet::new();
    for value in values {
        if bytes > ROW_BYTES {
            break;
        }
        let Some(handle) = named_immutable(arena, value) else {
            continue;
        };
        if !named.insert(handle.index()) {
            continue;
        }
        if let Ok(head) = arena.head(handle) {
            bytes += VALUE_BYTES + encode_body(&head).len();
        }
    }
    bytes
}

/// Calls `visit` with every value large object `row` holds, its chunks
/// read in order, until it answers `true`; gives whether one did.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Corrupt`] for an index or chunk
/// page that is not the one the row describes.
pub(crate) fn any_value(
    access: &Access<'_>,
    row: &Chunked,
    visit: &mut dyn FnMut(&RunValue) -> bool,
) -> Result<bool, HeapFault> {
    row.index.each(access, &row.shape(), &mut |at, chunk| {
        let (page, _) = chunk_page(access, chunk)?;
        check_tuple_chunk(row, at, chunk, &page)?;
        Ok(page.members.values().into_iter().any(&mut *visit))
    })
}

/// Of items ordered by their first placement, the one a placement falls
/// in: the last whose first is at most it, else the first.
fn placed_in<'a>(firsts: impl Iterator<Item = &'a Option<Placement>>, at: &Placement) -> usize {
    firsts
        .take_while(|first| first.as_ref().is_none_or(|first| first <= at))
        .count()
        .saturating_sub(1)
}

/// Every id `values` reference, once per reference, ascending.
fn edges_of<'a>(values: impl IntoIterator<Item = &'a RunValue>) -> Vec<ObjectId> {
    let mut edges: Vec<ObjectId> = values.into_iter().filter_map(RunValue::id).collect();
    edges.sort_unstable();
    edges
}

/// One chunk to write: its page encoded, not yet sealed.
struct Piece {
    first: Option<Placement>,
    len: u32,
    tokens: Vec<u8>,
    seen: Seen,
    /// A tuple chunk's content leaf.
    leaf: Option<[u8; 32]>,
}

impl Piece {
    fn new(members: Members, heads: Heads, first: Option<Placement>) -> Self {
        let edges = edges_of(members.values());
        let len = members.len() as u32;
        let tokens = encode_body(&ChunkPage { members, heads });
        let digest = logical_digest(&tokens);
        Self {
            first,
            len,
            tokens,
            seen: Seen { digest, edges },
            leaf: None,
        }
    }
}

/// What the seal does with one of the old row's chunks.
enum Outcome {
    /// Unchanged: its reference stands.
    Same,
    /// Rewritten as these pieces (none when it emptied); the first keeps
    /// the old chunk's first placement when `keep_first`.
    New {
        pieces: Vec<Piece>,
        keep_first: bool,
    },
}

/// A table entry being written, its key's placement known.
type Placed = Entry;

/// What a chunk of the table being written holds.
enum Group {
    Cold,
    Entries(Vec<Placed>),
}

impl Large {
    /// The row of the large object `id`, when the heap opened or wrote it.
    pub(crate) fn row(&self, id: ObjectId) -> Option<&Chunked> {
        self.opened.get(&id).map(|opened| &opened.row)
    }

    /// The large object `id` was opened from `row`.
    pub(crate) fn opened(&mut self, id: ObjectId, row: Chunked) {
        self.opened.insert(
            id,
            Opened {
                row,
                seen: HashMap::new(),
                firsts: None,
            },
        );
    }

    /// The object `id` is written whole from now on.
    pub(crate) fn forget(&mut self, id: ObjectId) {
        self.opened.remove(&id);
    }

    /// The chunk of the table `id` that holds `normal`'s key, if any
    /// does: one index page read per level.
    pub(crate) fn chunk_of_key(
        &self,
        access: &Access<'_>,
        id: ObjectId,
        at: &Placement,
    ) -> Result<u32, HeapFault> {
        let row = &self.opened_row(id)?;
        let (chunk, _) = row
            .index
            .get(access, &row.shape(), Seek::Key(Some(*at)))?
            .ok_or_else(|| HeapFault::Malformed(format!("large object {} has no chunk", id.0)))?;
        u32::try_from(chunk)
            .map_err(|_| HeapFault::Malformed("a large object's chunk is past a u32".into()))
    }

    fn opened_row(&self, id: ObjectId) -> Result<&Chunked, HeapFault> {
        self.row(id)
            .ok_or_else(|| HeapFault::Malformed(format!("large object {} is not open", id.0)))
    }

    /// Chunk `chunk` of the large object `id`: one index page read per
    /// level, each checked against its parent, and one chunk read.
    pub(crate) fn read(
        &mut self,
        access: &Access<'_>,
        id: ObjectId,
        chunk: u32,
    ) -> Result<ChunkPage, HeapFault> {
        let opened = self
            .opened
            .get_mut(&id)
            .ok_or_else(|| HeapFault::Malformed(format!("large object {} is not open", id.0)))?;
        let (_, held) = opened
            .row
            .index
            .get(access, &opened.row.shape(), Seek::At(u64::from(chunk)))?
            .ok_or_else(|| {
                HeapFault::Malformed(format!("large object {} has no chunk {chunk}", id.0))
            })?;
        let (page, digest) = chunk_page(access, &held)?;
        check_tuple_chunk(&opened.row, u64::from(chunk), &held, &page)?;
        let edges = edges_of(page.members.values());
        opened.seen.insert(chunk, Seen { digest, edges });
        Ok(page)
    }

    /// The row of the large object `handle` as the seal writes it: its
    /// changed chunks and their index pages written. Gives the row and
    /// whether the arena's chunks are no longer the row's.
    pub(crate) fn write(
        &mut self,
        arena: &Arena,
        handle: ArenaHandle,
        id: ObjectId,
        access: &Access<'_>,
    ) -> Result<(Row, bool), SealError> {
        let mut old = self.opened.remove(&id);
        let body = arena
            .body(handle)
            .map_err(|error| fault(error.to_string()))?;
        let (kind, outcomes, len, next, fresh_firsts) = match body {
            Body::List(sequence) | Body::Tuple(sequence) => {
                let kind = match body {
                    Body::List(_) => ObjectKind::List,
                    _ => ObjectKind::Tuple,
                };
                let outcomes =
                    list_outcomes(arena, sequence, old.as_ref(), kind == ObjectKind::Tuple)?;
                (kind, outcomes, sequence.len, sequence.len, None)
            }
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                let kind = match body {
                    Body::Map(_) => ObjectKind::Map,
                    Body::Set(_) => ObjectKind::Set,
                    _ => ObjectKind::Record,
                };
                let next = arena
                    .next_sequence(handle)
                    .map_err(|error| fault(error.to_string()))?;
                let (outcomes, firsts) =
                    table_outcomes(arena, handle, table, old.as_mut(), access)?;
                (kind, outcomes, table.len, next, firsts)
            }
            _ => return Err(fault("a large object of another kind")),
        };
        let relayout = matches!(body, Body::Map(_) | Body::Set(_) | Body::Record(_))
            && (arena
                .keyed_overflow(handle)
                .map_err(|error| fault(error.to_string()))?
                .flatten()
                .is_some()
                || outcomes.iter().any(
                    |outcome| matches!(outcome, Outcome::New { pieces, .. } if pieces.len() != 1),
                ));
        let (chunked, seen, firsts) =
            assemble(access, kind, len, next, old.as_ref(), outcomes).map_err(SealError::Heap)?;
        let firsts = fresh_firsts.or(firsts);
        self.opened.insert(
            id,
            Opened {
                row: chunked.clone(),
                seen,
                firsts,
            },
        );
        Ok((
            Row::of(Record::Chunked(Box::new(chunked)), Heads(Vec::new())),
            relayout,
        ))
    }
}

fn values(arena: &Arena, values: &[Value]) -> Result<(Vec<RunValue>, Heads), SealError> {
    let members = values
        .iter()
        .map(|value| run_value(arena, value))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((members, heads(arena, values)?))
}

/// What becomes of each chunk of a list or, when `tuple`, a tuple: its
/// chunks are positional, so chunk `k` of the arena is chunk `k` of the
/// row, and a tuple's chunk carries its content leaf.
fn list_outcomes(
    arena: &Arena,
    sequence: &Sequence,
    old: Option<&Opened>,
    tuple: bool,
) -> Result<Vec<Outcome>, SealError> {
    let mut groups: Vec<Option<&[Value]>> = Vec::new();
    match &sequence.chunks {
        Chunks::Split(chunks) => {
            if old.is_none() {
                return Err(fault("a chunked list the heap did not open"));
            }
            let last = chunks.len().saturating_sub(1);
            for (at, chunk) in chunks.iter().enumerate() {
                let len = match chunk {
                    Chunk::Resident(values) => values.len(),
                    Chunk::Cold { len, .. } => *len as usize,
                };
                if (at < last && len != CHUNK) || len > CHUNK {
                    return Err(fault("a chunked list lost its positions"));
                }
                groups.push(match chunk {
                    Chunk::Resident(values) => Some(values),
                    Chunk::Cold { .. } => None,
                });
            }
        }
        chunks => {
            let all = chunks
                .resident(0)
                .ok_or_else(|| fault("a whole list holds no chunk"))?;
            groups.extend(all.chunks(CHUNK).map(Some));
        }
    }
    let old_count = old.map_or(0, |old| old.row.chunks());
    groups
        .into_iter()
        .enumerate()
        .map(|(at, group)| {
            let Some(group) = group else {
                return Ok(Outcome::Same);
            };
            let (members, heads) = values(arena, group)?;
            let leaf = if tuple {
                Some(
                    chunk_leaf((at * CHUNK) as u64, &members, &heads)
                        .ok_or_else(|| fault("a tuple's chunk names an immutable with no head"))?,
                )
            } else {
                None
            };
            let mut piece = Piece::new(Members::List(members), heads, None);
            piece.leaf = leaf;
            let same = (at as u64) < old_count
                && old
                    .and_then(|old| old.seen.get(&(at as u32)))
                    .is_some_and(|seen| seen.digest == piece.seen.digest);
            Ok(if same {
                Outcome::Same
            } else {
                Outcome::New {
                    pieces: vec![piece],
                    keep_first: false,
                }
            })
        })
        .collect()
}

/// The entries of a table's resident chunk `values`, whose sequences are
/// `seqs`, with their keys' placements.
fn placed_entries(arena: &Arena, pairs: &[Value], seqs: &[u64]) -> Result<Vec<Placed>, SealError> {
    if pairs.len() != seqs.len() * 2 {
        return Err(fault("a table chunk and its sequences disagree"));
    }
    let semantics = arena.semantics();
    pairs
        .as_chunks::<2>()
        .0
        .iter()
        .zip(seqs)
        .map(|([key, value], seq)| {
            let normal = semantics
                .normal_key(key)
                .map_err(|error| fault(error.to_string()))?
                .ok_or_else(|| fault("a table key has no normal form"))?;
            Ok(Entry {
                seq: *seq,
                placement: placement(&normal),
                key: run_value(arena, key)?,
                value: run_value(arena, value)?,
            })
        })
        .collect()
}

/// The heads of the immutables a table chunk's entries name, from the
/// resident objects their ids hold.
fn entry_heads(arena: &Arena, pairs: &[Value]) -> Result<Heads, SealError> {
    heads(arena, pairs)
}

/// What becomes of each chunk of a table, and, for a table written whole
/// for the first time, every chunk's first placement.
#[allow(clippy::type_complexity)]
fn table_outcomes(
    arena: &Arena,
    handle: ArenaHandle,
    table: &Table,
    mut old: Option<&mut Opened>,
    access: &Access<'_>,
) -> Result<(Vec<Outcome>, Option<Vec<Option<Placement>>>), SealError> {
    let sequences = |chunk: usize| {
        arena
            .chunk_sequences(handle, chunk)
            .map_err(|error| fault(error.to_string()))
    };
    // The resident pairs every entry's heads come from.
    let mut resident: Vec<Value> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut extra: Vec<Placed> = Vec::new();
    match (&table.chunks, old.as_deref_mut()) {
        (Chunks::Split(chunks), Some(opened)) => {
            let count = opened.row.chunks() as usize;
            for (at, chunk) in chunks.iter().enumerate() {
                let entries = match chunk {
                    Chunk::Cold { .. } => None,
                    Chunk::Resident(pairs) => {
                        resident.extend_from_slice(pairs);
                        Some(placed_entries(arena, pairs, &sequences(at)?)?)
                    }
                };
                match (at < count, entries) {
                    (true, None) => groups.push(Group::Cold),
                    (true, Some(entries)) => groups.push(Group::Entries(entries)),
                    (false, Some(entries)) => extra.extend(entries),
                    (false, None) => return Err(fault("a table's overflow chunk is cold")),
                }
            }
        }
        (Chunks::Split(_), None) => return Err(fault("a chunked table the heap did not open")),
        (chunks, opened) => {
            let pairs = chunks
                .resident(0)
                .ok_or_else(|| fault("a whole table holds no chunk"))?;
            resident.extend_from_slice(pairs);
            let entries = placed_entries(arena, pairs, &sequences(0)?)?;
            let firsts = match opened {
                Some(opened) => Some(match &opened.firsts {
                    Some(firsts) => firsts.clone(),
                    None => {
                        let mut firsts = Vec::new();
                        opened
                            .row
                            .index
                            .each(access, &opened.row.shape(), &mut |_, chunk| {
                                firsts.push(chunk.first);
                                Ok(false)
                            })
                            .map_err(SealError::Heap)?;
                        firsts
                    }
                }),
                None => None,
            };
            match firsts {
                Some(firsts) if !firsts.is_empty() => {
                    let mut by_chunk: Vec<Vec<Placed>> =
                        firsts.iter().map(|_| Vec::new()).collect();
                    for entry in entries {
                        by_chunk[placed_in(firsts.iter(), &entry.placement)].push(entry);
                    }
                    groups.extend(by_chunk.into_iter().map(Group::Entries));
                }
                _ => {
                    // First written: by placement, `FILL` to a chunk.
                    let heads = entry_heads(arena, &resident)?;
                    let mut entries = entries;
                    entries.sort_unstable_by_key(|entry| entry.placement);
                    let mut outcomes = Vec::new();
                    let mut firsts = Vec::new();
                    for run in entries.chunks(FILL) {
                        let first = run.first().map(|entry| entry.placement);
                        let mut members = run.to_vec();
                        members.sort_unstable_by_key(|entry| entry.seq);
                        firsts.push(first);
                        outcomes.push(Outcome::New {
                            pieces: vec![table_piece(members, &heads, first)?],
                            keep_first: false,
                        });
                    }
                    return Ok((outcomes, Some(firsts)));
                }
            }
        }
    }
    let opened = old.ok_or_else(|| fault("a chunked table the heap did not open"))?;

    // New entries go to the chunks their keys' placements name; a cold
    // chunk read back here names its immutables by its page's heads.
    let mut read_heads = Vec::new();
    for entry in extra {
        let (chunk, held) = opened
            .row
            .index
            .get(
                access,
                &opened.row.shape(),
                Seek::Key(Some(entry.placement)),
            )
            .map_err(SealError::Heap)?
            .ok_or_else(|| fault("a chunked table's index names no chunk"))?;
        let chunk = usize::try_from(chunk)
            .ok()
            .filter(|chunk| *chunk < groups.len())
            .ok_or_else(|| fault("a chunked table's index names a chunk past its row"))?;
        if matches!(groups[chunk], Group::Cold) {
            let (held, digest) = chunk_page(access, &held).map_err(SealError::Heap)?;
            let edges = edges_of(held.members.values());
            opened.seen.insert(chunk as u32, Seen { digest, edges });
            let Members::Table(entries) = held.members else {
                return Err(fault("a table chunk holds a list's members"));
            };
            read_heads.extend(held.heads.0);
            groups[chunk] = Group::Entries(entries);
        }
        if let Group::Entries(entries) = &mut groups[chunk] {
            entries.push(entry);
        }
    }

    let mut pool: BTreeMap<ObjectId, _> = entry_heads(arena, &resident)?.0.into_iter().collect();
    pool.extend(read_heads);
    let heads = Heads(pool.into_iter().collect());
    let mut outcomes = Vec::with_capacity(groups.len());
    for (at, group) in groups.into_iter().enumerate() {
        let Group::Entries(mut entries) = group else {
            outcomes.push(Outcome::Same);
            continue;
        };
        entries.sort_unstable_by_key(|entry| entry.seq);
        if entries.len() <= CHUNK {
            let piece = table_piece(entries, &heads, None)?;
            let same = opened
                .seen
                .get(&(at as u32))
                .is_some_and(|seen| seen.digest == piece.seen.digest);
            outcomes.push(if same {
                Outcome::Same
            } else {
                Outcome::New {
                    pieces: if piece.len == 0 {
                        Vec::new()
                    } else {
                        vec![piece]
                    },
                    keep_first: true,
                }
            });
            continue;
        }
        // Past one leaf: split by placement.
        entries.sort_unstable_by_key(|entry| entry.placement);
        let count = entries.len().div_ceil(FILL);
        let width = entries.len().div_ceil(count);
        let mut pieces = Vec::with_capacity(count);
        for run in entries.chunks(width) {
            let first = run.first().map(|entry| entry.placement);
            let mut members = run.to_vec();
            members.sort_unstable_by_key(|entry| entry.seq);
            pieces.push(table_piece(members, &heads, first)?);
        }
        outcomes.push(Outcome::New {
            pieces,
            keep_first: true,
        });
    }
    Ok((outcomes, None))
}

/// One table chunk of `entries`, with the heads of the immutables they
/// name from `heads`.
fn table_piece(
    entries: Vec<Placed>,
    heads: &Heads,
    first: Option<Placement>,
) -> Result<Piece, SealError> {
    let mut named: Vec<ObjectId> = entries
        .iter()
        .flat_map(|entry| [&entry.key, &entry.value])
        .filter_map(|value| match value {
            RunValue::Object(kind, id) if !kind.mutable() => Some(*id),
            _ => None,
        })
        .collect();
    named.sort_unstable();
    named.dedup();
    let held = named
        .into_iter()
        .map(|id| {
            heads
                .get(id)
                .map(|head| (id, head.clone()))
                .ok_or_else(|| fault(format!("immutable {} has no head", id.0)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Piece::new(Members::Table(entries), Heads(held), first))
}

/// The row of a large object of `kind`, `len` members below `next`, from
/// the old row and what becomes of each chunk: the new chunk pages and
/// the index pages on their paths sealed, every other page kept.
#[allow(clippy::type_complexity)]
fn assemble(
    access: &Access<'_>,
    kind: ObjectKind,
    len: u64,
    next: u64,
    old: Option<&Opened>,
    outcomes: Vec<Outcome>,
) -> Result<(Chunked, HashMap<u32, Seen>, Option<Vec<Option<Placement>>>), HeapFault> {
    let shape = Chunked::shape_of(kind, len);
    let old_count = old.map_or(0, |old| old.row.chunks());
    let old_firsts = match old {
        Some(old) => old.firsts.as_ref().map(Some),
        None => Some(None),
    };
    // What the changed chunks add to and take from the row's edges, what
    // the heap knows of each chunk of the new row, and each one's first
    // placement while every one is known.
    let mut delta: BTreeMap<ObjectId, i64> = BTreeMap::new();
    let mut seen = HashMap::new();
    let mut firsts = Some(Vec::new());
    let mut changed = BTreeSet::new();
    let mut replaced: HashMap<u64, (Vec<Piece>, bool)> = HashMap::new();
    let mut tail = Vec::new();
    let mut chunk = 0u32;
    let known =
        |firsts: &mut Option<Vec<Option<Placement>>>, first: Option<Option<Placement>>| match (
            firsts.as_mut(),
            first,
        ) {
            (Some(firsts), Some(first)) => firsts.push(first),
            _ => *firsts = None,
        };
    let old_first = |at: u64| -> Option<Option<Placement>> {
        match old_firsts {
            Some(Some(firsts)) => firsts.get(usize::try_from(at).ok()?).copied(),
            _ => None,
        }
    };
    for (at, outcome) in (0u64..).zip(outcomes) {
        match outcome {
            Outcome::Same if at < old_count => {
                if let Some(held) = old.and_then(|old| old.seen.get(&(at as u32))) {
                    seen.insert(chunk, held.clone());
                }
                known(&mut firsts, old_first(at));
                chunk += 1;
            }
            Outcome::Same => {
                return Err(HeapFault::Malformed(
                    "a chunk past the old row is unchanged".into(),
                ));
            }
            Outcome::New { pieces, keep_first } => {
                if at < old_count {
                    let held = old
                        .and_then(|old| old.seen.get(&(at as u32)))
                        .ok_or_else(|| {
                            HeapFault::Malformed(format!("chunk {at} changed and was never read"))
                        })?;
                    for id in &held.edges {
                        *delta.entry(*id).or_insert(0) -= 1;
                    }
                }
                for (piece_at, piece) in pieces.iter().enumerate() {
                    for id in &piece.seen.edges {
                        *delta.entry(*id).or_insert(0) += 1;
                    }
                    seen.insert(chunk, piece.seen.clone());
                    let first = if keep_first && piece_at == 0 && at < old_count {
                        old_first(at)
                    } else {
                        Some(piece.first)
                    };
                    known(&mut firsts, first);
                    chunk += 1;
                }
                if at < old_count {
                    changed.insert(at);
                    replaced.insert(at, (pieces, keep_first));
                } else {
                    tail.extend(pieces);
                }
            }
        }
    }
    let seal = |piece: Piece| -> Result<ChunkRef, HeapFault> {
        Ok(ChunkRef {
            first: piece.first,
            len: piece.len,
            page: access.seal(PageKind::SEQUENCE_LEAF, piece.tokens, Vec::new())?,
            leaf: piece.leaf,
        })
    };
    let tail = tail.into_iter().map(seal).collect::<Result<Vec<_>, _>>()?;
    let index = match old {
        Some(old) => old.row.index.rebuild(
            access,
            &shape,
            &changed,
            &mut |at, held| {
                let (pieces, keep_first) = replaced.remove(&at).ok_or_else(|| {
                    HeapFault::Malformed(format!("chunk {at} was rewritten twice"))
                })?;
                pieces
                    .into_iter()
                    .enumerate()
                    .map(|(piece_at, mut piece)| {
                        if keep_first && piece_at == 0 {
                            piece.first = held.first;
                        }
                        seal(piece)
                    })
                    .collect()
            },
            tail,
        )?,
        None => Index::build(access, &shape, tail)?,
    };
    let no_edges = Index::empty(&EDGES);
    let edges = rewrite_edges(access, old.map_or(&no_edges, |old| &old.row.edges), &delta)?;
    let chunked = Chunked {
        kind,
        len,
        next,
        index,
        edges,
    };
    chunked.check().map_err(HeapFault::Malformed)?;
    Ok((chunked, seen, firsts))
}
