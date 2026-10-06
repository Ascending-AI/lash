//! What the store writes: event payloads, activity identities and the
//! doorbells it rings on its notification channel.

use std::sync::Arc;

use lash_core::transcript::TranscriptRowRecord;
use lash_core::{
    LiveReplayEventDraft, LiveReplayStoreError, ProcessId, SessionCursor, SessionObservationEvent,
    SessionObservationEventPayload, SessionProcessEventKind, SessionQueueEventKind,
    SessionRevision, TurnActivity, TurnId,
};
use lash_sansio::SessionId;

/// The stored form of [`SessionObservationEventPayload`], one variant each.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredPayload {
    TurnActivity {
        activity: Box<TurnActivity>,
    },
    Committed {
        base_revision: SessionRevision,
        rows: Vec<TranscriptRowRecord>,
    },
    ResidentChanged,
    AgentFrameSwitched {
        frame_id: String,
    },
    QueueChanged {
        queue: SessionQueueEventKind,
        batch_ids: Vec<String>,
    },
    ProcessChanged {
        process: SessionProcessEventKind,
        process_ids: Vec<ProcessId>,
    },
}

fn codec_error(context: &str, error: impl std::fmt::Display) -> LiveReplayStoreError {
    LiveReplayStoreError::Store(format!("postgres live replay {context}: {error}"))
}

/// How a draft's activity deduplicates (FIG-3753, FIG-5098): by the
/// ordinals of its replay key it covers, or exactly when its id names no
/// observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Identity {
    Opaque(String),
    Span { key: String, first: u32, last: u32 },
}

/// A draft ready to write: its encoded payload, identity and charge.
pub(super) struct EncodedDraft {
    pub(super) turn_id: Option<TurnId>,
    pub(super) payload: SessionObservationEventPayload,
    pub(super) bytes: Vec<u8>,
    pub(super) identity: Option<Identity>,
    /// The bytes this event counts against its session's retention: its
    /// encoded payload, its turn id, and a fixed share for its cursor and
    /// row.
    pub(super) charge: u64,
}

/// What every stored event is charged beyond its payload and turn id.
const ROW_CHARGE: u64 = 192;

pub(super) fn encode(
    session_id: &SessionId,
    draft: LiveReplayEventDraft,
) -> Result<EncodedDraft, LiveReplayStoreError> {
    let stored = match &draft.payload {
        SessionObservationEventPayload::TurnActivity(activity) => StoredPayload::TurnActivity {
            activity: Box::new(activity.clone()),
        },
        SessionObservationEventPayload::Committed {
            base_revision,
            rows,
        } => StoredPayload::Committed {
            base_revision: *base_revision,
            rows: rows.clone(),
        },
        SessionObservationEventPayload::ResidentChanged => StoredPayload::ResidentChanged,
        SessionObservationEventPayload::AgentFrameSwitched { frame_id } => {
            StoredPayload::AgentFrameSwitched {
                frame_id: frame_id.clone(),
            }
        }
        SessionObservationEventPayload::QueueChanged { kind, batch_ids } => {
            StoredPayload::QueueChanged {
                queue: *kind,
                batch_ids: batch_ids.clone(),
            }
        }
        SessionObservationEventPayload::ProcessChanged { kind, process_ids } => {
            StoredPayload::ProcessChanged {
                process: *kind,
                process_ids: process_ids.clone(),
            }
        }
    };
    let bytes = serde_json::to_vec(&stored).map_err(|error| codec_error("encode", error))?;
    let identity = match &draft.payload {
        SessionObservationEventPayload::TurnActivity(activity) => {
            Some(match activity.id.observed_span() {
                Some((key, ordinals)) => Identity::Span {
                    key: key.to_string(),
                    first: *ordinals.start(),
                    last: *ordinals.end(),
                },
                None => Identity::Opaque(activity.id.0.to_string()),
            })
        }
        _ => None,
    };
    let charge = ROW_CHARGE
        + bytes.len() as u64
        + session_id.len() as u64
        + draft.turn_id.as_ref().map_or(0, |turn| turn.len() as u64);
    Ok(EncodedDraft {
        turn_id: draft.turn_id,
        payload: draft.payload,
        bytes,
        identity,
        charge,
    })
}

