//! Content references within one handler's effect journal (FIG-4850).
//!
//! Only entries actually served by the engine populate the dictionary. A cold
//! attempt rebuilds it as it replays; an uncommitted run never supplies a ref.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt;
use lash_core::{RuntimeEffectControllerError, RuntimeErrorCode};
use lash_sansio::core_support::blake3_domain_hash_hex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::effect_journal::JournaledEntry;

const MIN_PAYLOAD_BYTES: usize = 1024;
/// version_surface = "coexist"
/// version_guard(items(PAYLOAD_DOMAIN, digest))
const PAYLOAD_DOMAIN: &str = "lash-journal-payload/v1";

type Dictionary = BTreeMap<String, Arc<str>>;

#[derive(Default)]
pub(super) struct JournalPayloads(Mutex<Dictionary>);

/// Paths address strings in the record. An offset addresses a JSON string
/// literal inside the canonical envelope, preserving its exact byte order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PayloadReference {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,
    digest: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct PayloadEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) payload_encoding_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    payload_references: Vec<PayloadReference>,
    #[serde(flatten)]
    pub(super) body: Value,
}

impl JournalPayloads {
    pub(super) fn encode(&self, entry: JournaledEntry) -> PayloadEntry {
        let mut body = match serde_json::to_value(entry) {
            Ok(body) => body,
            Err(error) => return PayloadEntry::failed(error.to_string()),
        };
        let retained = self.0.lock_recover();
        let mut dictionary = Dictionary::new();
        let mut references = Vec::new();
        let mut encoding_error = None;
        visit(&mut body, "", &mut |value, path| {
            if path == "/envelope/json" {
                if let Err(error) =
                    encode_envelope(value, path, &retained, &mut dictionary, &mut references)
                {
                    encoding_error = Some(error);
                }
            } else if value.len() >= MIN_PAYLOAD_BYTES {
                let digest = digest(value);
                let reference = PayloadReference {
                    path: path.to_owned(),
                    offset: None,
                    digest: digest.clone(),
                };
                if (retained.contains_key(&digest) || dictionary.contains_key(&digest))
                    && saves_bytes(&reference, value.len())
                {
                    references.push(reference);
                    *value = digest;
                } else {
                    dictionary.insert(digest, Arc::from(value.as_str()));
                }
            }
        });
        if let Some(error) = encoding_error {
            return PayloadEntry::failed(error);
        }
        PayloadEntry {
            payload_encoding_error: None,
            body,
            payload_references: references,
        }
    }

    pub(super) fn decode(
        &self,
        mut entry: PayloadEntry,
    ) -> Result<JournaledEntry, RuntimeEffectControllerError> {
        if let Some(error) = entry.payload_encoding_error {
            return Err(invalid(error));
        }
        let mut retained = self.0.lock_recover();
        let mut dictionary = Dictionary::new();
        visit(&mut entry.body, "", &mut |value, path| {
            if path == "/envelope/json" {
                for (_, _, text) in json_strings(value) {
                    remember(&mut dictionary, &text);
                }
            } else {
                remember(&mut dictionary, value);
            }
        });
        // Reverse order keeps offsets valid when several literals of one
        // canonical envelope expand. JSON pointers do not depend on order.
        for reference in entry.payload_references.into_iter().rev() {
            let payload = dictionary
                .get(&reference.digest)
                .or_else(|| retained.get(&reference.digest))
                .ok_or_else(|| {
                    invalid(format!(
                        "journal payload `{}` has no prior entry",
                        reference.digest
                    ))
                })?;
            let value = entry
                .body
                .pointer_mut(&reference.path)
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(|| {
                    invalid(format!(
                        "journal payload path `{}` is not a string",
                        reference.path
                    ))
                })?;
            let restored = if let Some(offset) = reference.offset {
                let encoded = serde_json::to_string(&reference.digest)
                    .map_err(|error| invalid(error.to_string()))?;
                let end = offset
                    .checked_add(encoded.len())
                    .ok_or_else(|| invalid("journal payload offset overflow"))?;
                if value.get(offset..end) != Some(encoded.as_str()) {
                    return Err(invalid("journal payload offset does not name its digest"));
                }
                let encoded_payload = serde_json::to_string(payload.as_ref())
                    .map_err(|error| invalid(error.to_string()))?;
                let mut restored = value;
                restored.replace_range(offset..end, &encoded_payload);
                restored
            } else {
                if value != reference.digest {
                    return Err(invalid("journal payload path does not name its digest"));
                }
                payload.to_string()
            };
            *entry
                .body
                .pointer_mut(&reference.path)
                .ok_or_else(|| invalid("journal payload path vanished"))? = Value::String(restored);
        }
        let decoded =
            serde_json::from_value(entry.body).map_err(|error| invalid(error.to_string()))?;
        retained.extend(dictionary);
        Ok(decoded)
    }
}

