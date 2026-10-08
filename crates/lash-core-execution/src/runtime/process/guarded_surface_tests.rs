//! FIG-3802's laws over the guarded surfaces `lash-core-execution` owns,
//! each executed through its production writer and decoder.

use lash_core_store::testing::guarded_surfaces::{self as laws, SurfaceProbe};

use super::effect_summary::{
    PROCESS_EVENT_VOCABULARY_VERSION, ProcessEffectOccurrence, ProcessEffectOutcomeClass,
};
use crate::{FleetFormat, SCOPE_STORAGE_PAYLOAD_VERSION};

const OWNER: &str = "lash-core-execution";

fn restamp(bytes: &[u8], field: &str, version: u32) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("a JSON record");
    value[field] = serde_json::json!(version);
    serde_json::to_vec(&value).expect("encode")
}

// --- PROCESS_EVENT_VOCABULARY_VERSION: a process log's effect summary. ---

fn write_occurrence(fleet: FleetFormat) -> Vec<u8> {
    serde_json::to_vec(&ProcessEffectOccurrence::new(
        "resource_operation:node",
        1,
        "fixture.operation",
        ProcessEffectOutcomeClass::Success,
        None,
        "lashlang:scope:resource:17:fixture.operation:23:resource_operation:node:1",
        fleet,
    ))
    .expect("encode the occurrence")
}

fn read_occurrence(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    let payload = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    ProcessEffectOccurrence::decode(payload, fleet)
        .map(|occurrence| format!("{occurrence:?}"))
        .map_err(|error| error.to_string())
}

fn restamp_occurrence(bytes: &[u8], version: u32) -> Vec<u8> {
    restamp(bytes, "vocabulary_version", version)
}

// --- SCOPE_STORAGE_PAYLOAD_VERSION: a ledger row's typed parent scope. ---

fn scope() -> crate::ScopeId {
    crate::ScopeId::Session(crate::SessionId::from("session"))
}

fn write_scope(fleet: FleetFormat) -> Vec<u8> {
    scope()
        .storage_payload(fleet)
        .expect("encode the scope")
        .into_bytes()
}

fn read_scope(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    let scope = scope();
    crate::ScopeId::from_storage_columns(
        scope.storage_kind(),
        &scope.storage_id(),
        std::str::from_utf8(bytes).map_err(|error| error.to_string())?,
        fleet,
    )
    .map(|scope| format!("{scope:?}"))
    .map_err(|error| error.to_string())
}

fn restamp_scope(bytes: &[u8], version: u32) -> Vec<u8> {
    restamp(bytes, "version", version)
}

fn write_admission(fleet: FleetFormat) -> Vec<u8> {
    let session = crate::SessionId::fixture("admission");
    let view = crate::plugin::PluginNativeView {
        request: crate::plugin::PluginTransitionRequest {
            id: crate::plugin::PluginTransitionId(
                crate::EffectAddress::new(
                    crate::ExecutionScope::turn(&session, "run"),
                    "plugin-transition",
                )
                .expect("address"),
            ),
            owner: crate::RuntimeOwner::Session(session),
            base: crate::plugin::PluginTransitionBase::Session {
                head: crate::store::SessionHeadRef {
                    generation: 0,
                    revision: 0,
                    leaf: None,
                    checkpoint: None,
                },
            },
            target: Default::default(),
        },
        source: crate::BlobRef::for_content(b"native"),
        generation: None,
        state: Default::default(),
        config: Default::default(),
    };
    view.encode(fleet).expect("encode admission").to_vec()
}
fn read_admission(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    crate::plugin::PluginNativeView::decode(bytes, fleet)
        .map_err(|error| error.to_string())
        .and_then(|view| serde_json::to_string(&view).map_err(|error| error.to_string()))
}
fn restamp_admission(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut value: serde_json::Value = rmp_serde::from_slice(bytes).expect("admission");
    value["format"] = serde_json::json!(version);
    rmp_serde::to_vec_named(&value).expect("restamp")
}
fn write_runtime_event(fleet: FleetFormat) -> Vec<u8> {
    let event = crate::session_model::plugin_runtime_protocol_event(
        "fixture",
        crate::PluginRuntimeEvent::Status {
            key: "status".into(),
            label: "ready".into(),
            detail: None,
        },
        fleet,
    )
    .expect("runtime event");
    serde_json::to_vec(&event.payload).expect("JSON")
}
fn read_runtime_event(bytes: &[u8], _fleet: FleetFormat) -> Result<String, String> {
    let event = crate::ProtocolEvent {
        plugin_id: crate::session_model::PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID.into(),
        payload: serde_json::from_slice(bytes).map_err(|error| error.to_string())?,
    };
    crate::session_model::plugin_runtime_event_from_protocol(&event)
        .map_err(|error| error.to_string())
        .and_then(|record| {
            serde_json::to_string(&record.map(|record| record.event))
                .map_err(|error| error.to_string())
        })
}
fn restamp_format(bytes: &[u8], version: u32) -> Vec<u8> {
    restamp(bytes, "format", version)
}

fn probes() -> Vec<SurfaceProbe> {
    vec![
        SurfaceProbe {
            constant: "PLUGIN_ADMISSION_CHECKPOINT_VERSION",
            newest: crate::plugin::PLUGIN_ADMISSION_CHECKPOINT_VERSION,
            write: write_admission,
            read: read_admission,
            restamp: restamp_admission,
        },
        SurfaceProbe {
            constant: "PLUGIN_RUNTIME_EVENT_VERSION",
            newest: crate::session_model::PLUGIN_RUNTIME_EVENT_VERSION,
            write: write_runtime_event,
            read: read_runtime_event,
            restamp: restamp_format,
        },
        SurfaceProbe {
            constant: "PROCESS_EVENT_VOCABULARY_VERSION",
            newest: PROCESS_EVENT_VOCABULARY_VERSION,
            write: write_occurrence,
            read: read_occurrence,
            restamp: restamp_occurrence,
        },
        SurfaceProbe {
            constant: "SCOPE_STORAGE_PAYLOAD_VERSION",
            newest: u32::from(SCOPE_STORAGE_PAYLOAD_VERSION),
            write: write_scope,
            read: read_scope,
            restamp: restamp_scope,
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
