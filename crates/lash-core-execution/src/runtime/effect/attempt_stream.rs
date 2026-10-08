//! The bounded stream an attempt captures, in the journal's own shape.
//!
//! A tool body that runs beside its live opener streams its session events
//! and turn activities to the opener as they happen. A body with no live
//! stream to reach records them instead: a Run attempt records them in its
//! attempt capture (X), and the Run emits them when it presents the call
//! (FIG-4880). They are late, never dropped, unless the budget below cut them.
//!
//! The journal owns this shape, not the stream types. A recorded event holds
//! its event's serialized form as an opaque payload, tagged only with the
//! channel it arrived on. A change to a stream or activity type therefore does
//! not change the capture's durable format: the opener decodes each payload
//! when it emits it, and a payload this build no longer decodes is skipped.
//! That is sound because the stream is presentation, never an outcome the
//! body or its opener acts on.
//!
//! The recording is bounded:
//!
//! * **Deltas are coalesced.** A text or reasoning delta for the same block as
//!   the channel's previous event is appended to it, so a streamed block costs
//!   one entry, not one per delta.
//! * **Call arguments and outputs are stored once.** A call's `args` and
//!   `output` appear on its session event and on its activity. A payload
//!   whose field equals the same call's earlier recorded field keeps a
//!   reference to that entry instead, and emission restores it.
//! * **A byte budget caps the whole stream.** Past
//!   the configured cut (standard: [`ATTEMPT_STREAM_BYTE_BUDGET`]), nothing more is recorded, and a typed
//!   [`AttemptStreamTruncation`] says how much was dropped.
//!
//! Order is kept within each channel. Across the two channels it is not
//! guaranteed, just as it is not on a live opener's two channels, whose
//! consumers read them independently.

use std::collections::BTreeMap;

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The standard preset for payload bytes in an attempt's recorded stream. What does not fit
/// is dropped and counted in its [`AttemptStreamTruncation`].
pub const ATTEMPT_STREAM_BYTE_BUDGET: usize =
    lash_trace::TraceLimits::standard().attempt_stream_bytes;

/// The fields a call's session event and activity both carry, stored
/// once per call.
const SHARED_CALL_FIELDS: [&str; 2] = ["args", "output"];

/// The recorded stream an attempt capture carries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptStream {
    /// The recorded events, in the order they were recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<AttemptStreamEvent>,
    /// Present when the budget cut the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<AttemptStreamTruncation>,
}

/// One recorded event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptStreamEvent {
    /// The channel it arrived on.
    pub channel: AttemptStreamChannel,
    /// The event's serialized form, less any field in `shared`.
    pub payload: Value,
    /// Fields removed from `payload` because an earlier entry recorded the
    /// same value for the same call: field name to that entry's index.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shared: BTreeMap<String, u32>,
}

/// The stream channel a recorded event arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStreamChannel {
    /// The session stream (`SessionStreamEvent`).
    Session,
    /// The turn activity stream (`TurnActivity`).
    Activity,
}

/// What the byte budget dropped from a recorded stream: everything from the
/// first event that did not fit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptStreamTruncation {
    pub dropped_events: u64,
    pub dropped_bytes: u64,
}

/// A recorded event decoded for emission.
#[derive(Clone, Debug)]
pub enum DecodedStreamEvent {
    Session(crate::SessionStreamEvent),
    Activity(crate::TurnActivity),
}

impl AttemptStream {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.truncated.is_none()
    }

    /// Decode events in channel order, restoring fields shared with earlier
    /// entries. A payload this build cannot decode is skipped and counted.
    pub fn decode(&self) -> (Vec<DecodedStreamEvent>, usize) {
        let mut decoded = Vec::with_capacity(self.events.len());
        let mut undecodable = 0;
        for event in &self.events {
            let mut payload = event.payload.clone();
            if let Value::Object(fields) = &mut payload {
                for (field, source) in &event.shared {
                    if let Some(value) = self
                        .events
                        .get(*source as usize)
                        .and_then(|source| source.payload.get(field))
                    {
                        fields.insert(field.clone(), value.clone());
                    }
                }
            }
            let event = match event.channel {
                AttemptStreamChannel::Session => {
                    serde_json::from_value(payload).map(DecodedStreamEvent::Session)
                }
                AttemptStreamChannel::Activity => {
                    serde_json::from_value(payload).map(DecodedStreamEvent::Activity)
                }
            };
            match event {
                Ok(event) => decoded.push(event),
                Err(_) => undecodable += 1,
            }
        }
        (decoded, undecodable)
    }
}

