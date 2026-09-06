//! Typed queued-work activity payloads.

use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum RemoteMessageRole {
    User,
    Assistant,
    System,
    Event,
}

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
        process_id: String,
        event_type: String,
        sequence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caused_by: Option<RemoteCausalRef>,
    },
    TurnInput {
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<String>,
    },
    TurnOutput {
        turn_id: String,
        source: RemoteTurnOutputSource,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePart {
    pub id: String,
    pub kind: RemotePartKind,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<RemotePartAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_replay: Option<RemoteProviderReplayMeta>,
    pub prune_state: RemotePruneState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_meta: Option<RemoteProviderReasoningReplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_meta: Option<RemoteResponseTextMeta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum RemotePartKind {
    Text,
    Attachment,
    Code,
    Output,
    Error,
    Prose,
    ToolCall,
    ToolResult,
    Reasoning,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePartAttachment {
    pub source: RemoteAttachmentSource,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum RemotePruneState {
    Intact,
    Cleared,
    Deleted {
        breadcrumb: String,
        archive_hash: String,
    },
    Summarized {
        summary: String,
        archive_hash: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePluginMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: RemoteMessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RemoteMessageOrigin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<RemotePart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<RemoteAttachmentSource>,
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
pub enum RemoteQueuedWorkClaimBoundary {
    ActiveTurnCheckpoint,
    Idle,
}

#[cfg(test)]
#[path = "queued_events_tests.rs"]
mod tests;
