//! The node envelope, its digest, and the generic check every reader runs
//! (FIG-5887 §2).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};

use lash_kernel_doc::graph_tokens::{
    CanonicalFault, ChildRef, RefForm, TokenCodec, TokenError, TokenEvent, TokenReader,
    TokenWriter, get_varint, put_varint,
};

use super::refusal::{DecodeLimits, GraphRefusal};
use super::registry::NodeKind;
use crate::BlobRef;

/// The four bytes every envelope starts with.
pub const MAGIC: [u8; 4] = *b"LASH";

/// The largest complete envelope of any node, in bytes. A format constant:
/// a host budget may be smaller, never larger.
pub const MAX_NODE_BYTES: u64 = 65_536;

/// The largest complete envelope of a root kind (ExecRoot, HostRoot, CellEnd,
/// PhaseRoot, SessionManifest, WholeStream, CheckpointRoot).
pub const MAX_ROOT_BYTES: u64 = 4_096;

/// A text or bytes token, its tag and length included, of at most this many
/// bytes is inline; a longer one is a child reference to a leaf or index.
pub const INLINE_TOKEN_MAX: u64 = 256;

/// The largest payload one TextLeaf or BytesLeaf holds. With its token and
/// envelope a full leaf stays below [`MAX_NODE_BYTES`].
pub const LEAF_PAYLOAD_MAX: usize = 65_520;

/// Children per index node: text, bytes, stream, summary and event indexes
/// group their children 32 at a time.
pub const INDEX_FANOUT: usize = 32;

/// A node's identity: the digest of its complete envelope under the blob hash
/// domain. Its lowercase hex is the node's SQL key; its raw bytes are how a
/// body references it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// The identity of an envelope's bytes.
    #[must_use]
    pub fn of_envelope(bytes: &[u8]) -> Self {
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_hash_pass(bytes.len());
        Self(BlobRef::digest_for_content(bytes))
    }

    /// The lowercase hex SQL key.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut text = String::with_capacity(64);
        self.write_hex(&mut text);
        text
    }

    /// Appends the lowercase hex SQL key to `out`.
    pub fn write_hex(&self, out: &mut String) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }

    /// Reads a lowercase hex SQL key back. Uppercase is refused: one spelling.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let nibble = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let mut digest = [0_u8; 32];
        for (index, slot) in digest.iter_mut().enumerate() {
            *slot = (nibble(bytes[index * 2])? << 4) | nibble(bytes[index * 2 + 1])?;
        }
        Some(Self(digest))
    }

    /// The blob reference a store keys this node by.
    #[must_use]
    pub fn blob_ref(&self) -> BlobRef {
        BlobRef(self.to_hex())
    }

    /// How a logical body names this node.
    #[must_use]
    pub const fn child_ref(&self) -> ChildRef {
        ChildRef(self.0)
    }
}

impl From<ChildRef> for NodeId {
    fn from(child: ChildRef) -> Self {
        Self(child.0)
    }
}

impl From<NodeId> for ChildRef {
    fn from(id: NodeId) -> Self {
        Self(id.0)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.to_hex())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// One complete node: its identity and its envelope bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedNode {
    pub id: NodeId,
    pub bytes: Arc<[u8]>,
}

impl fmt::Debug for EncodedNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedNode")
            .field("id", &self.id)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl EncodedNode {
    /// The envelope, already known to be well formed: staged nodes are only
    /// ever built by this module.
    ///
    /// # Errors
    ///
    /// A refusal of [`parse_envelope`] for bytes not built here.
    pub fn envelope(&self) -> Result<NodeEnvelope<'_>, GraphRefusal> {
        parse_envelope(&self.bytes, Some(self.id))
    }

    /// The node's storage edges: exactly its child table.
    ///
    /// # Errors
    ///
    /// A refusal of [`parse_envelope`].
    pub fn children(&self) -> Result<Vec<NodeId>, GraphRefusal> {
        Ok(self.envelope()?.children.iter().collect())
    }

    /// The envelope's length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// A node's sorted, unique child digests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildTable<'a>(&'a [[u8; 32]]);

