use crate::{ToolIntentIdentity, ToolIntentKind, ToolIntentRefusalReason};
use serde::{Deserialize, Serialize};

/// The only intent-to-command protocol understood by this build.
pub const TOOL_INTENT_PROTOCOL_V1: u16 = 1;
/// Maximum declarations accepted from one recorded attempt.
pub const TOOL_INTENT_MAX_COUNT: usize = 32;
/// Maximum canonical JSON bytes accepted from one recorded intent batch.
pub const TOOL_INTENT_MAX_CANONICAL_BYTES: usize = 64 * 1024;
/// Maximum declarations of any one kind accepted from one recorded attempt.
pub const TOOL_INTENT_MAX_PER_KIND: usize = 16;

/// Recorded declarations returned by a leaf tool attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolIntents {
    pub protocol_version: u16,
    pub intents: Vec<ToolIntent>,
}

impl Default for ToolIntents {
    fn default() -> Self {
        Self::v1(Vec::new())
    }
}

impl ToolIntents {
    pub fn v1(intents: Vec<ToolIntent>) -> Self {
        Self {
            protocol_version: TOOL_INTENT_PROTOCOL_V1,
            intents,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }
}

/// Durable follow-on work a recorded leaf attempt may request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "intent", rename_all = "snake_case")]
pub enum ToolIntent {
    StartProcess(Box<StartProcessIntent>),
    SignalProcess(SignalProcessIntent),
    CancelProcess(CancelProcessIntent),
    EmitProcessEvent(EmitProcessEventIntent),
}

impl ToolIntent {
    pub fn kind(&self) -> ToolIntentKind {
        match self {
            Self::StartProcess(_) => ToolIntentKind::StartProcess,
            Self::SignalProcess(_) => ToolIntentKind::SignalProcess,
            Self::CancelProcess(_) => ToolIntentKind::CancelProcess,
            Self::EmitProcessEvent(_) => ToolIntentKind::EmitProcessEvent,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::StartProcess(intent) => &intent.session_id,
            Self::SignalProcess(intent) => &intent.session_id,
            Self::CancelProcess(intent) => &intent.session_id,
            Self::EmitProcessEvent(intent) => &intent.session_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessParentEndPolicy {
    Abandon,
    Cancel,
    Terminate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartProcessIntent {
    pub session_id: String,
    pub request: crate::ProcessStartRequest,
    pub on_parent_end: ProcessParentEndPolicy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignalProcessIntent {
    pub session_id: String,
    pub process_id: String,
    pub signal_name: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelProcessIntent {
    pub session_id: String,
    pub process_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmitProcessEventIntent {
    pub session_id: String,
    pub process_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
}

/// The single identity seam for the v1 protocol.
pub fn derive_tool_intent_identity(
    session_id: &str,
    turn_id: &str,
    tool_call_id: Option<&str>,
    intent_index: usize,
) -> Result<ToolIntentIdentity, ToolIntentRefusalReason> {
    let tool_call_id = tool_call_id.ok_or(ToolIntentRefusalReason::MissingToolCallId)?;
    let intent_index =
        u32::try_from(intent_index).map_err(|_| ToolIntentRefusalReason::IntentIndexOverflow)?;

    // FIG-1203 rebase point: replace these current-tree components with the
    // frame-key-grade stable call identity supplied by FIG-1203.
    let mut encoder = crate::stable_identity::IdentityEncoder::new("lash.tool-intent", 1);
    encoder.string(session_id);
    encoder.string(turn_id);
    encoder.string(tool_call_id);
    encoder.u32(intent_index);
    let replay_key = crate::stable_identity::rendered_hash("tool-intent", 1, &encoder.finish());
    Ok(ToolIntentIdentity {
        session_id: session_id.to_string(),
        turn_id: turn_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        intent_index,
        replay_key,
    })
}

/// Value returned by a leaf provider body before the attempt effect records it.
#[derive(Clone, Debug)]
pub struct ToolAttemptResult {
    pub result: crate::ToolResult,
    pub intents: ToolIntents,
}

impl ToolAttemptResult {
    pub fn new(result: crate::ToolResult, intents: ToolIntents) -> Self {
        Self { result, intents }
    }

    pub fn without_intents(result: crate::ToolResult) -> Self {
        Self::new(result, ToolIntents::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_identity_has_a_literal_stable_oracle() {
        let identity = derive_tool_intent_identity("session-fig1292", "turn-7", Some("call-3"), 2)
            .expect("identity");
        assert_eq!(
            identity,
            ToolIntentIdentity {
                session_id: "session-fig1292".to_string(),
                turn_id: "turn-7".to_string(),
                tool_call_id: "call-3".to_string(),
                intent_index: 2,
                replay_key: "tool-intent:v1:sha256:8cd893b79abe66a4a894753ab43053964c7bc4253a841916235b90aeedf50719".to_string(),
            }
        );
    }

    #[test]
    fn missing_call_id_is_a_typed_refusal() {
        assert_eq!(
            derive_tool_intent_identity("session", "turn", None, 0),
            Err(ToolIntentRefusalReason::MissingToolCallId)
        );
    }
}
