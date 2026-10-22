//! Encoding: placement, chunking, streaming and staging (FIG-5887 §2, §3).
//!
//! A typed codec writes a **logical** body: canonical tokens whose child
//! references are raw digests, and whose text and bytes are written in full
//! whatever their length. [`GraphStaging::encode`] turns it into nodes:
//!
//! 1. **Placement.** In an enveloped kind, a text or bytes token longer than
//!    [`INLINE_TOKEN_MAX`] bytes, tag and length included, becomes a child
//!    reference to a TextLeaf or BytesLeaf, or to the index over its chunks.
//!    Chunks hold at most [`LEAF_PAYLOAD_MAX`] bytes; a text chunk ends at
//!    the last UTF-8 boundary at or below that. Equal text anywhere makes
//!    equal leaves, so it is stored once.
//! 2. **Indexing.** Leaves group 32 at a time, bottom up, into index nodes
//!    that record the total length and each child's length; the last group
//!    of a level may be short, and the single node of the top level is the
//!    value's root.
//! 3. **Inline or stream.** An enveloped body is `Inline(tokens)` when the
//!    whole node fits its bound and `Stream(ref<WholeStream>)` otherwise:
//!    the logical tokens are cut into StructureBlocks, each the longest
//!    run of whole token events whose envelope fits [`MAX_NODE_BYTES`],
//!    indexed 32 at a time by StreamIndex nodes.
//! 4. **Slots.** Each node's children are its referenced digests, sorted and
//!    unique; each reference becomes its child's slot.
//!
//! Every node made is staged until the commit that installs it is known to
//! have succeeded, and every new staged byte is charged to the encoder's
//! memory lease first.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use lash_kernel_doc::graph_tokens::{
    CanonicalFault, ChildRef, TokenCodec, TokenEvent, TokenReader, TokenWriter, encode_body,
    logical_digest, put_varint, varint_len,
};

use super::node::{
    EncodedNode, INDEX_FANOUT, INLINE_TOKEN_MAX, LEAF_PAYLOAD_MAX, MAX_NODE_BYTES, NodeId,
    envelope_len, parse_envelope, write_envelope,
};
use super::refusal::GraphRefusal;
use super::registry::{BodyShape, NodeKind};
use crate::EncodeBudget;
use crate::allocation::{AllocationBudget, AllocationHold};

/// A logical body: one typed value's canonical tokens, references as
/// digests, values unplaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypedBody<'a>(&'a [u8]);

impl<'a> TypedBody<'a> {
    #[must_use]
    pub const fn logical(tokens: &'a [u8]) -> Self {
        Self(tokens)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.0
    }

    /// The body's logical digest: what the encoder keys the nodes it makes
    /// from the body by, and the identity of a document or definition whose
    /// body this is (`K-ID-001`, `K-ID-002`).
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        logical_digest(self.0)
    }
}

/// The nodes an encoder made and has not yet seen installed.
#[derive(Clone, Debug)]
pub struct GraphStaging {
    nodes: BTreeMap<NodeId, EncodedNode>,
    budget: EncodeBudget,
    cache: Arc<Mutex<EncodingCache>>,
    recorded: Option<Vec<EncodedNode>>,
    allocation_budget: AllocationBudget,
}

// Canonical encoding depends only on the operation and its complete input,
// never on a store, owner, release catalog or decoder. Keep its immutable
// envelopes across captures and canonical read checks. Runtime reads still
// fetch and check every supplied envelope before deriving their logical body.
const CACHE_BYTES: usize = 32 << 20;
const CACHE_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EncodingKind {
    Node(NodeKind),
    Text,
    Bytes,
}

type EncodingKey = (EncodingKind, [u8; 32]);

struct PreparedEncoding {
    input: Box<[u8]>,
    root: NodeId,
    // In insertion order, including repeats: replaying this order preserves
    // partial staging and the first memory-lease refusal.
    nodes: Vec<EncodedNode>,
    bytes: usize,
}

#[derive(Default)]
struct EncodingCache {
    entries: BTreeMap<EncodingKey, (Arc<PreparedEncoding>, u64)>,
    bytes: usize,
    clock: u64,
    _allocation: AllocationHold,
}