/// The event a stored row holds.
pub(super) fn decode(
    cursor: SessionCursor,
    turn_id: Option<String>,
    payload: &[u8],
) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError> {
    let stored: StoredPayload =
        serde_json::from_slice(payload).map_err(|error| codec_error("decode", error))?;
    let payload = match stored {
        StoredPayload::TurnActivity { activity } => {
            SessionObservationEventPayload::TurnActivity(*activity)
        }
        StoredPayload::Committed {
            base_revision,
            rows,
        } => SessionObservationEventPayload::Committed {
            base_revision,
            rows,
        },
        StoredPayload::ResidentChanged => SessionObservationEventPayload::ResidentChanged,
        StoredPayload::AgentFrameSwitched { frame_id } => {
            SessionObservationEventPayload::AgentFrameSwitched { frame_id }
        }
        StoredPayload::QueueChanged { queue, batch_ids } => {
            SessionObservationEventPayload::QueueChanged {
                kind: queue,
                batch_ids,
            }
        }
        StoredPayload::ProcessChanged {
            process,
            process_ids,
        } => SessionObservationEventPayload::ProcessChanged {
            kind: process,
            process_ids,
        },
    };
    let turn_id = turn_id
        .map(TurnId::parse)
        .transpose()
        .map_err(|error| codec_error("decode turn id", error))?;
    SessionObservationEvent::new(turn_id, cursor, payload)
        .map(Arc::new)
        .map_err(LiveReplayStoreError::from)
}

/// One change a replica tells the others about. Notifications are only a
/// doorbell: a replica that misses one re-reads the table.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "k")]
pub(super) enum Doorbell {
    /// Positions `first..=last` of `session` were published at `revision`,
    /// in the generation that starts above `floor`.
    #[serde(rename = "p")]
    Published {
        #[serde(rename = "s")]
        session: String,
        #[serde(rename = "f")]
        floor: u64,
        #[serde(rename = "a")]
        first: u64,
        #[serde(rename = "b")]
        last: u64,
        #[serde(rename = "r")]
        revision: u64,
    },
    /// `session`'s continuity was invalidated: a new generation starts above
    /// `floor`.
    #[serde(rename = "i")]
    Invalidated {
        #[serde(rename = "s")]
        session: String,
        #[serde(rename = "f")]
        floor: u64,
    },
    /// Retention dropped `session`'s events below `first_retained`.
    #[serde(rename = "t")]
    Trimmed {
        #[serde(rename = "s")]
        session: String,
        #[serde(rename = "a")]
        first_retained: u64,
    },
    /// Idle `sessions` were forgotten; a session without a head starts
    /// above `watermark`.
    #[serde(rename = "d")]
    Forgotten {
        #[serde(rename = "s")]
        sessions: Vec<String>,
        #[serde(rename = "w")]
        watermark: u64,
    },
    /// The history was lost and `incarnation` replaces it.
    #[serde(rename = "x")]
    Rotated {
        #[serde(rename = "i")]
        incarnation: String,
    },
}

fn too_long() -> LiveReplayStoreError {
    codec_error(
        "doorbell",
        "a session id too long for a notification payload",
    )
}

/// PostgreSQL refuses a notification payload of 8000 bytes or more.
const PAYLOAD_LIMIT: usize = 7_900;

/// Pack `doorbells` into JSON-array payloads under the payload limit.
pub(super) fn pack_doorbells(doorbells: &[Doorbell]) -> Result<Vec<String>, LiveReplayStoreError> {
    let mut payloads = Vec::new();
    let mut current = String::from("[");
    for doorbell in doorbells {
        let encoded =
            serde_json::to_string(doorbell).map_err(|error| codec_error("doorbell", error))?;
        if encoded.len() + 2 > PAYLOAD_LIMIT {
            // A forgotten-session list too long to ring is split; any other
            // doorbell outgrows the limit only with a session id of
            // thousands of bytes, which the store refuses.
            let Doorbell::Forgotten {
                sessions,
                watermark,
            } = doorbell
            else {
                return Err(too_long());
            };
            if sessions.len() < 2 {
                return Err(too_long());
            }
            let (left, right) = sessions.split_at(sessions.len() / 2);
            for half in [left, right] {
                payloads.extend(pack_doorbells(&[Doorbell::Forgotten {
                    sessions: half.to_vec(),
                    watermark: *watermark,
                }])?);
            }
            continue;
        }
        if current.len() + encoded.len() + 2 > PAYLOAD_LIMIT {
            current.push(']');
            payloads.push(std::mem::replace(&mut current, String::from("[")));
        }
        if current.len() > 1 {
            current.push(',');
        }
        current.push_str(&encoded);
    }
    if current.len() > 1 {
        current.push(']');
        payloads.push(current);
    }
    Ok(payloads)
}

/// The doorbells one notification payload carries; a payload this store
/// did not write rings nothing.
pub(super) fn unpack_doorbells(payload: &str) -> Vec<Doorbell> {
    serde_json::from_str(payload).unwrap_or_else(|error| {
        tracing::warn!(%error, "ignoring a live replay notification this store cannot read");
        Vec::new()
    })
}
