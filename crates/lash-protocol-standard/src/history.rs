//! Standard mode history wording and the optional flat transcript view.
use lash_protocol_types::{AttachmentRef, ModelToolReturnPart};

use lash_protocol_types::{
    llm::types::LlmContentBlock, llm::types::LlmMessage, llm::types::LlmRole,
};

use lash_protocol_types::{
    session_model::message::HistoryPresentation, session_model::message::Message,
    session_model::message::MessageRole, session_model::message::Part,
    session_model::message::PartKind, session_model::message::RenderedPrompt,
};

use std::sync::{Arc, OnceLock};

/// Standard mode's flat tool-result view; attachments retain their slots.
pub fn tool_result_text(content: &[ModelToolReturnPart]) -> std::borrow::Cow<'_, str> {
    lash_render::tool_result_text(content.iter().map(ModelToolReturnPart::visible_text))
}

/// The standard mode's structured history policy.
pub fn presentation() -> HistoryPresentation {
    static POLICY: OnceLock<HistoryPresentation> = OnceLock::new();
    POLICY
        .get_or_init(|| {
            HistoryPresentation::new(|role, part| {
                let text = render_part_for_chat(role, part);
                if text.trim().is_empty() {
                    return text;
                }
                match role {
                    MessageRole::System => format!("Runtime note:\n{text}"),
                    MessageRole::Event => format!("Runtime event:\n{text}"),
                    _ => text,
                }
            })
        })
        .clone()
}

fn render_part(part: &Part) -> String {
    if matches!(part.kind(), PartKind::Attachment) {
        return if part.attachment().is_some() || part.content().trim().is_empty() {
            "[Attachment]".into()
        } else {
            part.content().into_owned()
        };
    }
    if let Some(content) = part.tool_result_content() {
        return tool_result_text(content).into_owned();
    }
    part.content().into_owned()
}

fn render_part_for_chat(role: MessageRole, part: &Part) -> String {
    let rendered = render_part(part);
    match (role, part) {
        (MessageRole::System, Part::Output { .. }) => format!("<output>\n{rendered}\n</output>"),
        (MessageRole::System, Part::Error { .. }) => format!("<error>\n{rendered}\n</error>"),
        (MessageRole::Assistant, Part::ToolCall { .. }) => {
            render_assistant_tool_call(part, &rendered)
        }
        _ => rendered,
    }
}

fn render_assistant_tool_call(part: &Part, rendered: &str) -> String {
    let tool_name = part.tool_name().unwrap_or("tool");
    let trimmed = rendered.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        format!("{tool_name}()")
    } else {
        format!("{tool_name}({trimmed})")
    }
}

fn attachment_from_part(part: &Part) -> Option<AttachmentRef> {
    if !matches!(part.kind(), PartKind::Attachment) {
        return None;
    }
    Some(part.attachment()?.reference.clone())
}

fn render_message_for_transcript(msg: &Message, attachments: &mut Vec<AttachmentRef>) -> String {
    let mut out = Vec::new();
    for part in msg.parts.iter() {
        // Reasoning items are display-only from the transcript's point of
        // view — they are never replayed as flat text. Provider adapters use
        // structured replay metadata when they can re-emit reasoning.
        if matches!(part.kind(), PartKind::Reasoning) {
            continue;
        }
        if let Some(attachment) = attachment_from_part(part) {
            attachments.push(attachment);
            out.push("[Attachment]".to_string());
            continue;
        }
        if let Some(content) = part.tool_result_content() {
            attachments.extend(
                content
                    .iter()
                    .filter_map(ModelToolReturnPart::attachment)
                    .cloned(),
            );
        }
        let rendered = render_part_for_chat(msg.role, part);
        if !rendered.trim().is_empty() {
            out.push(rendered);
        }
    }
    out.join("\n\n")
}

#[derive(Clone, Debug, Default)]
struct TranscriptTurn {
    user: Vec<String>,
    assistant: Vec<String>,
}

pub fn render_transcript_prompt(msgs: &[Message]) -> RenderedPrompt {
    let mut attachments = Vec::new();
    let mut turns = Vec::new();
    let mut current = TranscriptTurn::default();
    let mut has_current = false;

    for msg in msgs {
        let text = render_message_for_transcript(msg, &mut attachments);
        let has_text = !text.trim().is_empty();
        match msg.role {
            MessageRole::User | MessageRole::Event => {
                if has_current && (!current.user.is_empty() || !current.assistant.is_empty()) {
                    turns.push(current);
                    current = TranscriptTurn::default();
                }
                if has_text {
                    current
                        .user
                        .push(if matches!(msg.role, MessageRole::Event) {
                            format!("Event:\n{text}")
                        } else {
                            text
                        });
                }
                has_current = true;
            }
            MessageRole::Assistant | MessageRole::System => {
                if !has_current {
                    has_current = true;
                }
                if has_text {
                    current.assistant.push(text);
                }
            }
        }
    }

    if has_current && (!current.user.is_empty() || !current.assistant.is_empty()) {
        turns.push(current);
    }

    let mut text = String::new();
    text.push_str(
        "History:\nThis is a chronological transcript. `Assistant` refers to Lash, and you are continuing the same session.\n\n",
    );
    for (idx, turn) in turns.iter().enumerate() {
        text.push_str(&format!("=== Turn {} ===\n", idx + 1));
        text.push_str("User:\n");
        if turn.user.is_empty() {
            text.push_str("[No user content recorded]\n");
        } else {
            text.push_str(&turn.user.join("\n\n"));
            text.push('\n');
        }
        text.push('\n');
        text.push_str("Assistant (Lash, continuing this transcript):\n");
        let is_current_pending_turn = idx + 1 == turns.len() && turn.assistant.is_empty();
        if turn.assistant.is_empty() && !is_current_pending_turn {
            text.push_str("[No assistant content recorded]\n");
        } else if !turn.assistant.is_empty() {
            text.push_str(&turn.assistant.join("\n\n"));
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str(
        "Continue from the latest turn as Lash.\nIf the task is complete, provide the final answer.\nOtherwise produce the next valid step for this runtime.",
    );

    let mut message = LlmMessage::text(LlmRole::User, text);
    Arc::make_mut(&mut message.blocks).extend(attachments.into_iter().map(|source| {
        LlmContentBlock::Attachment {
            reference: Box::new(source),
        }
    }));
    RenderedPrompt {
        messages: vec![message],
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
