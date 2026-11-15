//! A large list's, tuple's or table's page form (cutover contract §6, cold
//! access): its row holds the top of the object's chunk index
//! ([`super::index`]), whose pages name its chunks, and each chunk page
//! holds at most [`CHUNK`] members with the heads of the immutables they
//! name. A point read loads the row, one index page per level below it
//! and one chunk; a point edit rewrites that chunk, the index pages on its
//! path and the row. Up to [`INDEX_REFS`] × [`INDEX_REFS`] chunks the
//! index has one level below the row.
//!
//! A list or a tuple is chunked by position: chunk `k` holds positions
//! `k * CHUNK ..`. A table is chunked by key placement, the SHA-256 of
//! its key's normal form ([`placement`]), so the index finds the one
//! chunk that may hold a key without a key index of its own; each entry
//! keeps its insertion sequence, and a chunk's entries are in sequence
//! order. Two keys share a placement only under the `D-KEY` collision
//! assumption; a load still compares the keys it decodes.
//!
//! The ids the chunks reference, with how many times each, are the row's
//! edge summary: kept in edge pages ([`EdgePage`]) under the row's edge
//! index, by their first id, so a row stays one bounded cell however many
//! objects its members name, a mark reads the edge pages and never a
//! chunk, and an edit that changes no reference rewrites no edge page.

use std::collections::BTreeMap;

use lash_kernel_doc::heap::{HeapFault, NormalKey, PageRef};
use lash_kernel_doc::heap_values::key_digest;
use lash_kernel_value::tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, decode_body, encode_body,
};

use super::super::PageKind;
use super::super::identity;
use super::super::tree::Access;

use super::ids::ObjectId;
use super::index::{self, Index, Key, Leaf, Part, Seek, Shape};
use super::objects::{Heads, ObjectKind, RunValue};

/// The most members one chunk page holds: one sequence leaf.
pub const CHUNK: usize = 64;

/// The most chunk references one index page holds.
pub const INDEX_REFS: usize = index::NODE_REFS;

/// A list, tuple or table of more than this many members is written
/// chunked; a smaller one is one row unless its row would pass
/// [`ROW_BYTES`].
pub const LARGE: usize = 4 * CHUNK;

/// The most bytes a list's, tuple's or table's row may take before it is
/// written chunked: the heads of the immutables its members name can make
/// a row of few members wide. Below half of a directory leaf's packing
/// cap, so a leaf holding it stays within the format's node bound.
pub const ROW_BYTES: usize = 16 * 1024;

/// The most ids one edge page holds.
pub const EDGE_REFS: usize = 1024;

/// The ids an edge page holds when it is written: room for a quarter more.
const EDGE_FILL: usize = EDGE_REFS * 3 / 4;

/// Where a table entry's key sits in its table's chunk order.
pub type Placement = [u8; 32];

/// The placement of the key whose normal form is `normal`.
pub fn placement(normal: &NormalKey) -> Placement {
    match normal {
        NormalKey::Inline(tokens) => key_digest(tokens),
        NormalKey::Digest(digest) => *digest,
    }
}

fn encode_placement(first: &Option<Placement>, out: &mut TokenWriter) {
    match first {
        None => out.sum(0, 0),
        Some(first) => {
            out.sum(1, 1);
            out.bytes(first);
        }
    }
}

fn decode_placement(input: &mut TokenReader<'_>) -> Result<Option<Placement>, TokenError> {
    let (tag, fields) = input.sum()?;
    match tag {
        0 => {
            TokenReader::expect_fields(fields, 0)?;
            Ok(None)
        }
        1 => {
            TokenReader::expect_fields(fields, 1)?;
            let bytes = input.bytes()?;
            Ok(Some(bytes.try_into().map_err(|_| {
                TokenError::Invalid("a placement is not 32 bytes".into())
            })?))
        }
        _ => Err(TokenError::UnsupportedVariant { variant: tag }),
    }
}

impl Key for Option<Placement> {
    const FIELDS: u64 = 1;
    fn write(&self, out: &mut TokenWriter) {
        encode_placement(self, out);
    }
    fn read(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        decode_placement(input)
    }
}

impl Key for ObjectId {
    const FIELDS: u64 = 1;
    fn write(&self, out: &mut TokenWriter) {
        self.encode(out);
    }
    fn read(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Self::decode(input)
    }
}

