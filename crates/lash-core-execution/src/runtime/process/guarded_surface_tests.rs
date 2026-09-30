//! FIG-3802's laws over the guarded surfaces `lash-core-execution` owns,
//! each driven through its production writer and decoder.

use lash_core_store::testing::guarded_surfaces::{self as laws, SurfaceProbe};

use super::effect_summary::{
    PROCESS_EVENT_VOCABULARY_VERSION, ProcessEffectOccurrence, ProcessEffectOutcomeClass,
};
use super::registry_transitions::decode_process_wake_delivery;
use crate::{FleetFormat, PROCESS_WAKE_DELIVERY_FORMAT_VERSION, SCOPE_STORAGE_PAYLOAD_VERSION};

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

// --- PROCESS_WAKE_DELIVERY_FORMAT_VERSION: an outbox row's wake. ---

fn write_wake(fleet: FleetFormat) -> Vec<u8> {
    let process_id = crate::process_id_for_test("process-1");
    serde_json::to_vec(&crate::ProcessWakeDelivery {
        version: fleet.writer_version(lash_core_store::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: "wake:law".to_owned(),
        target_session_id: crate::SessionId::from("target"),
        process_id: process_id.clone(),
        sequence: 7,
        event_type: "process.ready".to_owned(),
        event_invocation: crate::RuntimeInvocation {
            attribution: crate::RuntimeAttribution::for_session("target"),
            subject: crate::RuntimeSubject::ProcessEvent {
                process_id: process_id.clone(),
                sequence: 7,
                event_type: "process.ready".to_owned(),
            },
            caused_by: Some(crate::CausalRef::Process { process_id }),
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: "wake".to_owned(),
        created_at_ms: 123,
    })
    .expect("encode the wake")
}

fn read_wake(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    decode_process_wake_delivery(
        std::str::from_utf8(bytes).map_err(|error| error.to_string())?,
        fleet,
    )
    .map(|wake| format!("{wake:?}"))
    .map_err(|error| error.to_string())
}

fn restamp_wake(bytes: &[u8], version: u32) -> Vec<u8> {
    restamp(bytes, "version", version)
}

fn probes() -> Vec<SurfaceProbe> {
    vec![
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
        SurfaceProbe {
            constant: "PROCESS_WAKE_DELIVERY_FORMAT_VERSION",
            newest: PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            write: write_wake,
            read: read_wake,
            restamp: restamp_wake,
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
