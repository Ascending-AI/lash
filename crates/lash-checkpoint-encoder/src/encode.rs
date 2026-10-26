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

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use lash_kernel_value::tokens::CanonicalFault;
use lash_kernel_value::tokens::ChildRef;
use lash_kernel_value::tokens::TokenCodec;
use lash_kernel_value::tokens::TokenError;
use lash_kernel_value::tokens::TokenEvent;
use lash_kernel_value::tokens::TokenReader;
use lash_kernel_value::tokens::TokenWriter;
use lash_kernel_value::tokens::encode_body;
use lash_kernel_value::tokens::logical_digest;
use lash_kernel_value::tokens::put_varint;
use lash_kernel_value::tokens::varint_len;

use super::node::{
    EncodedNode, Framing, INDEX_FANOUT, INLINE_TOKEN_MAX, LEAF_PAYLOAD_MAX, MAX_NODE_BYTES, NodeId,
    check_decoded, envelope_len, write_envelope,
};
use super::refusal::GraphRefusal;
use super::registry::{BodyShape, NodeKind};
use crate::EncodeBudget;

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
        #[cfg(any(test, feature = "testing"))]
        record_analysis_work(0, self.0.len());
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_hash_pass(self.0.len());
        logical_digest(self.0)
    }
}

/// Borrowed facts for one immutable token representation within one read.
/// This is an internal decoder/encoder seam: it grants no store admission.
/// Transformed bytes require a separate analysis; the digest and scan cannot
/// be supplied independently of the bytes they describe.
pub struct AnalyzedBody<'a> {
    bytes: &'a [u8],
    digest: OnceCell<[u8; 32]>,
    scan: OnceCell<Arc<LogicalScan>>,
}

impl<'a> AnalyzedBody<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            digest: OnceCell::new(),
            scan: OnceCell::new(),
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    fn digest(&self) -> [u8; 32] {
        *self
            .digest
            .get_or_init(|| TypedBody::logical(self.bytes).digest())
    }

    /// Canonical placement facts for exactly this borrowed representation.
    pub fn scan(&self, kind: NodeKind) -> Result<&LogicalScan, GraphRefusal> {
        if let Some(scan) = self.scan.get() {
            return Ok(scan);
        }
        let scan = LogicalScan::scan_with_digest(self.bytes, kind, || self.digest())?;
        Ok(self.scan.get_or_init(|| scan))
    }

    /// Inline framing size, without another scan or digest lookup.
    pub fn inline_envelope_len(&self, kind: NodeKind) -> Result<u64, GraphRefusal> {
        let scan = self.scan(kind)?;
        Ok(envelope_len(
            kind,
            scan.children.len(),
            scan.physical_len(self.bytes.len()) + 3,
        ) as u64)
    }

    /// Requires exactly one token tree, consuming already prepared tree facts.
    pub fn one_token(&self, kind: NodeKind) -> Result<(), GraphRefusal> {
        let Some(scan) = self.scan.get() else {
            return one_token(self.bytes, kind);
        };
        match scan.tree_end {
            Some(end) if end == self.bytes.len() => Ok(()),
            Some(_) => Err(GraphRefusal::NonCanonical {
                node: None,
                reason: CanonicalFault::NotOneToken,
            }),
            None => Err(GraphRefusal::from_token(
                TokenError::Truncated,
                None,
                Some(kind.0),
            )),
        }
    }

    fn to_physical(&self, kind: NodeKind) -> Result<(Vec<[u8; 32]>, Vec<u8>), GraphRefusal> {
        let scan = self.scan(kind)?;
        let mut body = Vec::with_capacity(scan.physical_len(self.bytes.len()));
        scan.write_physical(self.bytes, &mut body);
        Ok((scan.children.clone(), body))
    }
}

/// The nodes an encoder made and has not yet seen installed.
#[derive(Clone, Debug)]
pub struct GraphStaging {
    nodes: BTreeMap<NodeId, EncodedNode>,
    budget: EncodeBudget,
    cache: Arc<Mutex<EncodingCache>>,
    recorded: Option<Vec<EncodedNode>>,
}

