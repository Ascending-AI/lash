//! Typed queued-work activity payloads.

use crate::*;
use lash_sansio::ProcessId;
use lash_sansio::TurnId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteTurnOutputSource {
    Runtime,
    Plugin { plugin_id: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteMessageOrigin {
    Plugin {
        plugin_id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        transient: bool,
    },
    Process {
        process_id: ProcessId,
        event_type: String,
        sequence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caused_by: Option<RemoteCausalRef>,
    },
    TurnInput {
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<String>,
    },
    TurnOutput {
        turn_id: TurnId,
        source: RemoteTurnOutputSource,
    },
    /// An origin this protocol version does not model: the runtime's
    /// `MessageOrigin` is non-exhaustive, and a peer reading a newer
    /// peer's origin lands here instead of failing the whole message.
    #[serde(other)]
    Unrecognized,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnCause {
    pub id: String,
    pub event_type: String,
    pub origin: RemoteMessageOrigin,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAdmissionBoundary {
    ActiveTurnCheckpoint,
    Idle,
}

#[cfg(test)]
#[path = "queued_events_tests.rs"]
mod tests;
