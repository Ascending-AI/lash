//! A large list's, tuple's or table's page form (cutover contract §6, cold
//! access):
//! its row holds the object's index, each index page names a run of its
//! chunks, and each chunk page holds at most [`CHUNK`] members with the
//! heads of the immutables they name. A point read loads the row, one
//! index page and one chunk; a point edit rewrites that chunk, its index
//! page and the row.
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
//! edge summary: kept in edge pages ([`EdgePage`]) the row names by their
//! first id, so a row stays one bounded cell however many objects its
//! members name, a mark reads the edge pages and never a chunk, and an
//! edit that changes no reference rewrites no edge page.

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
use super::objects::{Heads, ObjectKind, RunValue};

/// The most members one chunk page holds: one sequence leaf.
pub const CHUNK: usize = 64;

/// The most chunk references one index page holds.
pub const INDEX_REFS: usize = 96;

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
        out.seq(4);
        encode_placement(&self.first, out);
        out.unsigned(u64::from(self.len));
        self.page.encode(out);
        encode_digest(&self.leaf, out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        let first = decode_placement(input)?;
        let len = u32::try_from(input.unsigned()?)
            .map_err(|_| TokenError::Invalid("a chunk's length is past a u32".into()))?;
        Ok(Self {
            first,
            len,
            page: PageRef::decode(input)?,
            leaf: decode_digest(input)?,
        })
    }
}

fn encode_digest(digest: &Option<[u8; 32]>, out: &mut TokenWriter) {
    match digest {
        None => out.sum(0, 0),
        Some(digest) => {
            out.sum(1, 1);
            out.bytes(digest);
        }
    }
}

fn decode_digest(input: &mut TokenReader<'_>) -> Result<Option<[u8; 32]>, TokenError> {
    let (tag, fields) = input.sum()?;
    match tag {
        0 => {
            TokenReader::expect_fields(fields, 0)?;
            Ok(None)
        }
        1 => {
            TokenReader::expect_fields(fields, 1)?;
            Ok(Some(digest(input)?))
        }
        _ => Err(TokenError::UnsupportedVariant { variant: tag }),
    }
}

fn digest(input: &mut TokenReader<'_>) -> Result<[u8; 32], TokenError> {
    input
        .bytes()?
        .try_into()
        .map_err(|_| TokenError::Invalid("a content id is not 32 bytes".into()))
}

/// A chunked tuple's content index: its content root and its level-one
/// nodes, each of [`identity::FANOUT`] chunks, so a load checks every
/// index page against the row and the row against the head every referrer
/// names the tuple by. A tuple's index pages are each [`INDEX_REFS`]
/// chunks, the last fewer, so page `p` holds the chunks of nodes
/// `3p .. 3p + 3`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TupleIndex {
    pub root: [u8; 32],
    pub nodes: Vec<[u8; 32]>,
}

/// The level-one nodes one tuple index page spans.
pub const PAGE_NODES: usize = INDEX_REFS / identity::FANOUT;

const _: () = assert!(INDEX_REFS.is_multiple_of(identity::FANOUT) && CHUNK == identity::TUPLE_LEAF);

impl TokenCodec for TupleIndex {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.bytes(&self.root);
        out.seq(self.nodes.len() as u64);
        for node in &self.nodes {
            out.bytes(node);
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let root = digest(input)?;
        let count = input.seq()?;
        let mut nodes = Vec::with_capacity(usize::try_from(count).unwrap_or(0).min(4096));
        for _ in 0..count {
            nodes.push(digest(input)?);
        }
        Ok(Self { root, nodes })
    }
}

/// One index page, as the row names it: the first placement under it (a
/// table's), how many chunks it names and its page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub first: Option<Placement>,
    pub chunks: u32,
    pub page: PageRef,
}