/// One chunk, as its index page names it: the first placement it holds
/// (a table's), how many members it holds, its page and, a tuple's, the
/// content leaf its members make ([`identity::tuple_leaf`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    pub first: Option<Placement>,
    pub len: u32,
    pub page: PageRef,
    pub leaf: Option<[u8; 32]>,
}

impl TokenCodec for ChunkRef {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3 + u64::from(self.leaf.is_some()));
        encode_placement(&self.first, out);
        out.unsigned(u64::from(self.len));
        self.page.encode(out);
        index::encode_digest(&self.leaf, out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let present = index::digest_arity(input.seq()?, 3)?;
        let first = decode_placement(input)?;
        let len = u32::try_from(input.unsigned()?)
            .map_err(|_| TokenError::Invalid("a chunk's length is past a u32".into()))?;
        Ok(Self {
            first,
            len,
            page: PageRef::decode(input)?,
            leaf: index::decode_digest(input, present)?,
        })
    }
}

impl Leaf for ChunkRef {
    type Key = Option<Placement>;
    fn key(&self) -> Self::Key {
        self.first
    }
    fn page(&self) -> PageRef {
        self.page
    }
    fn page_mut(&mut self) -> &mut PageRef {
        &mut self.page
    }
    fn id(&self) -> Option<[u8; 32]> {
        self.leaf
    }
}

/// One edge page, as the row names it: the first id it holds, how many
/// ids it holds and its page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgePart {
    pub first: ObjectId,
    pub ids: u32,
    pub page: PageRef,
}

impl TokenCodec for EdgePart {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.first.encode(out);
        out.unsigned(u64::from(self.ids));
        self.page.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let first = ObjectId::decode(input)?;
        let ids = u32::try_from(input.unsigned()?)
            .map_err(|_| TokenError::Invalid("an edge part's count is past a u32".into()))?;
        Ok(Self {
            first,
            ids,
            page: PageRef::decode(input)?,
        })
    }
}

impl Leaf for EdgePart {
    type Key = ObjectId;
    fn key(&self) -> Self::Key {
        self.first
    }
    fn page(&self) -> PageRef {
        self.page
    }
    fn page_mut(&mut self) -> &mut PageRef {
        &mut self.page
    }
}

/// A row's edge index: its edge pages, by first id, kept read once
/// written.
pub const EDGES: Shape = Shape {
    kind: PageKind::KEYED_INDEX,
    min_height: 0,
    full: false,
    tuple: None,
    kept: true,
};

/// An edge page: ids ascending, once each, with how many times the
/// object's chunks reference each.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgePage(pub Vec<(ObjectId, u64)>);

impl TokenCodec for EdgePage {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(self.0.len() as u64 * 2);
        for (id, count) in &self.0 {
            id.encode(out);
            out.unsigned(*count);
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let count = input.seq()?;
        if count % 2 != 0 {
            return Err(TokenError::Invalid("an edge has no count".into()));
        }
        let mut edges = Vec::with_capacity(usize::try_from(count / 2).unwrap_or(0).min(EDGE_REFS));
        for _ in 0..count / 2 {
            edges.push((ObjectId::decode(input)?, input.unsigned()?));
        }
        if edges.is_empty()
            || edges.len() > EDGE_REFS
            || !edges.windows(2).all(|pair| pair[0].0 < pair[1].0)
            || edges.iter().any(|(_, count)| *count == 0)
        {
            return Err(TokenError::Invalid(
                "an edge page's ids are not ascending once each, within its bound".into(),
            ));
        }
        Ok(Self(edges))
    }
}

fn corrupt(page: PageRef, reason: impl Into<String>) -> HeapFault {
    HeapFault::Corrupt {
        page: page.id,
        reason: reason.into(),
    }
}

/// The edge page `part` names.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Corrupt`] for a page that is not
/// the edge page `part` describes.
pub fn edge_page(access: &Access<'_>, part: &EdgePart) -> Result<EdgePage, HeapFault> {
    let answer = access.read(part.page)?;
    if answer.kind != PageKind::KEYED_LEAF {
        return Err(corrupt(part.page, "an edge part is not an edge page"));
    }
    let page: EdgePage =
        decode_body(&answer.tokens).map_err(|error| corrupt(part.page, error.to_string()))?;
    if page.0.len() != part.ids as usize || page.0[0].0 != part.first {
        return Err(corrupt(part.page, "an edge page holds other ids"));
    }
    Ok(page)
}

