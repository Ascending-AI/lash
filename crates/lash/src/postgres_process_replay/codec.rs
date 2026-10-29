//! What the store writes: event payloads, observation identities and the
//! doorbells it rings on its notification channel.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::LanguageExecutionObservation;
use lash_core::ProcessObservationCursor;
use lash_core::ProcessObservationEvent;
use lash_core::ProcessObservationEventPayload;
use lash_core::ProcessObservationIdentity;
use lash_core::ProcessReplayEventDraft;
use lash_core::ProcessReplayStoreError;
use lash_core::ProcessSequence;
use lash_execution_contracts::process::ObservedProcessEvent;
use lash_vocabulary::identity::ProcessId;

/// The stored form of [`ProcessObservationEventPayload`], one variant each:
/// what a row reads back as.
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredPayload {
    Activity {
        event_key: String,
        event: Box<lash_core::TurnEvent>,
    },
    LanguageExecution {
        observation: Box<LanguageExecutionObservation>,
    },
    StepBodyStarted {
        observation: Box<lash_core::StepBodyStartedObservation>,
    },
    Committed {
        event: Box<ObservedProcessEvent>,
    },
}

/// [`StoredPayload`] borrowed from the payload a draft carries: what a row
/// is written from, without copying the payload the publication still
/// answers with.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredPayloadView<'a> {
    Activity {
        event_key: &'a str,
        event: &'a lash_core::TurnEvent,
    },
    LanguageExecution {
        observation: &'a LanguageExecutionObservation,
    },
    StepBodyStarted {
        observation: &'a lash_core::StepBodyStartedObservation,
    },
    Committed {
        #[serde(serialize_with = "serialize_event")]
        event: &'a ObservedProcessEvent,
    },
}

impl<'a> StoredPayloadView<'a> {
    fn of(payload: &'a ProcessObservationEventPayload) -> Self {
        match payload {
            ProcessObservationEventPayload::Activity { event_key, event } => {
                Self::Activity { event_key, event }
            }
            ProcessObservationEventPayload::LanguageExecution(observation) => {
                Self::LanguageExecution { observation }
            }
            ProcessObservationEventPayload::StepBodyStarted(observation) => {
                Self::StepBodyStarted { observation }
            }
            ProcessObservationEventPayload::Committed { event } => Self::Committed { event },
        }
    }
}

fn serialize_event<S: serde::Serializer>(
    event: &&ObservedProcessEvent,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&lash_protocol_types::Durable(*event), serializer)
}

fn codec_error(context: &str, error: impl std::fmt::Display) -> ProcessReplayStoreError {
    ProcessReplayStoreError::Store(format!("postgres process replay {context}: {error}"))
}

/// A draft ready to write: its encoded payload, identity and charge.
pub(super) struct EncodedDraft {
    pub(super) sequence: ProcessSequence,
    pub(super) payload: ProcessObservationEventPayload,
    pub(super) bytes: Vec<u8>,
    pub(super) identity: ProcessObservationIdentity,
    /// The identity as the dedupe table keys it.
    pub(super) key: String,
    /// The bytes this event counts against its process's retention: its
    /// encoded payload, its identity, and a fixed share for its cursor and
    /// rows.
    pub(super) charge: u64,
}

/// What every stored event is charged beyond its payload and identity.
const ROW_CHARGE: u64 = 192;

/// An identity as the dedupe table keys it: a committed fact by its
/// sequence, a provisional observation by its producer's event key.
fn identity_key(identity: &ProcessObservationIdentity) -> String {
    match identity {
        ProcessObservationIdentity::Activity { event_key } => format!("r:{event_key}"),
        ProcessObservationIdentity::Committed { sequence } => format!("c:{}", sequence.as_u64()),
        ProcessObservationIdentity::LanguageExecution { event_key } => format!("l:{event_key}"),
        ProcessObservationIdentity::StepBodyStarted { event_key } => format!("s:{event_key}"),
    }
}

pub(super) fn encode(
    process_id: &ProcessId,
    draft: ProcessReplayEventDraft,
) -> Result<EncodedDraft, ProcessReplayStoreError> {
    let sequence = draft.sequence();
    let payload = draft.into_payload();
    let bytes = serde_json::to_vec(&StoredPayloadView::of(&payload))
        .map_err(|error| codec_error("encode", error))?;
    let identity = payload.identity();
    let key = identity_key(&identity);
    let charge =
        ROW_CHARGE + bytes.len() as u64 + 2 * process_id.as_str().len() as u64 + key.len() as u64;
    Ok(EncodedDraft {
        sequence,
        payload,
        bytes,
        identity,
        key,
        charge,
    })
}

