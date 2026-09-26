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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
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

/// Start a process-scoped observation subscription for one exact process
/// lifetime, optionally resuming from a process cursor.
///
/// The cursor is typed: a malformed cursor, or one from a retired version, is
/// refused at decode, naming the version it found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessObservationRequest {
    pub process_id: ProcessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<lash_sansio::ProcessCursor>,
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
    CrossProcess,
    InvalidCursor,
    PublisherJoinedMidRun,
    IncompleteGraph,
    ProjectionTruncated,
    /// Retained live evidence cannot bridge the cursor's durable sequence to
    /// the durable high-water mark.
    SequenceUnbridged,
    /// The requested process lifetime is unknown or no longer retained.
    HistoryUnavailable,
}

/// Live-graph completeness, reported apart from durable-summary completeness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteProcessObservationCompleteness {
    Complete,
    Incomplete {
        reason: RemoteProcessObservationGapReason,
    },
}

/// The live half of a snapshot: the publisher's graph, when this core routes
/// the process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessObservationProjection {
    pub graph: Option<lash_trace::TraceLashlangGraph>,
    pub completeness: RemoteProcessObservationCompleteness,
}

/// Why a durable summary fold is not the whole history through its boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessDurableGapReason {
    /// Bounded acquisition stopped before the high-water mark.
    AcquisitionBudgetExhausted,
    /// A summary event in the history did not decode.
    SummaryUndecodable,
}

/// Durable-summary completeness, reported apart from live-graph completeness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteProcessDurableCompleteness {
    Complete,
    Incomplete {
        reason: RemoteProcessDurableGapReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessEffectOutcomeClass {
    Success,
    Failure,
    Cancelled,
}

/// One recorded effect occurrence in a durable summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessEffectOccurrence {
    pub occurrence: u64,
    pub operation: String,
    pub outcome_class: RemoteProcessEffectOutcomeClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<lash_sansio::FailureCode>,
    pub replay_key: String,
}

/// Omitted occurrences of one node, by outcome class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessEffectOmittedCounts {
    pub success: u64,
    pub failure: u64,
    pub cancelled: u64,
}

/// One effect node's recorded occurrences and omitted counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessEffectNodeSummary {
    pub node_id: String,
    pub occurrences: Vec<RemoteProcessEffectOccurrence>,
    pub omitted: RemoteProcessEffectOmittedCounts,
}

/// Why the exact lifetime a snapshot names has no durable history to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteProcessHistoryRetention {
    /// The process was pruned and its payload-free tombstone remains.
    Pruned {
        terminal_label: String,
        pruned_at_ms: u64,
    },
    /// No retained process or tombstone has this id.
    Unknown,
}

/// The durable half of a snapshot: status and the effect-summary fold
/// through one exact high-water sequence, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteProcessDurableSnapshot {
    Retained {
        sequence: u64,
        status: crate::RemoteProcessStatus,
        summary: Vec<RemoteProcessEffectNodeSummary>,
        completeness: RemoteProcessDurableCompleteness,
    },
    NoLongerRetained {
        retention: RemoteProcessHistoryRetention,
    },
}

/// A snapshot at one exact process lifetime and durable high-water sequence,
/// with the live graph at the cursor's live position.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessObservationSnapshot {
    pub durable: RemoteProcessDurableSnapshot,
    pub live: RemoteProcessObservationProjection,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteProcessObservationItem {
    Snapshot {
        process_id: ProcessId,
        cursor: lash_sansio::ProcessCursor,
        snapshot: RemoteProcessObservationSnapshot,
    },
    Event {
        process_id: ProcessId,
        cursor: lash_sansio::ProcessCursor,
        record: Box<lash_trace::TraceRecord>,
    },
    /// A durable event committed; read it through the events operation.
    Committed {
        process_id: ProcessId,
        cursor: lash_sansio::ProcessCursor,
        sequence: u64,
        event_type: String,
    },
    Gap {
        process_id: ProcessId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_cursor: Option<lash_sansio::ProcessCursor>,
        cursor: lash_sansio::ProcessCursor,
        reason: RemoteProcessObservationGapReason,
        snapshot: RemoteProcessObservationSnapshot,
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
        let invalid = |message: &str| RemoteProtocolError::InvalidEnvelope {
            type_name: "RemoteProcessObservationItem",
            message: message.to_string(),
        };
        let (process_id, cursor, snapshot) = match self {
            Self::Snapshot {
                process_id,
                cursor,
                snapshot,
            } => (process_id, cursor, Some(snapshot)),
            Self::Event {
                process_id, cursor, ..
            }
            | Self::Committed {
                process_id, cursor, ..
            } => (process_id, cursor, None),
            Self::Gap {
                process_id,
                cursor,
                snapshot,
                ..
            } => (process_id, cursor, Some(snapshot)),
        };
        if !cursor.reference().names(process_id) {
            return Err(invalid("cursor names another process lifetime"));
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
                        )
                ) =>
            {
                return Err(invalid("node event belongs to another process"));
            }
            Self::Committed { sequence, .. } if *sequence != cursor.sequence() => {
                return Err(invalid("committed item cursor must carry its sequence"));
            }
            Self::Snapshot { snapshot, .. } | Self::Gap { snapshot, .. } => {
                if let RemoteProcessDurableSnapshot::Retained { sequence, .. } = &snapshot.durable
                    && *sequence != cursor.sequence()
                {
                    return Err(invalid(
                        "snapshot cursor must carry the snapshot's durable high-water sequence",
                    ));
                }
            }
            _ => {}
        }
        if snapshot
            .and_then(|snapshot| snapshot.live.graph.as_ref())
            .is_some_and(|graph| {
                !matches!(&graph.subject,
                lash_trace::TraceRuntimeSubject::Process { process_id: observed }
                    if observed == process_id)
            })
        {
            return Err(invalid("graph belongs to another process"));
        }
        Ok(())
    }
}
