#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::{
    CommitId, DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, LEAF_PAYLOAD_MAX, NodeId, parse_envelope,
};
use lash_durable::DomainWrite;
use lash_durable::domain::SnapshotRev;
use lash_durable::domain::graph::ExecRootWrite;

use super::*;

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// What an immediate collection leaves: only the nodes `root` reaches, out
/// of every node `installed` holds.
fn collected(
    installed: &BTreeMap<NodeId, EncodedNode>,
    root: NodeId,
) -> BTreeMap<NodeId, EncodedNode> {
    let mut kept = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if kept.contains_key(&id) {
            continue;
        }
        let node = installed
            .get(&id)
            .expect("a reachable node is installed")
            .clone();
        pending.extend(
            parse_envelope(&node.bytes, Some(id))
                .unwrap()
                .children
                .iter(),
        );
        kept.insert(id, node);
    }
    kept
}

fn state(version: u32, bytes: Vec<u8>) -> EngineState {
    EngineState {
        format: EngineStateFormat {
            kind: "wasm-engine".to_owned(),
            version,
        },
        bytes,
    }
}

/// A process snapshot carry (FIG-5887 §10): an engine state is stored as
/// its own bytes, not as hex text, in leaves when large; a carry to a newer
/// format publishes one more root in place of the old one and reuses every
/// leaf whose bytes it left unchanged; and after an immediate collection
/// under the new root the exact bytes, format and engine read back. A root
/// read as another process's is refused.
#[test]
fn a_carried_process_state_is_one_new_root_that_reads_back_its_exact_bytes() {
    let process = ProcessId::fixture("carried");
    let bytes: Vec<u8> = (0..3 * LEAF_PAYLOAD_MAX + 17)
        .map(|index| (index % 251) as u8)
        .collect();
    let first = state(1, bytes.clone());
    let mut staging = staging();
    let (old, write) = stage_engine_state(
        &mut staging,
        &process,
        "engine",
        &first,
        (None, None),
        CommitId([1; 32]),
    )
    .unwrap();
    assert!(matches!(
        write,
        DomainWrite::ExecRoot(ExecRootWrite::Put { expected_root: None, root, .. })
            if root == old
    ));
    let stored: usize = staging.nodes().values().map(|node| node.bytes.len()).sum();
    assert!(
        stored < bytes.len() + bytes.len() / 8,
        "{stored} bytes stored for {} state bytes: not hex",
        bytes.len()
    );
    let mut installed = staging.into_nodes();

    // The carry rewrites only the tail.
    let mut carried_bytes = bytes;
    let at = carried_bytes.len() - 5;
    carried_bytes[at..].copy_from_slice(b"v2end");
    let carried = state(2, carried_bytes);
    let mut staging = self::staging();
    let (root, write) = stage_engine_state(
        &mut staging,
        &process,
        "engine",
        &carried,
        (Some(SnapshotRev(1)), Some(old)),
        CommitId([2; 32]),
    )
    .unwrap();
    assert!(matches!(
        write,
        DomainWrite::ExecRoot(ExecRootWrite::Put { expected_root: Some(expected), root: new, .. })
            if expected == old && new == root
    ));
    let reused = staging
        .nodes()
        .keys()
        .filter(|id| installed.contains_key(id))
        .count();
    assert!(reused >= 3, "the carry reuses the unchanged leaves");
    installed.append(&mut staging.into_nodes());

    let kept = collected(&installed, root);
    assert!(!kept.contains_key(&old));
    let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 24));
    let (engine, loaded) = load_engine_state(&mut decoder, root, &process).unwrap();
    assert_eq!(engine, "engine");
    assert_eq!(loaded, carried);

    let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 24));
    assert!(matches!(
        load_engine_state(&mut decoder, root, &ProcessId::fixture("other")),
        Err(GraphRefusal::OwnerMismatch)
    ));
}
