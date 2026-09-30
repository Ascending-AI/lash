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
        .to_canonical_bytes_stamped(SnapshotStamps::for_fleet(fleet))
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
        .to_canonical_bytes_stamped(SnapshotStamps::for_fleet(fleet))
        .expect("encode the heap snapshot")
}

fn heap_stamp(bytes: &[u8]) -> u32 {
    wire(bytes)
        .heap
        .expect("a heap snapshot")
        .size_schedule_version
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

/// The decoded graph under `fleet`'s window, rendered without its stamp:
/// two decodes of the same projection render alike whatever version either
/// was written at.
fn read_graph(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    let value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    WorkflowGraph::decode_json_value_for_fleet(value, fleet)
        .map(|graph| {
            format!(
                "{:?}",
                WorkflowGraph {
                    schema_version: 0,
                    ..graph
                }
            )
        })
        .map_err(|error| error.to_string())
}

fn graph_stamp(bytes: &[u8]) -> u32 {
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("a graph");
    value["schema_version"]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .expect("a stamped graph")
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

fn probe(constant: &str) -> SurfaceProbe {
    probes()
        .into_iter()
        .find(|probe| probe.constant == constant)
        .expect("a lashlang probe")
}

/// FIG-4262: the synthetic N+1 moves the heap size schedule; the snapshot
/// writer stamps what `F` assigns, and the heaps N wrote read on N+1.
#[test]
fn heap_schedules_written_by_n_read_across_the_roll() {
    laws::n_written_records_read_across_the_roll(
        OWNER,
        &probe("HEAP_SIZE_SCHEDULE_VERSION"),
        heap_stamp,
    );
}

/// FIG-4262: the synthetic N+1 moves the workflow graph; the projector
/// stamps what `F` assigns, N's documents read on N+1 while `F` is N's
/// epoch, and after finalize the projection regenerates.
#[test]
fn workflow_graphs_written_by_n_read_across_the_roll() {
    laws::n_written_records_read_across_the_roll(
        OWNER,
        &probe("WORKFLOW_GRAPH_SCHEMA_VERSION"),
        graph_stamp,
    );
}

/// FIG-4262: the durable writer the RLM executor persists through stamps the
/// heap schedule `F` assigns, and its fixed-point read admits N's schedule
/// under every epoch this build writes.
#[test]
fn the_durable_writer_stamps_the_heap_schedule_f_assigns() {
    let snapshot = wire(&write_heap(FleetFormat::current()));
    let state = State::from_snapshot(Snapshot::try_from(snapshot).expect("a heap snapshot"));
    let n = FleetFormat::seed(FleetFormat::writable());
    let written = [
        n,
        FleetFormat::current(),
        laws::pinned("HEAP_SIZE_SCHEDULE_VERSION", HEAP_SIZE_SCHEDULE_VERSION + 1),
    ]
    .map(|fleet| {
        let parts = state
            .durable_parts(&DurableBaseline::default(), fleet)
            .expect("capture the durable parts");
        let header: durable::CanonicalDurableHeader =
            rmp_serde::from_slice(&parts.header).expect("a durable header");
        let stamp = header
            .heap
            .expect("a heap-backed header")
            .size_schedule_version;
        assert_eq!(
            stamp,
            fleet.writer_version(surface_format!(HEAP_SIZE_SCHEDULE_VERSION)),
            "the durable header stamps the schedule F={fleet} assigns"
        );
        parts
    });
    let fragments = |parts: &DurableParts| {
        parts
            .fragments
            .iter()
            .map(|(name, fragment)| match fragment {
                DurableFragment::Changed(bytes) => (name.clone(), bytes.clone()),
                DurableFragment::Unchanged => panic!("a first capture changes every root"),
            })
            .collect::<Vec<_>>()
    };
    let by_n = &written[0];
    for fleet in (FleetFormat::writable().min()..=FleetFormat::writable().max())
        .map(FleetFormat::from_version)
    {
        let roots = fragments(by_n);
        let (read, _) = State::from_durable_parts(
            &by_n.header,
            roots
                .iter()
                .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
            fleet,
        )
        .unwrap_or_else(|error| panic!("N's durable heap reads under F={fleet}: {error}"));
        assert_eq!(
            read, state,
            "N's durable heap reads as written under F={fleet}"
        );
    }
}
