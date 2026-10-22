//! Decoding: from stored nodes back to the logical body a typed codec reads
//! (FIG-5887 §2, §3).
//!
//! [`GraphDecoder::logical_body`] checks the node's envelope and digest,
//! unwraps an enveloped kind's inline or streamed body, joins its structure
//! blocks, and puts every placed text and bytes value back inline. Every
//! non-canonical form is refused, never repaired: an inline value over the
//! inline bound, a placed value under it, a value chunked or indexed in any
//! shape but the canonical one, a stream whose declared lengths disagree
//! with its blocks, and a body streamed where it fits inline or cut at other
//! block boundaries. The final check re-encodes the decoded body and
//! requires the node's own identity back.

use std::borrow::Cow;
use std::collections::BTreeMap;

use lash_vocabulary::tokens::CanonicalFault;
use lash_vocabulary::tokens::DecodeDimension;
use lash_vocabulary::tokens::RefForm;
use lash_vocabulary::tokens::TokenCodec;
use lash_vocabulary::tokens::TokenEvent;
use lash_vocabulary::tokens::TokenReader;
use lash_vocabulary::tokens::TokenWriter;

use super::commit::{EncodeBudget, ExplicitLimit};
use super::encode::{GraphStaging, TypedBody, to_physical, token_len};
use super::node::{
    EncodedNode, INDEX_FANOUT, INLINE_TOKEN_MAX, NodeEnvelope, NodeId, check_envelope, envelope_len,
};
use super::refusal::{DecodeLimits, GraphRefusal};
use super::registry::{BodyShape, NodeKind};

/// How a stored blob's bytes are to be read: never sniffed from the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BlobEncoding {
    /// Raw bytes an adapter's codec owns; no edges.
    Opaque = 0,
    /// A canonical graph node envelope; its edges are its child table.
    Graph = 1,
}

/// One stored blob as a source holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredNode<'a> {
    pub encoding: BlobEncoding,
    pub bytes: &'a [u8],
}

/// Where a decoder reads nodes from: a hydrated graph, a pinned batch, or a
/// staging area.
pub trait NodeSource {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>>;
}

/// Every graph node `root` reaches in `source`, `root` included, each
/// envelope checked against its digest within `limits`: a root's whole
/// graph, as one map. An opaque blob a node names is not a graph node and
/// is not included; a persistent-heap page is included, and nothing it
/// reaches.
///
/// # Errors
///
/// [`GraphRefusal::MissingNode`] for a node `source` does not hold, or
/// any refusal of [`check_envelope`].
pub fn closure(
    source: &dyn NodeSource,
    root: NodeId,
    limits: &DecodeLimits,
) -> Result<BTreeMap<NodeId, EncodedNode>, GraphRefusal> {
    let mut nodes = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if nodes.contains_key(&id) {
            continue;
        }
        let stored = source.fetch(&id).ok_or(GraphRefusal::MissingNode(id))?;
        if stored.encoding != BlobEncoding::Graph {
            continue;
        }
        let envelope = check_envelope(stored.bytes, id, limits)?;
        // A persistent-heap page is the closure's leaf: what it reaches is
        // read through its heap, never as host graph (FIG-6193).
        if !super::node::is_semantic(envelope.kind) {
            pending.extend(envelope.children.iter());
        }
        nodes.insert(
            id,
            EncodedNode {
                id,
                bytes: std::sync::Arc::from(stored.bytes),
            },
        );
    }
    Ok(nodes)
}

impl NodeSource for BTreeMap<NodeId, EncodedNode> {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.get(id).map(|node| StoredNode {
            encoding: BlobEncoding::Graph,
            bytes: &node.bytes,
        })
    }
}

impl NodeSource for GraphStaging {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.nodes().fetch(id)
    }
}

/// Which family a placed value belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Text,
    Bytes,
}

