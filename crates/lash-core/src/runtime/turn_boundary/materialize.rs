use crate::facade_support::AgentFrameReasonFacadeOps;
use std::collections::BTreeSet;

use crate::{
    Message, MessageRole, Part, PartKind, ToolCallRecord, TurnFinish, TurnOutcome, shared_parts,
};

use super::RuntimeSessionState;

pub(super) fn agent_frame_switch_materializes(
    session_id: &str,
    requested_frame_key: &crate::FrameKey,
    current_frame_node_id: Option<&str>,
) -> bool {
    current_frame_node_id
        != Some(
            crate::session_graph::frame_node_id(session_id, requested_frame_key.as_str()).as_str(),
        )
}

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
    turn_id: &str,
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
            parts: shared_parts(vec![Part::prose(format!("{id}.p0"), text.clone(), None)]),
            origin: Some(crate::MessageOrigin::TurnOutput {
                turn_id: turn_id.to_string(),
                source: crate::TurnOutputSource::Runtime,
            }),
        }],
        clock,
    );
}

pub(super) fn materialize_agent_frame_switch(
    state: &mut RuntimeSessionState,
    outcome: &TurnOutcome,
    clock: &dyn crate::Clock,
    materializes: bool,
) {
    let TurnOutcome::AgentFrameSwitch {
        frame_key,
        initial_nodes,
        ..
    } = outcome
    else {
        return;
    };
    // The pre-snapshot decision and this post-snapshot state must never diverge;
    // fail in debug/tests instead of silently clearing the wrong frame's state.
    debug_assert_eq!(
        materializes,
        agent_frame_switch_materializes(
            &state.session_id,
            frame_key,
            state.current_frame_node_id.as_deref(),
        )
    );
    if !materializes {
        return;
    }
    super::super::open_agent_frame_in_state_with_clock(
        state,
        crate::OpenAgentFrameRequest::new(
            frame_key.as_str(),
            crate::AgentFrameReason::continue_as(),
        )
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
