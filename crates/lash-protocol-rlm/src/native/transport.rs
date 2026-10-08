//! A native reply's provider exchange is ordinary committed history: the
//! assistant message that carried the reply's parts, and for a call that ran
//! nothing the tool results that answered it. A reply whose `execute_code`
//! call ran names its cell in the message's origin; the cell's own record is
//! the channel-independent trajectory entry. The prompt projects each
//! exchange as a complete call/result pair or not at all.
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole};
use lash_core::session_model::{Message, MessageRole, shared_parts};
use lash_core::{MessageOrigin, Part, PartKind};
use lash_sansio::TurnId;
use std::collections::{HashMap, HashSet};

/// The assistant message of a reply whose `execute_code` call ran as the cell
/// `cell_id`: the reply's parts, unchanged, so provider replay material
/// survives.
pub(super) fn cell_context_message(
    turn_id: &TurnId,
    message_id: String,
    cell_id: String,
    parts: Vec<Part>,
) -> Message {
    Message {
        id: message_id,
        role: MessageRole::Assistant,
        parts: shared_parts(parts),
        origin: Some(MessageOrigin::TurnOutput {
            turn_id: turn_id.clone(),
            source: lash_core::TurnOutputSource::Plugin {
                plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            },
            cell_id: Some(cell_id),
        }),
        reply_marker: None,
    }
}

/// The exchange of a reply whose calls ran nothing: the assistant message
/// with the reply's parts, and one tool result per call carrying `text`, the
/// correction the model reads. The results pair with their calls by
/// `call_id` and speak in the user role, where every tool result does,
/// under the protocol's own origin.
pub(super) fn refused_call_messages(
    turn_id: &TurnId,
    call_message_id: String,
    result_message_id: String,
    parts: Vec<Part>,
    text: String,
) -> [Message; 2] {
    let results = parts
        .iter()
        .filter(|part| part.kind() == PartKind::ToolCall)
        .enumerate()
        .filter_map(|(index, part)| {
            Some(Part::tool_result(
                format!("{result_message_id}.p{index}"),
                vec![lash_core::facade_support::ModelToolReturnPart::text(&text)],
                part.call_id()?.clone(),
                part.tool_name().unwrap_or_default().to_string(),
            ))
        })
        .collect::<Vec<_>>();
    [
        Message {
            id: call_message_id,
            role: MessageRole::Assistant,
            parts: shared_parts(parts),
            origin: Some(MessageOrigin::TurnOutput {
                turn_id: turn_id.clone(),
                source: lash_core::TurnOutputSource::Plugin {
                    plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                },
                cell_id: None,
            }),
            reply_marker: None,
        },
        Message {
            id: result_message_id,
            role: MessageRole::User,
            parts: shared_parts(results),
            origin: Some(MessageOrigin::Plugin {
                plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                transient: false,
            }),
            reply_marker: None,
        },
    ]
}

/// The cell a protocol-authored assistant message is the context of.
fn context_cell_id(origin: Option<&MessageOrigin>) -> Option<&str> {
    match origin {
        Some(MessageOrigin::TurnOutput {
            source: lash_core::TurnOutputSource::Plugin { plugin_id },
            cell_id: Some(cell_id),
            ..
        }) if plugin_id == crate::plugin::RLM_PROTOCOL_PLUGIN_ID => Some(cell_id),
        _ => None,
    }
}

fn has_part(parts: &[Part], kind: PartKind) -> bool {
    parts.iter().any(|part| part.kind() == kind)
}

/// The correction a refused call's results carry.
fn refusal_text(parts: &[Part]) -> Option<&str> {
    parts
        .iter()
        .filter_map(Part::tool_result_content)
        .flatten()
        .find_map(|block| match block {
            lash_core::facade_support::ModelToolReturnPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
}

/// Whether a committed message is part of a native provider exchange: a
/// protocol-authored message carrying a tool call or a tool result. Such a
/// message is replayed as its call/result pair and is never prose.
pub(crate) fn is_exchange_message(origin: Option<&MessageOrigin>, parts: &[Part]) -> bool {
    crate::projection::is_rlm_protocol_output(origin)
        && (has_part(parts, PartKind::ToolCall) || has_part(parts, PartKind::ToolResult))
}

/// One render's native exchanges, shared by execution lookup, chronological
/// refusal rendering and failure scrubbing.
pub(super) struct NativeExchangeIndex<'a> {
    /// Every exchange message, by chronological index.
    messages: HashSet<usize>,
    /// The reply parts of each executed cell, by cell id.
    cells: HashMap<&'a str, &'a [Part]>,
    /// Each refused call, at its assistant message: the reply's parts and
    /// the correction that answered them.
    refusals: HashMap<usize, (&'a [Part], &'a str)>,
}

impl<'a> NativeExchangeIndex<'a> {
    pub(super) fn new(
        chronological: &'a lash_core::facade_support::ChronologicalProjection,
    ) -> Self {
        let mut index = Self {
            messages: HashSet::new(),
            cells: HashMap::new(),
            refusals: HashMap::new(),
        };
        // A refused call waits for the results that answer it; anything else
        // between them means it has none.
        let mut unanswered: Option<(usize, &'a [Part])> = None;
        for entry in chronological.entries() {
            let lash_core::facade_support::ChronologicalPayload::Message(message) = &entry.payload
            else {
                continue;
            };
            let call = unanswered.take();
            if !is_exchange_message(message.origin.as_ref(), &message.parts) {
                continue;
            }
            index.messages.insert(entry.index);
            match message.role {
                MessageRole::Assistant => match context_cell_id(message.origin.as_ref()) {
                    Some(cell_id) => {
                        index.cells.entry(cell_id).or_insert(&message.parts);
                    }
                    None => unanswered = Some((entry.index, &message.parts)),
                },
                _ => {
                    if let (Some((call_index, parts)), Some(text)) =
                        (call, refusal_text(&message.parts))
                    {
                        index.refusals.insert(call_index, (parts, text));
                    }
                }
            }
        }
        index
    }

    /// Whether the entry is an exchange message, rendered only as its pair.
    pub(super) fn contains(&self, entry: usize) -> bool {
        self.messages.contains(&entry)
    }

    pub(super) fn execution_parts(&self, cell_id: &str) -> Option<&'a [Part]> {
        self.cells.get(cell_id).copied()
    }

    /// The refused call whose assistant message is `entry`.
    pub(super) fn refusal(&self, entry: usize) -> Option<(&'a [Part], &'a str)> {
        self.refusals.get(&entry).copied()
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
