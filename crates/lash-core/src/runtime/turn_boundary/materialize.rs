use crate::facade_support::AgentFrameReasonFacadeOps;
use std::collections::BTreeSet;

use crate::{
    Message, MessageRole, OmittedToolCalls, Part, PartKind, ToolCallRecord, TurnFinish,
    TurnOutcome, shared_parts,
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
    omitted: Option<&OmittedToolCalls>,
) -> Vec<crate::AttachmentId> {
    let mut attachment_ids = BTreeSet::new();
    for call in tool_calls {
        for attachment in call.output.attachments() {
            if let Some(attachment_ref) = attachment.stored_ref() {
                attachment_ids.insert(attachment_ref.id.clone());
            }
        }
    }
    for attachment in omitted
        .into_iter()
        .flat_map(|omitted| omitted.attachments.iter())
    {
        if let Some(attachment_ref) = attachment.stored_ref() {
            attachment_ids.insert(attachment_ref.id.clone());
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
            frame_key.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    const UNBOUNDED: crate::TurnBudget = crate::TurnBudget::Unbounded;

    fn attachment_ref(id: &str) -> crate::AttachmentRef {
        crate::AttachmentMeta::new(
            crate::AttachmentId::parse(id).expect("valid attachment id"),
            crate::MediaType::parse("image/png").unwrap(),
            3,
            Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
            Some("tiny".to_string()),
        )
        .as_ref()
    }

    #[test]
    fn committed_attachment_ids_merge_tool_outputs_with_message_refs() {
        let tool_ref = attachment_ref("tool-output");
        let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED));
        let message = crate::Message {
            id: "message".to_string(),
            role: crate::MessageRole::User,
            parts: std::sync::Arc::new(vec![crate::Part::attachment_part(
                "message.p0".to_string(),
                String::new(),
                Some(crate::session_model::message::PartAttachment {
                    source: crate::AttachmentSource::stored(attachment_ref("message-ref")),
                }),
            )]),
            origin: None,
        };
        state.session_graph = crate::SessionGraph::from_active_read_state(&[message]);
        let tool_calls = vec![crate::ToolCallRecord {
            call_id: Some("call-1".to_string()),
            tool: "make_attachment".to_string(),
            args: serde_json::json!({}),
            output: crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(
                crate::AttachmentSource::stored(tool_ref),
            )),
            duration_ms: 1,
        }];

        let ids = committed_attachment_ids(&state, &tool_calls, None);

        assert_eq!(
            ids,
            vec![
                crate::AttachmentId::parse("message-ref").expect("valid attachment id"),
                crate::AttachmentId::parse("tool-output").expect("valid attachment id"),
            ]
        );
    }

    #[test]
    fn committed_attachment_ids_include_omitted_tool_call_attachments() {
        let state = RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED));
        let omitted = crate::OmittedToolCalls {
            count: 1,
            failures: 0,
            attachments: vec![crate::AttachmentSource::stored(attachment_ref(
                "omitted-tool-output",
            ))],
        };

        let ids = committed_attachment_ids(&state, &[], Some(&omitted));

        assert_eq!(
            ids,
            vec![crate::AttachmentId::parse("omitted-tool-output").expect("valid attachment id")]
        );
    }
}
