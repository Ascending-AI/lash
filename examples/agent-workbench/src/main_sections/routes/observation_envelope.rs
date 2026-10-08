//! The workbench's JSON projection of the local observation feed.

use super::*;
use lash::observe::{SessionObservationEvent, SessionObservationEventPayload};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ObservationEnvelope<T> {
    #[serde(flatten)]
    pub(crate) body: T,
}

impl<T> ObservationEnvelope<T> {
    pub(crate) fn new(body: T) -> Self {
        Self { body }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ObservationEvent {
    pub(crate) session_id: SessionId,
    pub(crate) replay_incarnation_id: String,
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) revision: u64,
    pub(crate) cursor: String,
    #[serde(flatten)]
    pub(crate) event: ObservationPayload,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ObservationPayload {
    TurnActivity {
        activity: Box<ObservationActivity>,
    },
    Committed {
        base_revision: u64,
        rows: Vec<crate::ChatRow>,
    },
    ResidentChanged,
    AgentFrameSwitched {
        frame_id: String,
    },
    QueueChanged {
        kind: lash::observe::SessionQueueEventKind,
        batch_ids: Vec<String>,
    },
    ProcessChanged {
        kind: lash::observe::SessionProcessEventKind,
        process_ids: Vec<ProcessId>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ObservationActivity {
    pub(crate) sequence: u64,
    pub(crate) id: String,
    pub(crate) correlation_id: String,
    #[serde(flatten)]
    pub(crate) event: lash::TurnEvent,
}

impl ObservationEvent {
    pub(crate) fn from_core(sequence: u64, event: &SessionObservationEvent) -> Self {
        let payload = match &event.payload {
            SessionObservationEventPayload::TurnActivity(activity) => {
                ObservationPayload::TurnActivity {
                    activity: Box::new(ObservationActivity {
                        sequence,
                        id: activity.id.0.to_string(),
                        correlation_id: activity.correlation_id.0.to_string(),
                        event: activity.event.clone(),
                    }),
                }
            }
            SessionObservationEventPayload::Committed {
                base_revision,
                entries,
            } => ObservationPayload::Committed {
                base_revision: base_revision.as_u64(),
                rows: crate::ChatRow::all(entries),
            },
            SessionObservationEventPayload::ResidentChanged => ObservationPayload::ResidentChanged,
            SessionObservationEventPayload::AgentFrameSwitched { frame_id, .. } => {
                ObservationPayload::AgentFrameSwitched {
                    frame_id: frame_id.clone(),
                }
            }
            SessionObservationEventPayload::QueueChanged { kind, batch_ids } => {
                ObservationPayload::QueueChanged {
                    kind: *kind,
                    batch_ids: batch_ids.clone(),
                }
            }
            SessionObservationEventPayload::ProcessChanged { kind, process_ids } => {
                ObservationPayload::ProcessChanged {
                    kind: *kind,
                    process_ids: process_ids.clone(),
                }
            }
        };
        Self {
            session_id: event.session_id(),
            replay_incarnation_id: event.replay_incarnation_id().to_string(),
            turn_id: event.turn_id.clone(),
            revision: event.revision().as_u64(),
            cursor: event.cursor.to_string(),
            event: payload,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ObservationSnapshot {
    pub(crate) session_id: SessionId,
    pub(crate) cursor: String,
    pub(crate) turn_index: u64,
    pub(crate) usage: lash::usage::LlmUsage,
}

impl From<lash::observe::SessionObservation> for ObservationSnapshot {
    fn from(value: lash::observe::SessionObservation) -> Self {
        Self {
            session_id: value.read_view.session_id().clone(),
            cursor: value.cursor.to_string(),
            turn_index: value.read_view.turn_index() as u64,
            usage: value.read_view.token_usage().clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ObservationGap {
    pub(crate) session_id: SessionId,
    pub(crate) requested_cursor: String,
    pub(crate) latest_cursor: String,
    pub(crate) latest_revision: u64,
    pub(crate) reason: lash::observe::LiveReplayGapReason,
}

impl From<lash::observe::LiveReplayGap> for ObservationGap {
    fn from(value: lash::observe::LiveReplayGap) -> Self {
        Self {
            session_id: value.session_id,
            requested_cursor: value.requested_cursor.to_string(),
            latest_cursor: value.latest_cursor.to_string(),
            latest_revision: value.latest_revision.as_u64(),
            reason: value.reason,
        }
    }
}