impl Family {
    fn of(kind: NodeKind) -> Option<(Self, bool)> {
        match kind {
            NodeKind::TEXT_LEAF => Some((Self::Text, true)),
            NodeKind::TEXT_INDEX => Some((Self::Text, false)),
            NodeKind::BYTES_LEAF => Some((Self::Bytes, true)),
            NodeKind::BYTES_INDEX => Some((Self::Bytes, false)),
            _ => None,
        }
    }
}

// Keep a prepared expansion shared until the typed decoder finishes. Bodies
// without slots still borrow their checked source; streamed and unplaced
// bodies own their receiving decoder's materialization.
enum LogicalBody<'a> {
    Borrowed(&'a [u8]),
    Prepared(std::sync::Arc<[u8]>),
    Owned(Vec<u8>),
}

impl LogicalBody<'_> {
    fn into_owned(self) -> Vec<u8> {
        match self {
            Self::Borrowed(bytes) => bytes.to_vec(),
            Self::Prepared(bytes) => bytes.to_vec(),
            Self::Owned(bytes) => bytes,
        }
    }
}

impl std::ops::Deref for LogicalBody<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Prepared(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}

/// Reads nodes from one source within one set of limits.
pub struct GraphDecoder<'s> {
    source: &'s dyn NodeSource,
    limits: DecodeLimits,
    nodes_read: u64,
    bytes_read: u64,
}

fn verifier() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

fn non_canonical(node: NodeId, reason: CanonicalFault) -> GraphRefusal {
    GraphRefusal::NonCanonical {
        node: Some(node),
        reason,
    }
}

impl<'s> GraphDecoder<'s> {
    #[must_use]
    pub fn new(source: &'s dyn NodeSource, limits: DecodeLimits) -> Self {
        Self {
            source,
            limits,
            nodes_read: 0,
            bytes_read: 0,
        }
    }

    /// The limits every read of this decoder is held to.
    #[must_use]
    pub fn limits(&self) -> DecodeLimits {
        self.limits
    }

    /// Nodes and envelope bytes read so far: a restore's read cost.
    #[must_use]
    pub fn read(&self) -> (u64, u64) {
        (self.nodes_read, self.bytes_read)
    }

