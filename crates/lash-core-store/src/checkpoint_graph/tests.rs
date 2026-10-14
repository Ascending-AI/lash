//! PC-CODEC (FIG-5887 §11), graph half: one canonical encoding with exact
//! round trips, the 256 B inline boundary, UTF-8 chunk boundaries, the
//! 64 KiB envelope and 4 KiB root bounds, child edges equal to the encoded
//! table, and every refusal; plus the shared grammar-vector fixture.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::graph_tokens::{
    CanonicalFault, RefForm, TokenEvent, TokenReader, TokenWriter, check_ascending, encode_body,
};

use super::bodies::{
    ExecRoot, ExecRootKind, ExecStatus, OpaqueLeaf, OpaquePayload, OwnerBinding, ProcessState,
    ProcessStateBody,
};
use super::node::{envelope_len, write_envelope};
use super::*;

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

fn limits() -> DecodeLimits {
    DecodeLimits::within_total(1 << 34)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

fn text_body(text: &str) -> Vec<u8> {
    let mut out = TokenWriter::new();
    out.text(text);
    out.into_bytes()
}

fn logical(source: &dyn NodeSource, id: NodeId, kind: NodeKind) -> Result<Vec<u8>, GraphRefusal> {
    GraphDecoder::new(source, limits()).logical_body(id, kind)
}

/// A raw envelope and its identity, for building what the encoder never
/// would.
fn raw(kind: u32, children: &[NodeId], body: &[u8]) -> EncodedNode {
    let children: Vec<[u8; 32]> = children.iter().map(|child| child.0).collect();
    let bytes = write_envelope(NodeKind(kind), &children, body);
    EncodedNode {
        id: NodeId::of_envelope(&bytes),
        bytes: bytes.into(),
    }
}

fn source(nodes: impl IntoIterator<Item = EncodedNode>) -> BTreeMap<NodeId, EncodedNode> {
    nodes.into_iter().map(|node| (node.id, node)).collect()
}

/// Every digest a node's physical body references, read generically.
fn referenced(node: &EncodedNode) -> BTreeSet<NodeId> {
    let envelope = node.envelope().unwrap();
    let mut reader = envelope.reader();
    let mut out = BTreeSet::new();
    while !reader.is_empty() {
        if let TokenEvent::Slot(slot) = reader.next_event().unwrap() {
            out.insert(envelope.children.get(slot).unwrap());
        }
    }
    out
}

/// The node's child table and the references of its body agree, and every
/// child is staged: a store projects exactly these edges.
fn assert_edges_are_the_table(staging: &GraphStaging) {
    for node in staging.nodes().values() {
        let table: BTreeSet<NodeId> = node.children().unwrap().into_iter().collect();
        assert_eq!(table, referenced(node), "{:?}", node.id);
        for child in table {
            assert!(staging.get(&child).is_some(), "{child:?} is staged");
        }
    }
}

fn exec_root(staging: &mut GraphStaging, identity: &str) -> (NodeId, ExecRoot) {
    let owner = staging
        .encode_value(
            NodeKind::OWNER_BINDING,
            &OwnerBinding::Process {
                process: "p".repeat(300),
            },
        )
        .unwrap();
    let state = staging
        .encode_value(
            NodeKind::PROCESS_STATE,
            &ProcessState {
                engine: "engine".into(),
                executable_identity: identity.into(),
                state_format: 3,
                state: ProcessStateBody::EngineBytes(vec![7; 70_000]),
            },
        )
        .unwrap();
    let root = ExecRoot {
        exec: ExecRootKind::Process,
        owner,
        executable_identity: identity.into(),
        format: 3,
        status: ExecStatus::Parked,
        state,
        effect_membership: None,
        host: None,
        envelope: None,
        owed: Vec::new(),
    };
    (
        staging.encode_value(NodeKind::EXEC_ROOT, &root).unwrap(),
        root,
    )
}

/// One value has one encoding: encoding it again gives the same nodes, and
/// decoding gives back the value and the logical tokens it was encoded
/// from, every tag, number and order included.
#[test]
fn one_canonical_encoding_round_trips_exactly() {
    let mut first = staging();
    let (id, root) = exec_root(&mut first, &"e".repeat(1_000));
    let mut second = staging();
    assert_eq!(exec_root(&mut second, &"e".repeat(1_000)).0, id);
    assert_eq!(first.nodes(), second.nodes());

    let mut decoder = GraphDecoder::new(&first, limits());
    let decoded: ExecRoot = decoder.decode(id, NodeKind::EXEC_ROOT).unwrap();
    assert_eq!(decoded, root);
    decoded.check_children(id, &mut decoder).unwrap();
    let state: ProcessState = decoder.decode(root.state, NodeKind::PROCESS_STATE).unwrap();
    assert_eq!(state.state, ProcessStateBody::EngineBytes(vec![7; 70_000]));
    state.check_children(root.state, &mut decoder).unwrap();

    let mut tokens = TokenWriter::new();
    tokens.seq(14);
    tokens.unit();
    tokens.bool(false);
    tokens.bool(true);
    tokens.unsigned(u64::MAX);
    tokens.signed(i64::MIN);
    tokens.big_integer(true, &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    tokens.float(-0.0);
    tokens.text(&"é".repeat(200));
    tokens.bytes(&[0, 1, 2]);
    tokens.map(1);
    tokens.text("k");
    tokens.none();
    tokens.child(id.child_ref());
    tokens.sum(4_000_000_000, 1);
    tokens.some();
    tokens.float(f64::NAN);
    tokens.seq(0);
    tokens.text("");
    let tokens = tokens.into_bytes();
    let value = first
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&tokens))
        .unwrap();
    assert_eq!(
        logical(&first, value, NodeKind::FROZEN_VALUE).unwrap(),
        tokens
    );

    let opaque = OpaqueLeaf {
        codec: "plugin/v1".into(),
        payload: OpaquePayload::Inline(vec![9; 300]),
    };
    let leaf = first.encode_value(NodeKind::OPAQUE_LEAF, &opaque).unwrap();
    let mut decoder = GraphDecoder::new(&first, limits());
    assert_eq!(
        decoder
            .decode::<OpaqueLeaf>(leaf, NodeKind::OPAQUE_LEAF)
            .unwrap(),
        opaque
    );
    assert_edges_are_the_table(&first);
}