impl<'a> ChildTable<'a> {
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The child at `slot`.
    #[must_use]
    pub fn get(&self, slot: u64) -> Option<NodeId> {
        usize::try_from(slot)
            .ok()
            .and_then(|slot| self.0.get(slot))
            .map(|digest| NodeId(*digest))
    }

    pub fn iter(&self) -> impl Iterator<Item = NodeId> + 'a {
        self.0.iter().map(|digest| NodeId(*digest))
    }

    /// The slot of `child`, when it is in the table.
    #[must_use]
    pub fn slot_of(&self, child: &NodeId) -> Option<u64> {
        self.0.binary_search(&child.0).ok().map(|slot| slot as u64)
    }
}

/// A parsed envelope, borrowing the node's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeEnvelope<'a> {
    pub kind: NodeKind,
    pub children: ChildTable<'a>,
    /// The physical body: child references are slots.
    pub body: &'a [u8],
}

impl<'a> NodeEnvelope<'a> {
    /// A physical reader over the body.
    #[must_use]
    pub fn reader(&self) -> TokenReader<'a> {
        TokenReader::with_form(self.body, RefForm::Physical)
    }
}

/// Writes an envelope from its parts. Every caller passes a sorted, unique
/// child table and a physical body that references each child.
pub fn write_envelope(kind: NodeKind, children: &[[u8; 32]], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(envelope_len(kind, children.len(), body.len()));
    out.extend_from_slice(&MAGIC);
    put_varint(&mut out, u64::from(kind.0));
    put_varint(&mut out, children.len() as u64);
    for child in children {
        out.extend_from_slice(child);
    }
    put_varint(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    #[cfg(feature = "perf-witness")]
    crate::perf_witness::record_graph_encode(out.len());
    out
}

/// The length [`write_envelope`] produces.
pub fn envelope_len(kind: NodeKind, children: usize, body: usize) -> usize {
    use lash_kernel_doc::graph_tokens::varint_len;
    child_table_offset(kind, children) + 32 * children + varint_len(body as u64) + body
}

/// Where the child table of an envelope of `kind` with `children` children
/// starts: after its magic, its kind and its child count.
#[must_use]
pub fn child_table_offset(kind: NodeKind, children: usize) -> usize {
    use lash_kernel_doc::graph_tokens::varint_len;
    MAGIC.len() + varint_len(u64::from(kind.0)) + varint_len(children as u64)
}

/// The envelope kinds of semantic heap pages (FIG-6187): direct-bodied,
/// with no inline or stream wrapper and no placement of large tokens.
pub const SEMANTIC_KINDS: std::ops::Range<u32> = 1_000..2_000;

/// Whether an envelope of `kind` is a persistent-heap page, which a host
/// graph read takes as a leaf and never descends into
/// ([`GraphRefusal::SemanticPage`]).
#[must_use]
pub fn is_semantic(kind: NodeKind) -> bool {
    SEMANTIC_KINDS.contains(&kind.0)
}

/// Frames a direct-bodied semantic page from its logical body, whose child
/// references are full digests: the body's distinct children become the
/// sorted child table and each reference its slot. Nothing is placed: a
/// body that does not fit `bound` (at most [`MAX_NODE_BYTES`]) is refused,
/// never split. The body is read once and the envelope written once, and
/// nothing is hashed: whoever sends or stages the page names it
/// ([`FramedPage::named`], FIG-6932).
///
/// # Errors
///
/// [`GraphRefusal::UnsupportedKind`] outside [`SEMANTIC_KINDS`], a token
/// refusal for a body that is not canonical, and
/// [`GraphRefusal::EncodedNodeTooLarge`] past the bound.
pub fn frame_semantic(
    kind: NodeKind,
    logical: &[u8],
    bound: u64,
) -> Result<FramedPage, GraphRefusal> {
    if !SEMANTIC_KINDS.contains(&kind.0) {
        return Err(GraphRefusal::UnsupportedKind(kind.0));
    }
    let references = super::encode::References::scan(logical, kind)?;
    let children = references.children.len();
    let body = references.physical_len(logical.len());
    let len = envelope_len(kind, children, body);
    let limit = bound.min(MAX_NODE_BYTES);
    if len as u64 > limit {
        return Err(GraphRefusal::EncodedNodeTooLarge {
            bytes: len as u64,
            limit,
        });
    }
    let mut bytes = Vec::with_capacity(len);
    bytes.extend_from_slice(&MAGIC);
    put_varint(&mut bytes, u64::from(kind.0));
    put_varint(&mut bytes, children as u64);
    let table = bytes.len();
    for child in &references.children {
        bytes.extend_from_slice(child);
    }
    put_varint(&mut bytes, body as u64);
    references.write_physical(logical, &mut bytes);
    debug_assert_eq!(bytes.len(), len);
    #[cfg(feature = "perf-witness")]
    crate::perf_witness::record_graph_encode(bytes.len());
    Ok(FramedPage {
        bytes,
        framing: Framing {
            kind,
            children_start: table,
            children_len: children * 32,
            body_start: len - body,
        },
    })
}

/// [`frame_semantic`], named by its digest.
///
/// # Errors
///
/// As [`frame_semantic`].
pub fn frame_direct(
    kind: NodeKind,
    logical: &[u8],
    bound: u64,
) -> Result<EncodedNode, GraphRefusal> {
    Ok(frame_semantic(kind, logical, bound)?.named().into_node())
}

/// A semantic page [`frame_semantic`] framed, not yet named: its envelope
/// and where its sorted child table is. Canonical by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FramedPage {
    bytes: Vec<u8>,
    framing: Framing,
}

