//! Large lists and tables (cutover contract §6, cold access): written as
//! a [`Chunked`] row over index pages and chunk pages, opened by their row
//! alone, and read one chunk at a time.
//!
//! A point read of a cold large object loads its row, one index page and
//! one chunk. A point edit dirties one chunk: the seal re-encodes the
//! object's resident chunks, writes only those whose encoding changed,
//! rewrites the index pages naming them and the row, and adjusts the row's
//! edge multiset by the changed chunks' references alone. A table's new
//! entries wait in an overflow chunk until the seal places them by key;
//! a chunk past [`CHUNK`] entries splits by placement, and an object whose
//! chunks moved goes back to its row as a proxy, so the arena's chunks are
//! always the row's.

use std::collections::{BTreeMap, HashMap};

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Chunks, Sequence, Table};
use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::tree::{Access, grant};
use lash_kernel_state::paged::PageKind;
use lash_kernel_state::paged::run::chunked::{
    CHUNK, ChunkPage, ChunkRef, Chunked, Entry, INDEX_REFS, IndexPage, LARGE, Members, Part,
    Placement, placement, rewrite_edges,
};
use lash_kernel_state::paged::run::directory::Row;
use lash_kernel_state::paged::run::ids::ObjectId;
use lash_kernel_state::paged::run::objects::{Heads, ObjectKind, Record, RunValue};
use lash_kernel_value::tokens::{decode_body, encode_body, logical_digest};

use super::seal::{fault, heads, run_value};
use crate::paged::SealError;

/// The entries a table's chunk holds when it is first written: room for a
/// quarter more before it splits.
const FILL: usize = CHUNK * 3 / 4;

/// The chunks an index page names when it is first written.
const INDEX_FILL: usize = INDEX_REFS * 3 / 4;

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

/// Whether `body` is written chunked: a list or table past [`LARGE`]
/// members, or one opened from its chunks.
pub(crate) fn is_large(body: &Body) -> bool {
    match body {
        Body::List(sequence) => {
            sequence.len > LARGE as u64 || matches!(sequence.chunks, Chunks::Split(_))
        }
        Body::Map(table) | Body::Set(table) | Body::Record(table) => {
            table.len > LARGE as u64 || matches!(table.chunks, Chunks::Split(_))
        }
        _ => false,
    }
}

fn corrupt(page: PageRef, reason: impl Into<String>) -> HeapFault {
    HeapFault::Corrupt {
        page: page.id,
        reason: reason.into(),
    }
}

/// The index page `part` names, its chunk references granted.
fn index_page(access: &Access<'_>, part: &Part) -> Result<IndexPage, HeapFault> {
    let answer = access.read(part.page)?;
    if answer.kind != PageKind::SEQUENCE_INDEX {
        return Err(corrupt(part.page, "an index part is not an index page"));
    }
    let mut page: IndexPage =
        decode_body(&answer.tokens).map_err(|error| corrupt(part.page, error.to_string()))?;
    if page.0.len() != part.chunks as usize {
        return Err(corrupt(part.page, "an index page names another count"));
    }
    for chunk in &mut page.0 {
        grant(&mut chunk.page, &answer.children)?;
    }
    Ok(page)
}

/// The chunk page `chunk` names and its encoding's digest.
fn chunk_page(access: &Access<'_>, chunk: &ChunkRef) -> Result<(ChunkPage, [u8; 32]), HeapFault> {
    let answer = access.read(chunk.page)?;
    if answer.kind != PageKind::SEQUENCE_LEAF {
        return Err(corrupt(chunk.page, "a chunk is not a leaf page"));
    }
    let page: ChunkPage =
        decode_body(&answer.tokens).map_err(|error| corrupt(chunk.page, error.to_string()))?;
    if page.members.len() != chunk.len as usize {
        return Err(corrupt(chunk.page, "a chunk holds another count"));
    }
    Ok((page, logical_digest(&answer.tokens)))
}

/// The part and slot of chunk `chunk`.
fn locate(row: &Chunked, chunk: u32) -> Option<(usize, usize)> {
    let mut start = 0u32;
    for (at, part) in row.parts.iter().enumerate() {
        if chunk < start + part.chunks {
            return Some((at, (chunk - start) as usize));
        }
        start += part.chunks;
    }
    None
}

