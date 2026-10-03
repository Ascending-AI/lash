//! Content references within one handler's effect journal (FIG-4850).
//!
//! Only entries actually served by the engine populate the dictionary. A cold
//! attempt rebuilds it as it replays; an uncommitted run never supplies a ref.

use std::collections::BTreeMap;
use std::sync::Mutex;

use lash_core::store::plugin_writers::PluginRevision;
use lash_core::sync::MutexExt;
use lash_core::tool_run::{
    MaterialEntry, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRef, MaterialRefusal,
    MaterialRole,
};
use lash_core::{ExecutionScope, RuntimeEffectControllerError, RuntimeErrorCode};
use lash_sansio::core_support::blake3_domain_hash_hex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::effect_journal::JournaledEntry;

const MIN_PAYLOAD_BYTES: usize = 1024;
/// version_surface = "coexist"
/// version_guard(items(PAYLOAD_DOMAIN, digest))
const PAYLOAD_DOMAIN: &str = "lash-journal-payload/v1";

#[derive(Default)]
struct Dictionary {
    entries: BTreeMap<MaterialRef, MaterialEntry>,
    content: BTreeMap<(MaterialOwner, String), MaterialRef>,
}

#[derive(Default)]
pub(super) struct JournalPayloads(Mutex<Dictionary>);

/// Paths address strings in the record. An offset addresses a JSON string
/// literal inside the canonical envelope, preserving its exact byte order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PayloadReference {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,
    material: MaterialRef,
}

#[derive(Serialize, Deserialize)]
pub(super) struct PayloadEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) payload_encoding_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    payload_references: Vec<PayloadReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    materials: Vec<MaterialEntry>,
    #[serde(flatten)]
    pub(super) body: Value,
}

/// The scope and codec binding come from the reconstructed command, not from
/// the reference a journal happened to return.
struct Binding {
    owner: Option<MaterialOwner>,
    source: Option<MaterialOwner>,
    revision: Option<PluginRevision>,
    presentation: bool,
}

impl Binding {
    fn of(json: &str) -> Result<Self, RuntimeEffectControllerError> {
        let envelope: Value =
            serde_json::from_str(json).map_err(|error| invalid(error.to_string()))?;
        let scope: ExecutionScope = serde_json::from_value(
            envelope
                .pointer("/invocation/address/execution_scope")
                .cloned()
                .ok_or_else(|| invalid("material has no execution scope"))?,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let owner = match scope {
            ExecutionScope::Process { process_id } => Some(MaterialOwner::Process { process_id }),
            ExecutionScope::Turn {
                session_id,
                turn_id,
            } => Some(MaterialOwner::Run {
                opener: lash_core::EffectOpener::turn(session_id, turn_id),
            }),
            ExecutionScope::SessionOperation {
                session_id,
                operation_id,
            } => Some(MaterialOwner::Run {
                opener: lash_core::EffectOpener::session_operation(session_id, operation_id),
            }),
            ExecutionScope::SessionDelete { .. } | ExecutionScope::RuntimeOperation { .. } => None,
        };
        let command = envelope
            .get("command")
            .ok_or_else(|| invalid("material has no command"))?;
        let source = match command.get("type").and_then(Value::as_str) {
            Some("await_event" | "peek_await_event") => command
                .get("key")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| invalid(error.to_string()))?
                .map(|source| MaterialOwner::Source { source }),
            _ => None,
        };
        let revision = command
            .pointer("/execution_grant/owner")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| invalid(error.to_string()))?;
        Ok(Self {
            owner,
            source,
            revision,
            presentation: command.get("type").and_then(Value::as_str)
                == Some("present_tool_result"),
        })
    }

    fn owner(&self, path: &str) -> Option<&MaterialOwner> {
        if path.starts_with("/outcome/") && self.source.is_some() {
            self.source.as_ref()
        } else {
            self.owner.as_ref()
        }
    }

    fn role(&self, path: &str) -> MaterialRole {
        if path == "/envelope/json" {
            MaterialRole::PreparedRequest
        } else if self.presentation {
            MaterialRole::Presentation
        } else {
            MaterialRole::AttemptOutput
        }
    }
}