// Canonical encoding depends only on the operation and its complete input,
// never on a store, owner, release catalog or decoder. Keep its immutable
// envelopes and framing across captures and canonical read checks. Runtime
// reads still fetch, bound, hash and compare every supplied envelope before
// reusing its framing or deriving its logical body.
const CACHE_BYTES: usize = 32 << 20;
const CACHE_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum EncodingKind {
    Node(NodeKind),
    Text,
    Bytes,
    Envelope,
    Logical(usize),
    LogicalScan,
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

enum PreparedGraph {
    Encoding(Arc<PreparedEncoding>),
    Envelope {
        input: Box<[u8]>,
        framing: Framing,
    },
    Logical {
        input: Box<[u8]>,
        tokens: Arc<[u8]>,
    },
    LogicalScan {
        input: Box<[u8]>,
        references: Arc<LogicalScan>,
    },
}

impl PreparedGraph {
    fn input(&self) -> &[u8] {
        match self {
            Self::Encoding(prepared) => &prepared.input,
            Self::Envelope { input, .. }
            | Self::Logical { input, .. }
            | Self::LogicalScan { input, .. } => input,
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Encoding(prepared) => prepared.bytes,
            Self::Envelope { input, .. } => input.len() + std::mem::size_of::<Framing>(),
            Self::Logical { input, tokens } => input.len() + tokens.len(),
            Self::LogicalScan { input, references } => {
                input.len()
                    + references.at.capacity() * std::mem::size_of::<(usize, [u8; 32])>()
                    + references.children.capacity() * std::mem::size_of::<[u8; 32]>()
                    + references.ends.capacity() * std::mem::size_of::<usize>()
                    + std::mem::size_of::<LogicalScan>()
            }
        }
    }
}

#[derive(Default)]
struct EncodingCache {
    entries: hashlink::LinkedHashMap<EncodingKey, PreparedGraph>,
    bytes: usize,
    #[cfg(test)]
    eviction_candidates: usize,
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
    fn get_prepared(&mut self, key: &EncodingKey, input: &[u8]) -> Option<&PreparedGraph> {
        match self.entries.raw_entry_mut().from_key(key) {
            hashlink::linked_hash_map::RawEntryMut::Occupied(mut entry) => {
                if entry.get().input() != input {
                    return None;
                }
                entry.to_back();
                Some(entry.into_mut())
            }
            hashlink::linked_hash_map::RawEntryMut::Vacant(_) => None,
        }
    }

    fn get(&mut self, key: &EncodingKey, input: &[u8]) -> Option<Arc<PreparedEncoding>> {
        match self.get_prepared(key, input)? {
            PreparedGraph::Encoding(prepared) => Some(Arc::clone(prepared)),
            PreparedGraph::Envelope { .. }
            | PreparedGraph::Logical { .. }
            | PreparedGraph::LogicalScan { .. } => None,
        }
    }

    fn envelope(&mut self, key: &EncodingKey, input: &[u8]) -> Option<Framing> {
        match self.get_prepared(key, input)? {
            PreparedGraph::Envelope { framing, .. } => Some(*framing),
            PreparedGraph::Encoding(_)
            | PreparedGraph::Logical { .. }
            | PreparedGraph::LogicalScan { .. } => None,
        }
    }

    fn logical(&mut self, key: &EncodingKey, input: &[u8]) -> Option<Arc<[u8]>> {
        match self.get_prepared(key, input)? {
            PreparedGraph::Logical { tokens, .. } => Some(Arc::clone(tokens)),
            PreparedGraph::Encoding(_)
            | PreparedGraph::Envelope { .. }
            | PreparedGraph::LogicalScan { .. } => None,
        }
    }

    fn references(&mut self, key: &EncodingKey, input: &[u8]) -> Option<Arc<LogicalScan>> {
        match self.get_prepared(key, input)? {
            PreparedGraph::LogicalScan { references, .. } => Some(Arc::clone(references)),
            _ => None,
        }
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
        self.insert_prepared(
            key,
            PreparedGraph::Encoding(Arc::new(PreparedEncoding {
                input: input.into(),
                root,
                nodes,
                bytes,
            })),
        );
    }

    fn insert_prepared(&mut self, key: EncodingKey, prepared: PreparedGraph) {
        let bytes = prepared.bytes();
        if bytes > CACHE_BYTES {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes -= previous.bytes();
        }
        while self.entries.len() >= CACHE_ENTRIES || self.bytes + bytes > CACHE_BYTES {
            let Some((_, removed)) = self.entries.pop_front() else {
                break;
            };
            #[cfg(test)]
            {
                self.eviction_candidates += 1;
            }
            self.bytes -= removed.bytes();
        }
        self.entries.insert(key, prepared);
        self.bytes += bytes;
    }
}

fn encoding_cache() -> Arc<Mutex<EncodingCache>> {
    static CACHE: OnceLock<Arc<Mutex<EncodingCache>>> = OnceLock::new();
    Arc::clone(CACHE.get_or_init(|| Arc::new(Mutex::new(EncodingCache::default()))))
}

/// Canonical framing belongs to the same bounded pure-preparation inventory
/// as encoding. Every lookup compares the current bytes in full; no stored
/// presence, referrer or receiving limit is memoized.
pub(crate) fn prepare_envelope(
    digest: NodeId,
    bytes: &[u8],
    parse: impl FnOnce() -> Result<Framing, GraphRefusal>,
) -> Result<Framing, GraphRefusal> {
    let cache = encoding_cache();
    let key = (EncodingKind::Envelope, digest.0);
    if let Some(framing) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .envelope(&key, bytes)
    {
        return Ok(framing);
    }
    let framing = parse()?;
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(framing) = cache.envelope(&key, bytes) {
        return Ok(framing);
    }
    cache.insert_prepared(
        key,
        PreparedGraph::Envelope {
            input: bytes.into(),
            framing,
        },
    );
    Ok(framing)
}

// A token expansion depends on the complete envelope and the body offset.
// Compare actual bytes on lookup and publication, under the same bounded
// preparation owner as canonical encoding and envelope framing.
pub(crate) fn prepare_logical(
    digest: NodeId,
    bytes: &[u8],
    skip: usize,
    expand: impl FnOnce() -> Result<Vec<u8>, GraphRefusal>,
) -> Result<Arc<[u8]>, GraphRefusal> {
    let cache = encoding_cache();
    let key = (EncodingKind::Logical(skip), digest.0);
    if let Some(tokens) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .logical(&key, bytes)
    {
        return Ok(tokens);
    }
    let tokens: Arc<[u8]> = expand()?.into();
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(tokens) = cache.logical(&key, bytes) {
        return Ok(tokens);
    }
    cache.insert_prepared(
        key,
        PreparedGraph::Logical {
            input: bytes.into(),
            tokens: Arc::clone(&tokens),
        },
    );
    Ok(tokens)
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
        }
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
            staging.encode_body(kind, &AnalyzedBody::new(body.as_bytes()), None)
        })
    }

    /// Canonically re-encodes an unplaced body, reusing read-local facts only
    /// after comparing its placement with the supplied immutable bytes.
    pub fn reencode(
        &mut self,
        kind: NodeKind,
        logical: &[u8],
        placed: &AnalyzedBody<'_>,
    ) -> Result<NodeId, GraphRefusal> {
        if std::ptr::eq(logical, placed.as_bytes()) || logical == placed.as_bytes() {
            self.encode_analyzed(kind, placed, Some(placed))
        } else {
            self.encode_analyzed(kind, &AnalyzedBody::new(logical), Some(placed))
        }
    }

    fn encode_analyzed(
        &mut self,
        kind: NodeKind,
        body: &AnalyzedBody<'_>,
        placed: Option<&AnalyzedBody<'_>>,
    ) -> Result<NodeId, GraphRefusal> {
        self.memoized_analysis(EncodingKind::Node(kind), body, |staging| {
            staging.encode_body(kind, body, placed)
        })
    }

    fn encode_body(
        &mut self,
        kind: NodeKind,
        body: &AnalyzedBody<'_>,
        prepared_placement: Option<&AnalyzedBody<'_>>,
    ) -> Result<NodeId, GraphRefusal> {
        let logical = body.as_bytes();
        let spec = kind.spec().ok_or(GraphRefusal::UnsupportedKind(kind.0))?;
        match spec.shape {
            BodyShape::Direct => {
                if kind != NodeKind::STRUCTURE_BLOCK {
                    body.one_token(kind)?;
                }
                let (children, physical) = body.to_physical(kind)?;
                self.insert(kind, &children, physical)
            }
            BodyShape::Enveloped => {
                let placed = self.place(body, kind)?;
                let analysis = AnalyzedBody::new(&placed);
                let analysis =
                    if std::ptr::eq(placed.as_ref(), logical) || placed.as_ref() == logical {
                        body
                    } else if let Some(prepared) =
                        prepared_placement.filter(|prepared| prepared.as_bytes() == placed.as_ref())
                    {
                        prepared
                    } else {
                        &analysis
                    };
                analysis.one_token(kind)?;
                let (children, physical) = analysis.to_physical(kind)?;
                let mut inline = Vec::with_capacity(physical.len() + 3);
                inline.extend_from_slice(&[0x0c, 0x00, 0x01]);
                inline.extend_from_slice(&physical);
                if envelope_len(kind, children.len(), inline.len()) as u64 <= spec.bound.bytes() {
                    return self.insert(kind, &children, inline);
                }
                let stream = self.stream_analyzed(kind, analysis)?;
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
        check_decoded(
            &node.bytes,
            node.id,
            &crate::DecodeLimits::within_total(u64::MAX),
        )?;
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
            bytes: Arc::from(bytes),
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
        self.memoized_analysis(kind, &AnalyzedBody::new(input), encode)
    }

    fn memoized_analysis(
        &mut self,
        kind: EncodingKind,
        analysis: &AnalyzedBody<'_>,
        encode: impl FnOnce(&mut Self) -> Result<NodeId, GraphRefusal>,
    ) -> Result<NodeId, GraphRefusal> {
        let input = analysis.as_bytes();
        if input.len() > CACHE_BYTES {
            return encode(self);
        }
        let key = (kind, analysis.digest());
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
    fn place<'a>(
        &mut self,
        body: &AnalyzedBody<'a>,
        kind: NodeKind,
    ) -> Result<Cow<'a, [u8]>, GraphRefusal> {
        let logical = body.as_bytes();
        if body
            .scan
            .get()
            .is_some_and(|scan| scan.oversized_inline.is_none())
        {
            return Ok(Cow::Borrowed(logical));
        }
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
        Ok(Cow::Owned(out.into_bytes()))
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
            let leaf =
                self.encode_analyzed(NodeKind::TEXT_LEAF, &AnalyzedBody::new(&logical), None)?;
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
            let leaf =
                self.encode_analyzed(NodeKind::BYTES_LEAF, &AnalyzedBody::new(&logical), None)?;
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
        self.stream_analyzed(kind, &AnalyzedBody::new(placed))
    }

    fn stream_analyzed(
        &mut self,
        kind: NodeKind,
        analysis: &AnalyzedBody<'_>,
    ) -> Result<NodeId, GraphRefusal> {
        let placed = analysis.as_bytes();
        let events: Vec<_> = analysis.scan(kind)?.events().collect();
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
            let (children, physical) = if logical.len() == placed.len() {
                analysis.to_physical(NodeKind::STRUCTURE_BLOCK)?
            } else {
                to_physical(logical, NodeKind::STRUCTURE_BLOCK)?
            };
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
    AnalyzedBody::new(logical).to_physical(kind)
}

/// An enveloped kind's size if `placed` were inline, without writing the
/// physical body. References and their lengths come from its logical scan.
///
/// # Errors
/// A token refusal if `placed` is not canonical logical tokens.
pub fn inline_envelope_len(placed: &[u8], kind: NodeKind) -> Result<u64, GraphRefusal> {
    AnalyzedBody::new(placed).inline_envelope_len(kind)
}

/// A logical reference's length: its tag and its child's digest.
const LOGICAL_REF_LEN: usize = 1 + 32;

/// Pure facts from one canonical logical token stream: ordered token
/// boundaries, child references, and the first oversized inline value.
/// These facts grant no store presence, receiving limits or child authority.
pub struct LogicalScan {
    at: Vec<(usize, [u8; 32])>,
    pub(crate) children: Vec<[u8; 32]>,
    ends: Vec<usize>,
    oversized_inline: Option<(usize, usize)>,
    tree_end: Option<usize>,
}

/// One validated logical token event, retained in its original order.
#[derive(Clone, Copy, Debug)]
pub struct PlacementStep {
    pub start: usize,
    pub end: usize,
    pub placement: Placement,
}

/// The part of a canonical token event graph placement needs.
#[derive(Clone, Copy, Debug)]
pub enum Placement {
    Oversized,
    Child(ChildRef),
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static REFERENCE_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

// Read-local work is captured only while a focused testing scope owns it.
#[cfg(any(test, feature = "testing"))]
thread_local! {
    static ANALYSIS_WORK: std::cell::RefCell<Option<AnalysisWork>> = const { std::cell::RefCell::new(None) };
}

#[cfg(any(test, feature = "testing"))]
#[derive(Default, Debug)]
pub struct AnalysisWork {
    pub logical_hashes: Vec<usize>,
    pub logical_scans: Vec<usize>,
    pub envelope_hashes: Vec<usize>,
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn record_analysis_work(operation: usize, bytes: usize) {
    ANALYSIS_WORK.with(|work| {
        if let Some(work) = work.borrow_mut().as_mut() {
            match operation {
                0 => work.logical_hashes.push(bytes),
                1 => work.logical_scans.push(bytes),
                _ => work.envelope_hashes.push(bytes),
            }
        }
    });
}

/// Measures codec work on this testing thread, without retaining observations
/// from unrelated reads. Dropping the scope also stops collection on panic.
#[cfg(any(test, feature = "testing"))]
pub fn with_analysis_work<T>(read: impl FnOnce() -> T) -> (T, AnalysisWork) {
    struct Capture;
    impl Drop for Capture {
        fn drop(&mut self) {
            ANALYSIS_WORK.with(|work| *work.borrow_mut() = None);
        }
    }
    let capture = Capture;
    ANALYSIS_WORK.with(|work| *work.borrow_mut() = Some(AnalysisWork::default()));
    let result = read();
    let work = ANALYSIS_WORK.with(|work| work.borrow_mut().take().unwrap_or_default());
    drop(capture);
    (result, work)
}

fn prepare_references(
    key: EncodingKey,
    logical: &[u8],
    scan: impl FnOnce() -> Result<LogicalScan, GraphRefusal>,
) -> Result<Arc<LogicalScan>, GraphRefusal> {
    let cache = encoding_cache();
    if let Some(references) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .references(&key, logical)
    {
        return Ok(references);
    }
    let references = Arc::new(scan()?);
    let prepared = PreparedGraph::LogicalScan {
        input: logical.into(),
        references: Arc::clone(&references),
    };
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(references) = cache.references(&key, logical) {
        return Ok(references);
    }
    cache.insert_prepared(key, prepared);
    Ok(references)
}

impl LogicalScan {
    /// Reads canonical logical tokens once, retaining their references and
    /// ordered placement facts in the bounded graph preparation owner.
    ///
    /// # Errors
    /// A token refusal if the supplied logical bytes are not canonical.
    pub fn scan(logical: &[u8], kind: NodeKind) -> Result<Arc<Self>, GraphRefusal> {
        Self::scan_with_digest(logical, kind, || TypedBody::logical(logical).digest())
    }

    fn scan_with_digest(
        logical: &[u8],
        kind: NodeKind,
        digest: impl FnOnce() -> [u8; 32],
    ) -> Result<Arc<Self>, GraphRefusal> {
        #[cfg(any(test, feature = "testing"))]
        record_analysis_work(1, logical.len());
        if logical.len() > CACHE_BYTES {
            return Self::scan_uncached(logical, kind).map(Arc::new);
        }
        let key = (EncodingKind::LogicalScan, digest());
        prepare_references(key, logical, || Self::scan_uncached(logical, kind))
    }

    fn scan_uncached(logical: &[u8], kind: NodeKind) -> Result<Self, GraphRefusal> {
        #[cfg(any(test, feature = "testing"))]
        REFERENCE_SCANS.with(|count| count.set(count.get() + 1));
        let mut at = Vec::new();
        let mut ends = Vec::new();
        let mut oversized_inline = None;
        let mut pending = 1_u64;
        let mut tree_end = None;
        let mut reader = TokenReader::new(logical);
        while !reader.is_empty() {
            let start = reader.position();
            let event = reader
                .next_event()
                .map_err(|error| GraphRefusal::from_token(error, None, Some(kind.0)))?;
            if tree_end.is_none() {
                pending = pending.saturating_sub(1).saturating_add(event.contents());
                if pending == 0 {
                    tree_end = Some(reader.position());
                }
            }
            match event {
                TokenEvent::Child(child) => at.push((start, child.0)),
                TokenEvent::Text(text) if token_len(text.len()) > INLINE_TOKEN_MAX => {
                    oversized_inline.get_or_insert((start, reader.position()));
                }
                TokenEvent::Bytes(bytes) if token_len(bytes.len()) > INLINE_TOKEN_MAX => {
                    oversized_inline.get_or_insert((start, reader.position()));
                }
                _ => {}
            }
            ends.push(reader.position());
        }
        let mut children: Vec<[u8; 32]> = at.iter().map(|(_, digest)| *digest).collect();
        children.sort_unstable();
        children.dedup();
        Ok(Self {
            at,
            children,
            ends,
            oversized_inline,
            tree_end,
        })
    }

    fn events(&self) -> impl Iterator<Item = Event> + '_ {
        let mut start = 0;
        let mut children = self.at.iter().peekable();
        self.ends.iter().map(move |end| {
            let child = if children.peek().is_some_and(|(at, _)| *at == start) {
                children.next().map(|(_, child)| *child)
            } else {
                None
            };
            let event = Event {
                start,
                end: *end,
                child,
            };
            start = *end;
            event
        })
    }

    /// Events that may change placement, in validated token order. Plain
    /// spans are admitted with [`Self::first_event_past`] instead of walking
    /// them. A consumer must refuse the first oversized inline value.
    pub fn placement_changes(&self) -> impl Iterator<Item = PlacementStep> + '_ {
        let mut references = self.at.iter().peekable();
        let mut oversized = self.oversized_inline;
        std::iter::from_fn(move || {
            if let Some((start, end)) = oversized
                && references.peek().is_none_or(|(at, _)| *at >= start)
            {
                oversized = None;
                return Some(PlacementStep {
                    start,
                    end,
                    placement: Placement::Oversized,
                });
            }
            references.next().map(|(start, digest)| PlacementStep {
                start: *start,
                end: *start + LOGICAL_REF_LEN,
                placement: Placement::Child(ChildRef(*digest)),
            })
        })
    }

    /// The first token whose original end exceeds `limit`. The common
    /// admitted case is constant work; a tight limit uses binary search,
    /// retaining the exact event the sequential reader would refuse at.
    pub fn first_event_past(&self, limit: u64) -> Option<usize> {
        if *self.ends.last()? as u64 <= limit {
            return None;
        }
        self.ends
            .get(self.ends.partition_point(|end| *end as u64 <= limit))
            .copied()
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

    /// FIG-7135: repeated physical framing scans each logical content once,
    /// independently of fresh allocations and the number of cold readers.
    #[test]
    fn reference_scans_are_bounded_by_content_not_reads() {
        let mut tokens = TokenWriter::new();
        tokens.seq(4);
        tokens.text("FIG-7135 repeated reference scan");
        tokens.child(ChildRef([0x61; 32]));
        tokens.child(ChildRef([0x62; 32]));
        tokens.child(ChildRef([0x61; 32]));
        let logical = tokens.into_bytes();
        REFERENCE_SCANS.with(|count| count.set(0));
        for _ in 0..12 {
            let (children, physical) =
                to_physical(&logical.clone(), NodeKind::FROZEN_VALUE).unwrap();
            assert_eq!(children, [[0x61; 32], [0x62; 32]]);
            assert!(!physical.is_empty());
            let _scan = LogicalScan::scan(&logical.clone(), NodeKind(0)).unwrap();
        }
        assert_eq!(
            REFERENCE_SCANS.with(std::cell::Cell::get),
            1,
            "reference scanning grows with reader count"
        );
    }

    /// FIG-7135: racing reference scans reclassify under the same owner
    /// and compare complete inputs, even when their lookup keys coincide.
    #[test]
    fn racing_reference_preparations_keep_each_inputs_children() {
        let barrier = std::sync::Barrier::new(2);
        let key = (EncodingKind::LogicalScan, [0x94; 32]);
        let bodies: Vec<_> = [0x81, 0x82]
            .into_iter()
            .map(|byte| {
                let mut body = TokenWriter::new();
                body.child(ChildRef([byte; 32]));
                body.into_bytes()
            })
            .collect();
        std::thread::scope(|scope| {
            for body in &bodies {
                let barrier = &barrier;
                scope.spawn(move || {
                    let refs = prepare_references(key, body, || {
                        let refs = LogicalScan::scan_uncached(body, NodeKind(1_002))?;
                        barrier.wait();
                        Ok(refs)
                    })
                    .unwrap();
                    assert_eq!(refs.children, [<[u8; 32]>::try_from(&body[1..]).unwrap()]);
                });
            }
        });
        for body in &bodies {
            let refs = prepare_references(key, body, || {
                LogicalScan::scan_uncached(body, NodeKind(1_002))
            })
            .unwrap();
            assert_eq!(refs.children, [<[u8; 32]>::try_from(&body[1..]).unwrap()]);
        }
    }

    /// PREPARATION-EVICTION-WORK: finding the least recent entry under the
    /// shared lock examines only the entry evicted, not every resident body.
    #[test]
    fn preparation_eviction_does_not_scan_resident_bodies() {
        let mut cache = EncodingCache::default();
        let root = NodeId([0; 32]);
        let key = |n: u32| (EncodingKind::Node(NodeKind(n)), [0; 32]);
        for n in 0..CACHE_ENTRIES as u32 {
            cache.insert(key(n), b"input", root, Vec::new());
        }
        cache.eviction_candidates = 0;
        for n in CACHE_ENTRIES as u32..(CACHE_ENTRIES * 3) as u32 {
            assert!(cache.get(&key(0), b"input").is_some());
            cache.insert(key(n), b"input", root, Vec::new());
        }
        assert_eq!(cache.entries.len(), CACHE_ENTRIES);
        assert_eq!(
            cache.eviction_candidates,
            CACHE_ENTRIES * 2,
            "each replacement selects only its oldest entry"
        );
        assert!(cache.get(&key(CACHE_ENTRIES as u32), b"input").is_none());
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