/// The payload a stored row holds.
pub(super) fn decode_payload(
    payload: &[u8],
) -> Result<ProcessObservationEventPayload, ProcessReplayStoreError> {
    let stored: StoredPayload =
        serde_json::from_slice(payload).map_err(|error| codec_error("decode", error))?;
    Ok(match stored {
        StoredPayload::Activity { event_key, event } => ProcessObservationEventPayload::Activity {
            event_key,
            event: *event,
        },
        StoredPayload::LanguageExecution { observation } => {
            ProcessObservationEventPayload::LanguageExecution(*observation)
        }
        StoredPayload::StepBodyStarted { observation } => {
            ProcessObservationEventPayload::StepBodyStarted(*observation)
        }
        StoredPayload::Committed { event } => {
            ProcessObservationEventPayload::Committed { event: *event }
        }
    })
}

/// The event a stored row holds.
pub(super) fn decode(
    cursor: ProcessObservationCursor,
    payload: &[u8],
) -> Result<Arc<ProcessObservationEvent>, ProcessReplayStoreError> {
    ProcessObservationEvent::new(cursor, decode_payload(payload)?)
        .map(Arc::new)
        .map_err(ProcessReplayStoreError::from)
}

/// What a replica tells the others changed. Notifications are only a
/// doorbell: a subscriber re-reads the table, and a replica that misses one
/// re-reads when its listener reconnects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Doorbell {
    /// Every process changed: the incarnation rotated.
    pub(super) all: bool,
    /// The processes whose windows changed.
    pub(super) processes: BTreeSet<String>,
}

impl Doorbell {
    pub(super) fn all() -> Self {
        Self {
            all: true,
            processes: BTreeSet::new(),
        }
    }

    pub(super) fn process(process: impl Into<String>) -> Self {
        Self {
            all: false,
            processes: BTreeSet::from([process.into()]),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.all && self.processes.is_empty()
    }

    /// The notification payloads that ring this doorbell, each under
    /// PostgreSQL's limit: `*` for every process, else JSON arrays of
    /// process ids.
    pub(super) fn pack(&self) -> Result<Vec<String>, ProcessReplayStoreError> {
        if self.all {
            return Ok(vec![EVERY_PROCESS.to_string()]);
        }
        let mut payloads = Vec::new();
        let mut current = String::from("[");
        for process in &self.processes {
            let encoded =
                serde_json::to_string(process).map_err(|error| codec_error("doorbell", error))?;
            if encoded.len() + 2 > PAYLOAD_LIMIT {
                return Err(codec_error(
                    "doorbell",
                    "a process id too long for a notification payload",
                ));
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

    /// The doorbell one notification payload rings; a payload this store
    /// did not write rings nothing.
    pub(super) fn unpack(payload: &str) -> Self {
        if payload == EVERY_PROCESS {
            return Self::all();
        }
        match serde_json::from_str::<BTreeSet<String>>(payload) {
            Ok(processes) => Self {
                all: false,
                processes,
            },
            Err(error) => {
                tracing::warn!(
                    %error,
                    "ignoring a process replay notification this store cannot read"
                );
                Self::default()
            }
        }
    }
}

const EVERY_PROCESS: &str = "*";

/// PostgreSQL refuses a notification payload of 8000 bytes or more.
const PAYLOAD_LIMIT: usize = 7_900;

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-7053: a row written from the draft's borrowed payload reads back
    /// as the payload it was written from, under its stored kind, and
    /// writes again to the very same bytes, for every stored variant.
    #[test]
    fn every_stored_variant_reads_back_as_the_bytes_it_was_written_as() {
        let process = ProcessId::fixture("view-codec");
        let payloads = [
            ProcessObservationEventPayload::LanguageExecution(
                lash_core::testing::process_language_observation(&process, "start", "cell"),
            ),
            ProcessObservationEventPayload::StepBodyStarted(
                lash_core::testing::process_step_body_started(&process, "step", 1, 7),
            ),
            ProcessObservationEventPayload::Committed {
                event: lash_core::testing::process_committed_event(3, "requester"),
            },
        ];
        let kinds = ["language_execution", "step_body_started", "committed"];
        for (payload, kind) in payloads.into_iter().zip(kinds) {
            let bytes =
                serde_json::to_vec(&StoredPayloadView::of(&payload)).expect("the payload encodes");
            let row: serde_json::Value =
                serde_json::from_slice(&bytes).expect("a stored payload is JSON");
            assert_eq!(row["kind"], kind);
            let decoded = decode_payload(&bytes).expect("the stored payload decodes");
            assert_eq!(decoded, payload, "{kind}");
            let rewritten = serde_json::to_vec(&StoredPayloadView::of(&decoded))
                .expect("the decoded payload encodes");
            assert_eq!(rewritten, bytes, "{kind}");
        }
    }
}
