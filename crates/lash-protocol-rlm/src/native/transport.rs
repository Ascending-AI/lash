//! Transport envelopes are stored separately from the channel-independent trajectory.
//! Each projection emits a complete call/result exchange or neither side.
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
use lash_core::{Part, PartKind, SessionHistoryRecord};
use lash_rlm_types::{RlmDiagnosticEvent, RlmProtocolEvent};
use lash_sansio::TurnId;
use std::collections::HashMap;

/// Schema version of the native RLM provider-call and repair envelopes
/// recorded in session history.
pub const NATIVE_TRANSPORT_VERSION: u32 = 1;

const PHASE: &str = "native_transport";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Transport {
    Execution {
        step_id: String,
        parts: Vec<Part>,
    },
    Repair {
        turn_id: TurnId,
        protocol_iteration: usize,
        parts: Vec<Part>,
        text: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub(super) enum DecodeError {
    #[error("native transport version {found} is newer than supported version {supported}")]
    NewerVersion { found: u32, supported: u32 },
    #[error("malformed native transport envelope: {0}")]
    Malformed(#[from] serde_json::Error),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    #[serde(default)]
    schema_version: u32,
    #[serde(flatten)]
    transport: Transport,
}

#[expect(
    clippy::expect_used,
    reason = "the envelope carries a u32 schema version and crate-owned transport data, so serde_json encoding cannot fail"
)]
fn event(transport: Transport, schema_version: u32) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(crate::projection::rlm_protocol_event(
        RlmProtocolEvent::RlmDiagnostic(RlmDiagnosticEvent {
            phase: PHASE.to_string(),
            payload: serde_json::to_value(Envelope {
                schema_version,
                transport,
            })
            .expect("native envelope serializes"),
        }),
    ))
}

pub(super) fn execution_event(
    step_id: String,
    parts: Vec<Part>,
    schema_version: u32,
) -> SessionHistoryRecord {
    event(Transport::Execution { step_id, parts }, schema_version)
}
pub(super) fn repair_event(
    turn_id: &TurnId,
    protocol_iteration: usize,
    parts: Vec<Part>,
    text: String,
    schema_version: u32,
) -> SessionHistoryRecord {
    event(
        Transport::Repair {
            turn_id: TurnId::from(turn_id.to_string()),
            protocol_iteration,
            parts,
            text,
        },
        schema_version,
    )
}
fn decode(event: &lash_core::ProtocolEvent) -> Result<Option<Transport>, DecodeError> {
    let Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) =
        crate::projection::decode_rlm_protocol_event(event)
    else {
        return Ok(None);
    };
    if diagnostic.phase != PHASE {
        return Ok(None);
    }
    decode_payload(diagnostic.payload).map(Some)
}

fn decode_payload(payload: serde_json::Value) -> Result<Transport, DecodeError> {
    #[cfg(test)]
    work::decoded();
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default)]
        schema_version: u32,
    }
    let version: Version = serde_json::from_value(payload.clone())?;
    if version.schema_version > NATIVE_TRANSPORT_VERSION {
        return Err(DecodeError::NewerVersion {
            found: version.schema_version,
            supported: NATIVE_TRANSPORT_VERSION,
        });
    }
    let envelope: Envelope = serde_json::from_value(payload)?;
    Ok(envelope.transport)
}

/// One render's native envelopes, shared by execution lookup, chronological
/// repair rendering and failure scrubbing. Refusals stay at their own entries.
pub(super) struct NativeTransportIndex {
    envelopes: HashMap<usize, Result<Transport, DecodeError>>,
    executions: HashMap<String, usize>,
}

impl NativeTransportIndex {
    pub(super) fn new(chronological: &lash_core::facade_support::ChronologicalProjection) -> Self {
        let mut index = Self {
            envelopes: HashMap::new(),
            executions: HashMap::new(),
        };
        for entry in chronological.entries() {
            #[cfg(test)]
            work::visited();
            let lash_core::facade_support::ChronologicalPayload::ProtocolEvent(event) =
                &entry.payload
            else {
                continue;
            };
            let Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) =
                crate::projection::decode_rlm_protocol_event(event)
            else {
                continue;
            };
            if diagnostic.phase != PHASE {
                continue;
            }
            let step_id = diagnostic
                .payload
                .get("step_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let envelope = decode_payload(diagnostic.payload);
            // A valid repair never claims execution authority, even with a raw
            // step_id. A refusal with that id does block later executions.
            if !matches!(&envelope, Ok(Transport::Repair { .. }))
                && let Some(step_id) = step_id
            {
                index.executions.entry(step_id).or_insert(entry.index);
            }
            index.envelopes.insert(entry.index, envelope);
        }
        index
    }

