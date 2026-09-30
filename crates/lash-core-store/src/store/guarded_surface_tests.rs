//! FIG-3802's laws over the guarded surfaces `lash-core-store` owns, each
//! driven through its production writer and decoder. In N's build every
//! surface reads its one version; in the synthetic N+1's it reads N's
//! through the registered lift, before and after finalize.

use crate::testing::guarded_surfaces::{self as laws, SurfaceProbe};
use crate::{SessionId, SessionNodeRecord};

use super::{
    BlobRef, CHECKPOINT_COMPONENT_ENCODING_VERSION, CURRENT_SESSION_STATE_VERSION,
    CheckpointComponentDescriptor, FleetFormat, OBLIGATION_LEDGER_VOCABULARY_VERSION,
    ObligationKind, RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION, RuntimeCommitReceipt,
    SESSION_CHECKPOINT_SCHEMA_VERSION, SESSION_HEAD_META_SCHEMA_VERSION, SessionCheckpoint,
    SessionHeadMeta, SessionHeadPayload,
};
use crate::session_graph::SESSION_NODE_BODY_SCHEMA_VERSION;

const OWNER: &str = "lash-core-store";

fn json_restamp(bytes: &[u8], field: &str, version: u32) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("a JSON record");
    value[field] = serde_json::json!(version);
    serde_json::to_vec(&value).expect("encode")
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("a UTF-8 record")
}

// --- SESSION_NODE_BODY_SCHEMA_VERSION: the immutable node body. ---

fn node() -> SessionNodeRecord {
    let body = serde_json::json!({
        "schema_version": SESSION_NODE_BODY_SCHEMA_VERSION,
        "timestamp": "2026-09-30T00:00:00Z",
        "kind": "plugin",
        "plugin_type": "guarded-surface-law",
        "body": {"value": 7},
    })
    .to_string();
    SessionNodeRecord::decode_storage_body("node-1".to_owned(), Some("parent-1".to_owned()), &body)
        .expect("the fixture body decodes")
}

fn write_node(fleet: FleetFormat) -> Vec<u8> {
    node()
        .encode_storage_body(fleet)
        .expect("encode the body")
        .into_bytes()
}

fn read_node(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    SessionNodeRecord::decode_storage_body_for_fleet(
        "node-1".to_owned(),
        Some("parent-1".to_owned()),
        text(bytes),
        fleet,
    )
    .map(|record| format!("{record:?}"))
    .map_err(|error| error.to_string())
}

fn restamp_schema_version(bytes: &[u8], version: u32) -> Vec<u8> {
    json_restamp(bytes, "schema_version", version)
}

// --- SESSION_CHECKPOINT_SCHEMA_VERSION: the checkpoint manifest. ---

fn write_checkpoint(fleet: FleetFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmp_serde::encode::write_named(&mut bytes, &SessionCheckpoint::for_fleet(fleet))
        .expect("encode the manifest");
    bytes
}

fn read_checkpoint(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    super::decode_versioned_msgpack_record_for_fleet::<SessionCheckpoint>(
        bytes,
        "SessionCheckpoint",
        crate::surface_format!(SESSION_CHECKPOINT_SCHEMA_VERSION),
        fleet,
    )
    .map(|manifest| format!("{manifest:?}"))
    .map_err(|error| error.to_string())
}

fn restamp_checkpoint(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut value: serde_json::Value = rmp_serde::from_slice(bytes).expect("a manifest");
    value["schema_version"] = serde_json::json!(version);
    let mut restamped = Vec::new();
    rmp_serde::encode::write_named(&mut restamped, &value).expect("encode");
    restamped
}

// --- CHECKPOINT_COMPONENT_ENCODING_VERSION: a component's byte encoding. ---

fn write_component(fleet: FleetFormat) -> Vec<u8> {
    serde_json::to_vec(&CheckpointComponentDescriptor {
        blob_ref: BlobRef::for_content(b"component"),
        encoding_version: fleet.writer_version(crate::surface_format!(
            CHECKPOINT_COMPONENT_ENCODING_VERSION
        )),
    })
    .expect("encode the descriptor")
}

fn read_component(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    let descriptor: CheckpointComponentDescriptor =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    super::checkpoint::ensure_checkpoint_component_encoding_version_for_fleet(
        "tool_state",
        descriptor.encoding_version,
        fleet,
    )
    .map_err(|error| error.to_string())?;
    // A `Lift::Decoder` surface: the component's bytes decode alike at every
    // admitted encoding, so the fact is the address.
    Ok(descriptor.blob_ref.to_string())
}

fn restamp_component(bytes: &[u8], version: u32) -> Vec<u8> {
    json_restamp(bytes, "encoding_version", version)
}

// --- RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION: a turn commit's receipt. ---

fn write_receipt(fleet: FleetFormat) -> Vec<u8> {
    serde_json::to_vec(&RuntimeCommitReceipt {
        schema_version: fleet.writer_version(crate::surface_format!(
            RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION
        )),
        head_revision: 3,
        checkpoint_ref: BlobRef::for_content(b"checkpoint"),
        manifest: SessionCheckpoint::for_fleet(FleetFormat::current()),
        committed_leaf_node_id: None,
        realized_node_timestamps: Vec::new(),
        failure_evidence: Vec::new(),
        outcome: None,
        pending_follow_on: None,
        turn_input_applications: Vec::new(),
        turn_cancel_input_outcome: crate::TurnCancelInputOutcome::default(),
        command_outcomes: Default::default(),
        receipt_replayed: false,
    })
    .expect("encode the receipt")
}

