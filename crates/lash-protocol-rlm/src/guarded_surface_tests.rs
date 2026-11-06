use lash_core_store::store::FleetFormat;
use lash_core_store::testing::guarded_surfaces::{self as laws, SurfaceProbe};

use crate::driver_state::{RlmDriverState, decode_rlm_driver_state, rlm_driver_state};
use crate::{RLM_DRIVER_STATE_VERSION, RLM_PROTOCOL_EVENT_VERSION};

const OWNER: &str = "lash-protocol-rlm";

fn write_driver_state(fleet: FleetFormat) -> Vec<u8> {
    let state = rlm_driver_state(
        RlmDriverState {
            assistant_parts: vec![lash_core::Part::text(
                "reply".to_string(),
                "run the cell".to_string(),
                None,
            )],
            code: "1 + 2".to_string(),
        },
        fleet.writer_version(lash_core_store::surface_format!(RLM_DRIVER_STATE_VERSION)),
    );
    serde_json::to_vec(&state.payload).expect("encode driver state")
}

fn read_driver_state(bytes: &[u8], fleet: FleetFormat) -> Result<String, String> {
    let payload = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    decode_rlm_driver_state(
        lash_core::ProtocolDriverState::new(crate::RLM_PROTOCOL_PLUGIN_ID, payload),
        fleet.writer_version(lash_core_store::surface_format!(RLM_DRIVER_STATE_VERSION)),
    )
    .and_then(|state| serde_json::to_string(&state).map_err(|error| error.to_string()))
}

fn restamp(bytes: &[u8], field: &str, version: u32) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(bytes).expect("JSON payload");
    payload[field] = serde_json::json!(version);
    serde_json::to_vec(&payload).expect("encode restamped payload")
}

fn restamp_driver_state(bytes: &[u8], version: u32) -> Vec<u8> {
    restamp(bytes, "schema_version", version)
}

fn write_protocol_event(fleet: FleetFormat) -> Vec<u8> {
    let event = crate::rlm_protocol_event(
        lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(std::sync::Arc::new(
            lash_core::CellRecord {
                id: "cell".to_string(),
                language: "typescript".to_string(),
                code: "1 + 2".to_string(),
                result: lash_core::CellOutcome::Completed,
                ..Default::default()
            },
        )),
        fleet.writer_version(lash_core_store::surface_format!(RLM_PROTOCOL_EVENT_VERSION)),
    );
    serde_json::to_vec(&event.payload).expect("encode protocol event")
}

fn read_protocol_event(bytes: &[u8], _fleet: FleetFormat) -> Result<String, String> {
    let event = lash_core::ProtocolEvent {
        plugin_id: crate::RLM_PROTOCOL_PLUGIN_ID.into(),
        payload: serde_json::from_slice(bytes).map_err(|error| error.to_string())?,
    };
    crate::projection::decode_rlm_protocol_event(&event)
        .map(|event| format!("{event:?}"))
        .map_err(|error| error.to_string())
}

fn restamp_protocol_event(bytes: &[u8], version: u32) -> Vec<u8> {
    restamp(bytes, "format", version)
}

fn probes() -> Vec<SurfaceProbe> {
    vec![
        SurfaceProbe {
            constant: "RLM_DRIVER_STATE_VERSION",
            newest: RLM_DRIVER_STATE_VERSION,
            write: write_driver_state,
            read: read_driver_state,
            restamp: restamp_driver_state,
        },
        SurfaceProbe {
            constant: "RLM_PROTOCOL_EVENT_VERSION",
            newest: RLM_PROTOCOL_EVENT_VERSION,
            write: write_protocol_event,
            read: read_protocol_event,
            restamp: restamp_protocol_event,
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
fn history_reads_are_epoch_invariant_and_writers_are_deterministic() {
    laws::history_reads_are_epoch_invariant_and_writers_are_deterministic(OWNER, &probes());
}
