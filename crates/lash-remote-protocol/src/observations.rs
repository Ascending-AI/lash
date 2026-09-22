//! Session observation: cursors, resumable observation events, and live
//! replay gap envelopes.

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::usage_activity::{RemoteTurnActivity, RemoteUsage};

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
    pub turn_id: TurnId,
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
    pub cursor: String,
}

impl RemoteSessionCursor {
    pub fn new(cursor: impl Into<String>) -> Self {
        Self {
            cursor: cursor.into(),
        }
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteSessionCursor", "cursor", &self.cursor)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSessionObservation {
    pub session_id: SessionId,
    pub cursor: String,
    pub turn_index: u64,
    pub usage: RemoteUsage,
}

impl RemoteSessionObservation {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteSessionObservation", "session_id", &self.session_id)?;
        require_non_empty("RemoteSessionObservation", "cursor", &self.cursor)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSessionObservationEvent {
    pub session_id: SessionId,
    pub replay_incarnation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub revision: u64,
    pub cursor: String,
    #[serde(flatten)]
    pub event: RemoteSessionObservationEventPayload,
}

impl RemoteSessionObservationEvent {
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    /// Decodes one JSON observation after refusing a mismatched protocol
    /// version, before the flattened event vocabulary is deserialized.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        Self::decode_json_expecting_protocol_version(bytes, crate::REMOTE_PROTOCOL_VERSION)
    }

