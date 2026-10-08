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
//! `lash-core-store` sits below the crates that own some of these surfaces,
//! so it cannot name their constants. It reads the version N writes of each
//! from [`PREDECESSOR_WRITES`], the table `scripts/release_baseline.py`
//! generates from the owners' constants, and spells no version itself. Each
//! owner's `every_guarded_surface_decodes_its_supported_range` law, run
//! under the same feature, fails when the table and the owner's constant
//! disagree.

use super::fleet_format::{Lift, RecordUpcaster, WriterPin};
use super::synthetic_next_versions::PREDECESSOR_WRITES;

/// The version N writes of the surface registered as `constant`.
const fn predecessor(constant: &'static str) -> u32 {
    let name = constant.as_bytes();
    let mut row = 0;
    while row < PREDECESSOR_WRITES.len() {
        let candidate = PREDECESSOR_WRITES[row].0.as_bytes();
        let mut at = 0;
        while at < name.len() && at < candidate.len() && name[at] == candidate[at] {
            at += 1;
        }
        if at == name.len() && at == candidate.len() {
            return PREDECESSOR_WRITES[row].1;
        }
        row += 1;
    }
    panic!("the release inventory lists no surface the synthetic N+1 moves under this name")
}

const PROCESS_EVENT_VOCABULARY_NEXT: u32 = predecessor("PROCESS_EVENT_VOCABULARY_VERSION") + 1;
const SCOPE_STORAGE_PAYLOAD_NEXT: u32 = predecessor("SCOPE_STORAGE_PAYLOAD_VERSION") + 1;
const RLM_DRIVER_STATE_NEXT: u32 = predecessor("RLM_DRIVER_STATE_VERSION") + 1;

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

/// `PROCESS_EVENT_VOCABULARY_VERSION` N+1: an effect-summary event carries
/// its vocabulary under `vocabulary_version`.
fn lift_process_event_vocabulary(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(
        value,
        "process effect event",
        "vocabulary_version",
        PROCESS_EVENT_VOCABULARY_NEXT,
    )
}

/// `SCOPE_STORAGE_PAYLOAD_VERSION` N+1.
fn lift_scope_storage_payload(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(
        value,
        "scope storage payload",
        "version",
        SCOPE_STORAGE_PAYLOAD_NEXT,
    )
}

/// `RLM_DRIVER_STATE_VERSION` N+1.
fn lift_native_driver_state(value: &mut serde_json::Value) -> Result<(), crate::StoreError> {
    restamp(
        value,
        "native driver state",
        "schema_version",
        RLM_DRIVER_STATE_NEXT,
    )
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

/// [`tree`] for a surface owned above this crate, from the version N writes.
const fn owner_tree(
    constant: &'static str,
    lift: fn(&mut serde_json::Value) -> Result<(), crate::StoreError>,
) -> RecordUpcaster {
    tree(constant, predecessor(constant), lift)
}

/// [`decoder`] for a surface owned above this crate, from the version N writes.
const fn owner_decoder(constant: &'static str) -> RecordUpcaster {
    decoder(constant, predecessor(constant))
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
    decoder(
        "CURRENT_SESSION_STATE_VERSION",
        super::CURRENT_SESSION_STATE_VERSION - 1,
    ),
    decoder(
        "OBLIGATION_LEDGER_VOCABULARY_VERSION",
        super::OBLIGATION_LEDGER_VOCABULARY_VERSION - 1,
    ),
    owner_tree(
        "PROCESS_EVENT_VOCABULARY_VERSION",
        lift_process_event_vocabulary,
    ),
    owner_tree("SCOPE_STORAGE_PAYLOAD_VERSION", lift_scope_storage_payload),
    owner_decoder("LASHLANG_SNAPSHOT_VERSION"),
    owner_decoder("RLM_SNAPSHOT_VERSION"),
    owner_tree("RLM_DRIVER_STATE_VERSION", lift_native_driver_state),
    owner_decoder("SQLITE_BLOB_ENVELOPE_VERSION"),
];

const fn pin(constant: &'static str, version: u32) -> WriterPin {
    WriterPin {
        constant,
        generation: 1,
        version,
    }
}

/// [`pin`] for a surface owned above this crate, at the version N writes.
const fn owner_pin(constant: &'static str) -> WriterPin {
    pin(constant, predecessor(constant))
}

/// While `F` is N's epoch, every surface N+1 moves is written at N's version:
/// each guarded surface's and the process cursor's.
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
        "CURRENT_SESSION_STATE_VERSION",
        super::CURRENT_SESSION_STATE_VERSION - 1,
    ),
    pin(
        "OBLIGATION_LEDGER_VOCABULARY_VERSION",
        super::OBLIGATION_LEDGER_VOCABULARY_VERSION - 1,
    ),
    owner_pin("PROCESS_EVENT_VOCABULARY_VERSION"),
    owner_pin("SCOPE_STORAGE_PAYLOAD_VERSION"),
    owner_pin("LASHLANG_SNAPSHOT_VERSION"),
    owner_pin("WORKFLOW_GRAPH_SCHEMA_VERSION"),
    owner_pin("RLM_SNAPSHOT_VERSION"),
    owner_pin("RLM_DRIVER_STATE_VERSION"),
    owner_pin("SQLITE_BLOB_ENVELOPE_VERSION"),
    pin(
        "PROCESS_CURSOR_VERSION",
        lash_sansio::PROCESS_CURSOR_VERSION - 1,
    ),
];