impl FramedPage {
    #[must_use]
    pub fn kind(&self) -> NodeKind {
        self.framing.kind
    }

    /// The page's children, ascending by digest.
    #[must_use]
    pub fn children(&self) -> ChildTable<'_> {
        self.framing.children(&self.bytes)
    }

    /// The envelope.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The page, named by the digest of its envelope: the one hash of it
    /// its framer takes.
    #[must_use]
    pub fn named(self) -> VerifiedNode {
        let id = NodeId::of_envelope(&self.bytes);
        VerifiedNode {
            node: EncodedNode {
                id,
                bytes: self.bytes.into(),
            },
            framing: self.framing,
        }
    }
}

/// A node whose envelope checked, carried intact from that check to every
/// consumer (FIG-6932): its bytes hash to its identity and frame
/// canonically within the format bound, and its child table is read where
/// the check found it, never parsed again. Only a check or this module's
/// framing makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedNode {
    node: EncodedNode,
    framing: Framing,
}

impl VerifiedNode {
    /// Checks `node` with [`check_envelope`].
    ///
    /// # Errors
    ///
    /// As [`check_envelope`].
    pub fn check(node: EncodedNode, limits: &DecodeLimits) -> Result<Self, GraphRefusal> {
        let framing = Framing::of(&node.bytes, check_envelope(&node.bytes, node.id, limits)?);
        Ok(Self { node, framing })
    }

    /// [`check_decoded`] of `node`.
    ///
    /// # Errors
    ///
    /// As [`check_decoded`].
    pub fn check_decoded(node: EncodedNode, limits: &DecodeLimits) -> Result<Self, GraphRefusal> {
        let framing = Framing::of(&node.bytes, check_decoded(&node.bytes, node.id, limits)?);
        Ok(Self { node, framing })
    }

    #[must_use]
    pub fn kind(&self) -> NodeKind {
        self.framing.kind
    }

    /// The node's children, ascending by digest, from its checked framing.
    #[must_use]
    pub fn children(&self) -> ChildTable<'_> {
        self.framing.children(&self.node.bytes)
    }

    #[must_use]
    pub fn into_node(self) -> EncodedNode {
        self.node
    }
}

impl std::ops::Deref for VerifiedNode {
    type Target = EncodedNode;

    fn deref(&self) -> &EncodedNode {
        &self.node
    }
}

