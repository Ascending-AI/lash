//! The stream a tool child records when no opener is live where it runs
//! (FIG-3712), in the journal's own shape.
//!
//! A child that runs beside its live opener streams its session events and
//! turn activities to the opener as they happen. A child whose context the
//! deployment built has no stream to reach, so it records them, and they ride
//! its [`ToolSettlement`](super::ToolSettlement) until the opener incorporates
//! it and emits them. They are late, never dropped, unless the budget below
//! cut them.
//!
//! The journal owns this shape, not the stream types. A recorded event holds
//! its event's serialized form as an opaque payload, tagged only with the
//! channel it arrived on. A change to a stream or activity type therefore does
//! not change the settlement's durable format: the opener decodes each payload
//! when it emits it, and a payload this build no longer decodes is skipped.
//! That is sound because the stream is presentation, never an outcome the
//! child or its opener acts on.
//!
//! The recording is bounded:
//!
//! * **Deltas are coalesced.** A text or reasoning delta for the same block as
//!   the channel's previous event is appended to it, so a streamed block costs
//!   one entry, not one per delta.
//! * **Call arguments and outputs are stored once.** A nested call's `args`
//!   and `output` appear on its session event and on its activity. A payload
//!   whose field equals the same call's earlier recorded field keeps a
//!   reference to that entry instead, and emission restores it.
//! * **What the settlement already holds is not recorded again.** A child's
//!   own result often embeds what its nested calls returned (a batch's
//!   results carry each nested output). Once the child's drive has returned,
//!   any sizable part of a recorded payload equal to a part of the child's
//!   own journaled call record is replaced by a reference to it, and
//!   emission restores it from that record.
//! * **A byte budget caps the whole stream.** Past
//!   [`CHILD_STREAM_BYTE_BUDGET`], nothing more is recorded, and a typed
//!   [`ChildStreamTruncation`] says how much was dropped.
//!
//! Order is kept within each channel. Across the two channels it is not
//! guaranteed, just as it is not on a live opener's two channels, whose
//! consumers read them independently.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most payload bytes a child's recorded stream holds. What does not fit
/// is dropped and counted in its [`ChildStreamTruncation`].
pub const CHILD_STREAM_BYTE_BUDGET: usize = 256 * 1024;

/// The fields a nested call's session event and activity both carry, stored
/// once per call.
const SHARED_CALL_FIELDS: [&str; 2] = ["args", "output"];

/// The smallest serialized part of a payload worth replacing by a reference
/// to the child's own call record: below this, the reference costs about as
/// much as the value.
const SETTLED_MIN_BYTES: usize = 64;

/// The recorded stream a settlement carries.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedChildStream {
    /// The recorded events, in the order they were recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<RecordedChildEvent>,
    /// Present when the budget cut the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<ChildStreamTruncation>,
}

/// One recorded event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedChildEvent {
    /// The channel it arrived on.
    pub channel: RecordedChildChannel,
    /// The event's serialized form, less any field in `shared`.
    pub payload: Value,
    /// Fields removed from `payload` because an earlier entry recorded the
    /// same value for the same call: field name to that entry's index.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shared: BTreeMap<String, u32>,
    /// Parts of `payload` held by the child's own journaled call record
    /// instead: a JSON pointer into `payload` (left `null` there) to a JSON
    /// pointer into that record.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settled: BTreeMap<String, String>,
}

/// The stream channel a recorded event arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedChildChannel {
    /// The session stream (`SessionStreamEvent`).
    Session,
    /// The turn activity stream (`TurnActivity`).
    Activity,
}

/// What the byte budget dropped from a recorded stream: everything from the
/// first event that did not fit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildStreamTruncation {
    pub dropped_events: u64,
    pub dropped_bytes: u64,
}

/// A recorded event decoded for emission.
#[derive(Clone, Debug)]
pub enum DecodedChildEvent {
    Session(crate::SessionStreamEvent),
    Activity(crate::TurnActivity),
}