/// A text or bytes token of at most 256 bytes, tag and length included, is
/// inline; one byte more and it is a shared leaf. Either way the logical
/// body comes back identical.
#[test]
fn the_inline_boundary_is_256_token_bytes() {
    for (len, placed) in [(253_usize, false), (254, true)] {
        let mut staging = staging();
        let text = "t".repeat(len);
        let mut body = TokenWriter::new();
        body.seq(2);
        body.text(&text);
        body.bytes(text.as_bytes());
        let body = body.into_bytes();
        let id = staging
            .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&body))
            .unwrap();
        let node = staging.get(&id).unwrap();
        let children = node.children().unwrap();
        assert_eq!(children.len(), if placed { 2 } else { 0 }, "length {len}");
        if placed {
            let kinds: BTreeSet<NodeKind> = children
                .iter()
                .map(|child| staging.get(child).unwrap().envelope().unwrap().kind)
                .collect();
            assert_eq!(
                kinds,
                BTreeSet::from([NodeKind::TEXT_LEAF, NodeKind::BYTES_LEAF])
            );
        }
        assert_eq!(logical(&staging, id, NodeKind::FROZEN_VALUE).unwrap(), body);
        assert_edges_are_the_table(&staging);
    }
}

/// Text chunks hold at most 65,520 bytes and end on a UTF-8 boundary; more
/// than 32 chunks index in two levels; equal text anywhere shares its
/// leaves.
#[test]
fn text_chunks_end_on_utf8_boundaries_and_equal_text_shares_leaves() {
    let leaf_lengths = |staging: &GraphStaging| -> Vec<usize> {
        let mut lengths: Vec<usize> = staging
            .nodes()
            .values()
            .filter(|node| node.envelope().unwrap().kind == NodeKind::TEXT_LEAF)
            .map(|node| {
                let envelope = node.envelope().unwrap();
                TokenReader::with_form(envelope.body, RefForm::Physical)
                    .text()
                    .unwrap()
                    .len()
            })
            .collect();
        lengths.sort_unstable();
        lengths
    };
    for (text, lengths) in [
        ("x".repeat(LEAF_PAYLOAD_MAX), vec![LEAF_PAYLOAD_MAX]),
        ("x".repeat(LEAF_PAYLOAD_MAX + 1), vec![1, LEAF_PAYLOAD_MAX]),
        (
            format!("{}é", "x".repeat(LEAF_PAYLOAD_MAX - 1)),
            vec![2, LEAF_PAYLOAD_MAX - 1],
        ),
        (
            format!("{}€", "x".repeat(LEAF_PAYLOAD_MAX - 2)),
            vec![3, LEAF_PAYLOAD_MAX - 2],
        ),
        (
            format!("{}€", "x".repeat(LEAF_PAYLOAD_MAX - 3)),
            vec![LEAF_PAYLOAD_MAX],
        ),
    ] {
        let mut staging = staging();
        let id = staging
            .encode(
                NodeKind::FROZEN_VALUE,
                TypedBody::logical(&text_body(&text)),
            )
            .unwrap();
        assert_eq!(leaf_lengths(&staging), lengths);
        let leaf = staging
            .nodes()
            .values()
            .find(|node| node.envelope().unwrap().kind == NodeKind::TEXT_LEAF)
            .unwrap();
        assert!(leaf.len() < MAX_NODE_BYTES);
        assert_eq!(
            logical(&staging, id, NodeKind::FROZEN_VALUE).unwrap(),
            text_body(&text)
        );
    }

    let mut staging = staging();
    let long: String = (0..33 * LEAF_PAYLOAD_MAX)
        .map(|index| char::from(b'a' + (index / LEAF_PAYLOAD_MAX) as u8))
        .collect();
    let first = staging
        .encode(
            NodeKind::FROZEN_VALUE,
            TypedBody::logical(&text_body(&long)),
        )
        .unwrap();
    let indexes = staging
        .nodes()
        .values()
        .filter(|node| node.envelope().unwrap().kind == NodeKind::TEXT_INDEX)
        .count();
    assert_eq!(indexes, 3, "32 + 1 leaves under one top index");
    let before = staging.nodes().len();
    let mut other = TokenWriter::new();
    other.seq(2);
    other.text("another document");
    other.text(&long);
    let other = other.into_bytes();
    let second = staging
        .encode(NodeKind::DOCUMENT, TypedBody::logical(&other))
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        staging.nodes().len(),
        before + 1,
        "the second node shares every leaf"
    );
    assert_eq!(
        logical(&staging, second, NodeKind::DOCUMENT).unwrap(),
        other
    );
    assert_edges_are_the_table(&staging);
}

/// A complete envelope of 65,536 bytes is admitted and one byte more is
/// refused, by the encoder and by the generic check alike.
#[test]
fn envelopes_are_bounded_at_64_kib() {
    // A direct TextLeaf envelope is 13 bytes plus its payload.
    let mut staging = staging();
    let fits = staging.encode(
        NodeKind::TEXT_LEAF,
        TypedBody::logical(&text_body(&"x".repeat(65_523))),
    );
    let fits = fits.unwrap();
    assert_eq!(staging.get(&fits).unwrap().len(), MAX_NODE_BYTES);
    assert_eq!(
        staging.encode(
            NodeKind::TEXT_LEAF,
            TypedBody::logical(&text_body(&"x".repeat(65_524)))
        ),
        Err(GraphRefusal::EncodedNodeTooLarge {
            bytes: MAX_NODE_BYTES + 1,
            limit: MAX_NODE_BYTES
        })
    );
    let node = staging.get(&fits).unwrap();
    assert!(check_envelope(&node.bytes, node.id, &limits()).is_ok());
    let over = raw(1, &[], &text_body(&"x".repeat(65_524)));
    assert_eq!(over.len(), MAX_NODE_BYTES + 1);
    assert_eq!(
        check_envelope(&over.bytes, over.id, &limits()),
        Err(GraphRefusal::EncodedNodeTooLarge {
            bytes: MAX_NODE_BYTES + 1,
            limit: MAX_NODE_BYTES
        })
    );
    // A leaf that large is not canonical: a placed value chunks at 65,520.
    assert_eq!(
        GraphDecoder::new(&staging, limits()).text(fits),
        Err(GraphRefusal::NonCanonical {
            node: Some(fits),
            reason: CanonicalFault::InvalidChunking
        })
    );
}