impl JournalPayloads {
    pub(super) fn encode(&self, entry: JournaledEntry) -> PayloadEntry {
        let mut body = match serde_json::to_value(entry) {
            Ok(body) => body,
            Err(error) => return PayloadEntry::failed(error.to_string()),
        };
        let Some(json) = body.pointer("/envelope/json").and_then(Value::as_str) else {
            return PayloadEntry {
                payload_encoding_error: None,
                payload_references: Vec::new(),
                materials: Vec::new(),
                body,
            };
        };
        let binding = match Binding::of(json) {
            Ok(binding) => binding,
            Err(error) => return PayloadEntry::failed(error.to_string()),
        };
        let retained = self.0.lock_recover();
        let mut encoding = Encoding {
            binding: &binding,
            retained: &retained,
            dictionary: Dictionary::default(),
            materials: Vec::new(),
            references: Vec::new(),
        };
        let mut encoding_error = None;
        visit(&mut body, "", &mut |value, path| {
            let result = if path == "/envelope/json" {
                encode_envelope(value, &mut |text, offset| {
                    encoding.payload(text, path, Some(offset))
                })
            } else {
                encoding.payload(value, path, None)
            };
            if let Err(error) = result {
                encoding_error = Some(error.to_string());
            }
        });
        if let Some(error) = encoding_error {
            return PayloadEntry::failed(error);
        }
        PayloadEntry {
            payload_encoding_error: None,
            body,
            payload_references: encoding.references,
            materials: encoding.materials,
        }
    }

    pub(super) fn decode(
        &self,
        mut entry: PayloadEntry,
        reconstructed: &str,
    ) -> Result<JournaledEntry, RuntimeEffectControllerError> {
        if let Some(error) = entry.payload_encoding_error {
            return Err(invalid(error));
        }
        let binding = Binding::of(reconstructed)?;
        let available: Vec<_> = binding.revision.iter().cloned().collect();
        let mut retained = self.0.lock_recover();
        let mut dictionary = Dictionary::default();
        for material in entry.materials {
            dictionary.insert(material)?;
        }
        // Reverse order keeps offsets valid when several literals of one
        // canonical envelope expand. JSON pointers do not depend on order.
        for reference in entry.payload_references.into_iter().rev() {
            let owner = binding
                .owner(&reference.path)
                .ok_or_else(|| invalid("administrative material has no owner"))?;
            let dictionary = if dictionary.entries.contains_key(&reference.material) {
                &dictionary
            } else {
                &retained
            };
            let payload = dictionary.read(&reference.material, owner, &available)?;
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
                let encoded = serde_json::to_string(reference.material.digest.as_str())
                    .map_err(|error| invalid(error.to_string()))?;
                let end = offset
                    .checked_add(encoded.len())
                    .ok_or_else(|| invalid("journal payload offset overflow"))?;
                if value.get(offset..end) != Some(encoded.as_str()) {
                    return Err(invalid("journal payload offset does not name its digest"));
                }
                let encoded_payload = serde_json::to_string(payload.text.as_str())
                    .map_err(|error| invalid(error.to_string()))?;
                let mut restored = value;
                restored.replace_range(offset..end, &encoded_payload);
                restored
            } else {
                if value != reference.material.digest.as_str() {
                    return Err(invalid("journal payload path does not name its digest"));
                }
                payload.text.clone()
            };
            *entry
                .body
                .pointer_mut(&reference.path)
                .ok_or_else(|| invalid("journal payload path vanished"))? = Value::String(restored);
        }
        let decoded =
            serde_json::from_value(entry.body).map_err(|error| invalid(error.to_string()))?;
        // Only successfully served journal records can supply future refs.
        retained.entries.extend(dictionary.entries);
        retained.content.extend(dictionary.content);
        Ok(decoded)
    }
}