impl RecordedChildStream {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.truncated.is_none()
    }

    /// The events to emit, in recorded order, with every referenced part
    /// restored: from `record`, the child's own journaled call record (as
    /// JSON), and from the earlier entries that hold a shared field. A
    /// payload this build cannot decode is skipped and counted.
    pub fn decode(&self, record: &Value) -> (Vec<DecodedChildEvent>, usize) {
        let restored: Vec<Value> = self
            .events
            .iter()
            .map(|event| {
                let mut payload = event.payload.clone();
                for (at, from) in &event.settled {
                    if let (Some(slot), Some(value)) =
                        (payload.pointer_mut(at), record.pointer(from))
                    {
                        *slot = value.clone();
                    }
                }
                payload
            })
            .collect();
        let mut decoded = Vec::with_capacity(self.events.len());
        let mut undecodable = 0;
        for (event, payload) in self.events.iter().zip(&restored) {
            let mut payload = payload.clone();
            if let Value::Object(fields) = &mut payload {
                for (field, source) in &event.shared {
                    if let Some(value) = restored
                        .get(*source as usize)
                        .and_then(|source| source.get(field))
                    {
                        fields.insert(field.clone(), value.clone());
                    }
                }
            }
            let event = match event.channel {
                RecordedChildChannel::Session => {
                    serde_json::from_value(payload).map(DecodedChildEvent::Session)
                }
                RecordedChildChannel::Activity => {
                    serde_json::from_value(payload).map(DecodedChildEvent::Activity)
                }
            };
            match event {
                Ok(event) => decoded.push(event),
                Err(_) => undecodable += 1,
            }
        }
        (decoded, undecodable)
    }

    /// Replaces every sizable part of a recorded payload that equals a part
    /// of `record`, the child's own journaled call record (as JSON), by a
    /// reference to it. The largest matching part wins; nothing inside it is
    /// searched further.
    pub(crate) fn settle_against(&mut self, record: &Value) {
        let mut index: BTreeMap<String, String> = BTreeMap::new();
        index_parts(record, String::new(), &mut index);
        if index.is_empty() {
            return;
        }
        for event in &mut self.events {
            let mut settled = BTreeMap::new();
            settle_parts(&mut event.payload, String::new(), &index, &mut settled);
            event.settled.extend(settled);
        }
    }
}

/// Every part of `value` at least [`SETTLED_MIN_BYTES`] long, by serialized
/// form, to its JSON pointer: the first, in document order, of equal parts.
fn index_parts(value: &Value, pointer: String, index: &mut BTreeMap<String, String>) {
    let serialized = serde_json::to_string(value).unwrap_or_default();
    if serialized.len() < SETTLED_MIN_BYTES {
        return;
    }
    match value {
        Value::Object(fields) => {
            for (key, field) in fields {
                index_parts(field, format!("{pointer}/{}", escape_pointer(key)), index);
            }
        }
        Value::Array(items) => {
            for (position, item) in items.iter().enumerate() {
                index_parts(item, format!("{pointer}/{position}"), index);
            }
        }
        _ => {}
    }
    index.entry(serialized).or_insert(pointer);
}

