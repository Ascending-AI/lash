//! The shared SQLite-row / PostgreSQL-NOTIFY wake envelope (FIG-5555).
//!
//! A JSON array of complete actor-key strings carries owned-mail hints;
//! the JSON strings `"ready"` and `"poll_store"` carry the two scan hints;
//! a JSON object `{"appended": [keys]}` names actors whose event logs grew.
//! Every payload is at most [`MAX_BYTES`] UTF-8 bytes, including JSON
//! escaping, brackets and separators. This is below PostgreSQL's exclusive
//! 8000-byte NOTIFY limit, and also bounds each SQLite wake row.
//!
//! Batches split between keys. Actor keys themselves have no length limit:
//! an individually oversized encoded key requests a store scan instead of
//! riding the transport. Wakes remain hints; the durable polls still find
//! work after a missing, malformed or delayed envelope.

use std::collections::BTreeSet;

use crate::{ActorKey, NodeWakeEvent};

/// Inclusive size bound for one encoded wake envelope, in UTF-8 bytes.
pub const MAX_BYTES: usize = 7_900;

/// The encoded hint to poll claimable actors.
pub const READY: &str = "\"ready\"";

const POLL_STORE: &str = "\"poll_store\"";

/// What opens and closes the envelope that names appended logs.
const APPENDED_OPEN: &str = "{\"appended\":[";
const APPENDED_CLOSE: &str = "]}";

/// Encode owned-mail hints, splitting at the encoded byte bound. Emit at
/// most one [`NodeWakeEvent::PollStore`] hint for keys that cannot fit alone.
#[must_use]
pub fn owned(actors: &BTreeSet<ActorKey>) -> Vec<String> {
    let (mut payloads, oversized) = key_lists(actors, "[", "]");
    if oversized {
        payloads.push(POLL_STORE.to_owned());
    }
    payloads
}

/// Encode appended-log hints, splitting at the encoded byte bound. A key
/// that cannot fit alone is left out: its followers re-read on their own
/// cadence.
#[must_use]
pub fn appended(actors: &BTreeSet<ActorKey>) -> Vec<String> {
    key_lists(actors, APPENDED_OPEN, APPENDED_CLOSE).0
}

/// `actors`' keys as JSON strings between `open` and `close`, in as many
/// envelopes as the byte bound needs, and whether some key fits in none.
fn key_lists(actors: &BTreeSet<ActorKey>, open: &str, close: &str) -> (Vec<String>, bool) {
    let mut payloads = Vec::new();
    let mut payload = String::from(open);
    let mut oversized = false;
    for actor in actors {
        // Even without escaping, a key needs two quotes around it.
        if actor.as_str().len() + 2 + open.len() + close.len() > MAX_BYTES {
            oversized = true;
            continue;
        }
        let key = serde_json::Value::from(actor.as_str()).to_string();
        if key.len() + open.len() + close.len() > MAX_BYTES {
            oversized = true;
            continue;
        }
        let separator = usize::from(payload.len() > open.len());
        if payload.len() + separator + key.len() + close.len() > MAX_BYTES {
            payload.push_str(close);
            payloads.push(std::mem::replace(&mut payload, String::from(open)));
        }
        if payload.len() > open.len() {
            payload.push(',');
        }
        payload.push_str(&key);
    }
    if payload.len() > open.len() {
        payload.push_str(close);
        payloads.push(payload);
    }
    (payloads, oversized)
}

/// Decode one complete envelope. Reject an oversized, malformed, unknown
/// or empty envelope, or an array containing any invalid actor key: no
/// fragment of an invalid hint is treated as an actor identity.
#[must_use]
pub fn decode(payload: &str) -> Option<NodeWakeEvent> {
    if payload.len() > MAX_BYTES {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(payload).ok()? {
        serde_json::Value::String(hint) => match hint.as_str() {
            "ready" => Some(NodeWakeEvent::Ready),
            "poll_store" => Some(NodeWakeEvent::PollStore),
            _ => None,
        },
        serde_json::Value::Array(keys) => actor_keys(&keys).map(NodeWakeEvent::Owned),
        serde_json::Value::Object(hint) if hint.len() == 1 => {
            let serde_json::Value::Array(keys) = hint.get("appended")? else {
                return None;
            };
            actor_keys(keys).map(NodeWakeEvent::Appended)
        }
        _ => None,
    }
}

/// A non-empty list of actor keys, every one valid.
fn actor_keys(keys: &[serde_json::Value]) -> Option<Vec<ActorKey>> {
    if keys.is_empty() {
        return None;
    }
    keys.iter()
        .map(|key| ActorKey::parse(key.as_str()?).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-5555: malformed hints cannot invent identities by partially
    /// decoding their valid fragments; unknown signals never request work.
    #[test]
    fn malformed_envelopes_are_refused_whole() {
        for payload in [
            "",
            "[]",
            "s/a\np/b",
            "[\"s/a\",\"invalid\"]",
            "[\"s/a\",null]",
            "[\"s/\"]",
            "[\"s/a\"] trailing",
            "\"unknown\"",
            "{\"appended\":[]}",
            "{\"appended\":[\"s/a\",\"invalid\"]}",
            "{\"appended\":[\"s/a\"],\"owned\":[\"s/a\"]}",
            "{\"grew\":[\"s/a\"]}",
        ] {
            assert_eq!(decode(payload), None, "invalid envelope {payload:?}");
        }
        assert_eq!(decode(&format!("[\"s/{}\"]", "x".repeat(MAX_BYTES))), None);
    }
}
