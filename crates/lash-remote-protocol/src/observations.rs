//! Session observation: cursors, resumable observation events, and live
//! replay gap envelopes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::usage_activity::{RemoteTurnActivity, RemoteUsage};
use crate::{REMOTE_PROTOCOL_VERSION, ensure_protocol_version};

/// Stable identity proving an admitted input became canonical turn input.
///
/// This wire DTO intentionally contains no display text. `input_id` and
/// `source_key` correlate to the admission receipt, while `turn_id` and
/// `committed_message_id` identify the canonical conversation application.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnInputApplication {
    pub input_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub turn_id: String,
    pub committed_message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<RemoteTurnInputCheckpoint>,
}

impl RemoteTurnInputApplication {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteTurnInputApplication", "input_id", &self.input_id)?;
        require_non_empty("RemoteTurnInputApplication", "turn_id", &self.turn_id)?;
        require_non_empty(
            "RemoteTurnInputApplication",
            "committed_message_id",
            &self.committed_message_id,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTurnInputCheckpoint {
    AfterWork,
    BeforeCompletion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSessionCursor {
    pub protocol_version: u32,
    pub cursor: String,
}

impl RemoteSessionCursor {
    pub fn new(cursor: impl Into<String>) -> Self {
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            cursor: cursor.into(),
        }
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteSessionCursor", "cursor", &self.cursor)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSessionObservation {
    pub protocol_version: u32,
    pub session_id: String,
    pub cursor: String,
    pub turn_index: u64,
    pub usage: RemoteUsage,
}

impl RemoteSessionObservation {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteSessionObservation", "session_id", &self.session_id)?;
        require_non_empty("RemoteSessionObservation", "cursor", &self.cursor)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSessionObservationEvent {
    pub protocol_version: u32,
    pub session_id: String,
    pub revision: u64,
    pub cursor: String,
    #[serde(flatten)]
    pub event: RemoteSessionObservationEventPayload,
}

impl RemoteSessionObservationEvent {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty(
            "RemoteSessionObservationEvent",
            "session_id",
            &self.session_id,
        )?;
        require_non_empty("RemoteSessionObservationEvent", "cursor", &self.cursor)?;
        if let RemoteSessionObservationEventPayload::TurnActivity { activity } = &self.event {
            activity.validate()?;
            if activity.protocol_version != self.protocol_version {
                return Err(RemoteProtocolError::MismatchedNestedProtocolVersion {
                    parent: "RemoteSessionObservationEvent",
                    child: "activity",
                    parent_version: self.protocol_version,
                    child_version: activity.protocol_version,
                });
            }
            if let crate::usage_activity::RemoteTurnEvent::TurnInputApplied { applications } =
                &activity.event
            {
                for application in applications {
                    application.validate()?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteSessionObservationEventPayload {
    TurnActivity {
        activity: Box<RemoteTurnActivity>,
    },
    Committed,
    AgentFrameSwitched {
        frame_id: String,
    },
    QueueChanged {
        kind: RemoteSessionQueueEventKind,
        batch_ids: Vec<String>,
    },
    ProcessChanged {
        kind: RemoteSessionProcessEventKind,
        process_ids: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionQueueEventKind {
    Enqueued,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionProcessEventKind {
    Started,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLiveReplayGap {
    pub protocol_version: u32,
    pub session_id: String,
    pub requested_cursor: String,
    pub latest_cursor: String,
    pub latest_revision: u64,
    pub reason: RemoteLiveReplayGapReason,
}

impl RemoteLiveReplayGap {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteLiveReplayGap", "session_id", &self.session_id)?;
        require_non_empty(
            "RemoteLiveReplayGap",
            "requested_cursor",
            &self.requested_cursor,
        )?;
        require_non_empty("RemoteLiveReplayGap", "latest_cursor", &self.latest_cursor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteLiveReplayGapReason {
    Trimmed,
    Unavailable,
}
