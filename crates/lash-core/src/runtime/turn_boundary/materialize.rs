use crate::TurnId;
use std::collections::BTreeSet;

use crate::{Message, MessageRole, Part, PartKind, TurnFinish, TurnOutcome, shared_parts};
use lash_sansio::core_support::TurnReplyCoreSupport;

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

/// Commits the turn's one reply and marks it (FIG-1493 §5.1, §5.5).
///
/// Every finished turn has a reply, whatever finished it: prose the protocol
/// already appended is marked where it stands, a turn that finished as an
/// assistant message the protocol did not append gets the runtime's reply
/// node, and a turn that finished with a final or tool value gets a runtime
/// reply node rendering that value. A stopped turn has no reply.
///
/// The reply is keyed on the turn, never on the commit attempt: a turn whose
/// reply is already in the transcript, marked or as the runtime's own node,
/// mints nothing, so a redriven or re-executed turn commits one reply.
pub(super) fn materialize_turn_reply(
    state: &mut RuntimeSessionState,
    outcome: &TurnOutcome,
    clock: &dyn crate::Clock,
    turn_id: &TurnId,
    message_id: &str,
    protocol_output: &ProtocolTerminalOutput,
    cuts: lash_sansio::session_model::RuntimeOutputCuts,
) {
    let TurnOutcome::Finished(finish) = outcome else {
        return;
    };
    let read_model = state.read_model();
    if read_model.messages.iter().any(|message| {
        message.id == message_id
            || message
                .reply_marker
                .as_ref()
                .is_some_and(|reply| reply.turn_id() == turn_id)
    }) {
        return;
    }
    let text = match finish {
        TurnFinish::AssistantMessage { text } => {
            let protocol_reply = read_model
                .messages
                .iter()
                .rev()
                .find(|message| protocol_output.names(&message.id));
            if let Some(protocol_reply) = protocol_reply {
                if let Some(part_id) = reply_part_id(protocol_reply) {
                    let message_id = protocol_reply.id.clone();
                    drop(read_model);
                    state.mark_pending_turn_reply(
                        &message_id,
                        crate::TurnReply::mint(turn_id.clone(), part_id),
                    );
                }
                return;
            }
            text.clone()
        }
        TurnFinish::FinalValue { value } | TurnFinish::ToolValue { value, .. } => {
            render_value_reply(value, cuts.value_reply_max_chars)
        }
    };
    let id = message_id.to_string();
    let part_id = format!("{id}.p0");
    state.append_active_conversation_messages_with_clock(
        &[Message {
            id: id.clone(),
            role: MessageRole::Assistant,
            parts: shared_parts(vec![Part::prose(part_id.clone(), text, None)]),
            origin: Some(crate::MessageOrigin::TurnOutput {
                turn_id: turn_id.clone(),
                source: crate::TurnOutputSource::Runtime,
            }),
            reply_marker: Some(crate::TurnReply::mint(turn_id.clone(), part_id)),
        }],
        clock,
    );
}

/// The part of a protocol-authored reply that carries its text: the last
/// prose or text part, so a reasoning-then-prose message resolves to prose.
fn reply_part_id(message: &Message) -> Option<String> {
    message
        .parts
        .iter()
        .rev()
        .find(|part| matches!(part.kind(), PartKind::Prose | PartKind::Text))
        .map(|part| part.id().to_string())
}

