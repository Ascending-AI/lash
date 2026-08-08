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
            calls: Vec::new(),
            calls_omitted: 0,
            error: None,
            final_output: None,
        }),
    ))
}

fn observation_text(message: &lash_core::llm::types::LlmMessage) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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

#[test]
fn failed_observation_lists_executed_calls_and_frames_retry() {
    let event = SessionHistoryRecord::Protocol(rlm_protocol_event(
        lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(lash_rlm_types::RlmTrajectoryEntry {
            id: "lashlang_step_failed".to_string(),
            protocol_iteration: 0,
            code: "first = await module.ok({ secret: 1 })\nsecond = await module.fail({})"
                .to_string(),
            output: Vec::new(),
            images: Vec::new(),
            calls: vec![
                lash_rlm_types::RlmExecutedCall {
                    operation: "module.ok".to_string(),
                    outcome: lash_rlm_types::RlmExecutedCallOutcome::Ok,
                },
                lash_rlm_types::RlmExecutedCall {
                    operation: "module.fail".to_string(),
                    outcome: lash_rlm_types::RlmExecutedCallOutcome::Err,
                },
            ],
            calls_omitted: 0,
            error: Some("read failed at secret.txt; cache failed at .cache/lash/state".to_string()),
            final_output: None,
        }),
    ));

    let messages = render(&[event]);
    let observation = observation_text(&messages[1]);

    insta::assert_snapshot!(observation, @r#"
    Calls:
    - module.ok → ok
    - module.fail → err

    Error:
    read failed at secret.txt; cache failed at .cache/lash/state

    This step failed; you may retry with a corrected program.
    "#);
    assert!(observation.contains("Calls:\n- module.ok → ok\n- module.fail → err"));
    assert!(!observation.contains("secret: 1"), "arguments stay elided");
    assert!(observation.contains("This step failed; you may retry with a corrected program."));
    assert!(observation.contains("secret.txt"));
    assert!(observation.contains(".cache/lash/state"));
}

#[test]
fn successful_observation_keeps_calls_and_exact_earlier_omission_marker() {
    let event = SessionHistoryRecord::Protocol(rlm_protocol_event(
        lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(lash_rlm_types::RlmTrajectoryEntry {
            id: "lashlang_step_success".to_string(),
            protocol_iteration: 0,
            code: "value = module.ok()".to_string(),
            output: Vec::new(),
            images: Vec::new(),
            calls: vec![lash_rlm_types::RlmExecutedCall {
                operation: "module.ok".to_string(),
                outcome: lash_rlm_types::RlmExecutedCallOutcome::Ok,
            }],
            calls_omitted: 3,
            error: None,
            final_output: None,
        }),
    ));

    let messages = render(&[event]);
    let observation = observation_text(&messages[1]);

    assert_eq!(
        observation,
        "Calls:\n- … 3 earlier executed calls omitted\n- module.ok → ok"
    );
}

#[test]
fn legacy_unredacted_trajectory_errors_render_verbatim() {
    let event = SessionHistoryRecord::Protocol(rlm_protocol_event(
        lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(lash_rlm_types::RlmTrajectoryEntry {
            id: "lashlang_step_legacy".to_string(),
            protocol_iteration: 0,
            code: "value = read()".to_string(),
            output: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            error: Some("read failed at /legacy/worker/private.txt".to_string()),
            final_output: None,
        }),
    ));

    let messages = render(&[event]);
    assert!(observation_text(&messages[1]).contains("/legacy/worker/private.txt"));
}
