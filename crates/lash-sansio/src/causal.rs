use crate::{EffectAddress, ProcessId, SessionId, TurnId};
use serde::{Deserialize, Serialize};

/// Stable semantic reference to the runtime fact that caused another fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CausalRef {
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Effect {
        address: EffectAddress,
    },
    ToolCall {
        session_id: SessionId,
        call_id: String,
    },
    Process {
        process_id: ProcessId,
    },
    ProcessEvent {
        process_id: ProcessId,
        sequence: u64,
    },
    TriggerOccurrence {
        occurrence_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_incarnation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_revision: Option<u64>,
    },
    SessionNode {
        session_id: SessionId,
        node_id: String,
    },
}
