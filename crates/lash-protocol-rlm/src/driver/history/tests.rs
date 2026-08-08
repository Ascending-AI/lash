use lash_core::llm::types::{LlmContentBlock, ProviderReasoningReplay};
use lash_core::session_model::{ConversationRecord, MessageRole, Part, PartKind, PruneState};
use lash_core::{MessageOrigin, SessionHistoryRecord};

use super::{RlmHistoryRenderInput, render_history_messages};
use crate::projection::rlm_protocol_event;

fn replay(item_id: &str, encrypted_content: &str) -> ProviderReasoningReplay {
    ProviderReasoningReplay {
        item_id: Some(item_id.to_string()),
        encrypted_content: Some(encrypted_content.to_string()),
        signature: None,
        redacted: false,
        summary: Vec::new(),
    }
}

fn assistant_reasoning_event(
    reasoning: &[(&str, ProviderReasoningReplay)],
    prose: &str,
) -> SessionHistoryRecord {
    let id = "a1";
    let mut parts = reasoning
        .iter()
        .enumerate()
        .map(|(index, (text, replay))| {
            lash_core::facade_support::reasoning_part(
                id,
                index,
                (*text).to_string(),
                Some(replay.clone()),
            )
        })
        .collect::<Vec<_>>();
    parts.push(Part {
        id: format!("{id}.p{}", parts.len()),
        kind: PartKind::Prose,
        content: prose.to_string(),
        attachment: None,
        tool_call_id: None,
        tool_name: None,
        tool_replay: None,
        prune_state: PruneState::Intact,
        reasoning_meta: None,
        response_meta: None,
    });
    SessionHistoryRecord::Conversation(ConversationRecord {
        id: id.to_string(),
        role: MessageRole::Assistant,
        parts: parts.into(),
        origin: Some(MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    })
}

fn step_event(code: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(
        lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(lash_rlm_types::RlmTrajectoryEntry {
            id: "lashlang_step_0".to_string(),
            protocol_iteration: 0,
            code: code.to_string(),
            output: vec!["ok".to_string()],
            images: Vec::new(),
            error: None,
            final_output: None,
        }),
    ))
}

fn render(events: &[SessionHistoryRecord]) -> Vec<lash_core::llm::types::LlmMessage> {
    render_history_messages(
        &RlmHistoryRenderInput {
            events,
            turn_messages: &lash_core::facade_support::MessageSequence::default(),
            turn_causes: &[],
            max_output_chars: 1000,
            protocol_iteration: 0,
            finalization: "",
            required_output: None,
            final_answer_format: None,
            budget_suffix: None,
            bound_variables: "",
        },
        &mut Vec::new(),
    )
}

#[test]
fn ordered_reasoning_replay_round_trips_through_standalone_history_message() {
    let first = replay("rs_rlm_1", "first-replay-payload");
    let mut second = replay("rs_rlm_2", "second-replay-payload");
    second.signature = Some("second-signature".to_string());
    second.redacted = true;
    let messages = render(&[assistant_reasoning_event(
        &[
            ("first reasoning", first.clone()),
            ("second reasoning", second.clone()),
        ],
        "Ready.",
    )]);

    assert!(matches!(
        messages[0].blocks.as_slice(),
        [
            LlmContentBlock::Reasoning { text, replay: Some(actual) },
            LlmContentBlock::Reasoning { text: second_text, replay: Some(second_actual) },
            LlmContentBlock::Text { text: prose, .. }
        ] if text == "first reasoning"
            && actual == &first
            && second_text == "second reasoning"
            && second_actual == &second
            && prose.as_ref() == "Ready."
    ));
}

#[test]
fn ordered_reasoning_replay_precedes_cell_in_folded_history_message() {
    let first = replay("fold_1", "fold-replay-1");
    let second = replay("fold_2", "fold-replay-2");
    let messages = render(&[
        assistant_reasoning_event(
            &[
                ("first folded reasoning", first.clone()),
                ("second folded reasoning", second.clone()),
            ],
            "Working.",
        ),
        step_event("value = inspect()"),
    ]);

    assert!(matches!(
        messages[0].blocks.as_slice(),
        [
            LlmContentBlock::Reasoning { text: first_text, replay: Some(first_actual) },
            LlmContentBlock::Reasoning { text: second_text, replay: Some(second_actual) },
            LlmContentBlock::Text { text: cell, .. }
        ] if first_text == "first folded reasoning"
            && first_actual == &first
            && second_text == "second folded reasoning"
            && second_actual == &second
            && cell.as_ref()
                == crate::cell_scan::render_lashlang_cell_text("Working.", "value = inspect()")
    ));
}
