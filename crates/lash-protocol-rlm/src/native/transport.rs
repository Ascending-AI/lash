//! Transport envelopes are stored separately from the channel-independent trajectory.
//! Each projection emits a complete call/result exchange or neither side.
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
use lash_core::{Part, PartKind, SessionHistoryRecord};
use lash_rlm_types::{RlmDiagnosticEvent, RlmProtocolEvent};

const PHASE: &str = "native_transport";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Envelope {
    Execution {
        step_id: String,
        parts: Vec<Part>,
    },
    Repair {
        turn_id: String,
        protocol_iteration: usize,
        parts: Vec<Part>,
        text: String,
    },
}

fn event(envelope: Envelope) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(crate::projection::rlm_protocol_event(
        RlmProtocolEvent::RlmDiagnostic(RlmDiagnosticEvent {
            phase: PHASE.to_string(),
            payload: serde_json::to_value(envelope).expect("native envelope serializes"),
        }),
    ))
}

pub(super) fn execution_event(step_id: String, parts: Vec<Part>) -> SessionHistoryRecord {
    event(Envelope::Execution { step_id, parts })
}
pub(super) fn repair_event(
    turn_id: &str,
    protocol_iteration: usize,
    parts: Vec<Part>,
    text: String,
) -> SessionHistoryRecord {
    event(Envelope::Repair {
        turn_id: turn_id.to_string(),
        protocol_iteration,
        parts,
        text,
    })
}
fn decode(event: &lash_core::ProtocolEvent) -> Option<Envelope> {
    match crate::projection::decode_rlm_protocol_event(event)? {
        RlmProtocolEvent::RlmDiagnostic(diagnostic) if diagnostic.phase == PHASE => Some(
            serde_json::from_value(diagnostic.payload)
                .expect("recorded native transport envelope must decode"),
        ),
        _ => None,
    }
}
pub(super) fn execution_parts(events: &[SessionHistoryRecord], id: &str) -> Option<Vec<Part>> {
    events.iter().find_map(|event| {
        let SessionHistoryRecord::Protocol(event) = event else {
            return None;
        };
        match decode(event)? {
            Envelope::Execution { step_id, parts } if step_id == id => Some(parts),
            _ => None,
        }
    })
}
pub(super) fn repair_parts(event: &lash_core::ProtocolEvent) -> Option<(Vec<Part>, String)> {
    match decode(event)? {
        Envelope::Repair { parts, text, .. } => Some((parts, text)),
        _ => None,
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