impl std::fmt::Debug for EncodingCache {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("EncodingCache")
            .field("entries", &self.entries.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl EncodingCache {
    fn get(&mut self, key: &EncodingKey, input: &[u8]) -> Option<Arc<PreparedEncoding>> {
        let (prepared, used) = self.entries.get_mut(key)?;
        if prepared.input.as_ref() != input {
            return None;
        }
        self.clock = self.clock.wrapping_add(1);
        *used = self.clock;
        Some(Arc::clone(prepared))
    }

    fn insert(&mut self, key: EncodingKey, input: &[u8], root: NodeId, nodes: Vec<EncodedNode>) {
        let bytes = input.len().saturating_add(
            nodes
                .iter()
                .map(|node| node.bytes.len() + std::mem::size_of::<EncodedNode>())
                .sum::<usize>(),
        );
        if bytes > CACHE_BYTES {
            return;
        }
        if let Some((previous, _)) = self.entries.remove(&key) {
            self.bytes -= previous.bytes;
        }
        while self.entries.len() >= CACHE_ENTRIES || self.bytes + bytes > CACHE_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some((removed, _)) = self.entries.remove(&oldest) {
                self.bytes -= removed.bytes;
            }
        }
        self.clock = self.clock.wrapping_add(1);
        self.entries.insert(
            key,
            (
                Arc::new(PreparedEncoding {
                    input: input.into(),
                    root,
                    nodes,
                    bytes,
                }),
                self.clock,
            ),
        );
        self.bytes += bytes;
    }
}

fn encoding_cache() -> Arc<Mutex<EncodingCache>> {
    static CACHE: OnceLock<Arc<Mutex<EncodingCache>>> = OnceLock::new();
    Arc::clone(CACHE.get_or_init(|| Arc::new(Mutex::new(EncodingCache::default()))))
}

/// Encodes one node of `kind` into `staging` and returns it. Placed leaves,
/// indexes and stream blocks it needs are staged beside it.
///
/// # Errors
///
/// As [`GraphStaging::encode`].
pub fn encode_node(
    kind: NodeKind,
    body: TypedBody<'_>,
    staging: &mut GraphStaging,
) -> Result<EncodedNode, GraphRefusal> {
    let id = staging.encode(kind, body)?;
    staging
        .get(&id)
        .cloned()
        .ok_or(GraphRefusal::MissingNode(id))
}

/// The length of a text or bytes token holding `len` bytes.
pub fn token_len(len: usize) -> u64 {
    1 + varint_len(len as u64) as u64 + len as u64
}

impl GraphStaging {
    #[must_use]
    pub fn new(budget: EncodeBudget) -> Self {
        Self {
            nodes: BTreeMap::new(),
            budget,
            cache: encoding_cache(),
            recorded: None,
            allocation_budget: AllocationBudget::unowned(),
        }
    }

    /// Construct serving staging under the same ledger as its source owner.
    pub fn new_in(
        budget: EncodeBudget,
        allocation_budget: &AllocationBudget,
    ) -> Result<Self, GraphRefusal> {
        let allocation =
            allocation_budget.reserve((std::mem::size_of::<EncodingCache>() + 64) as u64)?;
        Ok(Self {
            nodes: BTreeMap::new(),
            budget,
            cache: Arc::new(Mutex::new(EncodingCache {
                _allocation: allocation,
                ..EncodingCache::default()
            })),
            recorded: None,
            allocation_budget: allocation_budget.clone(),
        })
    }

