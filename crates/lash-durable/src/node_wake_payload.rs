//! The shared SQLite-row / PostgreSQL-NOTIFY wake envelope (FIG-5555).
//!
//! A JSON array of complete actor-key strings carries owned-mail hints;
//! the JSON strings `"ready"` and `"poll_store"` carry the two scan hints.
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

/// Encode owned-mail hints, splitting at the encoded byte bound. Emit at
/// most one [`NodeWakeEvent::PollStore`] hint for keys that cannot fit alone.
#[must_use]
pub fn owned(actors: &BTreeSet<ActorKey>) -> Vec<String> {
    let mut payloads = Vec::new();
    let mut payload = String::from("[");
    let mut poll_store = false;
    for actor in actors {
        // Even without escaping, a key needs two quotes and two brackets.
        if actor.as_str().len() > MAX_BYTES - 4 {
            poll_store = true;
            continue;
        }
        let key = serde_json::Value::from(actor.as_str()).to_string();
        if key.len() + 2 > MAX_BYTES {
            poll_store = true;
            continue;
        }
        let separator = usize::from(payload.len() > 1);
        if payload.len() + separator + key.len() + 1 > MAX_BYTES {
            payload.push(']');
            payloads.push(std::mem::replace(&mut payload, String::from("[")));
        }
        if payload.len() > 1 {
            payload.push(',');
        }
        payload.push_str(&key);
    }
    if payload.len() > 1 {
        payload.push(']');
        payloads.push(payload);
    }
    if poll_store {
        payloads.push(POLL_STORE.to_owned());
    }
    payloads
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
        serde_json::Value::Array(keys) if !keys.is_empty() => keys
            .iter()
            .map(|key| ActorKey::parse(key.as_str()?).ok())
            .collect::<Option<Vec<_>>>()
            .map(NodeWakeEvent::Owned),
        _ => None,
    }
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
        ] {
            assert_eq!(decode(payload), None, "invalid envelope {payload:?}");
        }
        assert_eq!(decode(&format!("[\"s/{}\"]", "x".repeat(MAX_BYTES))), None);
    }
}