/// A root kind inlines while its whole envelope is at most 4,096 bytes and
/// streams one byte later; a body of many small values streams through
/// complete blocks and a two-level stream index, and comes back exactly.
#[test]
fn roots_are_bounded_at_4_kib_and_spill_into_streams() {
    let units = |count: usize| {
        let mut body = TokenWriter::new();
        body.seq(count as u64);
        for _ in 0..count {
            body.unit();
        }
        body.into_bytes()
    };
    let inline_len =
        |count: usize| envelope_len(NodeKind::KERNEL_WHOLE, 0, units(count).len() + 3) as u64;
    let edge = (1..10_000)
        .find(|count| inline_len(*count + 1) > MAX_ROOT_BYTES)
        .unwrap();
    assert_eq!(inline_len(edge), MAX_ROOT_BYTES);
    for (count, streams) in [(edge, false), (edge + 1, true)] {
        let mut staging = staging();
        let body = units(count);
        let id = staging
            .encode(NodeKind::KERNEL_WHOLE, TypedBody::logical(&body))
            .unwrap();
        let root = staging.get(&id).unwrap();
        assert!(root.len() <= MAX_ROOT_BYTES);
        let envelope = root.envelope().unwrap();
        assert_eq!(envelope.body[1], u8::from(streams), "variant at {count}");
        assert_eq!(logical(&staging, id, NodeKind::KERNEL_WHOLE).unwrap(), body);
    }

    let mut staging = staging();
    let mut body = TokenWriter::new();
    let count = 40 * 65_536 / 2;
    body.seq(count);
    for index in 0..count {
        body.unsigned(index % 100);
    }
    let body = body.into_bytes();
    let id = staging
        .encode(NodeKind::SESSION_EXECUTION, TypedBody::logical(&body))
        .unwrap();
    let kinds = |kind: NodeKind| {
        staging
            .nodes()
            .values()
            .filter(|node| node.envelope().unwrap().kind == kind)
            .count()
    };
    assert!(kinds(NodeKind::STRUCTURE_BLOCK) > INDEX_FANOUT);
    assert_eq!(
        kinds(NodeKind::STREAM_INDEX),
        3,
        "two level-1 indexes and one top"
    );
    assert_eq!(kinds(NodeKind::WHOLE_STREAM), 1);
    assert_eq!(
        logical(&staging, id, NodeKind::SESSION_EXECUTION).unwrap(),
        body
    );
    for node in staging.nodes().values() {
        assert!(node.len() <= MAX_NODE_BYTES);
        let spec = node.envelope().unwrap().kind.spec().unwrap();
        assert!(node.len() <= spec.bound.bytes());
    }
    assert_edges_are_the_table(&staging);
}

/// The generic check refuses each malformed envelope with its own cause.
#[test]
fn every_envelope_refusal_names_its_cause() {
    for (name, envelope, refusal) in envelope_refusals() {
        assert_eq!(
            check_envelope(&envelope.bytes, envelope.id, &limits()),
            Err(refusal),
            "{name}"
        );
    }
    let node = raw(16, &[], &[0x00]);
    assert_eq!(
        check_envelope(&node.bytes, NodeId([0; 32]), &limits()),
        Err(GraphRefusal::HashMismatch(NodeId([0; 32])))
    );
    assert_eq!(
        check_envelope(
            &node.bytes,
            node.id,
            &DecodeLimits {
                node_bytes: 4,
                ..limits()
            }
        ),
        Err(GraphRefusal::DecodeLimit {
            dimension: DecodeDimension::NodeBytes,
            actual: node.len(),
            limit: 4
        })
    );
    // A semantic kind this build does not register passes the generic
    // check, so a reader can follow its children, and is refused by an
    // execution decoder.
    let future = raw(1_999, &[], &[0x00]);
    assert!(check_envelope(&future.bytes, future.id, &limits()).is_ok());
    assert_eq!(
        logical(&source([future.clone()]), future.id, NodeKind(1_999)),
        Err(GraphRefusal::UnsupportedKind(1_999))
    );
}