    pub fn allocation_budget(&self) -> &AllocationBudget {
        &self.allocation_budget
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn isolated(budget: EncodeBudget) -> Self {
        Self {
            cache: Arc::new(Mutex::new(EncodingCache::default())),
            ..Self::new(budget)
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn sharing_cache(&self, budget: EncodeBudget) -> Self {
        Self {
            cache: Arc::clone(&self.cache),
            ..Self::new(budget)
        }
    }

    /// Encodes one node of `kind` from its logical body and returns its
    /// identity.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::UnsupportedKind`] for a kind 1.0 does not register;
    /// a token refusal for a malformed body; [`CanonicalFault::NotOneToken`]
    /// for a body that is not exactly one token (a structure block's
    /// excepted); [`GraphRefusal::EncodedNodeTooLarge`] or
    /// [`GraphRefusal::RootTooLarge`] for a direct body over its bound;
    /// [`GraphRefusal::EncodeMemoryLimit`] past the lease.
    pub fn encode(&mut self, kind: NodeKind, body: TypedBody<'_>) -> Result<NodeId, GraphRefusal> {
        self.memoized(EncodingKind::Node(kind), body.as_bytes(), |staging| {
            staging.encode_body(kind, body.as_bytes())
        })
    }

    fn encode_body(&mut self, kind: NodeKind, logical: &[u8]) -> Result<NodeId, GraphRefusal> {
        let spec = kind.spec().ok_or(GraphRefusal::UnsupportedKind(kind.0))?;
        match spec.shape {
            BodyShape::Direct => {
                if kind != NodeKind::STRUCTURE_BLOCK {
                    one_token(logical, kind)?;
                }
                let (children, physical) = to_physical(logical, kind)?;
                self.insert(kind, &children, physical)
            }
            BodyShape::Enveloped => {
                let placed = self.place(logical, kind)?;
                one_token(&placed, kind)?;
                let (children, physical) = to_physical(&placed, kind)?;
                let mut inline = Vec::with_capacity(physical.len() + 3);
                inline.extend_from_slice(&[0x0c, 0x00, 0x01]);
                inline.extend_from_slice(&physical);
                if envelope_len(kind, children.len(), inline.len()) as u64 <= spec.bound.bytes() {
                    return self.insert(kind, &children, inline);
                }
                let stream = self.stream(kind, &placed)?;
                self.insert(kind, &[stream.0], vec![0x0c, 0x01, 0x01, 0x0b, 0x00])
            }
        }
    }

    /// Encodes `value` as one node of `kind`.
    ///
    /// # Errors
    ///
    /// As [`Self::encode`].
    pub fn encode_value<T: TokenCodec>(
        &mut self,
        kind: NodeKind,
        value: &T,
    ) -> Result<NodeId, GraphRefusal> {
        let logical = encode_body(value);
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_graph_serialized_body(logical.len());
        self.encode(kind, TypedBody::logical(&logical))
    }

    /// Stages a node another encoder made (a worker's, say) after checking
    /// it hashes to its identity and is well formed.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::HashMismatch`], any refusal of the envelope, or
    /// [`GraphRefusal::EncodeMemoryLimit`].
    pub fn adopt(&mut self, node: EncodedNode) -> Result<(), GraphRefusal> {
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_graph_adopt(node.bytes.len());
        if NodeId::of_envelope(&node.bytes) != node.id {
            return Err(GraphRefusal::HashMismatch(node.id));
        }
        if node.len() > MAX_NODE_BYTES {
            return Err(GraphRefusal::EncodedNodeTooLarge {
                bytes: node.len(),
                limit: MAX_NODE_BYTES,
            });
        }
        parse_envelope(&node.bytes, Some(node.id))?;
        if !self.nodes.contains_key(&node.id) {
            self.budget.charge(node.len())?;
            self.nodes.insert(node.id, node);
        }
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: &NodeId) -> Option<&EncodedNode> {
        self.nodes.get(id)
    }

    #[must_use]
    pub fn nodes(&self) -> &BTreeMap<NodeId, EncodedNode> {
        &self.nodes
    }

    #[must_use]
    pub fn into_nodes(self) -> BTreeMap<NodeId, EncodedNode> {
        self.nodes
    }

    #[must_use]
    pub fn budget(&self) -> &EncodeBudget {
        &self.budget
    }

    /// Envelope bytes of every staged node.
    #[must_use]
    pub fn staged_bytes(&self) -> u64 {
        self.nodes.values().map(EncodedNode::len).sum()
    }

    fn insert(
        &mut self,
        kind: NodeKind,
        children: &[[u8; 32]],
        body: Vec<u8>,
    ) -> Result<NodeId, GraphRefusal> {
        let bound = kind
            .spec()
            .map_or(MAX_NODE_BYTES, |spec| spec.bound.bytes());
        let len = envelope_len(kind, children.len(), body.len()) as u64;
        if len > bound {
            return Err(if bound < MAX_NODE_BYTES {
                GraphRefusal::RootTooLarge {
                    bytes: len,
                    limit: bound,
                }
            } else {
                GraphRefusal::EncodedNodeTooLarge {
                    bytes: len,
                    limit: bound,
                }
            });
        }
        let bytes = write_envelope(kind, children, &body);
        let id = NodeId::of_envelope(&bytes);
        self.stage_prepared(EncodedNode {
            id,
            bytes: bytes.into(),
        })?;
        Ok(id)
    }

    fn stage_prepared(&mut self, node: EncodedNode) -> Result<(), GraphRefusal> {
        if !self.nodes.contains_key(&node.id) {
            self.budget.charge(node.len())?;
            self.nodes.insert(node.id, node.clone());
        }
        if let Some(recorded) = &mut self.recorded {
            recorded.push(node);
        }
        Ok(())
    }

    fn memoized(
        &mut self,
        kind: EncodingKind,
        input: &[u8],
        encode: impl FnOnce(&mut Self) -> Result<NodeId, GraphRefusal>,
    ) -> Result<NodeId, GraphRefusal> {
        if input.len() > CACHE_BYTES {
            return encode(self);
        }
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_hash_pass(input.len());
        let key = (kind, TypedBody::logical(input).digest());
        let prepared = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key, input);
        if let Some(prepared) = prepared {
            for node in &prepared.nodes {
                self.stage_prepared(node.clone())?;
            }
            return Ok(prepared.root);
        }
        let outermost = self.recorded.is_none();
        let start = self.recorded.get_or_insert_with(Vec::new).len();
        let result = encode(self);
        if let Ok(root) = result {
            let nodes = self
                .recorded
                .as_ref()
                .map(|nodes| nodes[start..].to_vec())
                .unwrap_or_default();
            self.cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, input, root, nodes);
        }
        if outermost {
            self.recorded = None;
        }
        result
    }

    /// Replaces every text and bytes token over the inline bound with a
    /// reference to its placed value.
    fn place(&mut self, logical: &[u8], kind: NodeKind) -> Result<Vec<u8>, GraphRefusal> {
        let mut reader = TokenReader::new(logical);
        let mut out = TokenWriter::with_capacity(logical.len().min(MAX_NODE_BYTES as usize));
        while !reader.is_empty() {
            let start = reader.position();
            let event = reader
                .next_event()
                .map_err(|error| GraphRefusal::from_token(error, None, Some(kind.0)))?;
            match event {
                TokenEvent::Text(text) if token_len(text.len()) > INLINE_TOKEN_MAX => {
                    let root = self.place_text(text)?;
                    out.child(root.child_ref());
                }
                TokenEvent::Bytes(bytes) if token_len(bytes.len()) > INLINE_TOKEN_MAX => {
                    let root = self.place_bytes(bytes)?;
                    out.child(root.child_ref());
                }
                _ => out.raw(&logical[start..reader.position()]),
            }
        }
        Ok(out.into_bytes())
    }

    /// Stages `text`'s leaves and index and returns its root, whatever its
    /// length: the caller has decided it is placed. A text that fits one
    /// leaf is that one TextLeaf. The runtime graph decoder reads it back.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::EncodeMemoryLimit`] past the lease.
    ///
    pub fn place_text(&mut self, text: &str) -> Result<NodeId, GraphRefusal> {
        self.memoized(EncodingKind::Text, text.as_bytes(), |staging| {
            staging.encode_text(text)
        })
    }

    fn encode_text(&mut self, text: &str) -> Result<NodeId, GraphRefusal> {
        let mut leaves = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(LEAF_PAYLOAD_MAX);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let (chunk, tail) = rest.split_at(end);
            let mut body = TokenWriter::with_capacity(token_len(chunk.len()) as usize);
            body.text(chunk);
            let logical = body.into_bytes();
            let leaf = self.encode(NodeKind::TEXT_LEAF, TypedBody::logical(&logical))?;
            leaves.push((leaf, chunk.len() as u64));
            rest = tail;
        }
        self.value_index(NodeKind::TEXT_INDEX, leaves)
    }