/// Builds a [`AttemptStream`] as events arrive.
pub struct AttemptStreamBuilder {
    limit: usize,
    stream: AttemptStream,
    bytes: usize,
    /// The entry holding each call's field value: `(call_id, field)` to index.
    call_fields: BTreeMap<(String, &'static str), u32>,
}

impl Default for AttemptStreamBuilder {
    fn default() -> Self {
        Self::new(ATTEMPT_STREAM_BYTE_BUDGET)
    }
}

impl AttemptStreamBuilder {
    /// Capture at most `limit` payload bytes, with typed omission counts.
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            stream: Default::default(),
            bytes: 0,
            call_fields: Default::default(),
        }
    }
    pub fn push_session(&mut self, event: &crate::SessionStreamEvent) {
        self.push(AttemptStreamChannel::Session, serde_json::to_value(event));
    }

    pub fn push_activity(&mut self, activity: &crate::TurnActivity) {
        self.push(
            AttemptStreamChannel::Activity,
            serde_json::to_value(activity),
        );
    }

    #[must_use]
    pub fn finish(self) -> AttemptStream {
        self.stream
    }

    fn push(&mut self, channel: AttemptStreamChannel, payload: serde_json::Result<Value>) {
        // A stream DTO always serializes; one that did not has nothing to
        // record.
        let Ok(mut payload) = payload else {
            return;
        };
        if let Some(truncated) = &mut self.stream.truncated {
            truncated.dropped_events += 1;
            truncated.dropped_bytes += payload_bytes(&payload) as u64;
            return;
        }
        if self.coalesce(channel, &payload) {
            return;
        }
        let shared = self.share_call_fields(&mut payload);
        let bytes = payload_bytes(&payload);
        if self.bytes + bytes > self.limit {
            self.stream.truncated = Some(AttemptStreamTruncation {
                dropped_events: 1,
                dropped_bytes: bytes as u64,
            });
            return;
        }
        self.bytes += bytes;
        let index = self.stream.events.len() as u32;
        if let Some(call_id) = call_id(&payload) {
            for field in SHARED_CALL_FIELDS {
                if payload.get(field).is_some() {
                    self.call_fields
                        .entry((call_id.to_owned(), field))
                        .or_insert(index);
                }
            }
        }
        self.stream.events.push(AttemptStreamEvent {
            channel,
            payload,
            shared,
        });
    }

    /// Appends a delta to the channel's previous event when both are deltas
    /// of the same kind for the same block.
    fn coalesce(&mut self, channel: AttemptStreamChannel, payload: &Value) -> bool {
        let Some(delta) = delta_text(payload) else {
            return false;
        };
        let Some(previous) = self
            .stream
            .events
            .iter_mut()
            .rev()
            .find(|event| event.channel == channel)
        else {
            return false;
        };
        let same_block = delta_text(&previous.payload).is_some()
            && ["kind", "block", "correlation_id"]
                .iter()
                .all(|field| previous.payload.get(field) == payload.get(field));
        if !same_block || self.bytes + delta.len() > self.limit {
            return false;
        }
        let Some(Value::String(text)) = previous.payload.get_mut("text") else {
            return false;
        };
        text.push_str(delta);
        self.bytes += delta.len();
        true
    }

    /// Removes each shared call field an earlier entry already holds with the
    /// same value, and returns where to find it.
    fn share_call_fields(&self, payload: &mut Value) -> BTreeMap<String, u32> {
        let mut shared = BTreeMap::new();
        let Some(call_id) = call_id(payload).map(str::to_owned) else {
            return shared;
        };
        for field in SHARED_CALL_FIELDS {
            let Some(&source) = self.call_fields.get(&(call_id.clone(), field)) else {
                continue;
            };
            let recorded = self.stream.events[source as usize].payload.get(field);
            if recorded.is_some() && recorded == payload.get(field) {
                if let Value::Object(fields) = payload {
                    fields.remove(field);
                }
                shared.insert(field.to_owned(), source);
            }
        }
        shared
    }
}