/// A checked semantic page's logical body: each child slot written back as
/// its child's full digest. The inverse of [`frame_direct`].
///
/// # Errors
///
/// [`GraphRefusal::UnsupportedKind`] outside [`SEMANTIC_KINDS`], and a token
/// refusal for a body that is not canonical.
pub fn direct_logical(envelope: &NodeEnvelope<'_>, id: NodeId) -> Result<Vec<u8>, GraphRefusal> {
    if !SEMANTIC_KINDS.contains(&envelope.kind.0) {
        return Err(GraphRefusal::UnsupportedKind(envelope.kind.0));
    }
    let refuse = |error| GraphRefusal::from_token(error, Some(id), Some(envelope.kind.0));
    let mut out = lash_kernel_doc::graph_tokens::TokenWriter::with_capacity(
        envelope.body.len() + 32 * envelope.children.len(),
    );
    let mut reader = envelope.reader();
    while !reader.is_empty() {
        let start = reader.position();
        match reader.next_event().map_err(refuse)? {
            TokenEvent::Slot(slot) => {
                let child = envelope
                    .children
                    .get(slot)
                    .ok_or(GraphRefusal::NonCanonical {
                        node: Some(id),
                        reason: CanonicalFault::ChildSlotOutOfRange(slot),
                    })?;
                out.child(child.child_ref());
            }
            _ => out.raw(&envelope.body[start..reader.position()]),
        }
    }
    Ok(out.into_bytes())
}

/// The generic check: verifies `bytes` hash to `expected`, then parses them
/// with [`parse_envelope`], within `limits`' node-size bound.
///
/// It accepts any nonzero kind, registered or not, so a reader can follow
/// a future kind's declared children; an execution decoder refuses kinds it
/// does not support. Unknown primitive framing is never skipped.
///
/// # Errors
///
/// [`GraphRefusal::EncodedNodeTooLarge`] past the format bound,
/// [`GraphRefusal::DecodeLimit`] past the host bound,
/// [`GraphRefusal::HashMismatch`], or any refusal of [`parse_envelope`].
pub fn check_envelope<'a>(
    bytes: &'a [u8],
    expected: NodeId,
    limits: &DecodeLimits,
) -> Result<NodeEnvelope<'a>, GraphRefusal> {
    check_size(bytes.len() as u64, limits)?;
    if let Some(envelope) = IMMUTABLE_CATALOG
        .get()
        .and_then(|catalog| catalog.get(expected, bytes))
    {
        return Ok(envelope);
    }
    if NodeId::of_envelope(bytes) != expected {
        return Err(GraphRefusal::HashMismatch(expected));
    }
    parse_envelope(bytes, Some(expected))
}

/// [`check_envelope`] of `bytes` whose identity `expected` their receiver
/// derived from exactly these bytes as it decoded them: a page envelope
/// that crossed between a worker and its parent (FIG-6321). The digest is
/// not taken again (FIG-6932); the size bounds and [`parse_envelope`] run
/// as [`check_envelope`] runs them.
///
/// # Errors
///
/// As [`check_envelope`], never [`GraphRefusal::HashMismatch`].
pub fn check_decoded<'a>(
    bytes: &'a [u8],
    expected: NodeId,
    limits: &DecodeLimits,
) -> Result<NodeEnvelope<'a>, GraphRefusal> {
    check_size(bytes.len() as u64, limits)?;
    parse_envelope(bytes, Some(expected))
}

/// The format's and the host's node-size bounds on an envelope of `len`
/// bytes.
fn check_size(len: u64, limits: &DecodeLimits) -> Result<(), GraphRefusal> {
    if len > MAX_NODE_BYTES {
        return Err(GraphRefusal::EncodedNodeTooLarge {
            bytes: len,
            limit: MAX_NODE_BYTES,
        });
    }
    if len > limits.node_bytes {
        return Err(GraphRefusal::DecodeLimit {
            dimension: lash_kernel_doc::graph_tokens::DecodeDimension::NodeBytes,
            actual: len,
            limit: limits.node_bytes,
        });
    }
    Ok(())
}

/// The process's finite, immutable build catalog: its nodes' bytes by
/// identity. Runtime data never enters it, so it cannot grow with turns or
/// stored values. A member is checked (its digest and framing) the first
/// time it is used, never at registration, so a process pays only for the
/// members it reads; a member that does not check is never used.
struct ImmutableCatalog(BTreeMap<NodeId, CatalogEntry>);

struct CatalogEntry {
    id: NodeId,
    bytes: Arc<[u8]>,
    /// The member's checked framing; none when its bytes do not check.
    framing: OnceLock<Option<Framing>>,
    /// Everything the member reaches; none when a node it reaches is not a
    /// member that checks.
    closure: OnceLock<Option<CatalogClosure>>,
}