    /// Stages `bytes`' leaves and index and returns its root.
    pub fn place_bytes(&mut self, bytes: &[u8]) -> Result<NodeId, GraphRefusal> {
        self.memoized(EncodingKind::Bytes, bytes, |staging| {
            staging.encode_bytes(bytes)
        })
    }

    fn encode_bytes(&mut self, bytes: &[u8]) -> Result<NodeId, GraphRefusal> {
        let mut leaves = Vec::new();
        for chunk in bytes.chunks(LEAF_PAYLOAD_MAX) {
            let mut body = TokenWriter::with_capacity(token_len(chunk.len()) as usize);
            body.bytes(chunk);
            let logical = body.into_bytes();
            let leaf = self.encode(NodeKind::BYTES_LEAF, TypedBody::logical(&logical))?;
            leaves.push((leaf, chunk.len() as u64));
        }
        self.value_index(NodeKind::BYTES_INDEX, leaves)
    }

    /// Groups a placed value's leaves 32 at a time, level by level, until
    /// one node remains. An empty value has no leaves and is never placed.
    fn value_index(
        &mut self,
        kind: NodeKind,
        mut level: Vec<(NodeId, u64)>,
    ) -> Result<NodeId, GraphRefusal> {
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(INDEX_FANOUT));
            for group in level.chunks(INDEX_FANOUT) {
                let total: u64 = group.iter().map(|(_, len)| len).sum();
                let mut body = TokenWriter::new();
                body.seq(2);
                body.unsigned(total);
                body.seq(group.len() as u64);
                for (child, len) in group {
                    body.seq(2);
                    body.child(child.child_ref());
                    body.unsigned(*len);
                }
                let logical = body.into_bytes();
                let (children, physical) = to_physical(&logical, kind)?;
                next.push((self.insert(kind, &children, physical)?, total));
            }
            level = next;
        }
        level
            .first()
            .map(|(root, _)| *root)
            .ok_or(GraphRefusal::NonCanonical {
                node: None,
                reason: CanonicalFault::InvalidChunking,
            })
    }

    /// Streams a placed logical body of `kind` through structure blocks and
    /// returns its WholeStream.
    pub fn stream(&mut self, kind: NodeKind, placed: &[u8]) -> Result<NodeId, GraphRefusal> {
        let events = scan(placed, kind)?;
        let mut blocks = Vec::new();
        let mut start = 0;
        while start < events.len() {
            let end = longest_block(&events, start);
            if end == start {
                return Err(GraphRefusal::EncodedNodeTooLarge {
                    bytes: block_len(&events[start..=start]) as u64,
                    limit: MAX_NODE_BYTES,
                });
            }
            let slice = &events[start..end];
            let logical = &placed[slice[0].start..slice[slice.len() - 1].end];
            let (children, physical) = to_physical(logical, NodeKind::STRUCTURE_BLOCK)?;
            blocks.push((
                self.insert(NodeKind::STRUCTURE_BLOCK, &children, physical)?,
                1_u64,
            ));
            start = end;
        }
        let block_count = blocks.len() as u64;
        let mut level = blocks;
        let mut depth = 1_u64;
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(INDEX_FANOUT));
            for group in level.chunks(INDEX_FANOUT) {
                let count: u64 = group.iter().map(|(_, count)| count).sum();
                let mut body = TokenWriter::new();
                body.seq(3);
                body.unsigned(depth);
                body.unsigned(count);
                body.seq(group.len() as u64);
                for (child, count) in group {
                    body.seq(2);
                    body.child(child.child_ref());
                    body.unsigned(*count);
                }
                let logical = body.into_bytes();
                let (children, physical) = to_physical(&logical, NodeKind::STREAM_INDEX)?;
                next.push((
                    self.insert(NodeKind::STREAM_INDEX, &children, physical)?,
                    count,
                ));
            }
            level = next;
            depth += 1;
        }
        let tree = level
            .first()
            .map(|(root, _)| *root)
            .ok_or(GraphRefusal::NonCanonical {
                node: None,
                reason: CanonicalFault::NotOneToken,
            })?;
        let mut body = TokenWriter::new();
        body.seq(4);
        body.unsigned(u64::from(kind.0));
        body.unsigned(placed.len() as u64);
        body.child(tree.child_ref());
        body.unsigned(block_count);
        let logical = body.into_bytes();
        let (children, physical) = to_physical(&logical, NodeKind::WHOLE_STREAM)?;
        self.insert(NodeKind::WHOLE_STREAM, &children, physical)
    }
}

