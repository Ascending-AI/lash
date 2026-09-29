use lash_core::AttachmentRef;
use lash_core::llm::types::ProviderReasoningReplay;

use lash_rlm_types::RlmExecutedCall;

use lash_rlm_types::CellOutcome;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct RlmReasoningPart {
    pub(super) text: String,
    pub(super) replay: Option<ProviderReasoningReplay>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct RlmDriverState {
    #[serde(default)]
    pub(super) reasoning: Vec<RlmReasoningPart>,
    pub(super) prose: String,
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

#[expect(
    clippy::expect_used,
    reason = "the driver state is a crate-owned struct, so serde_json encoding cannot fail"
)]
pub(super) fn rlm_driver_state(state: RlmDriverState) -> lash_core::ProtocolDriverState {
    lash_core::ProtocolDriverState::new(
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        serde_json::to_value(state).expect("RLM driver state must serialize"),
    )
}

pub(super) fn decode_rlm_driver_state(
    state: lash_core::ProtocolDriverState,
) -> Result<RlmDriverState, String> {
    if state.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
        return Err(format!(
            "driver state belongs to plugin `{}`, expected `{}`",
            state.plugin_id,
            crate::plugin::RLM_PROTOCOL_PLUGIN_ID
        ));
    }
    serde_json::from_value(state.payload)
        .map_err(|err| format!("invalid RLM driver state payload: {err}"))
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
            let encoded = rlm_driver_state(state);
            payloads.push(encoded.payload.clone());
            let restored = decode_rlm_driver_state(encoded).unwrap();
            assert_eq!(restored.outcome.as_ref(), outcome.as_ref());
        }
        assert_ne!(
            payloads[0], payloads[1],
            "null finish and running need distinct parked bytes"
        );
        let mut encoded = rlm_driver_state(RlmDriverState::default());
        let old_state = &mut encoded.payload;
        old_state.as_object_mut().unwrap().remove("outcome");
        old_state["error"] = serde_json::Value::Null;
        old_state["terminal_finish"] = serde_json::Value::Null;
        assert!(
            decode_rlm_driver_state(encoded).is_err(),
            "ambiguous old parked bytes must be recreated"
        );
    }

    #[test]
    fn legacy_string_reasoning_driver_state_is_rejected() {
        let mut payload = serde_json::to_value(RlmDriverState::default())
            .expect("default RLM driver state serializes");
        payload["reasoning"] = serde_json::json!("legacy parked reasoning");

        let Err(error) = serde_json::from_value::<RlmDriverState>(payload) else {
            panic!("legacy string reasoning must be rejected");
        };
        assert!(error.to_string().contains("invalid type: string"));
    }
}
