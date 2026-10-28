//! A protocol event's payload, shared by every holder of the event.

use crate::derived::DerivedMemo;
use std::any::Any;
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

/// Immutable protocol JSON shared by history readers. Typed facts derive from
/// a transient tree; only an explicit value read retains that tree. Editing
/// detaches the value and invalidates derived facts. It serializes as its JSON.
///
/// ```
/// use lash_protocol_types::ProtocolPayload;
///
/// let payload = ProtocolPayload::from(serde_json::json!({"format": 1}));
/// let format = payload.derived(|value| value["format"].as_u64());
/// // The clone is the same payload: what was derived is shared.
/// let held = payload.clone();
/// assert!(std::sync::Arc::ptr_eq(&format, &held.derived(|_| None)));
/// assert_eq!(*format, Some(1));
/// ```
#[derive(Clone)]
pub struct ProtocolPayload(Arc<Inner>);

struct Inner {
    json: PayloadJson,
    derived: DerivedMemo,
}

enum PayloadJson {
    Encoded {
        text: Arc<str>,
        parsed: OnceLock<serde_json::Value>,
    },
    /// An edited value, or a constructed value beyond the text decoder's depth.
    Value(serde_json::Value),
}

#[expect(
    clippy::expect_used,
    reason = "JSON values serialize without an external writer"
)]
fn encode(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("JSON value serializes")
}

#[expect(
    clippy::expect_used,
    reason = "encoded payloads passed the same decoder at construction"
)]
fn decode(text: &str) -> serde_json::Value {
    #[cfg(test)]
    DECODE_CALLS.with(|calls| calls.set(calls.get() + 1));
    serde_json::from_str(text).expect("validated payload JSON decodes")
}

fn decodable(text: &str) -> bool {
    serde_transcode::transcode(
        &mut serde_json::Deserializer::from_str(text),
        &mut serde_json::Serializer::new(std::io::sink()),
    )
    .is_ok()
}

#[cfg(test)]
thread_local! {
    static DECODE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl PayloadJson {
    fn value(&self) -> &serde_json::Value {
        match self {
            Self::Encoded { text, parsed } => parsed.get_or_init(|| decode(text)),
            Self::Value(value) => value,
        }
    }

    /// Serialization and equality need a value only for this synchronous read.
    fn transient(&self) -> Cow<'_, serde_json::Value> {
        match self {
            Self::Encoded { text, parsed } => parsed
                .get()
                .map_or_else(|| Cow::Owned(decode(text)), Cow::Borrowed),
            Self::Value(value) => Cow::Borrowed(value),
        }
    }

    fn into_value(self) -> serde_json::Value {
        match self {
            Self::Encoded { text, parsed } => parsed.into_inner().unwrap_or_else(|| decode(&text)),
            Self::Value(value) => value,
        }
    }
}

impl ProtocolPayload {
    pub fn new(value: serde_json::Value) -> Self {
        let text = encode(&value);
        // A programmatically constructed Value can exceed serde_json's text
        // decoder depth. Keep that value intact rather than narrowing the API.
        let json = if decodable(&text) {
            PayloadJson::Encoded {
                text: text.into(),
                parsed: OnceLock::new(),
            }
        } else {
            PayloadJson::Value(value)
        };
        Self(Arc::new(Inner {
            json,
            derived: DerivedMemo::default(),
        }))
    }

    pub fn value(&self) -> &serde_json::Value {
        self.0.json.value()
    }

    /// The JSON, copied only when another holder shares it.
    pub fn into_value(self) -> serde_json::Value {
        match Arc::try_unwrap(self.0) {
            Ok(inner) => inner.json.into_value(),
            Err(shared) => shared.json.transient().into_owned(),
        }
    }