fn envelope_refusals() -> Vec<(&'static str, EncodedNode, GraphRefusal)> {
    let child_a = NodeId([1; 32]);
    let child_b = NodeId([2; 32]);
    let raw_bytes = |bytes: Vec<u8>| EncodedNode {
        id: NodeId::of_envelope(&bytes),
        bytes: bytes.into(),
    };
    let with_node = |node: &EncodedNode, reason| GraphRefusal::NonCanonical {
        node: Some(node.id),
        reason,
    };
    let mut cases = vec![
        (
            "bad magic",
            raw_bytes(b"LASX\x01\x00\x01\x00".to_vec()),
            GraphRefusal::BadMagic,
        ),
        (
            "truncated magic",
            raw_bytes(b"LAS".to_vec()),
            GraphRefusal::Truncated,
        ),
        (
            "kind zero",
            raw_bytes(b"LASH\x00\x00\x01\x00".to_vec()),
            GraphRefusal::UnsupportedKind(0),
        ),
        (
            "kind over u32",
            raw_bytes(b"LASH\x80\x80\x80\x80\x10\x00\x01\x00".to_vec()),
            GraphRefusal::LengthOverflow,
        ),
        (
            "truncated child table",
            raw_bytes(b"LASH\x01\x01\x00".to_vec()),
            GraphRefusal::Truncated,
        ),
        (
            "truncated body",
            raw_bytes(b"LASH\x01\x00\x02\x00".to_vec()),
            GraphRefusal::Truncated,
        ),
        (
            "trailing bytes",
            raw_bytes(b"LASH\x01\x00\x01\x00\x00".to_vec()),
            GraphRefusal::TrailingBytes,
        ),
    ];
    let overlong = raw_bytes(b"LASH\x81\x00\x00\x01\x00".to_vec());
    cases.push((
        "overlong kind",
        overlong.clone(),
        with_node(&overlong, CanonicalFault::NonMinimalVarint),
    ));
    let unsorted = raw(
        8,
        &[child_b, child_a],
        &[0x09, 0x02, 0x0b, 0x00, 0x0b, 0x01],
    );
    cases.push((
        "unsorted children",
        unsorted.clone(),
        with_node(&unsorted, CanonicalFault::UnsortedOrDuplicateChild),
    ));
    let duplicate = raw(
        8,
        &[child_a, child_a],
        &[0x09, 0x02, 0x0b, 0x00, 0x0b, 0x01],
    );
    cases.push((
        "duplicate children",
        duplicate.clone(),
        with_node(&duplicate, CanonicalFault::UnsortedOrDuplicateChild),
    ));
    let out_of_range = raw(8, &[child_a], &[0x09, 0x02, 0x0b, 0x00, 0x0b, 0x01]);
    cases.push((
        "slot out of range",
        out_of_range.clone(),
        with_node(&out_of_range, CanonicalFault::ChildSlotOutOfRange(1)),
    ));
    let unused = raw(8, &[child_a, child_b], &[0x0b, 0x01]);
    cases.push((
        "unused child",
        unused.clone(),
        with_node(&unused, CanonicalFault::UnusedChildSlot(0)),
    ));
    let non_minimal = raw(8, &[], &[0x03, 0x80, 0x00]);
    cases.push((
        "non-minimal body varint",
        non_minimal.clone(),
        with_node(&non_minimal, CanonicalFault::NonMinimalVarint),
    ));
    let nan = raw(8, &[], &[0x06, 1, 0, 0, 0, 0, 0, 0xf8, 0x7f]);
    cases.push((
        "non-canonical NaN",
        nan.clone(),
        with_node(&nan, CanonicalFault::InvalidNaN),
    ));
    let tag = raw(8, &[], &[0x0f]);
    cases.push((
        "unknown tag",
        tag.clone(),
        with_node(&tag, CanonicalFault::UnknownTag(0x0f)),
    ));
    cases.push((
        "invalid UTF-8",
        raw(8, &[], &[0x07, 0x01, 0xff]),
        GraphRefusal::InvalidUtf8,
    ));
    cases.push((
        "truncated token",
        raw(8, &[], &[0x07, 0x05, 0x61]),
        GraphRefusal::Truncated,
    ));
    cases
}

