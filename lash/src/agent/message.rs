pub use crate::llm::types::{LlmMessage as ChatMsg, LlmRole};

pub const IMAGE_REF_PREFIX: &str = "__LASH_IMAGE_IDX:";

// ─── Structured message types for context-aware pruning ───

/// A structured message with typed parts for context management.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub id: String,
    pub role: MessageRole,
    pub parts: Vec<Part>,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Part {
    /// e.g. "m3.p0"
    pub id: String,
    pub kind: PartKind,
    pub content: String,
    pub prune_state: PruneState,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PartKind {
    Text,
    Code,
    Output,
    Error,
    Prose,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum PruneState {
    Intact,
    Deleted {
        breadcrumb: String,
        archive_hash: String,
    },
    Summarized {
        summary: String,
        archive_hash: String,
    },
}

impl Part {
    pub(crate) fn render(&self) -> String {
        match &self.prune_state {
            PruneState::Intact => self.content.clone(),
            PruneState::Deleted {
                breadcrumb,
                archive_hash,
            } => format!("[pruned:{} — {}]", archive_hash, breadcrumb),
            PruneState::Summarized {
                summary,
                archive_hash,
            } => format!("[SUMMARY of original {}]\n{}", archive_hash, summary),
        }
    }
}

impl Message {
    /// Convert structured message to ChatMsg for LLM prompt.
    pub fn to_chat_msg(&self) -> ChatMsg {
        let content = if self.role == MessageRole::System {
            self.parts
                .iter()
                .map(|p| {
                    let rendered = p.render();
                    match p.kind {
                        PartKind::Code => format!("<repl>\n{}\n</repl>", rendered),
                        PartKind::Output => format!("<output>\n{}\n</output>", rendered),
                        PartKind::Error => format!("<error>\n{}\n</error>", rendered),
                        PartKind::Text | PartKind::Prose => rendered,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        } else if self.role == MessageRole::Assistant {
            // Assistant messages: prose renders as-is, code wrapped in <repl> tags
            self.parts
                .iter()
                .map(|p| {
                    let rendered = p.render();
                    match p.kind {
                        PartKind::Code => format!("<repl>\n{}\n</repl>", rendered),
                        PartKind::Prose | PartKind::Text => rendered,
                        _ => rendered,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        } else {
            self.parts
                .iter()
                .map(|p| p.render())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        ChatMsg {
            role: match self.role {
                MessageRole::User => LlmRole::User,
                MessageRole::Assistant => LlmRole::Assistant,
                MessageRole::System => LlmRole::System,
            },
            content,
            kind: "text".to_string(),
            image_idx: -1,
        }
    }

    /// Total character count of all parts (rendered).
    pub fn char_count(&self) -> usize {
        self.parts.iter().map(|p| p.render().len()).sum()
    }
}

fn render_part_for_chat(role: MessageRole, part: &Part) -> String {
    let rendered = part.render();
    match role {
        MessageRole::System => match part.kind {
            PartKind::Code => format!("<repl>\n{}\n</repl>", rendered),
            PartKind::Output => format!("<output>\n{}\n</output>", rendered),
            PartKind::Error => format!("<error>\n{}\n</error>", rendered),
            PartKind::Text | PartKind::Prose => rendered,
        },
        MessageRole::Assistant => match part.kind {
            PartKind::Code => format!("<repl>\n{}\n</repl>", rendered),
            PartKind::Prose | PartKind::Text => rendered,
            _ => rendered,
        },
        MessageRole::User => rendered,
    }
}

/// Convert Vec<Message> to Vec<ChatMsg> (for LLM calls).
pub fn messages_to_chat(msgs: &[Message]) -> Vec<ChatMsg> {
    let mut out: Vec<ChatMsg> = Vec::new();
    for msg in msgs {
        if msg.parts.is_empty() {
            out.push(ChatMsg {
                role: match msg.role {
                    MessageRole::User => LlmRole::User,
                    MessageRole::Assistant => LlmRole::Assistant,
                    MessageRole::System => LlmRole::System,
                },
                content: String::new(),
                kind: "text".to_string(),
                image_idx: -1,
            });
            continue;
        }

        for part in &msg.parts {
            let rendered = render_part_for_chat(msg.role, part);
            if let Some(idx_str) = rendered.strip_prefix(IMAGE_REF_PREFIX)
                && let Ok(idx) = idx_str.parse::<i64>()
            {
                // Route image payloads as user messages for broad provider compatibility.
                out.push(ChatMsg {
                    role: LlmRole::User,
                    content: String::new(),
                    kind: "image".to_string(),
                    image_idx: idx,
                });
                continue;
            }

            out.push(ChatMsg {
                role: match msg.role {
                    MessageRole::User => LlmRole::User,
                    MessageRole::Assistant => LlmRole::Assistant,
                    MessageRole::System => LlmRole::System,
                },
                content: rendered,
                kind: "text".to_string(),
                image_idx: -1,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(kind: PartKind, content: &str) -> Part {
        Part {
            id: "p0".to_string(),
            kind,
            content: content.to_string(),
            prune_state: PruneState::Intact,
        }
    }

    #[test]
    fn messages_to_chat_preserves_system_output_wrapping() {
        let msgs = vec![Message {
            id: "m0".to_string(),
            role: MessageRole::System,
            parts: vec![part(PartKind::Output, "hello")],
        }];

        let out = messages_to_chat(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, LlmRole::System);
        assert_eq!(out[0].kind, "text");
        assert_eq!(out[0].content, "<output>\nhello\n</output>");
    }

    #[test]
    fn messages_to_chat_turns_system_image_ref_into_image_message() {
        let msgs = vec![Message {
            id: "m0".to_string(),
            role: MessageRole::System,
            parts: vec![part(PartKind::Text, "__LASH_IMAGE_IDX:3")],
        }];

        let out = messages_to_chat(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, LlmRole::User);
        assert_eq!(out[0].kind, "image");
        assert_eq!(out[0].image_idx, 3);
    }
}
