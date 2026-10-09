//! FIG-3802's laws over the guarded surfaces `lash_vm` owns, each driven
//! through its production writer and decoder. The canonical snapshot reads
//! N's version natively in the synthetic N+1: its fixed point re-encodes at
//! the recorded version, so N's bytes stay N's.

use lash_core_execution::FleetFormat;
use lash_core_store::testing::guarded_surfaces::{self as laws, SurfaceProbe};

use super::tests::named_bytes;
use super::*;
use crate::testing::ast_builders as b;
use crate::{WORKFLOW_GRAPH_SCHEMA_VERSION, WorkflowGraph, WorkflowGraphProjector};

const OWNER: &str = "lashvm";

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

// --- LASH_VM_SNAPSHOT_VERSION: the canonical execution snapshot. ---

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

// --- WORKFLOW_GRAPH_SCHEMA_VERSION: a projection regenerated from its
// module, read only at its newest. ---

fn write_graph(fleet: FleetFormat) -> Vec<u8> {
    let program = b::program(vec![b::assign("total", b::num(0.0))]);
    let graph = WorkflowGraphProjector::new(&program)
        .with_fleet_format(fleet)
        .project();
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
            constant: "LASH_VM_SNAPSHOT_VERSION",
            newest: LASH_VM_SNAPSHOT_VERSION,
            write: write_globals,
            read: read_snapshot,
            restamp: restamp_snapshot,
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
        .expect("a lash_vm probe")
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