/// A value reply's text: a string value as itself, any other value as compact
/// JSON, capped at the host's character cut.
fn render_value_reply(value: &serde_json::Value, max_chars: usize) -> String {
    let rendered = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    match rendered.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &rendered[..cut]),
        None => rendered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNBOUNDED: crate::TurnBudget = crate::TurnBudget::Unbounded;

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
            reply_marker: None,
        }
    }

    fn state_with_messages(messages: &[Message]) -> RuntimeSessionState {
        let mut state = RuntimeSessionState::ambient_fixture(crate::SessionPolicy::new(
            UNBOUNDED,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ));
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

        materialize_turn_reply(
            &mut state,
            &reply("first response"),
            &crate::SystemClock,
            &TurnId::from(TURN_ID),
            TERMINAL_ID,
            &protocol_output,
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
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
                    turn_id: TurnId::fixture(TURN_ID.to_string()),
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

        materialize_turn_reply(
            &mut state,
            &reply("first response"),
            &crate::SystemClock,
            &TurnId::from(TURN_ID),
            TERMINAL_ID,
            &ProtocolTerminalOutput::default(),
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
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
                turn_id: TurnId::fixture(TURN_ID.to_string()),
                source: crate::TurnOutputSource::Runtime,
            })
        );
        assert_eq!(terminal.parts[0].content(), "first response");
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
                    turn_id: TurnId::fixture(TURN_ID.to_string()),
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

        materialize_turn_reply(
            &mut state,
            &reply("first response"),
            &crate::SystemClock,
            &TurnId::from(TURN_ID),
            TERMINAL_ID,
            &ProtocolTerminalOutput::default(),
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        );

        assert_eq!(
            message_ids(&state),
            vec!["m_ingress", TERMINAL_ID, "m_plugin_turn-1:after_turn_0"]
        );
    }

    fn turn() -> TurnId {
        TurnId::from(TURN_ID)
    }

    fn reply_markers(state: &RuntimeSessionState) -> Vec<(String, crate::TurnReply)> {
        state
            .read_model()
            .messages
            .iter()
            .filter_map(|message| {
                message
                    .reply_marker
                    .clone()
                    .map(|reply| (message.id.clone(), reply))
            })
            .collect()
    }

    fn mark_all_persisted(state: &mut RuntimeSessionState) {
        let node_ids = state
            .session_graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        for node_id in node_ids {
            state.persisted_node_ids.insert(node_id);
        }
    }

    fn materialize(state: &mut RuntimeSessionState, outcome: &TurnOutcome) {
        materialize_turn_reply(
            state,
            outcome,
            &crate::SystemClock,
            &turn(),
            TERMINAL_ID,
            &ProtocolTerminalOutput::default(),
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        );
    }

    fn final_value(value: serde_json::Value) -> TurnOutcome {
        TurnOutcome::Finished(TurnFinish::FinalValue { value })
    }

    /// FIG-1493 §5.5: a turn that finished with a value commits the runtime's
    /// reply, rendering the value, and marks it as the turn's reply.
    #[test]
    fn a_value_finished_turn_commits_one_marked_reply() {
        let mut state = after_turn_enqueue_state();

        materialize(&mut state, &final_value(serde_json::json!("the answer")));

        let messages = state.read_model().messages.clone();
        let reply = messages.last().expect("the reply");
        assert_eq!(reply.id, TERMINAL_ID);
        assert_eq!(reply.role, MessageRole::Assistant);
        assert_eq!(reply.parts[0].content(), "the answer");
        assert_eq!(
            reply.origin,
            Some(crate::MessageOrigin::TurnOutput {
                turn_id: turn(),
                source: crate::TurnOutputSource::Runtime,
            })
        );
        let marker = reply.reply_marker.as_ref().expect("the reply is marked");
        assert_eq!(marker.turn_id(), &turn());
        assert_eq!(marker.part_id(), reply.parts[0].id());
        assert_eq!(reply_markers(&state).len(), 1);
    }

    #[test]
    fn a_tool_value_reply_renders_a_non_string_value_as_compact_json() {
        let mut state = after_turn_enqueue_state();

        materialize(
            &mut state,
            &TurnOutcome::Finished(TurnFinish::ToolValue {
                tool_name: "lookup".to_string(),
                value: serde_json::json!({"rows": [1, 2]}),
            }),
        );

        let messages = state.read_model().messages.clone();
        let reply = messages.last().expect("the reply");
        assert_eq!(reply.parts[0].content(), r#"{"rows":[1,2]}"#);
        assert!(reply.reply_marker.is_some());
    }

    #[test]
    fn a_value_reply_is_capped_at_the_stated_bound() {
        let mut state = after_turn_enqueue_state();

        materialize(
            &mut state,
            &final_value(serde_json::json!("é".repeat(
                lash_sansio::session_model::RuntimeOutputCuts::standard().value_reply_max_chars
                    + 10
            ))),
        );

        let messages = state.read_model().messages.clone();
        let text = messages.last().expect("the reply").parts[0]
            .content()
            .into_owned();
        assert_eq!(
            text.chars().count(),
            lash_sansio::session_model::RuntimeOutputCuts::standard().value_reply_max_chars + 1
        );
        assert!(text.ends_with('…'));
    }

    /// The protocol's own reply is marked where it stands, on the part that
    /// carries its prose, and the runtime appends nothing beside it.
    #[test]
    fn the_protocol_reply_is_marked_on_its_prose_part() {
        let mut protocol_reply = message(
            "m_standard_turn-1_0_assistant",
            MessageRole::Assistant,
            "first response",
            None,
        );
        protocol_reply.parts = shared_parts(vec![
            Part::reasoning(
                "m_standard_turn-1_0_assistant.p0".to_string(),
                "think".to_string(),
                None,
            ),
            Part::prose(
                "m_standard_turn-1_0_assistant.p1".to_string(),
                "first response".to_string(),
                None,
            ),
        ]);
        let mut state = state_with_messages(&[
            message("m_ingress", MessageRole::User, "first request", None),
            protocol_reply,
        ]);
        let mut protocol_output = ProtocolTerminalOutput::default();
        protocol_output.record(["m_standard_turn-1_0_assistant".to_string()]);

        materialize_turn_reply(
            &mut state,
            &reply("first response"),
            &crate::SystemClock,
            &turn(),
            TERMINAL_ID,
            &protocol_output,
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        );

        assert_eq!(
            message_ids(&state),
            vec!["m_ingress", "m_standard_turn-1_0_assistant"]
        );
        let markers = reply_markers(&state);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].0, "m_standard_turn-1_0_assistant");
        assert_eq!(markers[0].1.turn_id(), &turn());
        assert_eq!(markers[0].1.part_id(), "m_standard_turn-1_0_assistant.p1");
    }

    #[test]
    fn a_fresh_commit_identity_reexecution_preserves_the_first_value_reply() {
        let mut state = after_turn_enqueue_state();
        let first = crate::OperationId::turn("root", turn(), "first-final");
        let fresh = crate::OperationId::turn("root", turn(), "fresh-final");
        assert_ne!(
            first.storage_key().expect("first operation"),
            fresh.storage_key().expect("fresh operation")
        );
        materialize_turn_reply(
            &mut state,
            &final_value(serde_json::json!("first answer")),
            &crate::SystemClock,
            &turn(),
            "first-attempt-reply",
            &ProtocolTerminalOutput::default(),
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        );
        mark_all_persisted(&mut state);
        let before = message_ids(&state);
        materialize_turn_reply(
            &mut state,
            &final_value(serde_json::json!("reexecuted answer")),
            &crate::SystemClock,
            &turn(),
            "fresh-attempt-reply",
            &ProtocolTerminalOutput::default(),
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        );
        assert_eq!(
            reply_markers(&state).len(),
            1,
            "a fresh operation must retain the first turn reply"
        );
        assert_eq!(message_ids(&state), before);
        assert!(
            !state
                .read_model()
                .messages
                .iter()
                .any(|message| message.id == "fresh-attempt-reply")
        );
    }

    /// A durable node is immutable history: a protocol reply that is already
    /// durable is never rewritten to carry a marker.
    #[test]
    fn a_durable_protocol_reply_is_never_rewritten() {
        let mut state = after_turn_enqueue_state();
        mark_all_persisted(&mut state);
        let mut protocol_output = ProtocolTerminalOutput::default();
        protocol_output.record(["m_standard_turn-1_0_assistant".to_string()]);

        materialize_turn_reply(
            &mut state,
            &reply("first response"),
            &crate::SystemClock,
            &turn(),
            TERMINAL_ID,
            &protocol_output,
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        );

        assert!(reply_markers(&state).is_empty());
        assert!(matches!(
            state.pending_graph_commit(),
            crate::GraphAppend::PreserveHead
        ));
    }

    #[test]
    fn terminal_output_ignores_non_reply_outcomes() {
        let mut state = after_turn_enqueue_state();
        let before = message_ids(&state);

        materialize_turn_reply(
            &mut state,
            &TurnOutcome::Stopped(crate::TurnStop::MaxTurns),
            &crate::SystemClock,
            &TurnId::from(TURN_ID),
            TERMINAL_ID,
            &ProtocolTerminalOutput::default(),
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        );

        assert_eq!(message_ids(&state), before);
    }
}