impl Dictionary {
    fn insert(&mut self, material: MaterialEntry) -> Result<(), RuntimeEffectControllerError> {
        match &material {
            MaterialEntry::Available { reference, payload } => {
                // Integrity is checked before this record is ever used as a source.
                payload.verify(reference, &reference.owner, payload.revision.as_slice())?;
                self.content.insert(
                    (reference.owner.clone(), digest(&payload.text)),
                    reference.clone(),
                );
                self.entries.insert(reference.clone(), material);
            }
            MaterialEntry::Retired { reference } => {
                self.entries.insert(reference.clone(), material);
            }
        }
        Ok(())
    }

    fn read(
        &self,
        reference: &MaterialRef,
        owner: &MaterialOwner,
        available: &[PluginRevision],
    ) -> Result<&MaterialPayload, RuntimeEffectControllerError> {
        reference.verify(owner, &reference.digest)?;
        match self.entries.get(reference) {
            Some(MaterialEntry::Available { payload, .. }) => {
                payload.verify(reference, owner, available)?;
                Ok(payload)
            }
            Some(MaterialEntry::Retired { .. }) => Err(MaterialRefusal::Retired {
                reference: Box::new(reference.clone()),
            }
            .into()),
            None => Err(MaterialRefusal::Missing {
                reference: Box::new(reference.clone()),
            }
            .into()),
        }
    }
}

struct Encoding<'a> {
    binding: &'a Binding,
    retained: &'a Dictionary,
    dictionary: Dictionary,
    materials: Vec<MaterialEntry>,
    references: Vec<PayloadReference>,
}

impl Encoding<'_> {
    fn payload(
        &mut self,
        text: &mut String,
        path: &str,
        offset: Option<usize>,
    ) -> Result<(), RuntimeEffectControllerError> {
        if text.len() < MIN_PAYLOAD_BYTES {
            return Ok(());
        }
        let Some(owner) = self.binding.owner(path) else {
            return Ok(());
        };
        let key = (owner.clone(), digest(text));
        let material = match self
            .dictionary
            .content
            .get(&key)
            .or_else(|| self.retained.content.get(&key))
        {
            Some(reference) => reference.clone(),
            None => {
                let payload = MaterialPayload::new(
                    owner.clone(),
                    self.binding.role(path),
                    // The dictionary stores native UTF-8, whose decoder is
                    // independent of the callback that produced it. A
                    // plugin-owned artifact codec supplies its own revision.
                    None,
                    text.clone(),
                );
                let reference = payload.reference(MaterialLocation::JournalLocal)?;
                let material = MaterialEntry::Available {
                    reference: reference.clone(),
                    payload: Box::new(payload),
                };
                self.dictionary.insert(material.clone())?;
                self.materials.push(material);
                reference
            }
        };
        *text = material.digest.to_string();
        self.references.push(PayloadReference {
            path: path.to_owned(),
            offset,
            material,
        });
        Ok(())
    }
}