/// Every non-canonical body form, placement, chunking, streaming, kind and
/// limit is refused by the decoder with its own cause, never repaired.
#[test]
fn every_noncanonical_body_form_is_refused() {
    let inline_text = |text: &str| {
        let mut body = vec![0x0c, 0x00, 0x01];
        body.extend(text_body(text));
        body
    };
    // An inline value over the bound.
    let over = raw(8, &[], &inline_text(&"x".repeat(300)));
    assert_eq!(
        logical(&source([over.clone()]), over.id, NodeKind::FROZEN_VALUE),
        Err(GraphRefusal::NonCanonical {
            node: Some(over.id),
            reason: CanonicalFault::InvalidPlacement
        })
    );
    // A placed value under it.
    let small = raw(1, &[], &text_body("short"));
    let placed_small = raw(8, &[small.id], &[0x0c, 0x00, 0x01, 0x0b, 0x00]);
    assert_eq!(
        logical(
            &source([small, placed_small.clone()]),
            placed_small.id,
            NodeKind::FROZEN_VALUE
        ),
        Err(GraphRefusal::NonCanonical {
            node: Some(placed_small.id),
            reason: CanonicalFault::InvalidPlacement
        })
    );
    // A value chunked at another boundary.
    let left = raw(1, &[], &text_body(&"x".repeat(65_000)));
    let right = raw(1, &[], &text_body(&"x".repeat(521)));
    let index_body = |total: u64, lengths: [u64; 2]| {
        let mut ids = [left.id, right.id];
        ids.sort();
        let slot = |id: NodeId| ids.iter().position(|other| *other == id).unwrap() as u64;
        let mut body = TokenWriter::new();
        body.seq(2);
        body.unsigned(total);
        body.seq(2);
        for (id, len) in [(left.id, lengths[0]), (right.id, lengths[1])] {
            body.seq(2);
            body.slot(slot(id));
            body.unsigned(len);
        }
        body.into_bytes()
    };
    let index = raw(
        3,
        &[left.id.min(right.id), left.id.max(right.id)],
        &index_body(65_521, [65_000, 521]),
    );
    let mut decoder_source = source([left.clone(), right.clone(), index.clone()]);
    assert_eq!(
        GraphDecoder::new(&decoder_source, limits()).text(index.id),
        Err(GraphRefusal::NonCanonical {
            node: Some(index.id),
            reason: CanonicalFault::InvalidChunking
        })
    );
    // An index whose declared lengths disagree with its children.
    let lying = raw(
        3,
        &[left.id.min(right.id), left.id.max(right.id)],
        &index_body(65_522, [65_000, 522]),
    );
    decoder_source.insert(lying.id, lying.clone());
    assert_eq!(
        GraphDecoder::new(&decoder_source, limits()).text(lying.id),
        Err(GraphRefusal::NonCanonical {
            node: Some(lying.id),
            reason: CanonicalFault::AggregateLengthMismatch
        })
    );

    // A body streamed where it fits inline.
    let mut staging = staging();
    let body = encode_body(&7_u64);
    let stream = staging.stream(NodeKind::FROZEN_VALUE, &body).unwrap();
    let streamed = raw(8, &[stream], &[0x0c, 0x01, 0x01, 0x0b, 0x00]);
    staging.adopt(streamed.clone()).unwrap();
    assert_eq!(
        logical(&staging, streamed.id, NodeKind::FROZEN_VALUE),
        Err(GraphRefusal::NonCanonical {
            node: Some(streamed.id),
            reason: CanonicalFault::InvalidBodyForm
        })
    );
    // A body variant that does not exist, and a body of two tokens.
    let variant = raw(8, &[], &[0x0c, 0x02, 0x01, 0x00]);
    assert_eq!(
        logical(
            &source([variant.clone()]),
            variant.id,
            NodeKind::FROZEN_VALUE
        ),
        Err(GraphRefusal::UnsupportedVariant {
            kind: 8,
            variant: 2
        })
    );
    let two = raw(8, &[], &[0x0c, 0x00, 0x01, 0x00, 0x00]);
    assert_eq!(
        logical(&source([two.clone()]), two.id, NodeKind::FROZEN_VALUE),
        Err(GraphRefusal::NonCanonical {
            node: Some(two.id),
            reason: CanonicalFault::NotOneToken
        })
    );

    // Kinds: read as another kind, a child of the wrong kind, a missing
    // child, an opaque blob read as a node.
    let mut staging = self::staging();
    let (id, root) = exec_root(&mut staging, "exe");
    assert_eq!(
        logical(&staging, id, NodeKind::OWNER_BINDING),
        Err(GraphRefusal::KindMismatch {
            node: id,
            expected: NodeKind::OWNER_BINDING.0,
            found: NodeKind::EXEC_ROOT.0
        })
    );
    let wrong = ExecRoot {
        owner: root.state,
        ..root.clone()
    };
    let wrong_id = staging.encode_value(NodeKind::EXEC_ROOT, &wrong).unwrap();
    let mut decoder = GraphDecoder::new(&staging, limits());
    assert_eq!(
        wrong.check_children(wrong_id, &mut decoder),
        Err(GraphRefusal::WrongChildKind {
            parent: wrong_id,
            child: root.state
        })
    );
    let mut partial = staging.nodes().clone();
    partial.remove(&root.state);
    assert_eq!(
        root.check_children(id, &mut GraphDecoder::new(&partial, limits())),
        Err(GraphRefusal::MissingNode(root.state))
    );
    struct Opaque(EncodedNode);
    impl NodeSource for Opaque {
        fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
            (*id == self.0.id).then_some(StoredNode {
                encoding: BlobEncoding::Opaque,
                bytes: &self.0.bytes,
            })
        }
    }
    let blob = raw(8, &[], &[0x0c, 0x00, 0x01, 0x00]);
    assert_eq!(
        GraphDecoder::new(&Opaque(blob.clone()), limits()).envelope(blob.id),
        Err(GraphRefusal::EncodingConflict(blob.id))
    );

    // Typed decoders refuse wrong field counts and variants.
    let mut tokens = TokenWriter::new();
    tokens.sum(1, 2);
    tokens.text("p");
    tokens.text("extra");
    let tokens = tokens.into_bytes();
    let binding = staging
        .encode(NodeKind::OWNER_BINDING, TypedBody::logical(&tokens))
        .unwrap();
    assert_eq!(
        GraphDecoder::new(&staging, limits())
            .decode::<OwnerBinding>(binding, NodeKind::OWNER_BINDING),
        Err(GraphRefusal::NonCanonical {
            node: Some(binding),
            reason: CanonicalFault::FieldCount {
                expected: 1,
                found: 2
            }
        })
    );
    let tokens = encode_body(&ExecRootKind::Cell);
    let mut variant = tokens.clone();
    variant[1] = 2;
    let unknown = staging
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&variant))
        .unwrap();
    assert_eq!(
        GraphDecoder::new(&staging, limits())
            .decode::<ExecRootKind>(unknown, NodeKind::FROZEN_VALUE),
        Err(GraphRefusal::UnsupportedVariant {
            kind: 8,
            variant: 2
        })
    );
    assert_eq!(
        check_ascending(&["b", "a"]).map_err(|error| GraphRefusal::from_token(error, None, None)),
        Err(GraphRefusal::NonCanonical {
            node: None,
            reason: CanonicalFault::UnsortedOrDuplicateKey
        })
    );

    // Decode limits bound every dimension before the work is done.
    assert_eq!(
        GraphDecoder::new(
            &staging,
            DecodeLimits {
                nodes: 2,
                ..limits()
            }
        )
        .logical_body(root.state, NodeKind::PROCESS_STATE),
        Err(GraphRefusal::DecodeLimit {
            dimension: DecodeDimension::Nodes,
            actual: 3,
            limit: 2
        })
    );
    assert!(matches!(
        GraphDecoder::new(
            &staging,
            DecodeLimits {
                logical_bytes: 1_000,
                ..limits()
            }
        )
        .logical_body(root.state, NodeKind::PROCESS_STATE),
        Err(GraphRefusal::DecodeLimit {
            dimension: DecodeDimension::LogicalBytes,
            ..
        })
    ));
}