/// Every id `edges` name, ascending: one read of each edge page and of
/// each edge index page.
///
/// # Errors
///
/// As [`edge_page`].
pub fn edge_ids(access: &Access<'_>, edges: &Index<EdgePart>) -> Result<Vec<ObjectId>, HeapFault> {
    let mut ids = Vec::new();
    edges.each(access, &EDGES, &mut |_, part| {
        ids.extend(edge_page(access, part)?.0.into_iter().map(|(id, _)| id));
        Ok(false)
    })?;
    Ok(ids)
}

/// Checks chunk `position` of `row`, which `chunk` names and whose page is
/// `page`, when `row` is a tuple's: its members, as the page's heads name
/// them, make the leaf its checked index page commits to.
///
/// # Errors
///
/// [`HeapFault::Corrupt`] for a page that is not the tuple's.
pub fn check_tuple_chunk(
    row: &Chunked,
    position: u64,
    chunk: &ChunkRef,
    page: &ChunkPage,
) -> Result<(), HeapFault> {
    if row.kind != ObjectKind::Tuple {
        return Ok(());
    }
    let Members::List(members) = &page.members else {
        return Err(corrupt(
            chunk.page,
            "a tuple's chunk holds a table's entries",
        ));
    };
    let start = position.saturating_mul(CHUNK as u64);
    if identity::chunk_leaf(start, members, &page.heads) != chunk.leaf {
        return Err(corrupt(
            chunk.page,
            "a tuple's chunk is not the one its index page commits to",
        ));
    }
    Ok(())
}

/// The chunk page `chunk` names.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Corrupt`] for a page that is not
/// a chunk page of the count `chunk` names.
pub fn chunk_page(
    access: &Access<'_>,
    chunk: &ChunkRef,
) -> Result<(ChunkPage, [u8; 32]), HeapFault> {
    let answer = access.read(chunk.page)?;
    if answer.kind != PageKind::SEQUENCE_LEAF {
        return Err(corrupt(chunk.page, "a chunk is not a leaf page"));
    }
    let page: ChunkPage =
        decode_body(&answer.tokens).map_err(|error| corrupt(chunk.page, error.to_string()))?;
    if page.members.len() != chunk.len as usize {
        return Err(corrupt(chunk.page, "a chunk holds another count"));
    }
    Ok((
        page,
        lash_kernel_value::tokens::logical_digest(&answer.tokens),
    ))
}

/// Calls `each` with the members of every chunk of `chunked`, in order:
/// one read of each index page and each chunk, a tuple's checked against
/// its content ids on the way down.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Corrupt`] for an index or chunk
/// page that is not one, or not the tuple's.
pub fn each_members(
    access: &Access<'_>,
    chunked: &Chunked,
    each: &mut dyn FnMut(&Members),
) -> Result<(), HeapFault> {
    chunked
        .index
        .each(access, &chunked.shape(), &mut |position, chunk| {
            let (page, _) = chunk_page(access, chunk)?;
            check_tuple_chunk(chunked, position, chunk, &page)?;
            each(&page.members);
            Ok(false)
        })?;
    Ok(())
}

