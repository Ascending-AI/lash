//! Phase A's synthetic N+1 (ADR 0115 §6): the compatibility policy of a
//! successor that moves every guarded surface one version on.
//!
//! Each surface keeps N's shape, so every lift is exact: a tree lift moves
//! the record's own version field, an object family's lift keeps the body as
//! it is, and a [`Lift::Decoder`] row admits N's version to a decoder that
//! reads it natively. While `F` is N's epoch every writer is pinned to the
//! version N reads, so before finalize N+1 writes only what N reads; after
//! finalize it writes its own, and it reads N's for as long as a record of
//! it can exist. The derived workflow projections register no lift: under
//! N's epoch their readers admit the pinned version, and after finalize an
//! older projection is regenerated from its module (FIG-4262).
//!
//! The versions of surfaces this crate does not own are spelled as literals:
//! `lash-core-store` sits below their crates. Each owner's
//! `every_guarded_surface_decodes_its_supported_range` law, run under the
//! same feature, fails when a literal and the owner's constant disagree.

use super::fleet_format::{Lift, RecordUpcaster, WriterPin};

/// A tree lift that moves the record's version `field` to `to`.
fn restamp(
    value: &mut serde_json::Value,
    record_kind: &'static str,
    field: &str,
    to: u32,
) -> Result<(), crate::StoreError> {
    let Some(record) = value.as_object_mut() else {
        return Err(crate::StoreError::StoredDataCorrupt {
            record_kind,
            message: format!("a {record_kind} record is not a JSON object"),
        });
    };
    record.insert(field.to_owned(), serde_json::json!(to));
    Ok(())
}

fn lift_session_checkpoint(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(
        value,
        "session checkpoint",
        "schema_version",
        super::SESSION_CHECKPOINT_SCHEMA_VERSION,
    )
}

fn lift_runtime_commit_receipt(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(
        value,
        "runtime commit receipt",
        "schema_version",
        super::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
    )
}

fn lift_session_head_meta(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(
        value,
        "session head meta",
        "schema_version",
        super::SESSION_HEAD_META_SCHEMA_VERSION,
    )
}

fn lift_process_wake_delivery(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(
        value,
        "process wake delivery",
        "version",
        crate::process_identity::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
    )
}

/// `PROCESS_EVENT_VOCABULARY_VERSION` N+1: an effect-summary event carries
/// its vocabulary under `vocabulary_version`.
fn lift_process_event_vocabulary(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(value, "process effect event", "vocabulary_version", 2)
}

/// `SCOPE_STORAGE_PAYLOAD_VERSION` N+1.
fn lift_scope_storage_payload(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(value, "scope storage payload", "version", 3)
}

/// `NATIVE_DRIVER_STATE_VERSION` N+1.
fn lift_native_driver_state(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(value, "native driver state", "schema_version", 3)
}

/// A Restate object family's stamped body, or a `LashTurn` outcome's: N+1
/// moves only the `{format, body}` stamp, so N's body is N+1's.
fn lift_object_body(_body: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    Ok(())
}

const fn tree(
    constant: &'static str,
    from_version: u32,
    lift: fn(&mut serde_json::Value) -> Result<(), crate::StoreError>,
) -> RecordUpcaster {
    RecordUpcaster {
        constant,
        from_version,
        lift: Lift::Tree(lift),
    }
}

const fn decoder(constant: &'static str, from_version: u32) -> RecordUpcaster {
    RecordUpcaster {
        constant,
        from_version,
        lift: Lift::Decoder,
    }
}

