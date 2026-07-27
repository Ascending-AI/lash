use std::collections::BTreeSet;

use crate::{
    Message, MessageRole, Part, PartKind, PruneState, ToolCallRecord, TurnFinish, TurnOutcome,
    shared_parts,
};

use super::RuntimeSessionState;

pub(super) fn committed_attachment_ids(
    state: &RuntimeSessionState,
    tool_calls: &[ToolCallRecord],
) -> Vec<crate::AttachmentId> {
    let mut attachment_ids = BTreeSet::new();
    for call in tool_calls {
        for attachment in call.output.attachments() {
            if let Some(attachment_ref) = attachment.stored_ref() {
                attachment_ids.insert(attachment_ref.id.clone());
            }
        }
    }
    for message in state.read_model().messages.iter() {
        for part in message.parts.iter() {
            if let Some(attachment_ref) = part
                .attachment
                .as_ref()
                .and_then(|attachment| attachment.source.stored_ref())
            {
                attachment_ids.insert(attachment_ref.id.clone());
            }
        }
    }
    attachment_ids.into_iter().collect()
}

pub(super) fn materialize_terminal_output(
    state: &mut RuntimeSessionState,
    outcome: &TurnOutcome,
    clock: &dyn crate::Clock,
    message_id: &str,
) {
    let TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) = outcome else {
        return;
    };
    if state
        .read_model()
        .messages
        .iter()
        .rfind(|message| !message.is_transient())
        .is_some_and(|message| {
            message.role == MessageRole::Assistant && message_rendered_text(message) == *text
        })
    {
        return;
    }

    let id = message_id.to_string();
    state.append_active_conversation_messages_with_clock(
        &[Message {
            id: id.clone(),
            role: MessageRole::Assistant,
            parts: shared_parts(vec![Part {
                id: format!("{id}.p0"),
                kind: PartKind::Prose,
                content: text.clone(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
            origin: None,
        }],
        clock,
    );
}

pub(super) fn materialize_agent_frame_switch(
    state: &mut RuntimeSessionState,
    outcome: &TurnOutcome,
    clock: &dyn crate::Clock,
) {
    let TurnOutcome::AgentFrameSwitch {
        frame_id,
        initial_nodes,
        ..
    } = outcome
    else {
        return;
    };
    if frame_id.trim().is_empty()
        || state.current_frame_node_id.as_deref() == Some(frame_id.as_str())
    {
        return;
    }
    super::super::open_agent_frame_in_state_with_clock(
        state,
        crate::OpenAgentFrameRequest::new(frame_id.clone(), crate::AgentFrameReason::continue_as())
            .with_initial_nodes(initial_nodes.clone()),
        clock,
    );
}

fn message_rendered_text(message: &Message) -> String {
    message
        .parts
        .iter()
        .filter(|part| {
            matches!(
                part.kind,
                PartKind::Prose | PartKind::Text | PartKind::Attachment | PartKind::ToolResult
            )
        })
        .map(|part| part.content.as_str())
        .collect::<Vec<_>>()
        .join("")
}