impl TokenCodec for Part {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        encode_placement(&self.first, out);
        out.unsigned(u64::from(self.chunks));
        self.page.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let first = decode_placement(input)?;
        let chunks = u32::try_from(input.unsigned()?)
            .map_err(|_| TokenError::Invalid("an index part's count is past a u32".into()))?;
        Ok(Self {
            first,
            chunks,
            page: PageRef::decode(input)?,
        })
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

/// Every id `parts` name, ascending: one read of each edge page.
///
/// # Errors
///
/// As [`edge_page`].
pub fn edge_ids(access: &Access<'_>, parts: &[EdgePart]) -> Result<Vec<ObjectId>, HeapFault> {
    let mut ids = Vec::new();
    for part in parts {
        ids.extend(edge_page(access, part)?.0.into_iter().map(|(id, _)| id));
    }
    Ok(ids)
}

/// Checks the index page `index` of part `part` of `row`, when `row` is
/// a tuple's, before any member under it is installed (seal design,
/// "Tuple identity and loads"): its chunks are positional and their
/// leaves make the row's level-one nodes the page spans. The row's nodes
/// are checked against its root and the head when it opens.
///
/// # Errors
///
/// [`HeapFault::Corrupt`] for a page that is not the tuple's.
pub fn check_tuple_index(row: &Chunked, part: usize, index: &IndexPage) -> Result<(), HeapFault> {
    let Some(tuple) = &row.tuple else {
        return Ok(());
    };
    let page = row.parts[part].page;
    let first = part * INDEX_REFS;
    let chunks = identity::tuple_leaves(row.len) as usize;
    let mut leaves = Vec::with_capacity(index.0.len());
    for (at, chunk) in index.0.iter().enumerate() {
        let position = first + at;
        let want = if position + 1 == chunks {
            row.len as usize - position * CHUNK
        } else {
            CHUNK
        };
        let leaf = chunk
            .leaf
            .filter(|_| chunk.len as usize == want)
            .ok_or_else(|| corrupt(page, "a tuple's index page is not positional"))?;
        leaves.push(leaf);
    }
    let node = part * PAGE_NODES;
    let nodes = identity::tuple_parents(row.len, CHUNK as u64, node as u64, &leaves);
    if tuple.nodes.get(node..node + nodes.len()) != Some(&nodes[..]) {
        return Err(corrupt(
            page,
            "a tuple's index page is not the one its row commits to",
        ));
    }
    Ok(())
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
    position: u32,
    chunk: &ChunkRef,
    page: &ChunkPage,
) -> Result<(), HeapFault> {
    if row.tuple.is_none() {
        return Ok(());
    }
    let Members::List(members) = &page.members else {
        return Err(corrupt(
            chunk.page,
            "a tuple's chunk holds a table's entries",
        ));
    };
    let start = u64::from(position) * CHUNK as u64;
    if identity::chunk_leaf(start, members, &page.heads) != chunk.leaf {
        return Err(corrupt(
            chunk.page,
            "a tuple's chunk is not the one its index page commits to",
        ));
    }
    Ok(())
}

/// Calls `each` with the members of every chunk of `chunked`, in order:
/// one read of each index page and each chunk, a tuple's checked against
/// its content index.
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
    if let Some(tuple) = &chunked.tuple
        && identity::tuple_root(chunked.len, &tuple.nodes) != tuple.root
    {
        return Err(HeapFault::Malformed(
            "a chunked tuple's nodes do not make its root".into(),
        ));
    }
    let mut position = 0;
    for (at, part) in chunked.parts.iter().enumerate() {
        let answer = access.read(part.page)?;
        if answer.kind != PageKind::SEQUENCE_INDEX {
            return Err(corrupt(part.page, "an index part is not an index page"));
        }
        let mut index: IndexPage =
            decode_body(&answer.tokens).map_err(|error| corrupt(part.page, error.to_string()))?;
        check_tuple_index(chunked, at, &index)?;
        for chunk in &mut index.0 {
            super::super::source::grant(&mut chunk.page, &answer.children)?;
            let read = access.read(chunk.page)?;
            if read.kind != PageKind::SEQUENCE_LEAF {
                return Err(corrupt(chunk.page, "a chunk is not a leaf page"));
            }
            let page: ChunkPage = decode_body(&read.tokens)
                .map_err(|error| corrupt(chunk.page, error.to_string()))?;
            check_tuple_chunk(chunked, position, chunk, &page)?;
            position += 1;
            each(&page.members);
        }
    }
    Ok(())
}

/// The edge parts of `parts` with `delta` applied to the counts: only the
/// edge pages an id of `delta` falls in are read and written again.
///
/// # Errors
///
/// As [`edge_page`]; [`HeapFault::Malformed`] when a count would fall
/// below zero.
pub fn rewrite_edges(
    access: &Access<'_>,
    parts: &[EdgePart],
    delta: &BTreeMap<ObjectId, i64>,
) -> Result<Vec<EdgePart>, HeapFault> {
    let mut touched: BTreeMap<usize, Vec<(ObjectId, i64)>> = BTreeMap::new();
    for (id, change) in delta.iter().filter(|(_, change)| **change != 0) {
        let at = parts
            .iter()
            .rposition(|part| part.first <= *id)
            .unwrap_or(0);
        touched.entry(at).or_default().push((*id, *change));
    }
    let mut out = Vec::with_capacity(parts.len() + 1);
    let write = |edges: BTreeMap<ObjectId, u64>, out: &mut Vec<EdgePart>| {
        let edges: Vec<(ObjectId, u64)> = edges.into_iter().filter(|(_, n)| *n > 0).collect();
        let width = if edges.len() > EDGE_REFS {
            EDGE_FILL
        } else {
            EDGE_REFS
        };
        for run in edges.chunks(width) {
            let page = EdgePage(run.to_vec());
            out.push(EdgePart {
                first: run[0].0,
                ids: run.len() as u32,
                page: access.seal_kept(PageKind::KEYED_LEAF, encode_body(&page))?,
            });
        }
        Ok::<(), HeapFault>(())
    };
    if parts.is_empty() {
        let mut edges = BTreeMap::new();
        for (id, change) in touched.into_values().flatten() {
            let count = u64::try_from(change)
                .map_err(|_| HeapFault::Malformed("an edge count falls below zero".into()))?;
            edges.insert(id, count);
        }
        write(edges, &mut out)?;
        return Ok(out);
    }
    for (at, part) in parts.iter().enumerate() {
        let Some(changes) = touched.remove(&at) else {
            out.push(*part);
            continue;
        };
        let mut edges: BTreeMap<ObjectId, u64> = edge_page(access, part)?.0.into_iter().collect();
        for (id, change) in changes {
            let count = edges.entry(id).or_insert(0);
            *count = count
                .checked_add_signed(change)
                .ok_or_else(|| HeapFault::Malformed("an edge count falls below zero".into()))?;
        }
        write(edges, &mut out)?;
    }
    Ok(out)
}