/// The encoder charges every new staged byte to its lease and stages
/// nothing past it; a node another encoder made is adopted only after its
/// digest and envelope check.
#[test]
fn encoding_stays_within_its_memory_lease() {
    let body = text_body(&"x".repeat(1_000));
    let mut probe = staging();
    probe
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&body))
        .unwrap();
    let needed = probe.staged_bytes();
    let mut exact = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(needed)));
    assert!(
        exact
            .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&body))
            .is_ok()
    );
    let mut short = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(needed - 1)));
    assert_eq!(
        short.encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&body)),
        Err(GraphRefusal::EncodeMemoryLimit)
    );
    assert!(short.budget().spent() < needed);
    let node = probe.nodes().values().next().unwrap().clone();
    let forged = EncodedNode {
        id: NodeId([0; 32]),
        bytes: node.bytes.clone(),
    };
    assert_eq!(
        staging().adopt(forged),
        Err(GraphRefusal::HashMismatch(NodeId([0; 32])))
    );
    let mut adopted = staging();
    adopted.adopt(node.clone()).unwrap();
    assert_eq!(adopted.get(&node.id), Some(&node));
}

/// The full commit plus its failure reserve must fit the explicit budget:
/// equality is admitted and one more byte or row is refused.
#[test]
fn a_commit_is_admitted_up_to_its_explicit_budget() {
    let budget = GraphCommitBudget {
        encoded_bytes: ExplicitLimit::new(1_000),
        mutation_rows: ExplicitLimit::new(10),
        encode_memory: ExplicitLimit::new(1_000),
        failure_reserve_bytes: 100,
    };
    let measured = |bytes, rows| EncodedCommitMeasurement {
        full_encoded_commit_bytes: bytes,
        planned_mutation_rows: rows,
        ..EncodedCommitMeasurement::default()
    };
    assert_eq!(budget.admit(&measured(900, 10)), Ok(()));
    assert_eq!(
        budget.admit(&measured(901, 10)),
        Err(GraphRefusal::CommitTooLarge {
            bytes: 1_001,
            limit: 1_000
        })
    );
    assert_eq!(
        budget.admit(&measured(900, 11)),
        Err(GraphRefusal::TooManyMutations {
            rows: 11,
            limit: 10
        })
    );
    assert_eq!(budget.admit_reserved_failure(&measured(1_000, 10)), Ok(()));
    assert!(budget.admit_reserved_failure(&measured(1_001, 10)).is_err());
}

/// The shared fixture: what the vectors pin, recomputed.
fn grammar_vectors() -> serde_json::Value {
    use serde_json::json;
    let registry: Vec<_> = REGISTRY
        .iter()
        .chain(SEMANTIC_REGISTRY)
        .map(|spec| {
            json!({
                "kind": spec.kind.0,
                "name": spec.name,
                "shape": format!("{:?}", spec.shape),
                "bound": spec.bound.bytes(),
                "owner": format!("{:?}", spec.owner),
            })
        })
        .collect();
    let roots: Vec<_> = RootSource::ALL
        .iter()
        .map(|source| {
            let sqlite = source.sqlite_field();
            let postgres = source.postgres_field();
            json!({
                "source": source.catalog_name(),
                "number": *source as u8,
                "sqlite": format!("{}.{}", sqlite.table, sqlite.column),
                "postgres": format!("{}.{}{}", postgres.table, postgres.column,
                    postgres.filter.map(|filter| format!(" where {filter}")).unwrap_or_default()),
            })
        })
        .collect();

    let mut staging = staging();
    let (exec, _) = exec_root(&mut staging, "exe");
    let mut values = Vec::new();
    for (name, kind, body) in [
        ("unit value", NodeKind::FROZEN_VALUE, vec![0x00]),
        (
            "text at the inline bound",
            NodeKind::FROZEN_VALUE,
            text_body(&"t".repeat(253)),
        ),
        (
            "text over the inline bound",
            NodeKind::FROZEN_VALUE,
            text_body(&"t".repeat(254)),
        ),
        ("bytes over the inline bound", NodeKind::FROZEN_VALUE, {
            let mut out = TokenWriter::new();
            out.bytes(&[0xab; 254]);
            out.into_bytes()
        }),
        (
            "owner binding",
            NodeKind::OWNER_BINDING,
            encode_body(&OwnerBinding::Cell {
                session: "s".into(),
                turn: "t".into(),
                cell: "c".into(),
            }),
        ),
    ] {
        let mut one = self::staging();
        let id = one.encode(kind, TypedBody::logical(&body)).unwrap();
        let nodes: Vec<_> = one
            .nodes()
            .values()
            .map(|node| json!({"id": node.id.to_hex(), "envelope": hex(&node.bytes)}))
            .collect();
        values.push(json!({
            "name": name,
            "kind": kind.0,
            "logical": hex(&body),
            "root": id.to_hex(),
            "nodes": nodes,
        }));
    }
    let large: Vec<_> = [
        ("text of 65,520 x", "x".repeat(LEAF_PAYLOAD_MAX)),
        ("text of 65,521 x", "x".repeat(LEAF_PAYLOAD_MAX + 1)),
        (
            "65,519 x then é",
            format!("{}é", "x".repeat(LEAF_PAYLOAD_MAX - 1)),
        ),
        ("33 chunks of x", "x".repeat(33 * LEAF_PAYLOAD_MAX)),
    ]
    .into_iter()
    .map(|(name, text)| {
        let mut one = self::staging();
        let id = one
            .encode(
                NodeKind::FROZEN_VALUE,
                TypedBody::logical(&text_body(&text)),
            )
            .unwrap();
        json!({
            "name": name,
            "kind": NodeKind::FROZEN_VALUE.0,
            "root": id.to_hex(),
            "node_count": one.nodes().len(),
            "staged_bytes": one.staged_bytes(),
        })
    })
    .collect();
    let refusals: Vec<_> = envelope_refusals()
        .into_iter()
        .map(|(name, node, refusal)| {
            json!({
                "name": name,
                "envelope": hex(&node.bytes),
                "expected": node.id.to_hex(),
                "refusal": format!("{refusal:?}"),
            })
        })
        .collect();
    json!({
        "about": "FIG-5887 §2 grammar vectors; regenerate with checkpoint_graph::tests::regenerate_grammar_vectors",
        "constants": {
            "magic": hex(&MAGIC),
            "max_node_bytes": MAX_NODE_BYTES,
            "max_root_bytes": MAX_ROOT_BYTES,
            "inline_token_max": INLINE_TOKEN_MAX,
            "leaf_payload_max": LEAF_PAYLOAD_MAX,
            "index_fanout": INDEX_FANOUT,
            "semantic_kinds": [*NodeKind::SEMANTIC.start(), *NodeKind::SEMANTIC.end()],
        },
        "registry": registry,
        "root_sources": roots,
        "exec_root": {"root": exec.to_hex(), "node_count": staging.nodes().len()},
        "nodes": values,
        "large": large,
        "envelope_refusals": refusals,
    })
}