    pub(super) fn execution_parts(&self, id: &str) -> Result<Option<&[Part]>, &DecodeError> {
        match self
            .executions
            .get(id)
            .and_then(|entry| self.envelopes.get(entry))
        {
            Some(Ok(Transport::Execution { parts, .. })) => Ok(Some(parts)),
            Some(Err(error)) => Err(error),
            _ => Ok(None),
        }
    }

    pub(super) fn repair_parts(
        &self,
        entry: usize,
    ) -> Result<Option<(&[Part], &str)>, &DecodeError> {
        match self.envelopes.get(&entry) {
            Some(Ok(Transport::Repair { parts, text, .. })) => Ok(Some((parts, text))),
            Some(Err(error)) => Err(error),
            _ => Ok(None),
        }
    }
}

pub(super) fn repair_parts(
    event: &lash_core::ProtocolEvent,
) -> Result<Option<(Vec<Part>, String)>, DecodeError> {
    Ok(match decode(event)? {
        Some(Transport::Repair { parts, text, .. }) => Some((parts, text)),
        _ => None,
    })
}

pub(super) fn degraded_binding(error: impl std::fmt::Display) -> lash_core::DegradedBinding {
    lash_core::DegradedBinding {
        name: PHASE.to_string(),
        reason: error.to_string(),
    }
}

pub(super) fn append_pair(messages: &mut Vec<LlmMessage>, parts: &[Part], output: &str) {
    let mut assistant = Vec::new();
    let mut results = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for part in parts {
        match part.kind() {
            PartKind::ToolCall => {
                // The repair exchange answers the provider under the call's
                // own correlation.
                let Some(call_id) = part.provider_call_id() else {
                    continue;
                };
                // Duplicate ids are rejected by normalization. Preserve every original
                // Part durably; project one matching pair per id, which is the only
                // representable exchange accepted by provider transcript validators.
                if !ids.insert(call_id) {
                    continue;
                }
                assistant.push(LlmContentBlock::ToolCall {
                    call_id: call_id.to_string(),
                    tool_name: part.tool_name().unwrap_or_default().to_string(),
                    input_json: part.content().to_string(),
                    replay: part.tool_replay().cloned(),
                });
                results.push(LlmContentBlock::ToolResult {
                    call_id: call_id.to_string(),
                    tool_name: part.tool_name().map(str::to_string),
                    content: vec![lash_core::facade_support::ModelToolReturnPart::text(output)],
                });
            }
            PartKind::Reasoning => assistant.push(LlmContentBlock::Reasoning {
                text: part.content().to_string(),
                replay: part.reasoning_meta().cloned(),
            }),
            PartKind::Prose | PartKind::Text => assistant.push(LlmContentBlock::Text {
                text: part.content().to_string().into(),
                cache_breakpoint: false,
                response_meta: part.response_meta().cloned(),
            }),
            _ => {}
        }
    }
    if !results.is_empty() {
        messages.push(LlmMessage::new(LlmRole::Assistant, assistant));
        messages.push(LlmMessage::new(LlmRole::User, results));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn recorded(payload: serde_json::Value) -> lash_core::ProtocolEvent {
        crate::projection::rlm_protocol_event(RlmProtocolEvent::RlmDiagnostic(RlmDiagnosticEvent {
            phase: "native_transport".into(),
            payload,
        }))
    }
    #[test]
    fn envelope_version_pin_and_refusal_witness() {
        let SessionHistoryRecord::Protocol(event) =
            execution_event("step".into(), Vec::new(), NATIVE_TRANSPORT_VERSION)
        else {
            panic!()
        };
        let Some(RlmProtocolEvent::RlmDiagnostic(d)) =
            crate::projection::decode_rlm_protocol_event(&event)
        else {
            panic!()
        };
        assert_eq!(d.payload["schema_version"], 1);
        assert_eq!(d.payload["kind"], "execution");
        assert!(decode(&event).unwrap().is_some());
        assert!(matches!(
            decode(&recorded(serde_json::json!({"schema_version":2}))),
            Err(DecodeError::NewerVersion {
                found: 2,
                supported: 1
            })
        ));
        assert!(matches!(
            decode(&recorded(serde_json::json!("malformed bytes"))),
            Err(DecodeError::Malformed(_))
        ));
        assert!(
            decode(&recorded(
                serde_json::json!({"kind":"execution","step_id":"legacy","parts":[]})
            ))
            .unwrap()
            .is_some()
        );
    }
}

#[cfg(test)]
pub(super) mod work {
    use std::cell::Cell;
    std::thread_local! {
        static COUNTS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    }
    pub(in crate::native) fn reset() {
        COUNTS.set((0, 0));
    }
    pub(in crate::native) fn counts() -> (usize, usize) {
        COUNTS.get()
    }
    pub(super) fn visited() {
        let (visits, decodes) = COUNTS.get();
        COUNTS.set((visits + 1, decodes));
    }
    pub(super) fn decoded() {
        let (visits, decodes) = COUNTS.get();
        COUNTS.set((visits, decodes + 1));
    }
}