impl PayloadEntry {
    fn failed(error: String) -> Self {
        Self {
            payload_encoding_error: Some(error),
            payload_references: Vec::new(),
            materials: Vec::new(),
            body: Value::Object(Default::default()),
        }
    }
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
    encode: &mut impl FnMut(&mut String, usize) -> Result<(), RuntimeEffectControllerError>,
) -> Result<(), RuntimeEffectControllerError> {
    let mut encoded = String::new();
    let mut copied = 0;
    for (start, end, mut text) in json_strings(json) {
        if text.len() < MIN_PAYLOAD_BYTES {
            continue;
        }
        encoded.push_str(
            json.get(copied..start)
                .ok_or_else(|| invalid("invalid envelope payload span"))?,
        );
        encode(&mut text, encoded.len())?;
        encoded
            .push_str(&serde_json::to_string(&text).map_err(|error| invalid(error.to_string()))?);
        copied = end;
    }
    if copied != 0 {
        encoded.push_str(
            json.get(copied..)
                .ok_or_else(|| invalid("invalid envelope payload tail"))?,
        );
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
    use std::sync::Arc;

    fn decode(
        reader: &JournalPayloads,
        entry: PayloadEntry,
    ) -> Result<JournaledEntry, RuntimeEffectControllerError> {
        let reconstructed = recorded("reconstruction", "");
        let JournaledEffectRecord::Recorded(recorded) = reconstructed.record else {
            unreachable!()
        };
        reader.decode(entry, recorded.envelope.json())
    }

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
        let decoded = decode(&writer, encoded_first).expect("serve first entry");
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
        decode(
            &reader,
            serde_json::from_slice(&first_bytes).expect("first wire entry"),
        )
        .expect("cold first entry");
        let decoded = decode(
            &reader,
            serde_json::from_slice(&second_bytes).expect("second wire entry"),
        )
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
        decode(&writer, writer.encode(recorded("first", &payload))).expect("serve first");
        let reference = writer.encode(recorded("second", &payload));
        let error =
            decode(&JournalPayloads::default(), reference).expect_err("missing payload refuses");
        assert_eq!(error.code, RuntimeErrorCode::RetainedResultRefused);
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
        assert!(decode(&reader, next).is_ok());
    }

    #[test]
    fn corrupt_prior_material_refuses_before_a_recorded_outcome_is_served() {
        let payload = "canonical-output".repeat(2048);
        let writer = JournalPayloads::default();
        decode(&writer, writer.encode(recorded("first", &payload)))
            .expect("serve canonical material");
        let reference = writer.encode(recorded("second", &payload));
        for entry in writer.0.lock_recover().entries.values_mut() {
            if let MaterialEntry::Available { payload, .. } = entry {
                payload.text = "corrupt-output".repeat(2048);
            }
        }
        let error =
            decode(&writer, reference).expect_err("corrupt material must never serve an outcome");
        assert_eq!(error.code.as_str(), "retained_result_refused");
    }
    #[test]
    fn retained_material_reconstructs_cold_and_retirement_never_falls_back() {
        let payload = "retained-output".repeat(2048);
        let writer = JournalPayloads::default();
        let original = recorded("first", &payload);
        let expected = serde_json::to_value(&original).unwrap();
        let mut entry = writer.encode(original);
        let artifact = lash_core::ArtifactName {
            store: lash_core::ArtifactStoreId::Engine("tool-material".into()),
            artifact_ref: "handover-0".into(),
        };
        for reference in &mut entry.payload_references {
            reference.material = reference.material.retained(artifact.clone());
        }
        for material in &mut entry.materials {
            if let MaterialEntry::Available { reference, .. } = material {
                *reference = reference.retained(artifact.clone());
            }
        }
        let wire = serde_json::to_vec(&entry).unwrap();
        let reader = JournalPayloads::default();
        let decoded = decode(&reader, serde_json::from_slice(&wire).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        let mut retired: PayloadEntry = serde_json::from_slice(&wire).unwrap();
        for material in &mut retired.materials {
            if let MaterialEntry::Available { reference, .. } = material {
                *material = MaterialEntry::Retired {
                    reference: reference.clone(),
                };
            }
        }
        let error = decode(&reader, retired).unwrap_err();
        assert!(
            matches!(error.cause, Some(lash_core::RuntimeErrorCause::MaterialRefused { refusal }) if matches!(*refusal, MaterialRefusal::Retired { .. }))
        );
    }

    #[test]
    fn wrong_owner_refuses_even_when_its_bytes_are_in_the_dictionary() {
        let writer = JournalPayloads::default();
        let mut reference = writer.encode(recorded("other-owner", &"output".repeat(2048)));
        for entry in &mut reference.payload_references {
            entry.material.owner = MaterialOwner::Run {
                opener: lash_core::EffectOpener::turn("session", "another-run"),
            };
        }
        let error = decode(&writer, reference).unwrap_err();
        assert!(
            matches!(error.cause, Some(lash_core::RuntimeErrorCause::MaterialRefused { refusal }) if matches!(*refusal, MaterialRefusal::WrongOwner { .. }))
        );
        assert!(
            writer.0.lock_recover().entries.is_empty(),
            "a failed entry cannot seed another reference"
        );
    }

    #[test]
    fn material_refusals_cover_process_source_format_and_revision() {
        use lash_core::store::plugin_writers::PluginRevision;
        let process_id = lash_core::ProcessId::parse("p_00000000000070008000000000000001").unwrap();
        let source = lash_core::AwaitEventKey {
            scope: ExecutionScope::turn("session", "root"),
            wait: lash_core::AwaitEventWaitIdentity::Custom {
                key: "completion".into(),
            },
            key_id: "source-key".into(),
            signature: "signature".into(),
        };
        let revision = PluginRevision::new("tool", std::num::NonZeroU32::MIN.into());
        for owner in [
            MaterialOwner::Process { process_id },
            MaterialOwner::Source { source },
        ] {
            let payload = MaterialPayload::new(
                owner.clone(),
                MaterialRole::AttemptOutput,
                Some(revision.clone()),
                "output".repeat(2048),
            );
            let reference = payload.reference(MaterialLocation::JournalLocal).unwrap();
            let mut dictionary = Dictionary::default();
            dictionary
                .insert(MaterialEntry::Available {
                    reference: reference.clone(),
                    payload: Box::new(payload.clone()),
                })
                .unwrap();
            assert_eq!(
                dictionary
                    .read(&reference, &owner, std::slice::from_ref(&revision))
                    .unwrap()
                    .text,
                payload.text
            );
            let error = dictionary.read(&reference, &owner, &[]).unwrap_err();
            assert!(
                matches!(error.cause, Some(lash_core::RuntimeErrorCause::MaterialRefused { refusal }) if matches!(*refusal, MaterialRefusal::RevisionMismatch { .. }))
            );
            let mut foreign_format = payload.clone();
            foreign_format.format += 1;
            let error = foreign_format
                .verify(&reference, &owner, std::slice::from_ref(&revision))
                .unwrap_err();
            assert!(
                matches!(error.cause, Some(lash_core::RuntimeErrorCause::MaterialRefused { refusal }) if matches!(*refusal, MaterialRefusal::FormatMismatch { .. }))
            );
            let mut foreign_role = payload;
            foreign_role.role = MaterialRole::PreparedRequest;
            let error = foreign_role
                .verify(&reference, &owner, std::slice::from_ref(&revision))
                .unwrap_err();
            assert!(
                matches!(error.cause, Some(lash_core::RuntimeErrorCause::MaterialRefused { refusal }) if matches!(*refusal, MaterialRefusal::RoleMismatch { .. }))
            );
        }
    }

    #[test]
    fn material_growth_is_canonical_once_and_coordination_is_fixed_size() {
        for width in [1, 2, 16] {
            let mut measurements = Vec::new();
            for bytes in [8192, 65536, 262144, 1048576] {
                let payload = "x".repeat(bytes);
                let writer = JournalPayloads::default();
                let reader = JournalPayloads::default();
                let mut coordination = 0;
                let mut journal = 0;
                let mut owners = 0;
                for ordinal in 0..width {
                    let original = recorded(&format!("call-{ordinal}"), &payload);
                    let expected = serde_json::to_value(&original).unwrap();
                    let encoded = writer.encode(original);
                    owners += encoded.materials.len();
                    coordination += serde_json::to_vec(&encoded.body).unwrap().len()
                        + serde_json::to_vec(&encoded.payload_references)
                            .unwrap()
                            .len();
                    let wire = serde_json::to_vec(&encoded).unwrap();
                    journal += wire.len();
                    decode(&writer, serde_json::from_slice(&wire).unwrap()).unwrap();
                    let replay = decode(&reader, serde_json::from_slice(&wire).unwrap()).unwrap();
                    assert_eq!(serde_json::to_value(replay).unwrap(), expected);
                }
                assert_eq!(owners, 1);
                eprintln!(
                    "K2 width={width} canonical_bytes={bytes} journal_bytes={journal} coordination_bytes={coordination} canonical_records={owners}"
                );
                measurements.push((bytes, journal, coordination));
            }
            for pair in measurements.windows(2) {
                assert_eq!(
                    pair[1].1 - pair[0].1,
                    pair[1].0 - pair[0].0,
                    "only canonical bytes grow"
                );
                assert_eq!(pair[1].2, pair[0].2, "coordination never copies the body");
            }
        }
    }

    #[test]
    fn presentation_reuses_attempt_material_and_owns_only_distinct_bytes() {
        let input = "prepared-input".repeat(2048);
        let output = "attempt-output".repeat(2048);
        let distinct = "presented-output".repeat(2048);
        let writer = JournalPayloads::default();
        let reader = JournalPayloads::default();
        let mut attempt = recorded("attempt", &input);
        let JournaledEffectRecord::Recorded(record) = &mut attempt.record else {
            unreachable!()
        };
        let Ok(RuntimeEffectOutcome::AssistantResponseHooks { response, .. }) = &mut record.outcome
        else {
            unreachable!()
        };
        response.parts = vec![lash_core::LlmOutputPart::Text {
            text: output.clone(),
            response_meta: None,
        }];
        let encoded = writer.encode(attempt);
        let roles: Vec<_> = encoded
            .materials
            .iter()
            .map(|entry| match entry {
                MaterialEntry::Available { reference, .. } => reference.role,
                MaterialEntry::Retired { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(
            roles,
            [MaterialRole::PreparedRequest, MaterialRole::AttemptOutput]
        );
        let wire = serde_json::to_vec(&encoded).unwrap();
        decode(&writer, encoded).unwrap();
        decode(&reader, serde_json::from_slice(&wire).unwrap()).unwrap();

        for text in [&output, &distinct] {
            let envelope = RuntimeEffectEnvelope::new(
                RuntimeEffectInvocation::new(
                    EffectAddress::new(ExecutionScope::turn("session", "root"), "present").unwrap(),
                    RuntimeAttribution::for_turn("session", "root", 1, 0),
                    "present",
                ),
                RuntimeEffectCommand::PresentToolResult {
                    call_id: format!("tc_{}", "0".repeat(64)).parse().unwrap(),
                    tool_id: "tool".into(),
                    tool_name: "tool".into(),
                    render: None,
                    args: serde_json::json!({ "input": input }),
                    output: Box::new(lash_core::ToolCallOutput::success(output.clone())),
                },
            );
            let reconstructed = envelope.canonical_form().unwrap();
            let original = JournaledEntry {
                build_generation: None,
                record: JournaledEffectRecord::Recorded(RecordedRuntimeEffect {
                    envelope: Arc::new(reconstructed.clone()),
                    outcome: Ok(RuntimeEffectOutcome::PresentToolResult {
                        presentation: Box::new(lash_core::runtime::ToolPresentation {
                            version: lash_core::TOOL_PRESENTATION_VERSION,
                            model_return: lash_core::facade_support::ModelToolReturn {
                                tool_name: "tool".into(),
                                parts: vec![lash_core::facade_support::ModelToolReturnPart::text(
                                    text.clone(),
                                )],
                                attachment_notices: Vec::new(),
                            },
                            artifacts: Vec::new(),
                            retention: Default::default(),
                        }),
                    }),
                }),
            };
            let expected = serde_json::to_value(&original).unwrap();
            let encoded = writer.encode(original);
            if text == &output {
                assert!(encoded.materials.is_empty(), "V reuses the recorded X");
            } else {
                assert!(
                    matches!(encoded.materials.as_slice(), [MaterialEntry::Available { reference, payload }] if reference.role == MaterialRole::Presentation && payload.text == distinct)
                );
            }
            let wire = serde_json::to_vec(&encoded).unwrap();
            writer.decode(encoded, reconstructed.json()).unwrap();
            let replay = reader
                .decode(serde_json::from_slice(&wire).unwrap(), reconstructed.json())
                .unwrap();
            assert_eq!(serde_json::to_value(replay).unwrap(), expected);
        }
    }
}