/// The edge index `edges` with `delta` applied to the counts: only the
/// edge pages an id of `delta` falls in, and the edge index pages on their
/// paths, are read and written again.
///
/// # Errors
///
/// As [`edge_page`]; [`HeapFault::Malformed`] when a count would fall
/// below zero.
pub fn rewrite_edges(
    access: &Access<'_>,
    edges: &Index<EdgePart>,
    delta: &BTreeMap<ObjectId, i64>,
) -> Result<Index<EdgePart>, HeapFault> {
    let write = |edges: BTreeMap<ObjectId, u64>| {
        let edges: Vec<(ObjectId, u64)> = edges.into_iter().filter(|(_, n)| *n > 0).collect();
        let width = if edges.len() > EDGE_REFS {
            EDGE_FILL
        } else {
            EDGE_REFS
        };
        let mut out = Vec::new();
        for run in edges.chunks(width) {
            let page = EdgePage(run.to_vec());
            out.push(EdgePart {
                first: run[0].0,
                ids: run.len() as u32,
                page: access.seal_kept(PageKind::KEYED_LEAF, encode_body(&page), Vec::new())?,
            });
        }
        Ok::<_, HeapFault>(out)
    };
    let below_zero = || HeapFault::Malformed("an edge count falls below zero".into());
    if edges.is_empty() {
        let mut counts = BTreeMap::new();
        for (id, change) in delta.iter().filter(|(_, change)| **change != 0) {
            counts.insert(*id, u64::try_from(*change).map_err(|_| below_zero())?);
        }
        return Index::build(access, &EDGES, write(counts)?);
    }
    let mut touched: BTreeMap<u64, Vec<(ObjectId, i64)>> = BTreeMap::new();
    for (id, change) in delta.iter().filter(|(_, change)| **change != 0) {
        let (at, _) = edges
            .get(access, &EDGES, Seek::Key(*id))?
            .ok_or_else(|| HeapFault::Malformed("an edge index found no page".into()))?;
        touched.entry(at).or_default().push((*id, *change));
    }
    if touched.is_empty() {
        return Ok(edges.clone());
    }
    let changed = touched.keys().copied().collect();
    edges.rebuild(
        access,
        &EDGES,
        &changed,
        &mut |at, part| {
            let mut counts: BTreeMap<ObjectId, u64> =
                edge_page(access, &part)?.0.into_iter().collect();
            for (id, change) in touched.remove(&at).unwrap_or_default() {
                let count = counts.entry(id).or_insert(0);
                *count = count.checked_add_signed(change).ok_or_else(below_zero)?;
            }
            write(counts)
        },
        Vec::new(),
    )
}

/// A large list's or table's row record: its kind and length, the next
/// insertion sequence of a table, the top of its chunk index and of the
/// index of the edge pages holding the exact multiset of ids its chunks
/// reference.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunked {
    pub kind: ObjectKind,
    /// Values of a list; entries of a table.
    pub len: u64,
    /// The sequence a table's next new entry takes; a list's length.
    pub next: u64,
    /// The chunk index: a tuple's references carry their content ids.
    pub index: Index<ChunkRef>,
    /// The edge index, ascending by first id.
    pub edges: Index<EdgePart>,
}

impl Chunked {
    /// How many chunks the object is in.
    pub fn chunks(&self) -> u64 {
        self.index.leaves()
    }

    /// The shape of the chunk index of an object of `kind` and `len`: a
    /// tuple's is full and carries content ids, so its pages are
    /// positional and its top makes its content root.
    pub fn shape_of(kind: ObjectKind, len: u64) -> Shape {
        let tuple = kind == ObjectKind::Tuple;
        Shape {
            kind: PageKind::SEQUENCE_INDEX,
            min_height: 1,
            full: tuple,
            tuple: tuple.then_some(len),
            kept: false,
        }
    }

    /// The shape of the object's chunk index.
    pub fn shape(&self) -> Shape {
        Self::shape_of(self.kind, self.len)
    }

    /// A tuple's content root, which its head must name.
    pub fn tuple_root(&self) -> Option<[u8; 32]> {
        (self.kind == ObjectKind::Tuple)
            .then(|| self.index.tuple_root(self.len))
            .flatten()
    }

    /// Checks what decoding cannot: the kind is a list's or a table's, a
    /// table's index is in placement order and a list's names none, a
    /// tuple's is positional with its content ids, and the edges ascend
    /// once each.
    ///
    /// # Errors
    ///
    /// What is wrong.
    pub fn check(&self) -> Result<(), String> {
        let table = match self.kind {
            ObjectKind::List | ObjectKind::Tuple => false,
            ObjectKind::Map | ObjectKind::Set | ObjectKind::Record => true,
            other => return Err(format!("a chunked {other:?}")),
        };
        let Index::Nodes { refs, .. } = &self.index else {
            return Err("a chunked object's row names its chunks with no index".into());
        };
        if refs
            .iter()
            .any(|part: &Part<_>| part.first.is_some() != table)
        {
            return Err("a chunked object's index places another kind's keys".into());
        }
        self.index.check(&self.shape())?;
        if !table && self.next != self.len {
            return Err("a chunked sequence's next is not its length".into());
        }
        if self.kind == ObjectKind::Tuple && self.chunks() != identity::tuple_leaves(self.len) {
            return Err("a chunked tuple's index is not positional".into());
        }
        if self.len > self.next {
            return Err("a chunked table holds more entries than it took".into());
        }
        self.edges.check(&EDGES)?;
        if let Index::Leaves(parts) = &self.edges
            && (!parts.windows(2).all(|pair| pair[0].first < pair[1].first)
                || parts
                    .iter()
                    .any(|part| part.ids == 0 || part.ids as usize > EDGE_REFS))
        {
            return Err("a chunked object's edge pages are out of order or bound".into());
        }
        Ok(())
    }
}

