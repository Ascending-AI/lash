use lash_core::AttachmentRef;
use lash_core::llm::types::ProviderReasoningReplay;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct RlmReasoningPart {
    pub(super) text: String,
    pub(super) replay: Option<ProviderReasoningReplay>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct RlmDriverState {
    #[serde(default, deserialize_with = "deserialize_reasoning")]
    pub(super) reasoning: Vec<RlmReasoningPart>,
    pub(super) prose: String,
    pub(super) images: Vec<AttachmentRef>,
    /// One entry per `print` from the executed lashlang block (plus any
    /// raw stdout-style emission). Replaces the old split between a
    /// concatenated `combined_output: String` and a sibling
    /// `observations: Vec<String>` — the two carried the same content.
    pub(super) output: Vec<String>,
    pub(super) exec_error: Option<String>,
    pub(super) executed_code: Option<String>,
    pub(super) terminal_finish: Option<Value>,
}

fn deserialize_reasoning<'de, D>(deserializer: D) -> Result<Vec<RlmReasoningPart>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ReasoningWire {
        Legacy(String),
        Parts(Vec<RlmReasoningPart>),
    }

    match ReasoningWire::deserialize(deserializer)? {
        ReasoningWire::Legacy(text) => Ok(vec![RlmReasoningPart { text, replay: None }]),
        ReasoningWire::Parts(parts) => Ok(parts),
    }
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
    fn legacy_string_reasoning_driver_state_decodes_as_one_part() {
        let mut payload = serde_json::to_value(RlmDriverState::default())
            .expect("default RLM driver state serializes");
        payload["reasoning"] = serde_json::json!("legacy parked reasoning");

        let decoded: RlmDriverState =
            serde_json::from_value(payload).expect("legacy RLM driver state decodes");

        assert_eq!(
            decoded.reasoning,
            vec![RlmReasoningPart {
                text: "legacy parked reasoning".to_string(),
                replay: None,
            }]
        );
    }
}