fn read_receipt(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    super::decode_runtime_commit_receipt_for_fleet(
        &SessionId::from("session"),
        "turn",
        text(bytes),
        fleet,
    )
    .map(|receipt| format!("{receipt:?}"))
    .map_err(|error| error.to_string())
}

// --- SESSION_HEAD_META_SCHEMA_VERSION: the mutable session head. ---

fn write_head(fleet: FleetFormat) -> Vec<u8> {
    let session = SessionId::from("session");
    let meta = SessionHeadMeta::created(
        &session,
        crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded),
        fleet,
    );
    serde_json::to_vec(&SessionHeadPayload {
        schema_version: meta.schema_version,
        session_id: session,
        config: meta.config,
        current_frame_node_id: None,
        published_by_drive: meta.published_by_drive,
    })
    .expect("encode the head")
}

fn read_head(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    super::decode_versioned_json_record_for_fleet::<SessionHeadPayload>(
        text(bytes),
        "SessionHeadMeta",
        crate::surface_format!(SESSION_HEAD_META_SCHEMA_VERSION),
        fleet,
    )
    .map(|payload| format!("{payload:?}"))
    .map_err(|error| error.to_string())
}

// --- CURRENT_SESSION_STATE_VERSION: the session-state generation marker. ---

fn write_state_marker(fleet: FleetFormat) -> Vec<u8> {
    fleet
        .writer_version(crate::surface_format!(CURRENT_SESSION_STATE_VERSION))
        .to_string()
        .into_bytes()
}

fn read_state_marker(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    let marker = text(bytes)
        .parse::<u32>()
        .map_err(|error| error.to_string())?;
    // A `Lift::Decoder` surface: every admitted generation is read as the
    // same state, so the fact is the admission.
    super::resolve_session_state_version(Some(marker), fleet)
        .map(|_| "admitted".to_owned())
        .map_err(|error| error.to_string())
}

fn restamp_state_marker(_bytes: &[u8], version: u32) -> Vec<u8> {
    version.to_string().into_bytes()
}

// --- OBLIGATION_LEDGER_VOCABULARY_VERSION: the ledger's label vocabulary. ---

/// The vocabulary's stored form is its labels; every label an earlier
/// vocabulary wrote is one this build decodes.
fn write_obligation_labels(_fleet: FleetFormat) -> Vec<u8> {
    serde_json::to_vec(
        &ObligationKind::ALL
            .into_iter()
            .map(ObligationKind::label)
            .collect::<Vec<_>>(),
    )
    .expect("encode the labels")
}

fn read_obligation_labels(bytes: &[u8], _fleet: FleetFormat) -> Result<String, String> {
    let labels: Vec<String> = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    labels
        .iter()
        .map(|label| ObligationKind::from_label(label).map(|kind| kind.label()))
        .collect::<Result<Vec<_>, _>>()
        .map(|kinds| kinds.join(","))
        .map_err(|error| error.to_string())
}

/// A vocabulary this build does not know shows as a label it cannot name.
fn restamp_obligation_labels(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut labels: Vec<String> = serde_json::from_slice(bytes).expect("labels");
    labels.push(format!("kind_of_vocabulary_{version}"));
    serde_json::to_vec(&labels).expect("encode")
}

fn probes() -> Vec<SurfaceProbe> {
    vec![
        SurfaceProbe {
            constant: "SESSION_NODE_BODY_SCHEMA_VERSION",
            newest: SESSION_NODE_BODY_SCHEMA_VERSION,
            write: write_node,
            read: read_node,
            restamp: restamp_schema_version,
        },
        SurfaceProbe {
            constant: "SESSION_CHECKPOINT_SCHEMA_VERSION",
            newest: SESSION_CHECKPOINT_SCHEMA_VERSION,
            write: write_checkpoint,
            read: read_checkpoint,
            restamp: restamp_checkpoint,
        },
        SurfaceProbe {
            constant: "CHECKPOINT_COMPONENT_ENCODING_VERSION",
            newest: CHECKPOINT_COMPONENT_ENCODING_VERSION,
            write: write_component,
            read: read_component,
            restamp: restamp_component,
        },
        SurfaceProbe {
            constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
            newest: RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
            write: write_receipt,
            read: read_receipt,
            restamp: restamp_schema_version,
        },
        SurfaceProbe {
            constant: "SESSION_HEAD_META_SCHEMA_VERSION",
            newest: SESSION_HEAD_META_SCHEMA_VERSION,
            write: write_head,
            read: read_head,
            restamp: restamp_schema_version,
        },
        SurfaceProbe {
            constant: "CURRENT_SESSION_STATE_VERSION",
            newest: CURRENT_SESSION_STATE_VERSION,
            write: write_state_marker,
            read: read_state_marker,
            restamp: restamp_state_marker,
        },
        SurfaceProbe {
            constant: "OBLIGATION_LEDGER_VOCABULARY_VERSION",
            newest: OBLIGATION_LEDGER_VOCABULARY_VERSION,
            write: write_obligation_labels,
            read: read_obligation_labels,
            restamp: restamp_obligation_labels,
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
