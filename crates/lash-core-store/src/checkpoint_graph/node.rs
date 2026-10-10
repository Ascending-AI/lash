//! The node envelope, its digest, and the generic check every reader and the
//! collector runs (FIG-5887 §2).

use std::fmt;
use std::sync::Arc;

use lash_kernel_doc::graph_tokens::{
    CanonicalFault, ChildRef, RefForm, TokenEvent, TokenReader, get_varint, put_varint,
};

use super::refusal::{DecodeLimits, GraphRefusal};
use super::registry::NodeKind;
use crate::store::BlobRef;

/// The four bytes every envelope starts with.
pub const MAGIC: [u8; 4] = *b"LASH";

/// The largest complete envelope of any node, in bytes. A format constant:
/// a host budget may be smaller, never larger.
pub const MAX_NODE_BYTES: u64 = 65_536;

/// The largest complete envelope of a root kind (ExecRoot, HostRoot, CellEnd,
/// KernelWhole, SessionExecution, PhaseRoot, SessionManifest, WholeStream).
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
        Self(BlobRef::digest_for_content(bytes))
    }

    /// The lowercase hex SQL key.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut text = String::with_capacity(64);
        for byte in self.0 {
            text.push_str(&format!("{byte:02x}"));
        }
        text
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
pub(crate) fn write_envelope(kind: NodeKind, children: &[[u8; 32]], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(envelope_len(kind, children.len(), body.len()));
    out.extend_from_slice(&MAGIC);
    put_varint(&mut out, u64::from(kind.0));
    put_varint(&mut out, children.len() as u64);
    for child in children {
        out.extend_from_slice(child);
    }
    put_varint(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    out
}

/// The length [`write_envelope`] produces.
pub(crate) fn envelope_len(kind: NodeKind, children: usize, body: usize) -> usize {
    use lash_kernel_doc::graph_tokens::varint_len;
    MAGIC.len()
        + varint_len(u64::from(kind.0))
        + varint_len(children as u64)
        + 32 * children
        + varint_len(body as u64)
        + body
}

/// The generic check: verifies `bytes` hash to `expected`, then parses them
/// with [`parse_envelope`], within `limits`' node-size bound.
///
/// It accepts any nonzero kind, registered or not, so a collector can follow
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
    let len = bytes.len() as u64;
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
    if NodeId::of_envelope(bytes) != expected {
        return Err(GraphRefusal::HashMismatch(expected));
    }
    parse_envelope(bytes, Some(expected))
}

/// Parses an envelope without hashing it: magic, minimal varints, a
/// strictly ascending child table, an exact body length, and a body of
/// canonical tokens whose every slot is in range and every child referenced.
///
/// # Errors
///
/// [`GraphRefusal::BadMagic`], [`GraphRefusal::UnsupportedKind`] for kind 0,
/// [`GraphRefusal::Truncated`], [`GraphRefusal::TrailingBytes`],
/// [`GraphRefusal::LengthOverflow`], or [`GraphRefusal::NonCanonical`].
pub fn parse_envelope(
    bytes: &[u8],
    node: Option<NodeId>,
) -> Result<NodeEnvelope<'_>, GraphRefusal> {
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
