use lash_core::llm::types::ProviderReasoningReplay;
use lash_core::{AttachmentRef, CellFailure};
use serde_json::Value;

use lash_rlm_types::RlmExecutedCall;

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
    pub(super) output: Vec<String>,
    pub(super) error: Option<CellFailure>,
    pub(super) code: String,
    /// Executor-requested terminal value, pending driver validation and adjudication.
    pub(super) terminal_finish: Option<Value>,
}

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