fn vectors_source(json: &str) -> String {
    format!(
        "//! The shared grammar-vector fixture (FIG-5887 §2): the registry, root\n\
         //! sources, pinned node envelopes and identities, large-value recipes and\n\
         //! envelope refusals every lane's codec is checked against. Generated by\n\
         //! `checkpoint_graph::tests::regenerate_grammar_vectors`; pinned by\n\
         //! `checkpoint_graph::tests::the_grammar_vectors_are_pinned`. Do not edit.\n\
         \n\
         /// The fixture's JSON text.\n\
         pub const GRAMMAR_VECTORS: &str = r##\"{json}\"##;\n"
    )
}

/// The committed fixture is exactly what this build encodes: every lane's
/// codec reads the same bytes, identities and refusals.
#[test]
fn the_grammar_vectors_are_pinned() {
    let expected = serde_json::to_string_pretty(&grammar_vectors()).unwrap();
    assert_eq!(
        vectors::GRAMMAR_VECTORS,
        expected,
        "regenerate with checkpoint_graph::tests::regenerate_grammar_vectors"
    );
    let pinned: serde_json::Value = serde_json::from_str(vectors::GRAMMAR_VECTORS).unwrap();
    for case in pinned["envelope_refusals"].as_array().unwrap() {
        let bytes = unhex(case["envelope"].as_str().unwrap());
        let expected = NodeId::from_hex(case["expected"].as_str().unwrap()).unwrap();
        let refusal = check_envelope(&bytes, expected, &limits()).unwrap_err();
        assert_eq!(format!("{refusal:?}"), case["refusal"].as_str().unwrap());
    }
    for case in pinned["nodes"].as_array().unwrap() {
        let nodes: BTreeMap<NodeId, EncodedNode> = case["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| {
                let id = NodeId::from_hex(node["id"].as_str().unwrap()).unwrap();
                let bytes = unhex(node["envelope"].as_str().unwrap());
                (
                    id,
                    EncodedNode {
                        id,
                        bytes: bytes.into(),
                    },
                )
            })
            .collect();
        let root = NodeId::from_hex(case["root"].as_str().unwrap()).unwrap();
        let kind = NodeKind(u32::try_from(case["kind"].as_u64().unwrap()).unwrap());
        assert_eq!(
            hex(&logical(&nodes, root, kind).unwrap()),
            case["logical"].as_str().unwrap()
        );
    }
}

#[test]
#[ignore = "regenerates crates/lash-core-store/src/checkpoint_graph/vectors.rs"]
#[expect(
    clippy::disallowed_methods,
    reason = "the opt-in test generator writes the fixture in the supplied workspace"
)]
fn regenerate_grammar_vectors() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let root = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("regeneration workspace");
    let json = serde_json::to_string_pretty(&grammar_vectors()).unwrap();
    std::fs::write(
        std::path::PathBuf::from(root)
            .join("crates/lash-core-store/src/checkpoint_graph/vectors.rs"),
        vectors_source(&json),
    )
    .expect("write grammar vectors");
}

#[test]
fn node_ids_spell_their_blob_ref() {
    let node = raw(8, &[], &[0x0c, 0x00, 0x01, 0x00]);
    assert_eq!(
        node.id.blob_ref(),
        crate::store::BlobRef::for_content(&node.bytes)
    );
    assert_eq!(NodeId::from_hex(&node.id.to_hex()), Some(node.id));
    assert_eq!(NodeId::from_hex(&node.id.to_hex().to_uppercase()), None);
}

/// CHECKPOINT-READ-POLICY: graph readers carry the deployment's recursive token limit.
#[test]
fn checkpoint_read_policy_honors_the_configured_kernel_decode_depth() {
    let value = lash_kernel_doc::FrozenDatum(lash_kernel_doc::Datum::Tuple(vec![
        lash_kernel_doc::Datum::Tuple(vec![lash_kernel_doc::Datum::Null]),
    ]));
    let mut staged = staging();
    let body = encode_body(&value);
    let id = staged
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&body))
        .unwrap();
    let policy = lash_kernel_doc::KernelDataLimits::new(64, 352, 1, 512, 128).unwrap();
    let mut decoder = GraphDecoder::new(
        &staged,
        DecodeLimits {
            kernel_data: policy,
            ..limits()
        },
    );
    assert!(matches!(
        decoder.decode::<lash_kernel_doc::FrozenDatum>(id, NodeKind::FROZEN_VALUE),
        Err(GraphRefusal::DecodeLimit {
            dimension: DecodeDimension::Depth,
            limit: 1,
            ..
        })
    ));
}

/// GRAPH-READ-ADMISSION: deployment read budgets are independent of what the writer admitted.
#[test]
fn a_deployment_graph_read_policy_refuses_its_configured_byte_budget() {
    let mut staged = staging();
    let value = lash_kernel_doc::FrozenDatum(lash_kernel_doc::Datum::Text("payload".into()));
    let body = encode_body(&value);
    let id = staged
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&body))
        .unwrap();
    let policy =
        GraphReadPolicy::new(1, 2, 3, 64, lash_kernel_doc::KernelDataLimits::standard()).unwrap();
    let mut decoder = GraphDecoder::new(&staged, policy.owner());
    assert!(matches!(
        decoder.decode::<lash_kernel_doc::FrozenDatum>(id, NodeKind::FROZEN_VALUE),
        Err(GraphRefusal::DecodeLimit { limit: 1, .. })
    ));
}