    /// Detach the JSON for editing and invalidate this holder's derived facts.
    pub fn make_mut(&mut self) -> &mut serde_json::Value {
        if Arc::get_mut(&mut self.0).is_none() {
            *self = Self(Arc::new(Inner {
                json: PayloadJson::Value(self.0.json.transient().into_owned()),
                derived: DerivedMemo::default(),
            }));
        }
        let inner = Arc::get_mut(&mut self.0).unwrap_or_else(|| unreachable!());
        inner.derived = DerivedMemo::default();
        if let PayloadJson::Encoded { text, parsed } = &mut inner.json {
            inner.json = PayloadJson::Value(parsed.take().unwrap_or_else(|| decode(text)));
        }
        match &mut inner.json {
            PayloadJson::Value(value) => value,
            PayloadJson::Encoded { .. } => unreachable!("editing materialized the value"),
        }
    }

    /// Derive a fact once, materializing JSON only for this computation.
    pub fn derived<T: Any + Send + Sync>(
        &self,
        compute: impl FnOnce(&serde_json::Value) -> T,
    ) -> Arc<T> {
        self.0
            .derived
            .get_or_compute(|| compute(self.0.json.transient().as_ref()))
    }

    /// Whether two holders share one payload, rather than merely equal values.
    pub fn same(left: &Self, right: &Self) -> bool {
        Arc::ptr_eq(&left.0, &right.0)
    }
}

impl std::ops::Deref for ProtocolPayload {
    type Target = serde_json::Value;
    fn deref(&self) -> &serde_json::Value {
        self.value()
    }
}

impl From<serde_json::Value> for ProtocolPayload {
    fn from(value: serde_json::Value) -> Self {
        Self::new(value)
    }
}

impl PartialEq for ProtocolPayload {
    fn eq(&self, other: &Self) -> bool {
        Self::same(self, other)
            || matches!((&self.0.json, &other.0.json),
                (PayloadJson::Encoded { text: a, .. }, PayloadJson::Encoded { text: b, .. }) if a == b)
            || self.0.json.transient() == other.0.json.transient()
    }
}

impl PartialEq<serde_json::Value> for ProtocolPayload {
    fn eq(&self, other: &serde_json::Value) -> bool {
        self.0.json.transient().as_ref() == other
    }
}

impl std::fmt::Debug for ProtocolPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.0.json.transient().as_ref(), f)
    }
}

impl std::fmt::Display for ProtocolPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let PayloadJson::Encoded { text, .. } = &self.0.json
            && !f.alternate()
        {
            return f.write_str(text);
        }
        std::fmt::Display::fmt(self.0.json.transient().as_ref(), f)
    }
}

impl serde::Serialize for ProtocolPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0.json {
            PayloadJson::Encoded { text, parsed } => match parsed.get() {
                Some(value) => value.serialize(serializer),
                None => serde_transcode::transcode(
                    &mut serde_json::Deserializer::from_str(text),
                    serializer,
                ),
            },
            PayloadJson::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ProtocolPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Self::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-7519: publishing an immutable meter receipt must not rebuild its
    /// JSON tree for every serialization, or retain one as a side effect.
    #[test]
    fn serializing_encoded_protocol_json_does_not_rebuild_a_tree() {
        let value = serde_json::json!({
            "consumption": [{"resource":"charges", "consumed":7, "limit":40}],
            "text": "line\nwith quotes \" and Unicode λ",
            "values": [null, true, -2, 1.5]
        });
        let payload = ProtocolPayload::new(value.clone());
        let before = DECODE_CALLS.with(std::cell::Cell::get);
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            serde_json::to_string(&value).unwrap()
        );
        assert_eq!(DECODE_CALLS.with(std::cell::Cell::get), before);
        assert_eq!(serde_json::to_value(&payload).unwrap(), value);
        let PayloadJson::Encoded { parsed, .. } = &payload.0.json else {
            panic!("the receipt is shallow JSON");
        };
        assert!(parsed.get().is_none());
        assert_eq!(payload.value(), &value);
        assert_eq!(DECODE_CALLS.with(std::cell::Cell::get), before + 1);
    }

    /// Constructed JSON beyond the text decoder's depth retains its value.
    #[test]
    fn deep_constructed_protocol_json_keeps_its_serialization() {
        let mut value = serde_json::Value::Null;
        for _ in 0..150 {
            value = serde_json::Value::Array(vec![value]);
        }
        let payload = ProtocolPayload::new(value.clone());
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            serde_json::to_string(&value).unwrap()
        );
        assert_eq!(payload.value(), &value);
    }
}