/// Records the stream events an attempt's body emits, for its capture to
/// carry.
///
/// The body's dispatch points its [`ObservationSink`] here: observation is
/// synchronous, so there is no channel to pin and no collector task to
/// await — every `observe` pushes into a bounded [`AttemptStreamBuilder`] in
/// program order, and [`Self::finish`] hands the stream back once the body
/// has returned. A session event is recorded raw, so the stream never stores
/// a payload twice. An observed activity is recorded with the
/// `{key}#{ordinal}` id the observation minted — the same id a live opener's
/// observer would publish.
///
/// [`ObservationSink`]: crate::engine::ObservationSink
#[derive(Default)]
pub struct AttemptStreamRecorder {
    stream: std::sync::Mutex<AttemptStreamBuilder>,
}

impl AttemptStreamRecorder {
    #[must_use]
    pub fn start(limit: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            stream: std::sync::Mutex::new(AttemptStreamBuilder::new(limit)),
        })
    }

    /// Every event the body emitted, once it has returned.
    pub fn finish(&self) -> AttemptStream {
        let mut stream = self.stream.lock_recover();
        let limit = stream.limit;
        std::mem::replace(&mut *stream, AttemptStreamBuilder::new(limit)).finish()
    }
}

impl crate::engine::ObservationSink for AttemptStreamRecorder {
    fn observe(&self, observation: crate::engine::ShiftObservation) {
        let crate::engine::ShiftObservation {
            key,
            ordinal,
            event,
        } = observation;
        let id = crate::TurnActivityId::new(format!("{key}#{ordinal}"));
        let mut stream = self.stream.lock_recover();
        match event {
            crate::engine::ObservedEvent::Session(event)
            | crate::engine::ObservedEvent::RecordedSession(event) => {
                stream.push_session(&event);
            }
            crate::engine::ObservedEvent::Activity {
                correlation_id,
                event,
            } => {
                stream.push_activity(&crate::TurnActivity {
                    correlation_id: correlation_id.unwrap_or_else(|| id.clone()),
                    id,
                    event,
                });
            }
            crate::engine::ObservedEvent::RecordedActivity(activity) => {
                stream.push_activity(&activity);
            }
        }
    }
}

fn call_id(payload: &Value) -> Option<&str> {
    payload.get("call_id").and_then(Value::as_str)
}

/// The text of a streamed block delta. Both channels carry the one
/// stream-block payload, so one spelling reads either.
fn delta_text(payload: &Value) -> Option<&str> {
    let field = |name: &str| payload.get(name).and_then(Value::as_str);
    (field("type")? == "stream_block" && field("phase")? == "delta")
        .then(|| field("text"))
        .flatten()
}

fn payload_bytes(payload: &Value) -> usize {
    serde_json::to_vec(payload).map_or(0, |bytes| bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder_with(payloads: Vec<(AttemptStreamChannel, Value)>) -> AttemptStream {
        let mut builder = AttemptStreamBuilder::default();
        for (channel, payload) in payloads {
            builder.push(channel, Ok(payload));
        }
        builder.finish()
    }

    #[test]
    fn the_budget_cuts_the_stream_with_a_typed_marker() {
        let big = "y".repeat(ATTEMPT_STREAM_BYTE_BUDGET / 2);
        let stream = builder_with(
            (0..4)
                .map(|index| {
                    (
                        AttemptStreamChannel::Session,
                        serde_json::json!({"type": "message", "kind": "k", "text": format!("{index}{big}")}),
                    )
                })
                .collect(),
        );
        assert_eq!(stream.events.len(), 1);
        let truncated = stream.truncated.expect("the budget cut the stream");
        assert_eq!(truncated.dropped_events, 3);
        assert!(truncated.dropped_bytes > (ATTEMPT_STREAM_BYTE_BUDGET as u64));
    }
}
