use super::*;
use lash::SessionId;
use lash::transcript::{TranscriptRowKind, TranscriptRowRecord};

pub(crate) fn product_chat_messages(state: &AppState, session_id: &SessionId) -> Vec<ChatMessage> {
    state
        .event_tx
        .snapshot(session_id)
        .events
        .iter()
        .filter_map(|event| match &event.item {
            StreamItem::Message { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Style a canonical row without inspecting committed messages or protocol data.
pub(crate) fn chat_message_from_row(row: &TranscriptRowRecord) -> Option<ChatMessage> {
    if row.suppressed.is_some() {
        return None;
    }
    let role = match row.kind {
        TranscriptRowKind::User => "user",
        TranscriptRowKind::AssistantReply => "assistant",
        TranscriptRowKind::Event | TranscriptRowKind::ToolCall => "event",
        _ => return None,
    };
    Some(ChatMessage {
        id: serde_json::to_value(&row.row_id)
            .expect("row id serializes")
            .as_str()
            .expect("row id is a wire token")
            .to_owned(),
        role: role.into(),
        text: row.content.text.clone(),
        at: row.timestamp.clone(),
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
    })
}

/// The UI owns its input copy and correlates only on the row's typed provenance.
pub(crate) fn displayed_messages(
    rows: &[TranscriptRowRecord],
    product: &[ChatMessage],
) -> Vec<ChatMessage> {
    let mut covered = BTreeSet::new();
    let mut messages = rows.iter().filter_map(|row| {
        let canonical = chat_message_from_row(row)?;
        if row.kind == TranscriptRowKind::User && let Some(turn_id) = &row.provenance.turn_id
            && covered.insert(turn_id.clone()) && let Some(owned) = product.iter().find(|message| matches!(&message.provenance, Some(ChatMessageProvenance::TurnInput { turn_id: owner }) if owner == turn_id)) {
            return Some(owned.clone());
        }
        Some(canonical)
    }).collect::<Vec<_>>();
    for message in product {
        if message.role == "event" {
            messages.push(message.clone());
        }
        if let Some(ChatMessageProvenance::TurnInput { turn_id }) = &message.provenance
            && !covered.contains(turn_id)
        {
            messages.push(message.clone());
        }
    }
    messages
}

pub(crate) fn republish_committed_ingress_messages(state: &AppState, session: &lash::LashSession) {
    let session_id = session.session_id();
    let product = product_chat_messages(state, &session_id);
    let mut covered = BTreeSet::new();
    for row in session.read_view().transcript().visible() {
        if row.kind != TranscriptRowKind::User {
            continue;
        }
        if let Some(turn_id) = &row.provenance.turn_id && covered.insert(turn_id.clone())
            && product.iter().any(|message| matches!(&message.provenance, Some(ChatMessageProvenance::TurnInput { turn_id: owner }) if owner == turn_id)) { continue; }
        if let Some(message) = chat_message_from_row(row) {
            state.publish_for_session_identified(
                &session_id,
                format!("message:{}", message.id),
                StreamItem::Message { message },
            );
        }
    }
}
