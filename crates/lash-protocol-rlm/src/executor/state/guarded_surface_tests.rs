//! FIG-3802's laws over the guarded surfaces `lash-protocol-rlm` owns, each
//! driven through its production writer and decoder: the RLM snapshot root,
//! the native transport envelopes in session history, and the native driver
//! state parked in the protocol driver-state slot.

use std::collections::BTreeMap;

use lash_core::FleetFormat;
use lash_core_store::testing::guarded_surfaces::{self as laws, SurfaceProbe};

use super::{RLM_SNAPSHOT_VERSION, RlmExecutionState, RlmSnapshotRoot};
use crate::native::state::{
    NATIVE_DRIVER_STATE_VERSION, RlmDriverState, decode_rlm_driver_state, rlm_driver_state,
};
use crate::native::transport::{NATIVE_TRANSPORT_VERSION, decode_payload, execution_event};

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("guarded-surface runtime")
        .block_on(future)
}

const OWNER: &str = "lash-protocol-rlm";

// --- RLM_SNAPSHOT_VERSION: the canonical execution-state root. ---

fn write_root(fleet: FleetFormat) -> Vec<u8> {
    run(async {
        let snapshot = RlmExecutionState::new()
            .snapshot_execution_state(fleet)
            .await
            .expect("snapshot a fresh state");
        assert!(
            snapshot.components.is_empty(),
            "a fresh state's root carries every value inline"
        );
        snapshot
            .root
            .expect("a fresh state snapshots a root")
            .to_vec()
    })
}

fn read_root(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    run(async {
        let hydration = lash_core::plugin::HydratedExecutionState {
            root: bytes.to_vec().into(),
            components: BTreeMap::new(),
        };
        let mut state = RlmExecutionState::new();
        state
            .restore_execution_state(&hydration, fleet)
            .await
            .map_err(|error| error.to_string())?;
        // The restored state is the fact: taken again at the newest root, every
        // admitted version restores to the same one.
        let again = state
            .snapshot_execution_state(FleetFormat::current())
            .await
            .map_err(|error| error.to_string())?;
        Ok(format!("{:?}", again.root))
    })
}

fn restamp_root(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut root: RlmSnapshotRoot = rmp_serde::from_slice(bytes).expect("a root");
    root.version = version;
    rmp_serde::to_vec_named(&root).expect("encode")
}

// --- NATIVE_TRANSPORT_VERSION: a provider call's envelope in history. ---

fn write_transport(fleet: FleetFormat) -> Vec<u8> {
    let lash_core::SessionHistoryRecord::Protocol(event) = execution_event(
        "step".to_owned(),
        Vec::new(),
        fleet.writer_version(lash_core::surface_format!(NATIVE_TRANSPORT_VERSION)),
    ) else {
        panic!("a transport envelope is a protocol event");
    };
    let Some(lash_rlm_types::RlmProtocolEvent::RlmDiagnostic(diagnostic)) =
        crate::projection::decode_rlm_protocol_event(&event)
    else {
        panic!("a transport envelope is an RLM diagnostic");
    };
    serde_json::to_vec(&diagnostic.payload).expect("encode the envelope")
}

fn read_transport(bytes: &[u8], _fleet: FleetFormat) -> Result<String, String> {
    let payload = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    decode_payload(payload)
        .map_err(|error| error.to_string())
        .and_then(|transport| serde_json::to_string(&transport).map_err(|e| e.to_string()))
}

// --- NATIVE_DRIVER_STATE_VERSION: the parked native driver state. ---

fn write_driver_state(fleet: FleetFormat) -> Vec<u8> {
    let state = rlm_driver_state(
        RlmDriverState::default(),
        fleet.writer_version(lash_core::surface_format!(NATIVE_DRIVER_STATE_VERSION)),
    );
    serde_json::to_vec(&state.payload).expect("encode the driver state")
}

fn read_driver_state(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    let payload = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let state = lash_core::ProtocolDriverState::new(crate::plugin::RLM_PROTOCOL_PLUGIN_ID, payload);
    decode_rlm_driver_state(
        state,
        fleet.writer_version(lash_core::surface_format!(NATIVE_DRIVER_STATE_VERSION)),
    )
    .and_then(|state| serde_json::to_string(&state).map_err(|error| error.to_string()))
}

fn restamp_schema_version(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("a JSON record");
    value["schema_version"] = serde_json::json!(version);
    serde_json::to_vec(&value).expect("encode")
}

fn probes() -> Vec<SurfaceProbe> {
    vec![
        SurfaceProbe {
            constant: "RLM_SNAPSHOT_VERSION",
            newest: RLM_SNAPSHOT_VERSION,
            write: write_root,
            read: read_root,
            restamp: restamp_root,
        },
        SurfaceProbe {
            constant: "NATIVE_TRANSPORT_VERSION",
            newest: NATIVE_TRANSPORT_VERSION,
            write: write_transport,
            read: read_transport,
            restamp: restamp_schema_version,
        },
        SurfaceProbe {
            constant: "NATIVE_DRIVER_STATE_VERSION",
            newest: NATIVE_DRIVER_STATE_VERSION,
            write: write_driver_state,
            read: read_driver_state,
            restamp: restamp_schema_version,
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