/// Refuses a logical body that is not exactly one token tree.
fn one_token(logical: &[u8], kind: NodeKind) -> Result<(), GraphRefusal> {
    let mut reader = TokenReader::new(logical);
    reader
        .skip_tree()
        .map_err(|error| GraphRefusal::from_token(error, None, Some(kind.0)))?;
    if reader.is_empty() {
        Ok(())
    } else {
        Err(GraphRefusal::NonCanonical {
            node: None,
            reason: CanonicalFault::NotOneToken,
        })
    }
}

/// A logical body's sorted, unique children and its physical form.
pub fn to_physical(
    logical: &[u8],
    kind: NodeKind,
) -> Result<(Vec<[u8; 32]>, Vec<u8>), GraphRefusal> {
    let references = References::scan(logical, kind)?;
    let mut body = Vec::with_capacity(references.physical_len(logical.len()));
    references.write_physical(logical, &mut body);
    Ok((references.children, body))
}

/// A logical reference's length: its tag and its child's digest.
const LOGICAL_REF_LEN: usize = 1 + 32;

/// The child references of a logical body, read in one pass: where each
/// starts and what it names, and the body's children, sorted and unique.
pub(crate) struct References {
    at: Vec<(usize, [u8; 32])>,
    pub(crate) children: Vec<[u8; 32]>,
}