    /// Fetches, counts and checks one graph node.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::MissingNode`], [`GraphRefusal::EncodingConflict`] for
    /// an opaque blob, [`GraphRefusal::DecodeLimit`], or any refusal of
    /// [`check_envelope`].
    pub fn envelope(&mut self, id: NodeId) -> Result<NodeEnvelope<'s>, GraphRefusal> {
        let stored = self
            .source
            .fetch(&id)
            .ok_or(GraphRefusal::MissingNode(id))?;
        if stored.encoding != BlobEncoding::Graph {
            return Err(GraphRefusal::EncodingConflict(id));
        }
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_graph_decode(stored.bytes.len());
        self.nodes_read += 1;
        self.bytes_read += stored.bytes.len() as u64;
        self.limit(DecodeDimension::Nodes, self.nodes_read, self.limits.nodes)?;
        self.limit(
            DecodeDimension::TotalBytes,
            self.bytes_read,
            self.limits.total_bytes,
        )?;
        Ok(check_envelope(stored.bytes, id, &self.limits)?)
    }

    fn limit(
        &self,
        dimension: DecodeDimension,
        actual: u64,
        limit: u64,
    ) -> Result<(), GraphRefusal> {
        if actual > limit {
            Err(GraphRefusal::DecodeLimit {
                dimension,
                actual,
                limit,
            })
        } else {
            Ok(())
        }
    }

    /// The kind of `child`, refused unless it is one of `allowed`: what a
    /// typed decoder checks of each reference it reads.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::WrongChildKind`], or any refusal of
    /// [`Self::envelope`].
    pub fn child_kind(
        &mut self,
        parent: NodeId,
        child: NodeId,
        allowed: &[NodeKind],
    ) -> Result<NodeKind, GraphRefusal> {
        let kind = self.envelope(child)?.kind;
        if allowed.contains(&kind) {
            Ok(kind)
        } else {
            Err(GraphRefusal::WrongChildKind { parent, child })
        }
    }

    /// Decodes node `id` of kind `expected` as one `T`.
    ///
    /// # Errors
    ///
    /// Any refusal of [`Self::logical_body`] or of `T`'s decoder.
    pub fn decode<T: TokenCodec>(
        &mut self,
        id: NodeId,
        expected: NodeKind,
    ) -> Result<T, GraphRefusal> {
        let logical = self.materialized_body(id, expected)?;
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_graph_decoded_body(logical.len());
        let refuse = |error| GraphRefusal::from_token(error, Some(id), Some(expected.0));
        let mut reader = TokenReader::new(&logical).with_data_limits(self.limits.kernel_data);
        let value = T::decode(&mut reader).map_err(refuse)?;
        reader.finish().map_err(refuse)?;
        Ok(value)
    }

    /// The logical body of node `id` of kind `expected`: references as
    /// digests, placed values inline, a streamed body joined.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::KindMismatch`], [`GraphRefusal::UnsupportedKind`],
    /// [`GraphRefusal::UnsupportedVariant`], [`GraphRefusal::NonCanonical`]
    /// for every non-canonical form, or any refusal of a node read.
    pub fn logical_body(
        &mut self,
        id: NodeId,
        expected: NodeKind,
    ) -> Result<Vec<u8>, GraphRefusal> {
        self.materialized_body(id, expected)
            .map(LogicalBody::into_owned)
    }

    fn materialized_body(
        &mut self,
        id: NodeId,
        expected: NodeKind,
    ) -> Result<LogicalBody<'s>, GraphRefusal> {
        let envelope = self.envelope(id)?;
        if envelope.kind != expected {
            return Err(GraphRefusal::KindMismatch {
                node: id,
                expected: expected.0,
                found: envelope.kind.0,
            });
        }
        let spec = expected
            .spec()
            .ok_or(GraphRefusal::UnsupportedKind(expected.0))?;
        let refuse = |error| GraphRefusal::from_token(error, Some(id), Some(expected.0));
        match spec.shape {
            BodyShape::Direct => to_logical(&envelope, envelope.body, id),
            BodyShape::Enveloped => {
                let mut reader = envelope.reader();
                let (variant, fields) = reader.sum().map_err(refuse)?;
                if variant > 1 {
                    return Err(GraphRefusal::UnsupportedVariant {
                        kind: expected.0,
                        variant,
                    });
                }
                TokenReader::expect_fields(fields, 1).map_err(refuse)?;
                let rest = &envelope.body[reader.position()..];
                let placed = if variant == 0 {
                    let placed = to_logical(&envelope, rest, id)?;
                    let mut tree = TokenReader::new(&placed);
                    tree.skip_tree().map_err(refuse)?;
                    if !tree.is_empty() {
                        return Err(non_canonical(id, CanonicalFault::NotOneToken));
                    }
                    placed
                } else {
                    let mut reader = TokenReader::with_form(rest, RefForm::Physical);
                    let slot = match reader.next_event().map_err(refuse)? {
                        TokenEvent::Slot(slot) => slot,
                        _ => return Err(non_canonical(id, CanonicalFault::InvalidBodyForm)),
                    };
                    reader.finish().map_err(refuse)?;
                    let stream = envelope
                        .children
                        .get(slot)
                        .ok_or(non_canonical(id, CanonicalFault::ChildSlotOutOfRange(slot)))?;
                    let placed = self.stream(id, stream, expected)?;
                    let (children, physical) = to_physical(&placed, expected)?;
                    if envelope_len(expected, children.len(), physical.len() + 3) as u64
                        <= spec.bound.bytes()
                    {
                        return Err(non_canonical(id, CanonicalFault::InvalidBodyForm));
                    }
                    LogicalBody::Owned(placed)
                };
                let logical = match self.unplace(id, &placed)? {
                    Cow::Borrowed(_) => placed,
                    Cow::Owned(logical) => LogicalBody::Owned(logical),
                };
                let mut check = verifier();
                if check.encode(expected, TypedBody::logical(&logical))? != id {
                    return Err(non_canonical(id, CanonicalFault::InvalidBodyForm));
                }
                Ok(logical)
            }
        }
    }

    /// The text placed at `id`, a TextLeaf or TextIndex.
    ///
    /// # Errors
    ///
    /// A wrong kind, a non-canonical chunking, or any node read's refusal.
    pub fn text(&mut self, id: NodeId) -> Result<String, GraphRefusal> {
        let bytes = self.placed(id, Family::Text)?;
        String::from_utf8(bytes.into_owned()).map_err(|_| GraphRefusal::InvalidUtf8)
    }

    /// The bytes placed at `id`, a BytesLeaf or BytesIndex.
    ///
    /// # Errors
    ///
    /// A wrong kind, a non-canonical chunking, or any node read's refusal.
    pub fn bytes(&mut self, id: NodeId) -> Result<Vec<u8>, GraphRefusal> {
        self.placed(id, Family::Bytes).map(Cow::into_owned)
    }

    /// A placed value, assembled and then required to be exactly what
    /// placing it again stages.
    fn placed(&mut self, id: NodeId, family: Family) -> Result<Cow<'s, [u8]>, GraphRefusal> {
        self.limit(DecodeDimension::Depth, 0, self.limits.depth)?;
        let envelope = self.envelope(id)?;
        let value = if Family::of(envelope.kind) == Some((family, true)) {
            let payload = leaf_payload(&envelope, family, id)?;
            self.limit(
                DecodeDimension::LogicalBytes,
                payload.len() as u64,
                self.limits.logical_bytes,
            )?;
            Cow::Borrowed(payload)
        } else {
            let mut value = Vec::new();
            self.assemble_envelope(id, family, 0, envelope, &mut value)?;
            Cow::Owned(value)
        };
        let mut check = verifier();
        let again = match family {
            Family::Text => {
                let text = std::str::from_utf8(&value).map_err(|_| GraphRefusal::InvalidUtf8)?;
                check.place_text(text)?
            }
            Family::Bytes => check.place_bytes(&value)?,
        };
        if again != id {
            return Err(non_canonical(id, CanonicalFault::InvalidChunking));
        }
        Ok(value)
    }

    /// Appends the value under `id` to `out` and returns its length.
    fn assemble(
        &mut self,
        id: NodeId,
        family: Family,
        depth: u64,
        out: &mut Vec<u8>,
    ) -> Result<u64, GraphRefusal> {
        self.limit(DecodeDimension::Depth, depth, self.limits.depth)?;
        let envelope = self.envelope(id)?;
        self.assemble_envelope(id, family, depth, envelope, out)
    }

    fn assemble_envelope(
        &mut self,
        id: NodeId,
        family: Family,
        depth: u64,
        envelope: NodeEnvelope<'s>,
        out: &mut Vec<u8>,
    ) -> Result<u64, GraphRefusal> {
        let refuse = |error| GraphRefusal::from_token(error, Some(id), Some(envelope.kind.0));
        let (found, leaf) = Family::of(envelope.kind).ok_or(GraphRefusal::KindMismatch {
            node: id,
            expected: match family {
                Family::Text => NodeKind::TEXT_LEAF.0,
                Family::Bytes => NodeKind::BYTES_LEAF.0,
            },
            found: envelope.kind.0,
        })?;
        if found != family {
            return Err(non_canonical(id, CanonicalFault::InvalidChunking));
        }
        let mut reader = envelope.reader();
        if leaf {
            let payload = leaf_payload(&envelope, family, id)?;
            self.grow(out, payload)?;
            return Ok(payload.len() as u64);
        }
        reader.expect_struct(2).map_err(refuse)?;
        let total = reader.unsigned().map_err(refuse)?;
        let count = reader.seq().map_err(refuse)?;
        if count == 0 || count > INDEX_FANOUT as u64 {
            return Err(non_canonical(id, CanonicalFault::InvalidChunking));
        }
        let mut sum = 0_u64;
        for _ in 0..count {
            reader.expect_struct(2).map_err(refuse)?;
            let slot = match reader.next_event().map_err(refuse)? {
                TokenEvent::Slot(slot) => slot,
                _ => return Err(non_canonical(id, CanonicalFault::InvalidChunking)),
            };
            let declared = reader.unsigned().map_err(refuse)?;
            let child = envelope
                .children
                .get(slot)
                .ok_or(non_canonical(id, CanonicalFault::ChildSlotOutOfRange(slot)))?;
            let actual = self.assemble(child, family, depth + 1, out)?;
            if actual != declared {
                return Err(non_canonical(id, CanonicalFault::AggregateLengthMismatch));
            }
            sum = sum.saturating_add(actual);
        }
        reader.finish().map_err(refuse)?;
        if sum != total {
            return Err(non_canonical(id, CanonicalFault::AggregateLengthMismatch));
        }
        Ok(total)
    }

    fn grow(&self, out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), GraphRefusal> {
        let len = (out.len() + bytes.len()) as u64;
        self.limit(
            DecodeDimension::LogicalBytes,
            len,
            self.limits.logical_bytes,
        )?;
        out.extend_from_slice(bytes);
        Ok(())
    }

    /// Joins the structure blocks of `stream`, the WholeStream of `parent`
    /// of `kind`, into the placed logical body.
    fn stream(
        &mut self,
        parent: NodeId,
        stream: NodeId,
        kind: NodeKind,
    ) -> Result<Vec<u8>, GraphRefusal> {
        let envelope = self.envelope(stream)?;
        if envelope.kind != NodeKind::WHOLE_STREAM {
            return Err(GraphRefusal::WrongChildKind {
                parent,
                child: stream,
            });
        }
        let refuse = |error| GraphRefusal::from_token(error, Some(stream), Some(5));
        let mut reader = envelope.reader();
        reader.expect_struct(4).map_err(refuse)?;
        let schema = reader.unsigned().map_err(refuse)?;
        if schema != u64::from(kind.0) {
            return Err(GraphRefusal::KindMismatch {
                node: stream,
                expected: kind.0,
                found: u32::try_from(schema).unwrap_or(u32::MAX),
            });
        }
        let declared_len = reader.unsigned().map_err(refuse)?;
        self.limit(
            DecodeDimension::LogicalBytes,
            declared_len,
            self.limits.logical_bytes,
        )?;
        let tree = match reader.next_event().map_err(refuse)? {
            TokenEvent::Slot(slot) => envelope.children.get(slot).ok_or(non_canonical(
                stream,
                CanonicalFault::ChildSlotOutOfRange(slot),
            ))?,
            _ => return Err(non_canonical(stream, CanonicalFault::InvalidChunking)),
        };
        let declared_blocks = reader.unsigned().map_err(refuse)?;
        reader.finish().map_err(refuse)?;
        let mut placed = Vec::new();
        let blocks = self.blocks(stream, tree, 0, &mut placed)?;
        if blocks != declared_blocks || placed.len() as u64 != declared_len {
            return Err(non_canonical(
                stream,
                CanonicalFault::AggregateLengthMismatch,
            ));
        }
        Ok(placed)
    }

    /// Appends the blocks under `node` and returns how many there were.
    fn blocks(
        &mut self,
        parent: NodeId,
        node: NodeId,
        depth: u64,
        out: &mut Vec<u8>,
    ) -> Result<u64, GraphRefusal> {
        self.limit(DecodeDimension::Depth, depth, self.limits.depth)?;
        let envelope = self.envelope(node)?;
        match envelope.kind {
            NodeKind::STRUCTURE_BLOCK => {
                let logical = to_logical(&envelope, envelope.body, node)?;
                self.grow(out, &logical)?;
                Ok(1)
            }
            NodeKind::STREAM_INDEX => {
                let refuse = |error| GraphRefusal::from_token(error, Some(node), Some(7));
                let mut reader = envelope.reader();
                reader.expect_struct(3).map_err(refuse)?;
                let _level = reader.unsigned().map_err(refuse)?;
                let declared = reader.unsigned().map_err(refuse)?;
                let count = reader.seq().map_err(refuse)?;
                if count == 0 || count > INDEX_FANOUT as u64 {
                    return Err(non_canonical(node, CanonicalFault::InvalidChunking));
                }
                let mut sum = 0_u64;
                for _ in 0..count {
                    reader.expect_struct(2).map_err(refuse)?;
                    let child = match reader.next_event().map_err(refuse)? {
                        TokenEvent::Slot(slot) => envelope.children.get(slot).ok_or(
                            non_canonical(node, CanonicalFault::ChildSlotOutOfRange(slot)),
                        )?,
                        _ => return Err(non_canonical(node, CanonicalFault::InvalidChunking)),
                    };
                    let child_declared = reader.unsigned().map_err(refuse)?;
                    let actual = self.blocks(node, child, depth + 1, out)?;
                    if actual != child_declared {
                        return Err(non_canonical(node, CanonicalFault::AggregateLengthMismatch));
                    }
                    sum = sum.saturating_add(actual);
                }
                reader.finish().map_err(refuse)?;
                if sum != declared {
                    return Err(non_canonical(node, CanonicalFault::AggregateLengthMismatch));
                }
                Ok(sum)
            }
            _ => Err(GraphRefusal::WrongChildKind {
                parent,
                child: node,
            }),
        }
    }

    /// Puts every placed value of `placed` back inline, refusing an inline
    /// value over the bound and a placed one under it.
    fn unplace<'p>(&mut self, id: NodeId, placed: &'p [u8]) -> Result<Cow<'p, [u8]>, GraphRefusal> {
        let refuse = |error| GraphRefusal::from_token(error, Some(id), None);
        let mut reader = TokenReader::new(placed);
        let mut out: Option<TokenWriter> = None;
        while !reader.is_empty() {
            let start = reader.position();
            match reader.next_event().map_err(refuse)? {
                TokenEvent::Text(text) if token_len(text.len()) > INLINE_TOKEN_MAX => {
                    return Err(non_canonical(id, CanonicalFault::InvalidPlacement));
                }
                TokenEvent::Bytes(bytes) if token_len(bytes.len()) > INLINE_TOKEN_MAX => {
                    return Err(non_canonical(id, CanonicalFault::InvalidPlacement));
                }
                TokenEvent::Child(child) => {
                    let child = NodeId::from(child);
                    let stored = self
                        .source
                        .fetch(&child)
                        .ok_or(GraphRefusal::MissingNode(child))?;
                    let kind = match stored.encoding {
                        BlobEncoding::Graph => Some(self.envelope(child)?.kind),
                        BlobEncoding::Opaque => None,
                    };
                    match kind.and_then(Family::of) {
                        Some((family, _)) => {
                            let value = self.placed(child, family)?;
                            if token_len(value.len()) <= INLINE_TOKEN_MAX {
                                return Err(non_canonical(id, CanonicalFault::InvalidPlacement));
                            }
                            let writer = out.get_or_insert_with(|| {
                                let capacity = placed.len() - (reader.position() - start)
                                    + token_len(value.len()) as usize;
                                let mut writer = TokenWriter::with_capacity(capacity);
                                writer.raw(&placed[..start]);
                                writer
                            });
                            match family {
                                Family::Text => writer.text(
                                    std::str::from_utf8(&value)
                                        .map_err(|_| GraphRefusal::InvalidUtf8)?,
                                ),
                                Family::Bytes => writer.bytes(&value),
                            }
                        }
                        None => {
                            if let Some(writer) = &mut out {
                                writer.raw(&placed[start..reader.position()]);
                            }
                        }
                    }
                }
                _ => {
                    if let Some(writer) = &mut out {
                        writer.raw(&placed[start..reader.position()]);
                    }
                }
            }
            self.limit(
                DecodeDimension::LogicalBytes,
                out.as_ref().map_or(reader.position(), TokenWriter::len) as u64,
                self.limits.logical_bytes,
            )?;
        }
        Ok(out.map_or(Cow::Borrowed(placed), |writer| {
            Cow::Owned(writer.into_bytes())
        }))
    }
}

