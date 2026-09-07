use crate::facade_support::AgentFrameReasonFacadeOps;
use std::collections::BTreeSet;

use crate::{
    Message, MessageRole, OmittedToolCalls, Part, ToolCallRecord, TurnFinish, TurnOutcome,
    shared_parts,
};

use super::RuntimeSessionState;

/// The turn's reply as the protocol driver materialized it: the assistant
/// messages the driver appended after its final model call, named by id.
///
/// Terminal materialization recognizes an already-materialized reply through
/// these ids (or the runtime's own terminal node id) and never through the
/// position of the last message: finalize-turn hooks append after the reply
/// and before the final commit, so the last message is not the reply.
#[derive(Clone, Debug, Default)]
pub(super) struct ProtocolTerminalOutput {
    message_ids: BTreeSet<String>,
}

impl ProtocolTerminalOutput {
    /// Replaces the recorded output with the ids the latest driver run named.
    pub(super) fn record(&mut self, message_ids: impl IntoIterator<Item = String>) {
        self.message_ids = message_ids.into_iter().collect();
    }

    fn names(&self, message_id: &str) -> bool {
        self.message_ids.contains(message_id)
    }
}

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

/// Appends the runtime's terminal reply node unless the reply is already
/// materialized: either the protocol appended it (identified through
/// `protocol_output`) or this node already exists (identified by `message_id`).
pub(super) fn materialize_terminal_output(
    state: &mut RuntimeSessionState,
    outcome: &TurnOutcome,
    clock: &dyn crate::Clock,
    turn_id: &str,
    message_id: &str,
    protocol_output: &ProtocolTerminalOutput,
) {
    let TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) = outcome else {
        return;
    };
    if state
        .read_model()
        .messages
        .iter()
        .any(|message| message.id == message_id || protocol_output.names(&message.id))
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

    fn message(
        id: &str,
        role: MessageRole,
        text: &str,
        origin: Option<crate::MessageOrigin>,
    ) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: shared_parts(vec![Part::prose(
                format!("{id}.p0"),
                text.to_string(),
                None,
            )]),
            origin,
        }
    }

    fn state_with_messages(messages: &[Message]) -> RuntimeSessionState {
        let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED));
        state.session_graph = crate::SessionGraph::from_active_read_state(messages);
        state
    }

    fn reply(text: &str) -> TurnOutcome {
        TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: text.to_string(),
        })
    }

    fn message_ids(state: &RuntimeSessionState) -> Vec<String> {
        state
            .read_model()
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect()
    }

    const TURN_ID: &str = "turn-1";
    const TERMINAL_ID: &str = "m_turn_turn-1_assistant";

    fn after_turn_enqueue_state() -> RuntimeSessionState {
        state_with_messages(&[
            message("m_ingress", MessageRole::User, "first request", None),
            message(
                "m_standard_turn-1_0_assistant",
                MessageRole::Assistant,
                "first response",
                None,
            ),
            message(
                "m_plugin_turn-1:after_turn_0",
                MessageRole::User,
                "enqueued after turn",
                Some(crate::MessageOrigin::Plugin {
                    plugin_id: "plugin".to_string(),
                    transient: false,
                }),
            ),
        ])
    }

    #[test]
    fn terminal_output_recognizes_the_protocol_reply_by_identity_behind_an_enqueue() {
        let mut state = after_turn_enqueue_state();
        let mut protocol_output = ProtocolTerminalOutput::default();
        protocol_output.record(["m_standard_turn-1_0_assistant".to_string()]);

        materialize_terminal_output(
            &mut state,
            &reply("first response"),
            &crate::SystemClock,
            TURN_ID,
            TERMINAL_ID,
            &protocol_output,
        );

        assert_eq!(
            message_ids(&state),
            vec![
                "m_ingress",
                "m_standard_turn-1_0_assistant",
                "m_plugin_turn-1:after_turn_0"
            ]
        );
    }

    #[test]
    fn terminal_output_materializes_when_the_protocol_named_no_reply() {
        // The retry prose predates the final model call, so it is not the
        // protocol's terminal output even though it is this turn's last
        // assistant message and shares the reply's text.
        let mut state = state_with_messages(&[
            message("m_ingress", MessageRole::User, "first request", None),
            message(
                "m_proto_turn-1_0_assistant_response",
                MessageRole::Assistant,
                "first response",
                Some(crate::MessageOrigin::TurnOutput {
                    turn_id: TURN_ID.to_string(),
                    source: crate::TurnOutputSource::Plugin {
                        plugin_id: "proto".to_string(),
                    },
                }),
            ),
            message(
                "m_proto_turn-1_0_reminder",
                MessageRole::System,
                "close the cell",
                None,
            ),
        ]);

        materialize_terminal_output(
            &mut state,
            &reply("first response"),
            &crate::SystemClock,
            TURN_ID,
            TERMINAL_ID,
            &ProtocolTerminalOutput::default(),
        );

        let messages = state.read_model().messages.clone();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.id.as_str())
                .next_back(),
            Some(TERMINAL_ID)
        );
        let terminal = messages.last().expect("materialized reply");
        assert_eq!(terminal.role, MessageRole::Assistant);
        assert_eq!(
            terminal.origin,
            Some(crate::MessageOrigin::TurnOutput {
                turn_id: TURN_ID.to_string(),
                source: crate::TurnOutputSource::Runtime,
            })
        );
        assert_eq!(terminal.parts[0].content, "first response");
    }

    #[test]
    fn terminal_output_is_idempotent_on_its_own_node_id() {
        let mut state = state_with_messages(&[
            message("m_ingress", MessageRole::User, "first request", None),
            message(
                TERMINAL_ID,
                MessageRole::Assistant,
                "first response",
                Some(crate::MessageOrigin::TurnOutput {
                    turn_id: TURN_ID.to_string(),
                    source: crate::TurnOutputSource::Runtime,
                }),
            ),
            message(
                "m_plugin_turn-1:after_turn_0",
                MessageRole::User,
                "enqueued after turn",
                None,
            ),
        ]);

        materialize_terminal_output(
            &mut state,
            &reply("first response"),
            &crate::SystemClock,
            TURN_ID,
            TERMINAL_ID,
            &ProtocolTerminalOutput::default(),
        );

        assert_eq!(
            message_ids(&state),
            vec!["m_ingress", TERMINAL_ID, "m_plugin_turn-1:after_turn_0"]
        );
    }

    #[test]
    fn terminal_output_ignores_non_reply_outcomes() {
        let mut state = after_turn_enqueue_state();
        let before = message_ids(&state);

        materialize_terminal_output(
            &mut state,
            &TurnOutcome::Stopped(crate::TurnStop::MaxTurns),
            &crate::SystemClock,
            TURN_ID,
            TERMINAL_ID,
            &ProtocolTerminalOutput::default(),
        );

        assert_eq!(message_ids(&state), before);
    }
}