/// Repeated captures materialize only changed nodes: immutable envelopes
/// keep their allocation, including an unchanged placed subtree of a new root.
#[test]
fn repeated_captures_share_unchanged_graph_materialization() {
    use std::sync::Arc;

    let body = |suffix: &str| {
        let mut out = TokenWriter::new();
        out.seq(2);
        out.text(&"FIG-6157 unchanged subtree ".repeat(100));
        out.text(&suffix.repeat(100));
        out.into_bytes()
    };
    let first_body = body("first capture ");
    let mut first = GraphStaging::isolated(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let root = first
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&first_body))
        .unwrap();
    let mut repeated = first.sharing_cache(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    assert_eq!(
        repeated
            .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&first_body))
            .unwrap(),
        root
    );
    for (id, node) in first.nodes() {
        assert!(
            Arc::ptr_eq(&node.bytes, &repeated.get(id).unwrap().bytes),
            "unchanged node {id} was materialized again"
        );
    }
    let mut changed = first.sharing_cache(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let changed_body = body("second capture ");
    let changed_root = changed
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&changed_body))
        .unwrap();
    assert_ne!(changed_root, root);
    let mut newly_materialized = 0;
    for (id, node) in changed.nodes() {
        match first.get(id) {
            Some(previous) => assert!(Arc::ptr_eq(&previous.bytes, &node.bytes)),
            None => newly_materialized += 1,
        }
    }
    assert_eq!(newly_materialized, 2, "only the changed leaf and its root");
    assert_eq!(
        logical(&changed, changed_root, NodeKind::FROZEN_VALUE).unwrap(),
        changed_body
    );
    // Sharing cannot change the committed baseline's bytes or identities.
    assert_eq!(
        serde_json::to_string_pretty(&grammar_vectors()).unwrap(),
        vectors::GRAMMAR_VECTORS
    );
    assert_eq!(changed.budget().spent(), changed.staged_bytes());
    // Changing a large value's tail keeps its earlier chunks too.
    let prefix = format!(
        "{}{}",
        "a".repeat(LEAF_PAYLOAD_MAX),
        "b".repeat(LEAF_PAYLOAD_MAX)
    );
    let mut before = first.sharing_cache(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut after = first.sharing_cache(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    before
        .encode(
            NodeKind::FROZEN_VALUE,
            TypedBody::logical(&text_body(&format!("{prefix}first"))),
        )
        .unwrap();
    after
        .encode(
            NodeKind::FROZEN_VALUE,
            TypedBody::logical(&text_body(&format!("{prefix}second"))),
        )
        .unwrap();
    let mut changed_nodes = 0;
    let mut shared_nodes = 0;
    for (id, node) in after.nodes() {
        match before.get(id) {
            Some(previous) => {
                assert!(Arc::ptr_eq(&previous.bytes, &node.bytes));
                shared_nodes += 1;
            }
            None => changed_nodes += 1,
        }
    }
    assert_eq!(
        (shared_nodes, changed_nodes),
        (2, 3),
        "two unchanged chunks, one changed chunk and its two ancestors"
    );
    // Cached replay preserves the original insertion order and partial writes
    // when the host's lease refuses at any node boundary.
    let mut boundaries = vec![0, first.staged_bytes()];
    let mut spent = 0;
    for node in first.nodes().values() {
        spent += node.len();
        boundaries.extend([spent - 1, spent]);
    }
    for limit in boundaries {
        let budget = EncodeBudget::new(ExplicitLimit::new(limit));
        let mut cold = GraphStaging::isolated(budget);
        let mut warm = first.sharing_cache(budget);
        assert_eq!(
            cold.encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&first_body)),
            warm.encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&first_body))
        );
        assert_eq!(cold.nodes(), warm.nodes());
        assert_eq!(cold.budget().spent(), warm.budget().spent());
    }
    let mut refused = first.sharing_cache(EncodeBudget::new(ExplicitLimit::new(1)));
    assert_eq!(
        refused.encode(NodeKind::FROZEN_VALUE, TypedBody::logical(&first_body)),
        Err(GraphRefusal::EncodeMemoryLimit),
        "shared encoding still charges each new staging area"
    );
}

/// Decoding a checked, unplaced body reads its authoritative bytes, rather
/// than materializing another complete copy before the typed decoder runs.
#[test]
fn checked_unplaced_bodies_are_borrowed_by_typed_decoders() {
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError};

    struct ReadText {
        address: usize,
        text: String,
    }
    impl TokenCodec for ReadText {
        fn encode(&self, out: &mut TokenWriter) {
            out.text(&self.text);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            let text = input.text()?;
            Ok(Self {
                address: text.as_ptr() as usize,
                text: text.to_owned(),
            })
        }
    }
    let body = text_body("FIG-6157 checked borrowed text");
    for kind in [NodeKind::TEXT_LEAF, NodeKind::FROZEN_VALUE] {
        let mut nodes = staging();
        let root = nodes.encode(kind, TypedBody::logical(&body)).unwrap();
        let envelope = nodes.get(&root).unwrap().envelope().unwrap();
        let physical = if kind == NodeKind::TEXT_LEAF {
            envelope.body
        } else {
            &envelope.body[3..]
        };
        let address = TokenReader::new(physical).text().unwrap().as_ptr() as usize;
        let mut decoder = GraphDecoder::new(&nodes, limits());
        let read: ReadText = decoder.decode(root, kind).unwrap();
        assert_eq!(read.text, "FIG-6157 checked borrowed text");
        assert_eq!(
            read.address, address,
            "the checked source body was copied before decoding"
        );
        assert_eq!(decoder.read(), (1, nodes.get(&root).unwrap().len()));
    }
    assert_eq!(
        serde_json::to_string_pretty(&grammar_vectors()).unwrap(),
        vectors::GRAMMAR_VECTORS
    );
}
