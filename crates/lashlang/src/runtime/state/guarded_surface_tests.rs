//! FIG-3802's laws over the guarded surfaces `lashlang` owns, each driven
//! through its production writer and decoder. The canonical snapshot reads
//! N's version natively in the synthetic N+1: its fixed point re-encodes at
//! the recorded version, so N's bytes stay N's.

use lash_core_execution::FleetFormat;
use lash_core_store::testing::guarded_surfaces::{self as laws, SurfaceProbe};

use super::tests::{canonical_heap_with, named_bytes};
use super::*;
use crate::runtime::HEAP_SIZE_SCHEDULE_VERSION;
use crate::testing::ast_builders as b;
use crate::{
    NoStatementText, WORKFLOW_GRAPH_SCHEMA_VERSION, WorkflowGraph, WorkflowGraphProjector,
};

const OWNER: &str = "lashlang";

fn wire(bytes: &[u8]) -> CanonicalSnapshot {
    rmp_serde::from_slice(bytes).expect("a canonical snapshot")
}

fn snapshot_writer(fleet: FleetFormat) -> u32 {
    fleet.writer_version(surface_format!(LASHLANG_SNAPSHOT_VERSION))
}

/// The decoded snapshot, rendered as its canonical encode at the newest
/// version: two decodes of the same state render alike, whatever version
/// either was stored at.
fn read_snapshot(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    Snapshot::from_canonical_bytes_for_fleet(bytes, fleet)
        .map_err(|error| error.to_string())?
        .to_canonical_bytes()
        .map(|canonical| format!("{canonical:?}"))
        .map_err(|error| error.to_string())
}

// --- LASHLANG_SNAPSHOT_VERSION: the canonical execution snapshot. ---

fn write_globals(fleet: FleetFormat) -> Vec<u8> {
    let mut globals = Record::new();
    globals.insert("total".to_string(), Value::Number(3.0));
    Snapshot::new(globals)
        .to_canonical_bytes_at_version(snapshot_writer(fleet))
        .expect("encode the snapshot")
}

fn restamp_snapshot(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut wire = wire(bytes);
    wire.version = version;
    named_bytes(&wire)
}

// --- HEAP_SIZE_SCHEDULE_VERSION: the schedule a heap's bytes were charged
// under, riding in the snapshot. ---

/// A heap holding one list under one root: a heap-backed state, so the
/// snapshot carries the heap and its schedule stamp.
fn write_heap(fleet: FleetFormat) -> Vec<u8> {
    let wire = canonical_heap_with(
        vec![CanonicalBinding {
            name: "root".to_string(),
            value: CanonicalValue::Ref {
                value: HeapId::from_counter(1),
            },
        }],
        vec![CanonicalHeapEntry {
            id: HeapId::from_counter(1),
            object: CanonicalHeapObject::List { items: Vec::new() },
        }],
        2,
        1,
        crate::runtime::heap::HeapObject::List(Vec::new()).logical_bytes(),
    );
    let snapshot = Snapshot::try_from(wire).expect("a heap-backed snapshot");
    snapshot
        .to_canonical_bytes_at_version(snapshot_writer(fleet))
        .expect("encode the heap snapshot")
}

fn restamp_heap(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut wire = wire(bytes);
    wire.heap
        .as_mut()
        .expect("a heap snapshot")
        .size_schedule_version = version;
    named_bytes(&wire)
}

// --- WORKFLOW_GRAPH_SCHEMA_VERSION: a projection regenerated from its
// module, read only at its newest. ---

fn write_graph(fleet: FleetFormat) -> Vec<u8> {
    let program = b::program(vec![b::assign("total", b::num(0.0))]);
    let graph = WorkflowGraphProjector::new(&program)
        .with_fleet_format(fleet)
        .project(&NoStatementText);
    serde_json::to_vec(&graph).expect("encode the graph")
}

fn read_graph(bytes: &[u8], _fleet: FleetFormat) -> Result<String, String> {
    WorkflowGraph::decode_json(std::str::from_utf8(bytes).map_err(|error| error.to_string())?)
        .map(|graph| format!("{graph:?}"))
        .map_err(|error| error.to_string())
}

fn restamp_graph(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("a graph");
    value["schema_version"] = serde_json::json!(version);
    serde_json::to_vec(&value).expect("encode")
}

fn probes() -> Vec<SurfaceProbe> {
    vec![
        SurfaceProbe {
            constant: "LASHLANG_SNAPSHOT_VERSION",
            newest: LASHLANG_SNAPSHOT_VERSION,
            write: write_globals,
            read: read_snapshot,
            restamp: restamp_snapshot,
        },
        SurfaceProbe {
            constant: "HEAP_SIZE_SCHEDULE_VERSION",
            newest: HEAP_SIZE_SCHEDULE_VERSION,
            write: write_heap,
            read: read_snapshot,
            restamp: restamp_heap,
        },
        SurfaceProbe {
            constant: "WORKFLOW_GRAPH_SCHEMA_VERSION",
            newest: WORKFLOW_GRAPH_SCHEMA_VERSION,
            write: write_graph,
            read: read_graph,
            restamp: restamp_graph,
        },
    ]
}

#[test]
fn every_guarded_surface_decodes_its_supported_range() {
    laws::every_guarded_surface_decodes_its_supported_range(OWNER, &probes());
}

#[test]
fn unknown_version_is_refused_with_zero_mutation() {
    laws::unknown_version_is_refused_with_zero_mutation(OWNER, &probes());
}

#[test]
fn upcast_preserves_immutable_bytes_and_hashes() {
    laws::upcast_preserves_immutable_bytes_and_hashes(OWNER, &probes());
}