/// Where a checked envelope's parts are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Framing {
    kind: NodeKind,
    children_start: usize,
    children_len: usize,
    body_start: usize,
}

impl Framing {
    /// The framing of `envelope`, parsed from `bytes`.
    fn of(bytes: &[u8], envelope: NodeEnvelope<'_>) -> Self {
        Self {
            kind: envelope.kind,
            children_start: envelope.children.0.as_ptr().addr() - bytes.as_ptr().addr(),
            children_len: envelope.children.len() * 32,
            body_start: bytes.len() - envelope.body.len(),
        }
    }

    fn children<'a>(&self, bytes: &'a [u8]) -> ChildTable<'a> {
        ChildTable(
            bytes[self.children_start..self.children_start + self.children_len]
                .as_chunks::<32>()
                .0,
        )
    }
}

/// A member of the build catalog and every node it reaches, each a checked
/// member: what a store holds once it holds the member.
#[derive(Debug)]
pub struct CatalogClosure {
    nodes: Vec<EncodedNode>,
}

impl CatalogClosure {
    /// The member first, then every node it reaches, each once.
    #[must_use]
    pub fn nodes(&self) -> &[EncodedNode] {
        &self.nodes
    }
}

impl CatalogEntry {
    fn framing(&self) -> Option<&Framing> {
        self.framing
            .get_or_init(|| {
                if self.bytes.len() as u64 > MAX_NODE_BYTES
                    || NodeId::of_envelope(&self.bytes) != self.id
                {
                    return None;
                }
                let envelope = parse_framed(&self.bytes, Some(self.id)).ok()?;
                Some(Framing::of(&self.bytes, envelope))
            })
            .as_ref()
    }

    fn envelope<'a>(&self, bytes: &'a [u8]) -> Option<NodeEnvelope<'a>> {
        let framing = self.framing()?;
        Some(NodeEnvelope {
            kind: framing.kind,
            children: framing.children(bytes),
            body: &bytes[framing.body_start..],
        })
    }
}

impl ImmutableCatalog {
    fn get<'a>(&self, id: NodeId, bytes: &'a [u8]) -> Option<NodeEnvelope<'a>> {
        let entry = self.0.get(&id)?;
        if bytes != &*entry.bytes {
            return None;
        }
        entry.envelope(bytes)
    }

    fn closure(&self, id: NodeId) -> Option<&CatalogClosure> {
        self.0
            .get(&id)?
            .closure
            .get_or_init(|| {
                let mut seen = std::collections::BTreeSet::new();
                let mut nodes = Vec::new();
                let mut pending = vec![id];
                while let Some(id) = pending.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    let entry = self.0.get(&id)?;
                    let envelope = entry.envelope(&entry.bytes)?;
                    pending.extend(envelope.children.iter());
                    nodes.push(EncodedNode {
                        id,
                        bytes: Arc::clone(&entry.bytes),
                    });
                }
                Some(CatalogClosure { nodes })
            })
            .as_ref()
    }
}

static IMMUTABLE_CATALOG: OnceLock<ImmutableCatalog> = OnceLock::new();

/// Registers the process's immutable build catalog; the first registration
/// wins. Nothing is checked here: [`check_envelope`] checks a member the
/// first time it meets its exact bytes, and only a member that checked is
/// reused. Caller size limits still run on every checked read.
pub fn register_immutable_catalog(nodes: &BTreeMap<NodeId, EncodedNode>) {
    let entries = nodes
        .iter()
        .map(|(id, node)| {
            (
                *id,
                CatalogEntry {
                    id: *id,
                    bytes: Arc::clone(&node.bytes),
                    framing: OnceLock::new(),
                    closure: OnceLock::new(),
                },
            )
        })
        .collect();
    let _ = IMMUTABLE_CATALOG.set(ImmutableCatalog(entries));
}

/// The closure of build catalog member `id`, when every node it reaches is
/// a member that checks. A store that holds the member holds all of it, so
/// a reader takes its nodes from here rather than reading them back.
#[must_use]
pub fn immutable_closure(id: NodeId) -> Option<&'static CatalogClosure> {
    IMMUTABLE_CATALOG.get()?.closure(id)
}