impl References {
    /// Reads every token of `logical`, each of which must be canonical.
    pub(crate) fn scan(logical: &[u8], kind: NodeKind) -> Result<Self, GraphRefusal> {
        let mut at = Vec::new();
        let mut reader = TokenReader::new(logical);
        while !reader.is_empty() {
            let start = reader.position();
            if let TokenEvent::Child(ChildRef(digest)) = reader
                .next_event()
                .map_err(|error| GraphRefusal::from_token(error, None, Some(kind.0)))?
            {
                at.push((start, digest));
            }
        }
        let mut children: Vec<[u8; 32]> = at.iter().map(|(_, digest)| *digest).collect();
        children.sort_unstable();
        children.dedup();
        Ok(Self { at, children })
    }

    fn slot(&self, digest: &[u8; 32]) -> u64 {
        self.children.partition_point(|child| child < digest) as u64
    }

    /// The length of the physical form of `logical`, `logical_len` bytes:
    /// each reference its tag and its child's slot.
    pub(crate) fn physical_len(&self, logical_len: usize) -> usize {
        self.at.iter().fold(logical_len, |len, (_, digest)| {
            len - LOGICAL_REF_LEN + 1 + varint_len(self.slot(digest))
        })
    }

    /// Appends the physical form of `logical`, the body these references
    /// were read from, to `out`.
    pub(crate) fn write_physical(&self, logical: &[u8], out: &mut Vec<u8>) {
        let mut copied = 0;
        for (start, digest) in &self.at {
            out.extend_from_slice(&logical[copied..*start]);
            // The reference's own tag, then its slot in place of its digest.
            out.push(logical[*start]);
            put_varint(out, self.slot(digest));
            copied = start + LOGICAL_REF_LEN;
        }
        out.extend_from_slice(&logical[copied..]);
    }
}

/// One token event of a logical body: its byte range and, for a
/// reference, its child.
#[derive(Clone, Copy, Debug)]
struct Event {
    start: usize,
    end: usize,
    child: Option<[u8; 32]>,
}

fn scan(logical: &[u8], kind: NodeKind) -> Result<Vec<Event>, GraphRefusal> {
    let mut events = Vec::new();
    let mut reader = TokenReader::new(logical);
    while !reader.is_empty() {
        let start = reader.position();
        let event = reader
            .next_event()
            .map_err(|error| GraphRefusal::from_token(error, None, Some(kind.0)))?;
        events.push(Event {
            start,
            end: reader.position(),
            child: match event {
                TokenEvent::Child(ChildRef(digest)) => Some(digest),
                _ => None,
            },
        });
    }
    Ok(events)
}

/// The envelope length of one structure block holding `events`.
fn block_len(events: &[Event]) -> usize {
    let children: BTreeSet<[u8; 32]> = events.iter().filter_map(|event| event.child).collect();
    let children: Vec<[u8; 32]> = children.into_iter().collect();
    let body: usize = events
        .iter()
        .map(|event| match event.child {
            Some(digest) => {
                let slot = children.binary_search(&digest).unwrap_or(0);
                1 + varint_len(slot as u64)
            }
            None => event.end - event.start,
        })
        .sum();
    envelope_len(NodeKind::STRUCTURE_BLOCK, children.len(), body)
}