impl PayloadEntry {
    fn failed(error: String) -> Self {
        Self {
            payload_encoding_error: Some(error),
            payload_references: Vec::new(),
            body: Value::Object(Default::default()),
        }
    }
}

fn saves_bytes(reference: &PayloadReference, payload_bytes: usize) -> bool {
    // Include the digest's replacement literal and the reference field's
    // framing, so compression cannot turn a budgeted entry into a larger one.
    serde_json::to_vec(reference).is_ok_and(|bytes| bytes.len() + 96 < payload_bytes)
}

fn invalid(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant,
        message,
    )
}

fn digest(text: &str) -> String {
    blake3_domain_hash_hex(PAYLOAD_DOMAIN, text.as_bytes())
}

fn remember(dictionary: &mut Dictionary, text: &str) {
    if text.len() >= MIN_PAYLOAD_BYTES {
        dictionary
            .entry(digest(text))
            .or_insert_with(|| Arc::from(text));
    }
}

fn visit(value: &mut Value, path: &str, visitor: &mut impl FnMut(&mut String, &str)) {
    match value {
        Value::String(text) => visitor(text, path),
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                visit(item, &format!("{path}/{index}"), visitor);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields.iter_mut() {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                visit(value, &format!("{path}/{escaped}"), visitor);
            }
        }
        _ => {}
    }
}