/// One lift per guarded surface, from the version N writes.
pub(super) const RECORD_UPCASTERS: &[RecordUpcaster] = &[
    tree(
        "SESSION_NODE_BODY_SCHEMA_VERSION",
        crate::session_graph::SESSION_NODE_BODY_SCHEMA_VERSION - 1,
        crate::session_graph::upcast_synthetic_node_body,
    ),
    tree(
        "SESSION_CHECKPOINT_SCHEMA_VERSION",
        super::SESSION_CHECKPOINT_SCHEMA_VERSION - 1,
        lift_session_checkpoint,
    ),
    decoder(
        "CHECKPOINT_COMPONENT_ENCODING_VERSION",
        super::CHECKPOINT_COMPONENT_ENCODING_VERSION - 1,
    ),
    tree(
        "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
        super::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION - 1,
        lift_runtime_commit_receipt,
    ),
    tree(
        "SESSION_HEAD_META_SCHEMA_VERSION",
        super::SESSION_HEAD_META_SCHEMA_VERSION - 1,
        lift_session_head_meta,
    ),
    tree(
        "PROCESS_WAKE_DELIVERY_FORMAT_VERSION",
        crate::process_identity::PROCESS_WAKE_DELIVERY_FORMAT_VERSION - 1,
        lift_process_wake_delivery,
    ),
    decoder(
        "CURRENT_SESSION_STATE_VERSION",
        super::CURRENT_SESSION_STATE_VERSION - 1,
    ),
    decoder(
        "OBLIGATION_LEDGER_VOCABULARY_VERSION",
        super::OBLIGATION_LEDGER_VOCABULARY_VERSION - 1,
    ),
    decoder(
        "PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION",
        crate::protocol_turn_options::PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION - 1,
    ),
    tree(
        "PROCESS_EVENT_VOCABULARY_VERSION",
        1,
        lift_process_event_vocabulary,
    ),
    tree(
        "SCOPE_STORAGE_PAYLOAD_VERSION",
        2,
        lift_scope_storage_payload,
    ),
    decoder("LASHLANG_SNAPSHOT_VERSION", 14),
    decoder("HEAP_SIZE_SCHEDULE_VERSION", 3),
    decoder("RLM_SNAPSHOT_VERSION", 26),
    decoder("NATIVE_TRANSPORT_VERSION", 1),
    tree("NATIVE_DRIVER_STATE_VERSION", 2, lift_native_driver_state),
    decoder("SQLITE_BLOB_ENVELOPE_VERSION", 1),
    tree("EFFECT_GROUP_STATE_FORMAT_VERSION", 1, lift_object_body),
    tree("EFFECT_GROUP_PAYLOAD_FORMAT_VERSION", 1, lift_object_body),
    tree("DURABLE_WAIT_REGISTRY_FORMAT_VERSION", 1, lift_object_body),
    tree("LASH_TURN_OUTCOME_FORMAT_VERSION", 1, lift_object_body),
];

const fn pin(constant: &'static str, version: u32) -> WriterPin {
    WriterPin {
        constant,
        generation: 1,
        version,
    }
}

/// While `F` is N's epoch, every surface N+1 moves is written at N's version:
/// each guarded surface's, the Restate wire's and the process cursor's.
pub(super) const WRITER_PINS: &[WriterPin] = &[
    pin(
        "SESSION_NODE_BODY_SCHEMA_VERSION",
        crate::session_graph::SESSION_NODE_BODY_SCHEMA_VERSION - 1,
    ),
    pin(
        "SESSION_CHECKPOINT_SCHEMA_VERSION",
        super::SESSION_CHECKPOINT_SCHEMA_VERSION - 1,
    ),
    pin(
        "CHECKPOINT_COMPONENT_ENCODING_VERSION",
        super::CHECKPOINT_COMPONENT_ENCODING_VERSION - 1,
    ),
    pin(
        "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
        super::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION - 1,
    ),
    pin(
        "SESSION_HEAD_META_SCHEMA_VERSION",
        super::SESSION_HEAD_META_SCHEMA_VERSION - 1,
    ),
    pin(
        "PROCESS_WAKE_DELIVERY_FORMAT_VERSION",
        crate::process_identity::PROCESS_WAKE_DELIVERY_FORMAT_VERSION - 1,
    ),
    pin(
        "CURRENT_SESSION_STATE_VERSION",
        super::CURRENT_SESSION_STATE_VERSION - 1,
    ),
    pin(
        "OBLIGATION_LEDGER_VOCABULARY_VERSION",
        super::OBLIGATION_LEDGER_VOCABULARY_VERSION - 1,
    ),
    pin(
        "PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION",
        crate::protocol_turn_options::PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION - 1,
    ),
    pin("PROCESS_EVENT_VOCABULARY_VERSION", 1),
    pin("SCOPE_STORAGE_PAYLOAD_VERSION", 2),
    pin("LASHLANG_SNAPSHOT_VERSION", 14),
    pin("HEAP_SIZE_SCHEDULE_VERSION", 3),
    pin("WORKFLOW_GRAPH_SCHEMA_VERSION", 21),
    pin("RLM_SNAPSHOT_VERSION", 26),
    pin("NATIVE_TRANSPORT_VERSION", 1),
    pin("NATIVE_DRIVER_STATE_VERSION", 2),
    pin("SQLITE_BLOB_ENVELOPE_VERSION", 1),
    pin("EFFECT_GROUP_STATE_FORMAT_VERSION", 1),
    pin("EFFECT_GROUP_PAYLOAD_FORMAT_VERSION", 1),
    pin("DURABLE_WAIT_REGISTRY_FORMAT_VERSION", 1),
    pin("LASH_TURN_OUTCOME_FORMAT_VERSION", 1),
    pin("RESTATE_WIRE_VERSION", 1),
    pin(
        "PROCESS_CURSOR_VERSION",
        lash_sansio::PROCESS_CURSOR_VERSION - 1,
    ),
];
