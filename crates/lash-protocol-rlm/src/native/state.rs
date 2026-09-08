use lash_core::{AttachmentRef, CellFailure};
use serde_json::Value;

use lash_rlm_types::RlmExecutedCall;

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
    pub(super) output: Vec<String>,
    pub(super) error: Option<CellFailure>,
    pub(super) code: String,
    /// Executor-requested terminal value, pending driver validation and adjudication.
    pub(super) terminal_finish: Option<Value>,
}

const NATIVE_DRIVER_STATE_VERSION: u32 = 2;

#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    schema_version: u32,
    state: RlmDriverState,
}

pub(super) fn rlm_driver_state(state: RlmDriverState) -> lash_core::ProtocolDriverState {
    lash_core::ProtocolDriverState::new(
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        serde_json::to_value(Envelope {
            schema_version: NATIVE_DRIVER_STATE_VERSION,
            state,
        })
        .expect("native driver state must serialize"),
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
    let envelope: Envelope = serde_json::from_value(state.payload)
        .map_err(|error| format!("invalid native driver state: {error}"))?;
    if envelope.schema_version != NATIVE_DRIVER_STATE_VERSION {
        return Err(format!(
            "unsupported native driver state version {}, expected {}",
            envelope.schema_version, NATIVE_DRIVER_STATE_VERSION
        ));
    }
    Ok(envelope.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_state_version_is_pinned_and_predecessors_are_refused() {
        let mut encoded = rlm_driver_state(RlmDriverState::default());
        assert_eq!(encoded.payload["schema_version"], 2);
        encoded.payload["schema_version"] = serde_json::json!(1);
        assert!(decode_rlm_driver_state(encoded).is_err());
        let unversioned = lash_core::ProtocolDriverState::new(
            crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
            serde_json::json!({"state":{}}),
        );
        assert!(decode_rlm_driver_state(unversioned).is_err());
    }
}