fn settle_parts(
    value: &mut Value,
    pointer: String,
    index: &BTreeMap<String, String>,
    settled: &mut BTreeMap<String, String>,
) {
    let serialized = serde_json::to_string(value).unwrap_or_default();
    if serialized.len() < SETTLED_MIN_BYTES {
        return;
    }
    if !pointer.is_empty()
        && let Some(from) = index.get(&serialized)
    {
        settled.insert(pointer, from.clone());
        *value = Value::Null;
        return;
    }
    match value {
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                settle_parts(
                    field,
                    format!("{pointer}/{}", escape_pointer(key)),
                    index,
                    settled,
                );
            }
        }
        Value::Array(items) => {
            for (position, item) in items.iter_mut().enumerate() {
                settle_parts(item, format!("{pointer}/{position}"), index, settled);
            }
        }
        _ => {}
    }
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Builds a [`RecordedChildStream`] as events arrive.
#[derive(Default)]
pub(crate) struct RecordedChildStreamBuilder {
    stream: RecordedChildStream,
    bytes: usize,
    /// The entry holding each call's field value: `(call_id, field)` to index.
    call_fields: BTreeMap<(String, &'static str), u32>,
}

impl RecordedChildStreamBuilder {
    pub(crate) fn push_session(&mut self, event: &crate::SessionStreamEvent) {
        self.push(RecordedChildChannel::Session, serde_json::to_value(event));
    }

    pub(crate) fn push_activity(&mut self, activity: &crate::TurnActivity) {
        self.push(
            RecordedChildChannel::Activity,
            serde_json::to_value(activity),
        );
    }

    pub(crate) fn finish(self) -> RecordedChildStream {
        self.stream
    }

