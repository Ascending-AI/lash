use lash_core::AttachmentRef;

use lash_rlm_types::RlmExecutedCall;

use lash_rlm_types::CellOutcome;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct RlmReasoningPart {
    pub(super) text: String,
    pub(super) replay: Option<lash_core::llm::types::ProviderReasoningReplay>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct RlmDriverState {
    #[serde(default)]
    pub(super) reasoning: Vec<RlmReasoningPart>,
    pub(super) assistant_parts: Vec<lash_core::Part>,
    pub(super) images: Vec<AttachmentRef>,
    #[serde(default)]
    pub(super) calls: Vec<RlmExecutedCall>,
    #[serde(default)]
    pub(super) calls_omitted: usize,
    /// One entry per `print` from the executed lashlang block (plus any
    /// raw stdout-style emission). Replaces the old split between a
    /// concatenated `combined_output: String` and a sibling
    /// `observations: Vec<String>` — the two carried the same content.
    pub(super) output: Vec<lash_rlm_types::RlmPrint>,
    /// The tagged outcome preserves null terminal values. Ambiguous parked
    /// states with the old optional pair must be recreated before 1.0.
    pub(super) outcome: CellOutcome<lash_core::CellFailure>,
    pub(super) code: String,
}

/// Schema version of the native RLM driver state parked in the protocol
/// driver-state slot; a decode refuses any other version.
pub const NATIVE_DRIVER_STATE_VERSION: u32 = 2;

#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    schema_version: u32,
    state: RlmDriverState,
}

#[expect(
    clippy::expect_used,
    reason = "the envelope carries a u32 schema version and crate-owned driver state, so serde_json encoding cannot fail"
)]
pub(super) fn rlm_driver_state(
    state: RlmDriverState,
    schema_version: u32,
) -> lash_core::ProtocolDriverState {
    lash_core::ProtocolDriverState::new(
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        serde_json::to_value(Envelope {
            schema_version,
            state,
        })
        .expect("native driver state must serialize"),
    )
}

/// `fleet_recorded_version` is the version the fleet's writers emit for this
/// surface — `F`'s recorded version, which the driver's `WriterFormats` table
/// reports (FIG-3796). The decode admits the pair `{fleet_recorded_version,
/// NATIVE_DRIVER_STATE_VERSION}` — ADR 0106 §2's `[N-1, N]` window — and an
/// admitted older payload climbs to the newest through the surface's
/// `RecordUpcaster` hooks; anything else is refused as unsupported.
pub(super) fn decode_rlm_driver_state(
    state: lash_core::ProtocolDriverState,
    fleet_recorded_version: u32,
) -> Result<RlmDriverState, String> {
    if state.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
        return Err(format!(
            "driver state belongs to plugin `{}`, expected `{}`",
            state.plugin_id,
            crate::plugin::RLM_PROTOCOL_PLUGIN_ID
        ));
    }
    let mut payload = state.payload;
    let actual = payload
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| "native driver state carries no u32 schema_version".to_string())?;
    if actual != NATIVE_DRIVER_STATE_VERSION && actual != fleet_recorded_version {
        return Err(format!(
            "unsupported native driver state version {actual}, expected {NATIVE_DRIVER_STATE_VERSION}"
        ));
    }
    if actual != NATIVE_DRIVER_STATE_VERSION {
        lash_core::store::upcast_json_record(
            "native driver state",
            lash_core::surface_format!(NATIVE_DRIVER_STATE_VERSION),
            actual,
            NATIVE_DRIVER_STATE_VERSION,
            &mut payload,
        )
        .map_err(|error| error.to_string())?;
    }
    let envelope: Envelope = serde_json::from_value(payload)
        .map_err(|error| format!("invalid native driver state: {error}"))?;
    Ok(envelope.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parked_outcomes_preserve_null_finish_and_typed_failures() {
        let mut payloads = Vec::new();
        for outcome in [
            CellOutcome::Finished(serde_json::Value::Null),
            CellOutcome::Running,
            CellOutcome::Finished(serde_json::json!({"answer": 42})),
            CellOutcome::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Program,
                "bad program",
            )),
            CellOutcome::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Policy,
                "bound exceeded",
            )),
            CellOutcome::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                "host unavailable",
            )),
        ] {
            let state = RlmDriverState {
                outcome: outcome.clone(),
                ..RlmDriverState::default()
            };
            let encoded = rlm_driver_state(state, NATIVE_DRIVER_STATE_VERSION);
            payloads.push(encoded.payload.clone());
            let restored = decode_rlm_driver_state(encoded, NATIVE_DRIVER_STATE_VERSION).unwrap();
            assert_eq!(restored.outcome.as_ref(), outcome.as_ref());
        }
        assert_ne!(
            payloads[0], payloads[1],
            "null finish and running need distinct parked bytes"
        );
        let mut encoded = rlm_driver_state(RlmDriverState::default(), NATIVE_DRIVER_STATE_VERSION);
        let old_state = &mut encoded.payload["state"];
        old_state.as_object_mut().unwrap().remove("outcome");
        old_state["error"] = serde_json::Value::Null;
        old_state["terminal_finish"] = serde_json::Value::Null;
        assert!(
            decode_rlm_driver_state(encoded, NATIVE_DRIVER_STATE_VERSION).is_err(),
            "ambiguous old parked bytes must be recreated"
        );
    }

    #[test]
    fn native_state_version_is_pinned_and_predecessors_are_refused() {
        let mut encoded = rlm_driver_state(RlmDriverState::default(), NATIVE_DRIVER_STATE_VERSION);
        assert_eq!(encoded.payload["schema_version"], 2);
        encoded.payload["schema_version"] = serde_json::json!(1);
        assert!(decode_rlm_driver_state(encoded, NATIVE_DRIVER_STATE_VERSION).is_err());
        let unversioned = lash_core::ProtocolDriverState::new(
            crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
            serde_json::json!({"state":{}}),
        );
        assert!(decode_rlm_driver_state(unversioned, NATIVE_DRIVER_STATE_VERSION).is_err());
    }
}