/// The first chunk of part `part`.
fn part_start(row: &Chunked, part: usize) -> u32 {
    row.parts[..part].iter().map(|part| part.chunks).sum()
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
    /// does: one index page read.
    pub(crate) fn chunk_of_key(
        &self,
        access: &Access<'_>,
        id: ObjectId,
        at: &Placement,
    ) -> Result<u32, HeapFault> {
        let row = &self.opened_row(id)?;
        let part = placed_in(row.parts.iter().map(|part| &part.first), at);
        let page = index_page(access, &row.parts[part])?;
        let slot = placed_in(page.0.iter().map(|chunk| &chunk.first), at);
        Ok(part_start(row, part) + slot as u32)
    }

    fn opened_row(&self, id: ObjectId) -> Result<&Chunked, HeapFault> {
        self.row(id)
            .ok_or_else(|| HeapFault::Malformed(format!("large object {} is not open", id.0)))
    }

    /// Chunk `chunk` of the large object `id`: one index page read and
    /// one chunk read.
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
        let (part, slot) = locate(&opened.row, chunk).ok_or_else(|| {
            HeapFault::Malformed(format!("large object {} has no chunk {chunk}", id.0))
        })?;
        let index = index_page(access, &opened.row.parts[part])?;
        let (page, digest) = chunk_page(access, &index.0[slot])?;
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
            Body::List(sequence) => {
                let outcomes = list_outcomes(arena, sequence, old.as_ref())?;
                (ObjectKind::List, outcomes, sequence.len, sequence.len, None)
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

/// What becomes of each chunk of a list: its chunks are positional, so
/// chunk `k` of the arena is chunk `k` of the row.
fn list_outcomes(
    arena: &Arena,
    sequence: &Sequence,
    old: Option<&Opened>,
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
            let piece = Piece::new(Members::List(members), heads, None);
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
                        for part in &opened.row.parts {
                            let page = index_page(access, part).map_err(SealError::Heap)?;
                            firsts.extend(page.0.iter().map(|chunk| chunk.first));
                        }
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
    let mut pages: HashMap<usize, IndexPage> = HashMap::new();
    let mut read_heads = Vec::new();
    for entry in extra {
        let part = placed_in(
            opened.row.parts.iter().map(|part| &part.first),
            &entry.placement,
        );
        if let std::collections::hash_map::Entry::Vacant(vacant) = pages.entry(part) {
            vacant.insert(index_page(access, &opened.row.parts[part]).map_err(SealError::Heap)?);
        }
        let page = &pages[&part];
        let slot = placed_in(page.0.iter().map(|chunk| &chunk.first), &entry.placement);
        let chunk = part_start(&opened.row, part) as usize + slot;
        if matches!(groups[chunk], Group::Cold) {
            let (held, digest) = chunk_page(access, &page.0[slot]).map_err(SealError::Heap)?;
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
/// the index pages naming them sealed, every other page kept.
#[allow(clippy::type_complexity)]
fn assemble(
    access: &Access<'_>,
    kind: ObjectKind,
    len: u64,
    next: u64,
    old: Option<&Opened>,
    outcomes: Vec<Outcome>,
) -> Result<(Chunked, HashMap<u32, Seen>, Option<Vec<Option<Placement>>>), HeapFault> {
    enum Build {
        /// An old part, unchanged, whose first chunk was `start`.
        Keep {
            part: Part,
            start: u32,
        },
        Refs(Vec<(ChunkRef, Option<Seen>)>),
    }
    let mut added: Vec<ObjectId> = Vec::new();
    let mut seal = |piece: Piece| -> Result<(ChunkRef, Option<Seen>), HeapFault> {
        let page = access.seal(PageKind::SEQUENCE_LEAF, piece.tokens, Vec::new())?;
        added.extend(piece.seen.edges.iter().copied());
        Ok((
            ChunkRef {
                first: piece.first,
                len: piece.len,
                page,
            },
            Some(piece.seen),
        ))
    };
    // What the changed chunks add to and take from the row's edges.
    let mut delta: BTreeMap<ObjectId, i64> = BTreeMap::new();
    let mut drop_edges = |seen: &Seen| -> Result<(), HeapFault> {
        for id in &seen.edges {
            *delta.entry(*id).or_insert(0) -= 1;
        }
        Ok(())
    };
    let old_seen = |chunk: u32| old.and_then(|old| old.seen.get(&chunk).cloned());
    let mut builds = Vec::new();
    let mut outcomes = outcomes.into_iter();
    let mut at = 0u32;
    if let Some(old) = old {
        for part in &old.row.parts {
            let range = at..at + part.chunks;
            at += part.chunks;
            let mine: Vec<Option<Outcome>> = range.clone().map(|_| outcomes.next()).collect();
            if mine
                .iter()
                .all(|outcome| matches!(outcome, Some(Outcome::Same)))
            {
                builds.push(Build::Keep {
                    part: *part,
                    start: range.start,
                });
                continue;
            }
            let page = index_page(access, part)?;
            let mut refs = Vec::with_capacity(page.0.len());
            for ((chunk, held), outcome) in range.zip(page.0).zip(mine) {
                if let Some(Outcome::Same) = outcome {
                    refs.push((held, old_seen(chunk)));
                    continue;
                }
                let seen = old.seen.get(&chunk).ok_or_else(|| {
                    HeapFault::Malformed(format!("chunk {chunk} changed and was never read"))
                })?;
                drop_edges(seen)?;
                if let Some(Outcome::New { pieces, keep_first }) = outcome {
                    for (piece_at, mut piece) in pieces.into_iter().enumerate() {
                        if keep_first && piece_at == 0 {
                            piece.first = held.first;
                        }
                        refs.push(seal(piece)?);
                    }
                }
            }
            builds.push(Build::Refs(refs));
        }
    }
    let mut tail = Vec::new();
    for outcome in outcomes {
        let Outcome::New { pieces, .. } = outcome else {
            return Err(HeapFault::Malformed(
                "a chunk past the old row is unchanged".into(),
            ));
        };
        for piece in pieces {
            tail.push(seal(piece)?);
        }
    }
    if !tail.is_empty() {
        match builds.pop() {
            Some(Build::Keep { part, start }) => {
                let page = index_page(access, &part)?;
                let mut refs: Vec<_> = (start..)
                    .zip(page.0)
                    .map(|(chunk, held)| (held, old_seen(chunk)))
                    .collect();
                refs.extend(tail);
                builds.push(Build::Refs(refs));
            }
            Some(Build::Refs(mut refs)) => {
                refs.extend(tail);
                builds.push(Build::Refs(refs));
            }
            None => builds.push(Build::Refs(tail)),
        }
    }
    for id in added {
        *delta.entry(id).or_insert(0) += 1;
    }
    let edges = rewrite_edges(access, old.map_or(&[][..], |old| &old.row.edges), &delta)?;

    let old_firsts = old.and_then(|old| old.firsts.as_ref());
    let mut firsts = Some(Vec::new());
    let mut parts = Vec::new();
    let mut seen = HashMap::new();
    let mut chunk = 0u32;
    for build in builds {
        match build {
            Build::Keep { part, start } => {
                for slot in 0..part.chunks {
                    if let Some(held) = old_seen(start + slot) {
                        seen.insert(chunk + slot, held);
                    }
                }
                firsts = match (firsts, old_firsts) {
                    (Some(mut firsts), Some(old_firsts)) => {
                        let from = start as usize;
                        firsts.extend(
                            old_firsts
                                .get(from..from + part.chunks as usize)
                                .ok_or_else(|| {
                                    HeapFault::Malformed("a row's firsts are short".into())
                                })?,
                        );
                        Some(firsts)
                    }
                    _ => None,
                };
                chunk += part.chunks;
                parts.push(part);
            }
            Build::Refs(refs) => {
                if refs.is_empty() {
                    continue;
                }
                let count = if refs.len() > INDEX_REFS {
                    refs.len().div_ceil(INDEX_FILL)
                } else {
                    1
                };
                let width = refs.len().div_ceil(count);
                for run in refs.chunks(width) {
                    let children: Vec<PageRef> = run.iter().map(|(held, _)| held.page).collect();
                    let index = IndexPage(run.iter().map(|(held, _)| *held).collect());
                    let page =
                        access.seal(PageKind::SEQUENCE_INDEX, encode_body(&index), children)?;
                    for (held, held_seen) in run {
                        if let Some(held_seen) = held_seen {
                            seen.insert(chunk, held_seen.clone());
                        }
                        if let Some(firsts) = &mut firsts {
                            firsts.push(held.first);
                        }
                        chunk += 1;
                    }
                    parts.push(Part {
                        first: run[0].0.first,
                        chunks: run.len() as u32,
                        page,
                    });
                }
            }
        }
    }
    let chunked = Chunked {
        kind,
        len,
        next,
        parts,
        edges,
    };
    chunked.check().map_err(HeapFault::Malformed)?;
    Ok((chunked, seen, firsts))
}