impl TokenCodec for Chunked {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
        self.kind.encode(out);
        out.unsigned(self.len);
        out.unsigned(self.next);
        self.index.encode(out);
        self.edges.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        let chunked = Self {
            kind: ObjectKind::decode(input)?,
            len: input.unsigned()?,
            next: input.unsigned()?,
            index: Index::decode(input)?,
            edges: Index::decode(input)?,
        };
        chunked.check().map_err(TokenError::Invalid)?;
        Ok(chunked)
    }
}

/// One table entry as its chunk holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Its insertion sequence.
    pub seq: u64,
    /// Its key's placement, which a load checks against the key.
    pub placement: Placement,
    pub key: RunValue,
    /// A set's entries hold [`RunValue::Absent`].
    pub value: RunValue,
}

/// What one chunk holds: a list's values in position order, or a table's
/// entries in sequence order.
#[derive(Clone, Debug, PartialEq)]
pub enum Members {
    List(Vec<RunValue>),
    Table(Vec<Entry>),
}

impl Members {
    /// How many members the chunk holds.
    pub fn len(&self) -> usize {
        match self {
            Self::List(values) => values.len(),
            Self::Table(entries) => entries.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every value the chunk holds, in member order.
    pub fn values(&self) -> Vec<&RunValue> {
        match self {
            Self::List(values) => values.iter().collect(),
            Self::Table(entries) => entries
                .iter()
                .flat_map(|entry| [&entry.key, &entry.value])
                .collect(),
        }
    }
}

/// One chunk page: its members and the heads of the immutables they name.
#[derive(Clone, Debug, PartialEq)]
pub struct ChunkPage {
    pub members: Members,
    pub heads: Heads,
}

impl TokenCodec for ChunkPage {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        match &self.members {
            Members::List(values) => {
                out.sum(0, 1);
                values.encode(out);
            }
            Members::Table(entries) => {
                out.sum(1, 1);
                out.seq(entries.len() as u64 * 4);
                for entry in entries {
                    out.unsigned(entry.seq);
                    out.bytes(&entry.placement);
                    entry.key.encode(out);
                    entry.value.encode(out);
                }
            }
        }
        self.heads.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let (tag, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        let members = match tag {
            0 => Members::List(Vec::decode(input)?),
            1 => {
                let count = input.seq()?;
                if count % 4 != 0 {
                    return Err(TokenError::Invalid("a table entry is cut short".into()));
                }
                let mut entries =
                    Vec::with_capacity(usize::try_from(count / 4).unwrap_or(0).min(CHUNK));
                for _ in 0..count / 4 {
                    let seq = input.unsigned()?;
                    let placement = input
                        .bytes()?
                        .try_into()
                        .map_err(|_| TokenError::Invalid("a placement is not 32 bytes".into()))?;
                    entries.push(Entry {
                        seq,
                        placement,
                        key: RunValue::decode(input)?,
                        value: RunValue::decode(input)?,
                    });
                }
                if !entries.windows(2).all(|pair| pair[0].seq < pair[1].seq) {
                    return Err(TokenError::Invalid(
                        "a table chunk's entries are out of sequence order".into(),
                    ));
                }
                Members::Table(entries)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        };
        if members.len() > CHUNK {
            return Err(TokenError::Invalid(
                "a chunk holds more members than one leaf".into(),
            ));
        }
        let heads = Heads::decode(input)?;
        heads.check(members.values()).map_err(TokenError::Invalid)?;
        Ok(Self { members, heads })
    }
}