    fn push(&mut self, channel: RecordedChildChannel, payload: serde_json::Result<Value>) {
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
        if self.bytes + bytes > CHILD_STREAM_BYTE_BUDGET {
            self.stream.truncated = Some(ChildStreamTruncation {
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
        self.stream.events.push(RecordedChildEvent {
            channel,
            payload,
            shared,
            settled: BTreeMap::new(),
        });
    }

    /// Appends a delta to the channel's previous event when both are deltas
    /// of the same kind for the same block.
    fn coalesce(&mut self, channel: RecordedChildChannel, payload: &Value) -> bool {
        let Some((text_field, delta)) = delta_text(payload) else {
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
        let same_block = previous.payload.get("type") == payload.get("type")
            && previous.payload.get("block") == payload.get("block")
            && previous.payload.get("correlation_id") == payload.get("correlation_id");
        if !same_block || self.bytes + delta.len() > CHILD_STREAM_BYTE_BUDGET {
            return false;
        }
        let Some(Value::String(text)) = previous.payload.get_mut(text_field) else {
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

fn call_id(payload: &Value) -> Option<&str> {
    payload.get("call_id").and_then(Value::as_str)
}

/// The text field and text of a streamed delta: a session event's `content`
/// or an activity's `text`.
fn delta_text(payload: &Value) -> Option<(&'static str, &str)> {
    match payload.get("type").and_then(Value::as_str)? {
        "text_delta" | "reasoning_delta" if payload.get("content").is_some() => {
            Some(("content", payload.get("content")?.as_str()?))
        }
        "assistant_prose_delta" | "reasoning_delta" => {
            Some(("text", payload.get("text")?.as_str()?))
        }
        _ => None,
    }
}

fn payload_bytes(payload: &Value) -> usize {
    serde_json::to_vec(payload).map_or(0, |bytes| bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_delta(content: &str) -> Value {
        serde_json::json!({"type": "text_delta", "content": content, "block": {"id": "b1"}})
    }

    fn builder_with(payloads: Vec<(RecordedChildChannel, Value)>) -> RecordedChildStream {
        let mut builder = RecordedChildStreamBuilder::default();
        for (channel, payload) in payloads {
            builder.push(channel, Ok(payload));
        }
        builder.finish()
    }

    #[test]
    fn deltas_of_one_block_coalesce_into_one_entry() {
        let stream = builder_with(vec![
            (RecordedChildChannel::Session, text_delta("hel")),
            (
                RecordedChildChannel::Activity,
                serde_json::json!({"type": "tool_call_started", "call_id": "c"}),
            ),
            (RecordedChildChannel::Session, text_delta("lo")),
        ]);
        assert_eq!(stream.events.len(), 2);
        assert_eq!(stream.events[0].payload["content"], "hello");
    }

    #[test]
    fn a_call_output_is_stored_once_and_restored_on_decode() {
        let output = serde_json::json!({"big": "x".repeat(64)});
        let stream = builder_with(vec![
            (
                RecordedChildChannel::Session,
                serde_json::json!({"type": "tool_call", "call_id": "c", "output": output}),
            ),
            (
                RecordedChildChannel::Activity,
                serde_json::json!({"type": "tool_call_completed", "call_id": "c", "output": output}),
            ),
        ]);
        assert!(stream.events[1].payload.get("output").is_none());
        assert_eq!(stream.events[1].shared.get("output"), Some(&0));
    }

    #[test]
    fn a_part_the_childs_own_record_holds_is_referenced_and_restored() {
        let nested = serde_json::json!({"status": "success", "value": {"rows": "z".repeat(128)}});
        let activity = serde_json::json!({
            "id": "a1",
            "event": {
                "type": "tool_call_completed",
                "call_id": "nested",
                "name": "leaf",
                "args": {},
                "output": nested.clone(),
                "duration_ms": 1,
            }
        });
        let mut stream = builder_with(vec![(RecordedChildChannel::Activity, activity.clone())]);
        let record = serde_json::json!({
            "tool": "batch",
            "args": {},
            "output": {"results": [{"index": 0, "result": nested}]},
        });
        stream.settle_against(&record);
        let entry = &stream.events[0];
        assert_eq!(entry.payload["event"]["output"], Value::Null);
        assert_eq!(
            entry.settled.get("/event/output").map(String::as_str),
            Some("/output/results/0/result")
        );
        let bytes = serde_json::to_vec(&stream).expect("serializes").len();
        assert!(
            bytes < 256,
            "the output is not recorded twice: {bytes} bytes"
        );
    }

    /// A real nested completion round-trips through the settled reference:
    /// emission restores the output from the child's own record.
    #[test]
    fn a_settled_nested_completion_decodes_to_its_output() {
        let output = crate::ToolCallOutput::success(serde_json::json!({"rows": "q".repeat(128)}));
        let mut builder = RecordedChildStreamBuilder::default();
        builder.push_activity(&crate::TurnActivity::new(
            crate::TurnActivityId::new("tool:nested"),
            crate::TurnEvent::ToolCallCompleted {
                call_id: Some("nested".to_string()),
                name: "leaf".to_string(),
                args: serde_json::json!({}),
                output: output.clone(),
                duration_ms: 1,
                graph_key: None,
                parent_call_id: Some("call-1".to_string()),
            },
        ));
        let mut stream = builder.finish();
        let record = serde_json::json!({"tool": "batch", "results": [output.clone()]});
        stream.settle_against(&record);
        assert!(
            !stream.events[0].settled.is_empty(),
            "the output is referenced"
        );
        let (decoded, undecodable) = stream.decode(&record);
        assert_eq!(undecodable, 0);
        match &decoded[..] {
            [DecodedChildEvent::Activity(activity)] => match &activity.event {
                crate::TurnEvent::ToolCallCompleted {
                    output: restored, ..
                } => {
                    assert_eq!(restored, &output);
                }
                other => panic!("unexpected event {other:?}"),
            },
            other => panic!("unexpected decode {other:?}"),
        }
    }

    #[test]
    fn the_budget_cuts_the_stream_with_a_typed_marker() {
        let big = "y".repeat(CHILD_STREAM_BYTE_BUDGET / 2);
        let stream = builder_with(
            (0..4)
                .map(|index| {
                    (
                        RecordedChildChannel::Session,
                        serde_json::json!({"type": "message", "kind": "k", "text": format!("{index}{big}")}),
                    )
                })
                .collect(),
        );
        assert_eq!(stream.events.len(), 1);
        let truncated = stream.truncated.expect("the budget cut the stream");
        assert_eq!(truncated.dropped_events, 3);
        assert!(truncated.dropped_bytes > (CHILD_STREAM_BYTE_BUDGET as u64));
    }
}