/// The members of the build catalog that no member names and whose
/// closures check: where a stored graph enters the catalog. A reader that
/// stops at them reads none of what they reach. Empty until a catalog is
/// registered; computed once, checking every member.
#[must_use]
pub fn immutable_catalog_heads() -> &'static [NodeId] {
    static HEADS: OnceLock<Vec<NodeId>> = OnceLock::new();
    let Some(catalog) = IMMUTABLE_CATALOG.get() else {
        return &[];
    };
    HEADS.get_or_init(|| {
        let mut named = std::collections::BTreeSet::new();
        for envelope in catalog
            .0
            .values()
            .filter_map(|entry| entry.envelope(&entry.bytes))
        {
            named.extend(envelope.children.iter());
        }
        catalog
            .0
            .keys()
            .copied()
            .filter(|id| !named.contains(id) && catalog.closure(*id).is_some())
            .collect()
    })
}

/// Parses an envelope without hashing it: magic, minimal varints, a
/// strictly ascending child table, an exact body length, and a body of
/// canonical tokens whose every slot is in range and every child referenced.
///
/// # Errors
///
/// [`GraphRefusal::BadMagic`], [`GraphRefusal::UnsupportedKind`] for kind 0,
/// [`GraphRefusal::RetiredFormat`] for a retired kind, [`GraphRefusal::Truncated`], [`GraphRefusal::TrailingBytes`],
/// [`GraphRefusal::LengthOverflow`], or [`GraphRefusal::NonCanonical`].
pub fn parse_envelope(
    bytes: &[u8],
    node: Option<NodeId>,
) -> Result<NodeEnvelope<'_>, GraphRefusal> {
    if let Some(id) = node
        && let Some(envelope) = IMMUTABLE_CATALOG
            .get()
            .and_then(|catalog| catalog.get(id, bytes))
    {
        return Ok(envelope);
    }
    parse_framed(bytes, node)
}

/// [`parse_envelope`] without the build catalog.
fn parse_framed(bytes: &[u8], node: Option<NodeId>) -> Result<NodeEnvelope<'_>, GraphRefusal> {
    let refuse = |error| GraphRefusal::from_token(error, node, None);
    if bytes.len() < MAGIC.len() {
        return Err(if MAGIC.starts_with(bytes) {
            GraphRefusal::Truncated
        } else {
            GraphRefusal::BadMagic
        });
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(GraphRefusal::BadMagic);
    }
    let mut pos = MAGIC.len();
    let kind = get_varint(bytes, &mut pos).map_err(refuse)?;
    let kind = u32::try_from(kind).map_err(|_| GraphRefusal::LengthOverflow)?;
    if kind == 0 {
        return Err(GraphRefusal::UnsupportedKind(0));
    }
    if let Some(format) = NodeKind(kind).retired_format() {
        return Err(GraphRefusal::RetiredFormat { node, kind, format });
    }
    let count = get_varint(bytes, &mut pos).map_err(refuse)?;
    let table_len = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(32))
        .ok_or(GraphRefusal::LengthOverflow)?;
    let table_end = pos
        .checked_add(table_len)
        .ok_or(GraphRefusal::LengthOverflow)?;
    let table = bytes.get(pos..table_end).ok_or(GraphRefusal::Truncated)?;
    let (children, rest) = table.as_chunks::<32>();
    debug_assert!(rest.is_empty());
    if children.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(GraphRefusal::NonCanonical {
            node,
            reason: CanonicalFault::UnsortedOrDuplicateChild,
        });
    }
    pos = table_end;
    let body_len = get_varint(bytes, &mut pos).map_err(refuse)?;
    let body_len = usize::try_from(body_len).map_err(|_| GraphRefusal::LengthOverflow)?;
    let body_end = pos
        .checked_add(body_len)
        .ok_or(GraphRefusal::LengthOverflow)?;
    let body = bytes.get(pos..body_end).ok_or(GraphRefusal::Truncated)?;
    if body_end != bytes.len() {
        return Err(GraphRefusal::TrailingBytes);
    }
    check_slots(body, children.len(), node)?;
    Ok(NodeEnvelope {
        kind: NodeKind(kind),
        children: ChildTable(children),
        body,
    })
}