/// A large list's or table's row record: its kind and length, the next
/// insertion sequence of a table, its index and the edge pages holding
/// the exact multiset of ids its chunks reference.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunked {
    pub kind: ObjectKind,
    /// Values of a list; entries of a table.
    pub len: u64,
    /// The sequence a table's next new entry takes; a list's length.
    pub next: u64,
    pub parts: Vec<Part>,
    /// The edge pages, ascending by first id.
    pub edges: Vec<EdgePart>,
    /// A tuple's content index; none for a list or a table.
    pub tuple: Option<TupleIndex>,
}

impl Chunked {
    /// How many chunks the object is in.
    pub fn chunks(&self) -> u64 {
        self.parts.iter().map(|part| u64::from(part.chunks)).sum()
    }

    /// Checks what decoding cannot: the kind is a list's or a table's, a
    /// table's index is in placement order and a list's names none, and
    /// the edges ascend once each.
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
        if self.parts.iter().any(|part| part.first.is_some() != table) {
            return Err("a chunked object's index places another kind's keys".into());
        }
        if self.parts.iter().any(|part| part.chunks == 0) {
            return Err("a chunked object's index names an empty part".into());
        }
        if !self
            .parts
            .windows(2)
            .all(|pair| pair[0].first <= pair[1].first)
        {
            return Err("a chunked table's index is out of placement order".into());
        }
        if !table && self.next != self.len {
            return Err("a chunked sequence's next is not its length".into());
        }
        match (&self.tuple, self.kind) {
            (None, ObjectKind::Tuple) | (Some(_), ObjectKind::List) => {
                return Err("a chunked sequence's content index is another kind's".into());
            }
            (None, _) => {}
            (Some(index), _) => {
                let chunks = identity::tuple_leaves(self.len);
                let last = self.parts.len().saturating_sub(1);
                if self.chunks() != chunks
                    || self
                        .parts
                        .iter()
                        .enumerate()
                        .any(|(at, part)| at < last && part.chunks as usize != INDEX_REFS)
                    || index.nodes.len() as u64 != chunks.div_ceil(identity::FANOUT as u64)
                {
                    return Err("a chunked tuple's index is not positional".into());
                }
            }
        }
        if self.len > self.next {
            return Err("a chunked table holds more entries than it took".into());
        }
        if !self
            .edges
            .windows(2)
            .all(|pair| pair[0].first < pair[1].first)
            || self
                .edges
                .iter()
                .any(|part| part.ids == 0 || part.ids as usize > EDGE_REFS)
        {
            return Err("a chunked object's edge pages are out of order or bound".into());
        }
        Ok(())
    }
}

impl TokenCodec for Chunked {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(6);
        self.kind.encode(out);
        out.unsigned(self.len);
        out.unsigned(self.next);
        self.parts.encode(out);
        self.edges.encode(out);
        match &self.tuple {
            None => out.sum(0, 0),
            Some(index) => {
                out.sum(1, 1);
                index.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        let kind = ObjectKind::decode(input)?;
        let len = input.unsigned()?;
        let next = input.unsigned()?;
        let parts = Vec::<Part>::decode(input)?;
        let edges = Vec::<EdgePart>::decode(input)?;
        let (tag, fields) = input.sum()?;
        let tuple = match tag {
            0 => {
                TokenReader::expect_fields(fields, 0)?;
                None
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Some(TupleIndex::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        };
        let chunked = Self {
            kind,
            len,
            next,
            parts,
            edges,
            tuple,
        };
        chunked.check().map_err(TokenError::Invalid)?;
        Ok(chunked)
    }
}

/// An index page: the chunks one part names, in chunk order.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexPage(pub Vec<ChunkRef>);

impl TokenCodec for IndexPage {
    fn encode(&self, out: &mut TokenWriter) {
        self.0.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let refs = Vec::<ChunkRef>::decode(input)?;
        if refs.is_empty() || refs.len() > INDEX_REFS {
            return Err(TokenError::Invalid(
                "an index page names no chunk, or more than it may".into(),
            ));
        }
        Ok(Self(refs))
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