/// The end of the longest run of whole events from `start` whose block fits
/// [`MAX_NODE_BYTES`]. A block's length never shrinks as it grows, so the
/// boundary is bracketed by doubling and then found by bisection.
fn longest_block(events: &[Event], start: usize) -> usize {
    let fits = |end: usize| block_len(&events[start..end]) as u64 <= MAX_NODE_BYTES;
    // Invariant: `low` fits (or is `start`, the empty run); `high` does not,
    // or is one past the last event.
    let (mut low, mut step) = (start, 1_usize);
    let mut high = loop {
        let probe = start.saturating_add(step).min(events.len());
        if !fits(probe) {
            break probe;
        }
        low = probe;
        if probe == events.len() {
            return probe;
        }
        step = step.saturating_mul(2);
    };
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if fits(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    low
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// LO-EVICT-ENCODER: refuse cache metadata before claiming its allocation;
    /// the obligation remains live until the final shared staging is dropped.
    #[test]
    fn serving_staging_admits_its_cache_before_claiming_it() {
        use crate::allocation::{
            AllocationError, AllocationLedger, AllocationReservation, ReserveError,
        };
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        #[derive(Clone)]
        struct Ledger {
            refused: Arc<AtomicBool>,
            used: Arc<AtomicU64>,
        }
        struct Hold {
            used: Arc<AtomicU64>,
            bytes: u64,
        }
        impl AllocationReservation for Hold {
            fn retain(&self) {}
        }
        impl Drop for Hold {
            fn drop(&mut self) {
                self.used.fetch_sub(self.bytes, Ordering::SeqCst);
            }
        }
        impl AllocationLedger for Ledger {
            fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
                if self.refused.load(Ordering::SeqCst) {
                    return Err(AllocationError {
                        kind: ReserveError::Pressure,
                        actor: "encoder".into(),
                        requested: bytes,
                        capacity: 1,
                    });
                }
                self.used.fetch_add(bytes, Ordering::SeqCst);
                Ok(AllocationHold::new(Hold {
                    used: self.used.clone(),
                    bytes,
                }))
            }
            fn reserve_shared(
                &self,
                _: usize,
                bytes: u64,
            ) -> Result<AllocationHold, AllocationError> {
                self.reserve(bytes)
            }
        }
        let ledger = Ledger {
            refused: Arc::new(AtomicBool::new(true)),
            used: Arc::new(AtomicU64::new(0)),
        };
        let budget = AllocationBudget::new(ledger.clone());
        let encoding = || EncodeBudget::new(crate::ExplicitLimit::new(u64::MAX));
        assert!(matches!(
            GraphStaging::new_in(encoding(), &budget),
            Err(GraphRefusal::Allocation(AllocationError {
                kind: ReserveError::Pressure,
                ..
            }))
        ));
        ledger.refused.store(false, Ordering::SeqCst);
        let staging = GraphStaging::new_in(encoding(), &budget).expect("admitted cache");
        assert_eq!(staging.allocation_budget().identity(), budget.identity());
        assert!(ledger.used.load(Ordering::SeqCst) >= std::mem::size_of::<EncodingCache>() as u64);
        let shared = staging.clone();
        drop(staging);
        assert!(ledger.used.load(Ordering::SeqCst) > 0);
        drop(shared);
        assert_eq!(ledger.used.load(Ordering::SeqCst), 0);
    }

    /// Preparation residency is bounded by both retained bytes and entries;
    /// eviction cannot change an encoding and an oversized input runs uncached.
    #[test]
    fn graph_preparation_residency_is_bounded() {
        let mut cache = EncodingCache::default();
        let root = NodeId([0; 32]);
        let key = |n: u32| (EncodingKind::Node(NodeKind(n)), [0; 32]);
        for n in 0..=CACHE_ENTRIES as u32 {
            cache.insert(key(n), b"input", root, Vec::new());
        }
        assert_eq!(cache.entries.len(), CACHE_ENTRIES);
        assert!(cache.get(&key(0), b"input").is_none());
        assert!(cache.get(&key(1), b"different").is_none());
        assert_eq!(cache.get(&key(1), b"input").unwrap().root, root);
        cache.insert(key(300), b"input", root, Vec::new());
        assert!(
            cache.get(&key(1), b"input").is_some(),
            "a hit updates recency"
        );
        assert!(cache.get(&key(2), b"input").is_none());

        cache.insert(key(301), &vec![0; CACHE_BYTES], root, Vec::new());
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, CACHE_BYTES);
        cache.insert(key(302), &vec![0; CACHE_BYTES + 1], root, Vec::new());
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.bytes <= CACHE_BYTES);
    }
}
