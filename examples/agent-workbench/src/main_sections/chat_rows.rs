use super::*;
use lash::SessionId;
use lash::transcript::{
    CellResult, EntryId, EntryProvenance, SuppressionReason, TerminalValue, ToolResultBlock,
    TranscriptBlock, TranscriptCell, TranscriptEntry, TranscriptItem, TranscriptMessage,
    TranscriptRole,
};

/// How the workbench lays out one committed transcript entry. Lash hands the
/// workbench typed history; this row is the workbench's own presentation of
/// it, in the shape its browser renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChatRowKind {
    User,
    AssistantReply,
    Reasoning,
    ToolCall,
    CodeBlock,
    Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatTool {
    pub(crate) operation: String,
    pub(crate) status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatContent {
    pub(crate) text: String,
    pub(crate) reasoning: Vec<String>,
    pub(crate) attachments: Vec<lash::attachments::AttachmentRef>,
    pub(crate) language: Option<String>,
    pub(crate) code: Option<String>,
    pub(crate) output: Option<String>,
    pub(crate) success: Option<bool>,
    pub(crate) error: Option<String>,
    pub(crate) tools: Vec<ChatTool>,
    pub(crate) tools_omitted: usize,
}

/// One committed entry as the browser renders it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChatRow {
    pub(crate) row_id: EntryId,
    pub(crate) kind: ChatRowKind,
    pub(crate) provenance: EntryProvenance,
    pub(crate) content: ChatContent,
    pub(crate) timestamp: lash::persistence::NodeTimestamp,
    pub(crate) suppressed: Option<SuppressionReason>,
}

impl ChatRow {
    pub(crate) fn of(entry: &TranscriptEntry) -> Self {
        let (kind, content, suppressed) = match &entry.item {
            TranscriptItem::Suppressed(reason) => {
                (ChatRowKind::Event, ChatContent::default(), Some(*reason))
            }
            TranscriptItem::Message(message) => {
                let (kind, content) = message_row(message, entry.provenance.is_turn_reply);
                (kind, content, None)
            }
            TranscriptItem::Cell(cell) => (ChatRowKind::CodeBlock, cell_row(cell), None),
        };
        Self {
            row_id: entry.entry_id.clone(),
            kind,
            provenance: entry.provenance.clone(),
            content,
            timestamp: entry.timestamp,
            suppressed,
        }
    }

    pub(crate) fn all(entries: &[TranscriptEntry]) -> Vec<Self> {
        entries.iter().map(Self::of).collect()
    }
}

fn message_row(message: &TranscriptMessage, is_turn_reply: bool) -> (ChatRowKind, ChatContent) {
    let mut content = ChatContent::default();
    let mut text = Vec::new();
    let mut tool = false;
    for block in &message.blocks {
        match block {
            TranscriptBlock::Reasoning { text } => content.reasoning.push(text.clone()),
            TranscriptBlock::Text { text: body } => text.push(body.clone()),
            TranscriptBlock::Attachment {
                attachment,
                text: body,
            } => {
                text.push(if attachment.is_some() || body.trim().is_empty() {
                    "[Attachment]".to_string()
                } else {
                    body.clone()
                });
                content.attachments.extend(attachment.iter().cloned());
            }
            TranscriptBlock::ToolCall { arguments, .. } => {
                tool = true;
                text.push(arguments.clone());
            }
            TranscriptBlock::ToolResult {
                content: blocks, ..
            } => {
                tool = true;
                let mut attachment = 0;
                let mut rendered = String::new();
                for block in blocks {
                    match block {
                        ToolResultBlock::Text { text } => rendered.push_str(text),
                        ToolResultBlock::Retained { output } => rendered.push_str(&output.witness),
                        ToolResultBlock::Attachment { attachment: stored } => {
                            attachment += 1;
                            rendered.push_str(&format!("[Attachment {attachment}]"));
                            content.attachments.extend(stored.iter().cloned());
                        }
                        _ => {}
                    }
                }
                text.push(rendered);
            }
            TranscriptBlock::Code { code } => text.push(code.clone()),
            TranscriptBlock::CodeOutput { text: body }
            | TranscriptBlock::CodeError { text: body } => text.push(body.clone()),
            _ => {}
        }
    }
    content.text = text.join("\n");
    let kind = if is_turn_reply {
        ChatRowKind::AssistantReply
    } else if tool {
        ChatRowKind::ToolCall
    } else {
        match message.role {
            TranscriptRole::User => ChatRowKind::User,
            TranscriptRole::Tool => ChatRowKind::ToolCall,
            TranscriptRole::Assistant => ChatRowKind::Reasoning,
            _ => ChatRowKind::Event,
        }
    };
    (kind, content)
}