/// A physical body's logical form: each slot its child's digest.
fn to_logical<'b>(
    envelope: &NodeEnvelope<'_>,
    body: &'b [u8],
    id: NodeId,
) -> Result<LogicalBody<'b>, GraphRefusal> {
    // Envelope admission has already checked every token, even for a body
    // without child slots. Such bodies can continue borrowing the source.
    if envelope.children.is_empty() {
        return Ok(LogicalBody::Borrowed(body));
    }
    let skip = envelope.body.len() - body.len();
    Ok(LogicalBody::Prepared(envelope.logical_tokens(skip, id)?))
}

fn leaf_payload<'s>(
    envelope: &NodeEnvelope<'s>,
    family: Family,
    id: NodeId,
) -> Result<&'s [u8], GraphRefusal> {
    let refuse = |error| GraphRefusal::from_token(error, Some(id), Some(envelope.kind.0));
    let mut reader = envelope.reader();
    let payload = match family {
        Family::Text => reader.text().map_err(refuse)?.as_bytes(),
        Family::Bytes => reader.bytes().map_err(refuse)?,
    };
    reader.finish().map_err(refuse)?;
    Ok(payload)
}

#[cfg(test)]
mod preparation_tests {
    use super::super::node::write_envelope;
    use super::*;

    /// A physical body's pure expansion is shared across cold readers;
    /// acquiring it never memoizes the receiving reader's admission.
    #[test]
    fn physical_expansion_is_shared_without_memoizing_read_admission() {
        let child = NodeId([0x37; 32]);
        let bytes = write_envelope(NodeKind::TEXT_INDEX, &[child.0], &[0x0b, 0]);
        let id = NodeId::of_envelope(&bytes);
        let nodes = BTreeMap::from([(
            id,
            EncodedNode {
                id,
                bytes: bytes.into(),
            },
        )]);
        let limits = DecodeLimits::within_total(1 << 20);
        let mut first = GraphDecoder::new(&nodes, limits);
        let first_envelope = first.envelope(id).unwrap();
        let first_body = to_logical(&first_envelope, first_envelope.body, id).unwrap();
        let mut second = GraphDecoder::new(&nodes, limits);
        let second_envelope = second.envelope(id).unwrap();
        let second_body = to_logical(&second_envelope, second_envelope.body, id).unwrap();
        assert_eq!(&*first_body, &*second_body);
        assert_eq!(first.read(), second.read());
        assert!(matches!(
            GraphDecoder::new(&nodes, DecodeLimits { nodes: 0, ..limits }).envelope(id),
            Err(GraphRefusal::DecodeLimit {
                dimension: DecodeDimension::Nodes,
                ..
            })
        ));
        assert_eq!(
            GraphDecoder::new(&BTreeMap::<NodeId, EncodedNode>::new(), limits).envelope(id),
            Err(GraphRefusal::MissingNode(id))
        );
        let mut changed = nodes.clone();
        changed.get_mut(&id).unwrap().bytes =
            write_envelope(NodeKind::TEXT_INDEX, &[[0x38; 32]], &[0x0b, 0]).into();
        assert_eq!(
            GraphDecoder::new(&changed, limits).envelope(id),
            Err(GraphRefusal::HashMismatch(id))
        );
        assert_eq!(
            first_body.as_ptr(),
            second_body.as_ptr(),
            "the same physical body was expanded twice"
        );
    }
}