/// The canonical envelope is JSON inside a string. Keep its original token
/// order and escaping: parsing it into a map would change its canonical hash.
fn json_strings(json: &str) -> Vec<(usize, usize, String)> {
    let bytes = json.as_bytes();
    let mut strings = Vec::new();
    let mut cursor = 0;
    while let Some(byte) = bytes.get(cursor) {
        if *byte != b'"' {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while let Some(byte) = bytes.get(cursor) {
            match byte {
                b'\\' => cursor += 2,
                b'"' => {
                    cursor += 1;
                    if let Some(literal) = json.get(start..cursor)
                        && let Ok(text) = serde_json::from_str::<String>(literal)
                    {
                        strings.push((start, cursor, text));
                    }
                    break;
                }
                _ => cursor += 1,
            }
        }
    }
    strings
}

fn encode_envelope(
    json: &mut String,
    path: &str,
    retained: &Dictionary,
    dictionary: &mut Dictionary,
    references: &mut Vec<PayloadReference>,
) -> Result<(), String> {
    let mut encoded = String::new();
    let mut copied = 0;
    for (start, end, text) in json_strings(json) {
        if text.len() < MIN_PAYLOAD_BYTES {
            continue;
        }
        let digest = digest(&text);
        let reference = PayloadReference {
            path: path.to_owned(),
            offset: Some(encoded.len() + start - copied),
            digest: digest.clone(),
        };
        if (retained.contains_key(&digest) || dictionary.contains_key(&digest))
            && saves_bytes(&reference, text.len())
        {
            encoded.push_str(
                json.get(copied..start)
                    .ok_or("invalid envelope payload span")?,
            );
            references.push(reference);
            encoded.push('"');
            encoded.push_str(&digest);
            encoded.push('"');
            copied = end;
        } else {
            dictionary.insert(digest, Arc::from(text));
        }
    }
    if copied != 0 {
        encoded.push_str(json.get(copied..).ok_or("invalid envelope payload tail")?);
        *json = encoded;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::effect_journal::{JournaledEffectRecord, RecordedRuntimeEffect};
    use super::*;
    use lash_core::{
        EffectAddress, ExecutionScope, RuntimeAttribution, RuntimeEffectCommand,
        RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectOutcome,
    };

    fn recorded(key: &str, payload: &str) -> JournaledEntry {
        let response = lash_core::LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: payload.to_owned(),
                response_meta: None,
            }],
            ..Default::default()
        };
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(ExecutionScope::turn("session", "root"), key).expect("address"),
                RuntimeAttribution::for_turn("session", "root", 1, 0),
                key,
            ),
            RuntimeEffectCommand::AssistantResponseHooks {
                response: Box::new(response.clone()),
                plan: Default::default(),
                stream_hook_states: Vec::new(),
            },
        );
        JournaledEntry {
            build_generation: None,
            record: JournaledEffectRecord::Recorded(RecordedRuntimeEffect {
                envelope: Arc::new(envelope.canonical_form().expect("canonical envelope")),
                outcome: Ok(RuntimeEffectOutcome::AssistantResponseHooks {
                    response: Box::new(response),
                    events: Vec::new(),
                }),
            }),
        }
    }

    #[test]
    fn replay_reconstructs_exact_envelope_bytes_and_payloads_from_prior_entries() {
        let payload = "quoted \"\\\n雪 🦀".repeat(2048);
        let writer = JournalPayloads::default();
        let first = recorded("first", &payload);
        let expected_first = serde_json::to_value(&first).expect("expected");
        let encoded_first = writer.encode(first);
        let first_bytes = serde_json::to_vec(&encoded_first).expect("stored entry");
        let decoded = writer.decode(encoded_first).expect("serve first entry");
        assert_eq!(
            serde_json::to_value(decoded).expect("decoded"),
            expected_first
        );
        let second = recorded("second", &payload);
        let expected_second = serde_json::to_value(&second).expect("expected");
        let second_bytes = serde_json::to_vec(&writer.encode(second)).expect("stored entry");
        assert!(second_bytes.len() < 2048, "a reference has fixed overhead");
        // All resident state is gone. Only the engine's journal survives.
        let reader = JournalPayloads::default();
        reader
            .decode(serde_json::from_slice(&first_bytes).expect("first wire entry"))
            .expect("cold first entry");
        let decoded = reader
            .decode(serde_json::from_slice(&second_bytes).expect("second wire entry"))
            .expect("cold reference");
        assert_eq!(
            serde_json::to_value(decoded).expect("decoded"),
            expected_second
        );
    }

    #[test]
    fn a_missing_prior_payload_is_refused_before_replay_uses_the_outcome() {
        let payload = "payload".repeat(2048);
        let writer = JournalPayloads::default();
        writer
            .decode(writer.encode(recorded("first", &payload)))
            .expect("serve first");
        let reference = writer.encode(recorded("second", &payload));
        let error = JournalPayloads::default()
            .decode(reference)
            .expect_err("missing payload refuses");
        assert_eq!(
            error.code,
            RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant
        );
    }
    #[test]
    fn an_uncommitted_entry_never_supplies_a_later_digest_reference() {
        let payload = "uncommitted".repeat(2048);
        let writer = JournalPayloads::default();
        let _lost = writer.encode(recorded("lost", &payload));
        let next = writer.encode(recorded("next", &payload));
        // There is no prior journal entry to replay. This entry must retain
        // the payload itself, including for its own second occurrence.
        let reader = JournalPayloads::default();
        assert!(reader.decode(next).is_ok());
    }
}