    pub(crate) fn decode_json_expecting_protocol_version(
        bytes: &[u8],
        expected_version: u32,
    ) -> Result<Self, RemoteProtocolError> {
        let event = crate::Envelope::<Self>::decode_json_expecting_protocol_version(
            bytes,
            expected_version,
        )?
        .into_body();
        event.validate()?;
        Ok(event)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty(
            "RemoteSessionObservationEvent",
            "session_id",
            &self.session_id,
        )?;
        require_non_empty(
            "RemoteSessionObservationEvent",
            "replay_incarnation_id",
            &self.replay_incarnation_id,
        )?;
        if let Some(turn_id) = self.turn_id.as_deref() {
            require_non_empty("RemoteSessionObservationEvent", "turn_id", turn_id)?;
        }
        require_non_empty("RemoteSessionObservationEvent", "cursor", &self.cursor)?;
        if let RemoteSessionObservationEventPayload::TurnActivity { activity } = &self.event {
            activity.validate()?;
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
    ResidentChanged,
    AgentFrameSwitched {
        frame_id: String,
    },
    QueueChanged {
        kind: RemoteSessionQueueEventKind,
        batch_ids: Vec<String>,
    },
    ProcessChanged {
        kind: RemoteSessionProcessEventKind,
        process_ids: Vec<ProcessId>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionQueueEventKind {
    Enqueued,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteSessionProcessEventKind {
    Started { sequence: u64 },
    Waiting { sequence: u64 },
    Resumed { sequence: u64 },
    CancelRequested { sequence: u64 },
    AbandonRequested { sequence: u64 },
    CallerDeparted { sequence: u64 },
    Completed { sequence: u64 },
    Failed { sequence: u64 },
    Cancelled { sequence: u64 },
    Abandoned { sequence: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLiveReplayGap {
    pub session_id: SessionId,
    pub requested_cursor: String,
    pub latest_cursor: String,
    pub latest_revision: u64,
    pub reason: RemoteLiveReplayGapReason,
}

impl RemoteLiveReplayGap {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
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

/// Start a process-scoped observation subscription at one publisher position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessObservationRequest {
    pub process_id: ProcessId,
    pub incarnation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl RemoteProcessObservationRequest {
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let request = crate::Envelope::<Self>::decode_json(bytes)?.into_body();
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty(
            "RemoteProcessObservationRequest",
            "process_id",
            &self.process_id,
        )?;
        if self.incarnation == 0 {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessObservationRequest",
                message: "incarnation must be greater than zero".to_string(),
            });
        }
        if let Some(cursor) = &self.cursor {
            require_non_empty("RemoteProcessObservationRequest", "cursor", cursor)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessObservationGapReason {
    Overflow,
    Expired,
    SubscriberLagged,
    PublisherReplaced,
    RoutingUnavailable,
    ProcessIdReused,
    CrossProcess,
    InvalidCursor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteProcessObservationCompleteness {
    Complete,
    Incomplete {
        reason: RemoteProcessObservationGapReason,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessObservationProjection {
    #[schemars(with = "Option<serde_json::Value>")]
    pub graph: Option<lash_trace::TraceLashlangGraph>,
    pub completeness: RemoteProcessObservationCompleteness,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteProcessObservationItem {
    Snapshot {
        process_id: ProcessId,
        incarnation: u64,
        cursor: String,
        projection: RemoteProcessObservationProjection,
    },
    Event {
        process_id: ProcessId,
        incarnation: u64,
        cursor: String,
        #[schemars(with = "serde_json::Value")]
        record: Box<lash_trace::TraceRecord>,
    },
    Gap {
        process_id: ProcessId,
        incarnation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_cursor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latest_cursor: Option<String>,
        projection: RemoteProcessObservationProjection,
        reason: RemoteProcessObservationGapReason,
    },
}

impl RemoteProcessObservationItem {
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let item = crate::Envelope::<Self>::decode_json(bytes)?.into_body();
        item.validate()?;
        Ok(item)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        let (process_id, incarnation, cursor) = match self {
            Self::Snapshot {
                process_id,
                incarnation,
                cursor,
                ..
            }
            | Self::Event {
                process_id,
                incarnation,
                cursor,
                ..
            } => (process_id, incarnation, Some(cursor.as_str())),
            Self::Gap {
                process_id,
                incarnation,
                latest_cursor,
                ..
            } => (process_id, incarnation, latest_cursor.as_deref()),
        };
        require_non_empty("RemoteProcessObservationItem", "process_id", process_id)?;
        if *incarnation == 0 {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessObservationItem",
                message: "incarnation must be greater than zero".to_string(),
            });
        }
        if let Some(cursor) = cursor {
            require_non_empty("RemoteProcessObservationItem", "cursor", cursor)?;
        }
        match self {
            Self::Event { record, .. }
                if !matches!(
                    &record.event,
                    lash_trace::TraceEvent::LanguageExecution { language, event }
                        if language == "lashlang" && matches!(
                            &event.identity.subject,
                            lash_trace::TraceRuntimeSubject::Process { process_id: observed }
                                if observed == process_id
                        ) && event.identity.incarnation() == Some(*incarnation)
                ) =>
            {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessObservationItem",
                    message: "node event belongs to another process incarnation".to_string(),
                });
            }
            Self::Snapshot { projection, .. }
                if projection.graph.is_none()
                    || projection.completeness
                        != RemoteProcessObservationCompleteness::Complete =>
            {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessObservationItem",
                    message: "snapshot requires a complete graph".to_string(),
                });
            }
            Self::Gap {
                projection, reason, ..
            } if projection.completeness
                != (RemoteProcessObservationCompleteness::Incomplete { reason: *reason }) =>
            {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessObservationItem",
                    message: "gap projection must name the same incompleteness reason".to_string(),
                });
            }
            _ => {}
        }
        let projection = match self {
            Self::Snapshot { projection, .. } | Self::Gap { projection, .. } => Some(projection),
            Self::Event { .. } => None,
        };
        if projection
            .and_then(|projection| projection.graph.as_ref())
            .is_some_and(|graph| {
                !matches!(&graph.subject,
                lash_trace::TraceRuntimeSubject::Process { process_id: observed }
                    if observed == process_id)
            })
        {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessObservationItem",
                message: "graph belongs to another process".to_string(),
            });
        }
        Ok(())
    }
}