/// Reads every token of a physical body: each must be canonical, each slot
/// in range, and each child referenced at least once.
fn check_slots(body: &[u8], children: usize, node: Option<NodeId>) -> Result<(), GraphRefusal> {
    let mut used = vec![false; children];
    let mut reader = TokenReader::with_form(body, RefForm::Physical);
    while !reader.is_empty() {
        if let TokenEvent::Slot(slot) = reader
            .next_event()
            .map_err(|error| GraphRefusal::from_token(error, node, None))?
        {
            let index = usize::try_from(slot)
                .ok()
                .filter(|index| *index < children)
                .ok_or(GraphRefusal::NonCanonical {
                    node,
                    reason: CanonicalFault::ChildSlotOutOfRange(slot),
                })?;
            used[index] = true;
        }
    }
    if let Some(unused) = used.iter().position(|used| !used) {
        return Err(GraphRefusal::NonCanonical {
            node,
            reason: CanonicalFault::UnusedChildSlot(unused as u64),
        });
    }
    Ok(())
}

impl TokenCodec for NodeId {
    fn encode(&self, out: &mut TokenWriter) {
        out.child(self.child_ref());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.child().map(Self::from)
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    /// FIG-5894: memoized build envelopes cannot certify changed bytes under
    /// an old digest or conceal an out-of-range child slot under a new one.
    /// FIG-5912: registration checks nothing, so a member whose bytes do not
    /// check is never reused, and no closure reaches through it. FIG-5924:
    /// nor is it a head a reader stops at.
    #[test]
    fn a_catalog_only_reuses_byte_identical_verified_envelopes() {
        let child = NodeId([1; 32]);
        let bytes: Arc<[u8]> =
            write_envelope(NodeKind::FROZEN_VALUE, &[child.0], &[0x0b, 0]).into();
        let id = NodeId::of_envelope(&bytes);
        let leaf: Arc<[u8]> = write_envelope(NodeKind::FROZEN_VALUE, &[], &[0]).into();
        let leaf_id = NodeId::of_envelope(&leaf);
        let parent: Arc<[u8]> =
            write_envelope(NodeKind::FROZEN_VALUE, &[leaf_id.0], &[0x0b, 0]).into();
        let parent_id = NodeId::of_envelope(&parent);
        // Registered under a digest its bytes do not have.
        let forged = NodeId([2; 32]);
        let forged_parent: Arc<[u8]> =
            write_envelope(NodeKind::FROZEN_VALUE, &[forged.0], &[0x0b, 0]).into();
        let forged_parent_id = NodeId::of_envelope(&forged_parent);
        let members = [
            (id, bytes.clone()),
            (leaf_id, leaf.clone()),
            (parent_id, parent.clone()),
            (forged, leaf.clone()),
            (forged_parent_id, forged_parent.clone()),
        ];
        register_immutable_catalog(
            &members
                .iter()
                .map(|(id, bytes)| {
                    (
                        *id,
                        EncodedNode {
                            id: *id,
                            bytes: bytes.clone(),
                        },
                    )
                })
                .collect(),
        );
        let catalog = IMMUTABLE_CATALOG.get().unwrap();
        assert_eq!(
            immutable_closure(parent_id)
                .unwrap()
                .nodes()
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            vec![parent_id, leaf_id]
        );
        assert!(catalog.get(forged, &leaf).is_none());
        assert_eq!(
            check_envelope(&leaf, forged, &DecodeLimits::within_total(u64::MAX)).unwrap_err(),
            GraphRefusal::HashMismatch(forged)
        );
        assert!(immutable_closure(forged).is_none());
        assert!(immutable_closure(forged_parent_id).is_none());
        assert!(immutable_closure(id).is_none(), "its child is no member");
        assert_eq!(immutable_catalog_heads(), &[parent_id]);
        let copied = bytes.to_vec();
        let limits = DecodeLimits::within_total(u64::MAX);
        let envelope = check_envelope(&copied, id, &limits).expect("identical copied bytes");
        assert_eq!(envelope.children.iter().collect::<Vec<_>>(), vec![child]);
        assert_eq!(envelope.body, &[0x0b, 0]);
        assert_eq!(
            check_envelope(
                &copied,
                id,
                &DecodeLimits {
                    node_bytes: 4,
                    ..limits
                }
            )
            .unwrap_err(),
            GraphRefusal::DecodeLimit {
                dimension: lash_kernel_doc::graph_tokens::DecodeDimension::NodeBytes,
                actual: copied.len() as u64,
                limit: 4,
            }
        );

        let mut changed = copied;
        *changed.last_mut().unwrap() = 1;
        assert!(catalog.get(id, &changed).is_none());
        assert_eq!(
            check_envelope(&changed, id, &limits).unwrap_err(),
            GraphRefusal::HashMismatch(id)
        );
        let changed_id = NodeId::of_envelope(&changed);
        assert!(catalog.get(changed_id, &changed).is_none());
        assert_eq!(
            check_envelope(&changed, changed_id, &limits).unwrap_err(),
            GraphRefusal::NonCanonical {
                node: Some(changed_id),
                reason: CanonicalFault::ChildSlotOutOfRange(1)
            }
        );
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The framing a logical body's envelope had before FIG-6932, built
    /// apart from [`frame_semantic`]: two passes over the body, a set of
    /// children, and each reference rewritten to its slot.
    fn reference(kind: NodeKind, logical: &[u8]) -> Vec<u8> {
        let mut children = BTreeSet::new();
        let mut reader = TokenReader::new(logical);
        while !reader.is_empty() {
            if let TokenEvent::Child(child) = reader.next_event().expect("canonical") {
                children.insert(child.0);
            }
        }
        let children: Vec<[u8; 32]> = children.into_iter().collect();
        let mut body = TokenWriter::new();
        let mut reader = TokenReader::new(logical);
        while !reader.is_empty() {
            match reader.next_event().expect("canonical") {
                TokenEvent::Child(child) => {
                    body.slot(children.binary_search(&child.0).expect("a child") as u64);
                }
                event => body.event(&event),
            }
        }
        write_envelope(kind, &children, body.as_bytes())
    }

    /// FIG-6932: a page is framed once, in one pass over its body, and its
    /// framing is canonical by construction: the envelope is the one the
    /// two-pass framing wrote, it checks under the digest its framer names
    /// it by, and its child table is the one that check reads. So whoever
    /// frames a page never reads its own framing back, and the parent's
    /// one check is the check.
    #[test]
    fn a_framed_page_is_the_envelope_its_check_reads() {
        let kind = NodeKind(1_002);
        let digest = |at: u32| {
            let mut digest = [0_u8; 32];
            digest[..4].copy_from_slice(&at.to_be_bytes());
            digest[31] = 7;
            ChildRef(digest)
        };
        let bodies: Vec<Vec<u8>> = vec![
            // No children.
            {
                let mut body = TokenWriter::new();
                body.seq(2);
                body.text("leaf");
                body.unsigned(1 << 40);
                body.into_bytes()
            },
            // Repeated references, out of digest order.
            {
                let mut body = TokenWriter::new();
                body.seq(4);
                for at in [9, 3, 9, 1] {
                    body.child(digest(at));
                }
                body.into_bytes()
            },
            // More children than a one-byte slot holds, in reverse order.
            {
                let mut body = TokenWriter::new();
                body.seq(300);
                for at in (0..300).rev() {
                    body.child(digest(at));
                }
                body.into_bytes()
            },
        ];
        for logical in bodies {
            let framed = frame_semantic(kind, &logical, MAX_NODE_BYTES).expect("frames");
            assert_eq!(framed.bytes(), reference(kind, &logical).as_slice());
            let node = framed.named();
            let checked =
                check_envelope(&node.bytes, node.id, &DecodeLimits::within_total(u64::MAX))
                    .expect("a framed page checks");
            assert_eq!(node.id, NodeId::of_envelope(&node.bytes));
            assert_eq!(node.kind(), checked.kind);
            assert_eq!(node.children(), checked.children);
            assert_eq!(
                direct_logical(&checked, node.id).expect("logical"),
                logical,
                "the framed body reads back as the body framed"
            );
        }
    }
}
