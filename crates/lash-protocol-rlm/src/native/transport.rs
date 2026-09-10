//! Transport envelopes are stored separately from the channel-independent trajectory.
//! Each projection emits a complete call/result exchange or neither side.
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
use lash_core::{Part, PartKind, SessionHistoryRecord};
use lash_rlm_types::{RlmDiagnosticEvent, RlmProtocolEvent};
use lash_sansio::TurnId;

const NATIVE_TRANSPORT_VERSION: u32 = 1;

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

fn event(transport: Transport) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(crate::projection::rlm_protocol_event(
        RlmProtocolEvent::RlmDiagnostic(RlmDiagnosticEvent {
            phase: PHASE.to_string(),
            payload: serde_json::to_value(Envelope {
                schema_version: NATIVE_TRANSPORT_VERSION,
                transport,
            })
            .expect("native envelope serializes"),
        }),
    ))
}

pub(super) fn execution_event(step_id: String, parts: Vec<Part>) -> SessionHistoryRecord {
    event(Transport::Execution { step_id, parts })
}
pub(super) fn repair_event(
    turn_id: &TurnId,
    protocol_iteration: usize,
    parts: Vec<Part>,
    text: String,
) -> SessionHistoryRecord {
    event(Transport::Repair {
        turn_id: TurnId::from(turn_id.to_string()),
        protocol_iteration,
        parts,
        text,
    })
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
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default)]
        schema_version: u32,
    }
    let version: Version = serde_json::from_value(diagnostic.payload.clone())?;
    if version.schema_version > NATIVE_TRANSPORT_VERSION {
        return Err(DecodeError::NewerVersion {
            found: version.schema_version,
            supported: NATIVE_TRANSPORT_VERSION,
        });
    }
    let envelope: Envelope = serde_json::from_value(diagnostic.payload)?;
    Ok(Some(envelope.transport))
}
pub(super) fn execution_parts(
    events: &[SessionHistoryRecord],
    id: &str,
) -> Result<Option<Vec<Part>>, DecodeError> {
    for event in events {
        let SessionHistoryRecord::Protocol(event) = event else {
            continue;
        };
        // Unrelated corrupt bindings are reported at their chronological entry;
        // they must not suppress a healthy step's complete provider exchange.
        let Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) =
            crate::projection::decode_rlm_protocol_event(event)
        else {
            continue;
        };
        if diagnostic.phase != PHASE
            || diagnostic
                .payload
                .get("step_id")
                .and_then(serde_json::Value::as_str)
                != Some(id)
        {
            continue;
        }
        if let Some(Transport::Execution { step_id, parts }) = decode(event)?
            && step_id == id
        {
            return Ok(Some(parts));
        }
    }
    Ok(None)
}
pub(super) fn repair_parts(
    event: &lash_core::ProtocolEvent,
) -> Result<Option<(Vec<Part>, String)>, DecodeError> {
    Ok(match decode(event)? {
        Some(Transport::Repair { parts, text, .. }) => Some((parts, text)),
        _ => None,
    })
}

pub(super) fn degraded_binding(error: DecodeError) -> lash_core::DegradedBinding {
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
        match part.kind {
            PartKind::ToolCall => {
                let Some(call_id) = part.tool_call_id.as_ref() else {
                    continue;
                };
                // Duplicate ids are rejected by normalization. Preserve every original
                // Part durably; project one matching pair per id, which is the only
                // representable exchange accepted by provider transcript validators.
                if !ids.insert(call_id) {
                    continue;
                }
                assistant.push(LlmContentBlock::ToolCall {
                    call_id: call_id.clone(),
                    tool_name: part.tool_name.clone().unwrap_or_default(),
                    input_json: part.content.clone(),
                    replay: part.tool_replay.clone(),
                });
                results.push(LlmContentBlock::ToolResult {
                    call_id: call_id.clone(),
                    tool_name: part.tool_name.clone(),
                    content: output.to_string(),
                });
            }
            PartKind::Reasoning => assistant.push(LlmContentBlock::Reasoning {
                text: part.content.clone(),
                replay: part.reasoning_meta.clone(),
            }),
            PartKind::Prose | PartKind::Text => assistant.push(LlmContentBlock::Text {
                text: part.content.clone().into(),
                cache_breakpoint: false,
                response_meta: part.response_meta.clone(),
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
        let SessionHistoryRecord::Protocol(event) = execution_event("step".into(), Vec::new())
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