fn cell_row(cell: &TranscriptCell) -> ChatContent {
    let mut output = match &cell.prints_retained {
        Some(archive) => archive.witness.clone(),
        None => cell
            .prints
            .iter()
            .map(|print| print.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    if let CellResult::Finished(value) = &cell.result {
        let terminal = match value {
            TerminalValue::Inline(value) => {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            }
            TerminalValue::Retained(value) => value.witness.clone(),
        };
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&terminal);
    }
    ChatContent {
        language: Some(cell.language.clone()),
        code: Some(cell.code.clone()),
        output: Some(output),
        success: Some(!matches!(cell.result, CellResult::Failed(_))),
        error: match &cell.result {
            CellResult::Failed(failure) => Some(failure.message.clone()),
            _ => None,
        },
        attachments: cell.images.clone(),
        tools: cell
            .calls
            .iter()
            .map(|call| ChatTool {
                operation: call.operation.clone(),
                status: match call.outcome {
                    lash::persistence::ExecutedCallOutcome::Ok => "success",
                    lash::persistence::ExecutedCallOutcome::Err => "failure",
                }
                .into(),
            })
            .collect(),
        tools_omitted: cell.calls_omitted,
        ..Default::default()
    }
}

pub(crate) fn ui_input_message_from_active_turn(active: &ActiveTurn) -> Option<ChatMessage> {
    if active.kind != WorkbenchTurnKind::User {
        return None;
    }
    let prompt = active.prompt.as_ref()?;
    Some(ChatMessage {
        id: prompt.row_id.clone(),
        role: "user".into(),
        text: prompt.text.clone(),
        at: prompt.at.clone(),
        attachments: prompt
            .attachment_id
            .iter()
            .cloned()
            .map(ChatAttachment::from_id)
            .collect(),
        provenance: Some(ChatMessageProvenance::TurnInput {
            turn_id: active.address.turn_id.clone(),
        }),
        client_nonce: None,
    })
}

/// Style a committed row as a chat message. Its opaque ID is transported
/// verbatim as the node identity's JSON string.
pub(crate) fn chat_message_from_row(
    row: &ChatRow,
) -> Result<Option<ChatMessage>, serde_json::Error> {
    if row.suppressed.is_some() {
        return Ok(None);
    }
    let role = match row.kind {
        ChatRowKind::User => "user",
        ChatRowKind::AssistantReply => "assistant",
        ChatRowKind::Event | ChatRowKind::ToolCall => "event",
        ChatRowKind::Reasoning | ChatRowKind::CodeBlock => return Ok(None),
    };
    Ok(Some(ChatMessage {
        id: serde_json::from_value(serde_json::to_value(&row.row_id)?)?,
        role: role.into(),
        text: row.content.text.clone(),
        at: row.timestamp.to_string(),
        attachments: row
            .content
            .attachments
            .iter()
            .map(|attachment| ChatAttachment::from_id(attachment.id.to_string()))
            .collect(),
        provenance: row.provenance.turn_id.as_ref().map(|turn_id| {
            if row.provenance.is_turn_reply {
                ChatMessageProvenance::TurnOutput {
                    turn_id: turn_id.clone(),
                }
            } else {
                ChatMessageProvenance::TurnInput {
                    turn_id: turn_id.clone(),
                }
            }
        }),
        client_nonce: None,
    }))
}

/// Retire the product rows a settled turn no longer needs (its live reply
/// and `done`, and mirrors of committed rows), once its claim is released.
/// Settlement runs this, never a read: `/api/state` only reads.
pub(crate) fn retire_settled_product_rows(
    state: &AppState,
    session_id: &SessionId,
    rows: &[ChatRow],
) -> Result<(), serde_json::Error> {
    let mut committed_message_ids = BTreeSet::new();
    for row in rows {
        if let Some(message) = chat_message_from_row(row)? {
            committed_message_ids.insert(message.id);
        }
    }
    let committed_input_turn_ids = rows
        .iter()
        .filter(|row| row.kind == ChatRowKind::User)
        .filter_map(|row| row.provenance.turn_id.clone())
        .collect::<BTreeSet<_>>();
    let active_turn_ids = state
        .active_turns
        .for_session(session_id)
        .map(|active| active.address.turn_id)
        .into_iter()
        .collect::<BTreeSet<_>>();
    state.event_tx.reconcile_settled(
        session_id,
        &committed_message_ids,
        &committed_input_turn_ids,
        &active_turn_ids,
    );
    Ok(())
}
