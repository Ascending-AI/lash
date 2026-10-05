use super::*;
use lash::SessionId;
use lash::transcript::{TranscriptRowKind, TranscriptRowRecord};

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

/// Style a canonical row without inspecting committed messages or protocol data.
/// Its opaque ID is transported verbatim as the node identity's JSON string.
pub(crate) fn chat_message_from_row(
    row: &TranscriptRowRecord,
) -> Result<Option<ChatMessage>, serde_json::Error> {
    if row.suppressed.is_some() {
        return Ok(None);
    }
    let role = match row.kind {
        TranscriptRowKind::User => "user",
        TranscriptRowKind::AssistantReply => "assistant",
        TranscriptRowKind::Event | TranscriptRowKind::ToolCall => "event",
        _ => return Ok(None),
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
    rows: &[TranscriptRowRecord],
) -> Result<(), serde_json::Error> {
    let mut committed_message_ids = BTreeSet::new();
    for row in rows {
        if let Some(message) = chat_message_from_row(row)? {
            committed_message_ids.insert(message.id);
        }
    }
    let committed_input_turn_ids = rows
        .iter()
        .filter(|row| row.kind == TranscriptRowKind::User)
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
