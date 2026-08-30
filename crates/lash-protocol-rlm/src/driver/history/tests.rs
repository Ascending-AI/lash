use lash_core::llm::types::{LlmContentBlock, ProviderReasoningReplay};
use lash_core::session_model::{ConversationRecord, MessageRole, Part};
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
        ..Default::default()
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
    parts.push(Part::prose(
        format!("{id}.p{}", parts.len()),
        prose.to_string(),
        None,
    ));
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
    let dialect = crate::dialect::LashlangDialect::prompt_only(
        lash_lashlang_runtime::LashlangSurface::default(),
    );
    render_history_messages(
        &RlmHistoryRenderInput {
            dialect: &dialect,
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
                == crate::cell_scan::render_cell_text(
                    crate::dialect::CellTags {
                        open: "<lashlang>",
                        close: "</lashlang>",
                    },
                    "Working.",
                    "value = inspect()",
                )
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

    [ERROR]
    read failed at secret.txt; cache failed at .cache/lash/state

    Next: the defect is in the program, not in what the runtime allows. Fix the cause named above, then send the corrected block.
    "#);
    assert!(observation.contains("Calls:\n- module.ok → ok\n- module.fail → err"));
    assert!(!observation.contains("secret: 1"), "arguments stay elided");
    assert!(observation.contains("Next: the defect is in the program"));
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

fn failed_step_event(id: &str, code: &str, error: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(
        lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(lash_rlm_types::RlmTrajectoryEntry {
            id: id.to_string(),
            protocol_iteration: 0,
            code: code.to_string(),
            output: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            error: Some(error.to_string()),
            final_output: None,
        }),
    ))
}

fn protocol_feedback(id: &str, text: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord {
        id: id.to_string(),
        role: MessageRole::System,
        parts: vec![Part::prose(format!("{id}.p0"), text.to_string(), None)].into(),
        origin: Some(MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    })
}

fn rendered_text(messages: &[lash_core::llm::types::LlmMessage]) -> String {
    messages
        .iter()
        .map(observation_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Leg 2 of the retry-hygiene triple: a failed cell has to be visible to the
/// model that must repair it. This is the leg the driver already had — pinned
/// because it is the one a scrub is most likely to break.
#[test]
fn a_failed_cell_stays_visible_for_the_repair_turn() {
    let transcript = rendered_text(&render(&[
        failed_step_event("lashlang_step_0", "print undefined_name", "unknown name"),
        protocol_feedback("s1", "That step failed; retry with a corrected program."),
    ]));

    assert!(transcript.contains("print undefined_name"), "{transcript}");
    assert!(transcript.contains("unknown name"), "{transcript}");
    assert!(
        transcript.contains("retry with a corrected program"),
        "{transcript}"
    );
}

/// Leg 3: once a later cell runs clean, the dead end goes — cell, error
/// observation, folded prose, and the repair instruction alike. A model
/// re-reading its own discarded attempts re-attempts them.
#[test]
fn a_repaired_failure_is_scrubbed_after_the_next_success() {
    let transcript = rendered_text(&render(&[
        assistant_reasoning_event(&[], "Trying the direct read."),
        failed_step_event("lashlang_step_0", "print undefined_name", "unknown name"),
        protocol_feedback("s1", "That step failed; retry with a corrected program."),
        step_event("print 1"),
    ]));

    assert!(!transcript.contains("undefined_name"), "{transcript}");
    assert!(!transcript.contains("unknown name"), "{transcript}");
    assert!(
        !transcript.contains("retry with a corrected program"),
        "{transcript}"
    );
    assert!(
        !transcript.contains("Trying the direct read."),
        "prose folded into a scrubbed cell must not reattach to the next one: {transcript}"
    );
    assert!(transcript.contains("print 1"), "{transcript}");
}

/// The scrub is scoped to the run of cells between turn boundaries. A user
/// message opens new work, so a success after it has repaired nothing that came
/// before and the earlier failure is still the model's own live context.
#[test]
fn a_failure_before_a_user_turn_survives_a_later_success() {
    let transcript = rendered_text(&render(&[
        failed_step_event("lashlang_step_0", "print undefined_name", "unknown name"),
        SessionHistoryRecord::Conversation(ConversationRecord {
            id: "u1".to_string(),
            role: MessageRole::User,
            parts: vec![Part::prose(
                "u1.p0".to_string(),
                "Now try something else.".to_string(),
                None,
            )]
            .into(),
            origin: None,
        }),
        step_event("print 1"),
    ]));

    assert!(transcript.contains("undefined_name"), "{transcript}");
}

/// Two failures then a success: both dead ends go, not just the last one.
#[test]
fn every_failure_in_the_repaired_run_is_scrubbed() {
    let transcript = rendered_text(&render(&[
        failed_step_event("lashlang_step_0", "print first_bad", "unknown name"),
        failed_step_event("lashlang_step_1", "print second_bad", "unknown name"),
        step_event("print 1"),
    ]));

    assert!(!transcript.contains("first_bad"), "{transcript}");
    assert!(!transcript.contains("second_bad"), "{transcript}");
    assert!(transcript.contains("print 1"), "{transcript}");
}

/// A refusal and a runtime failure call for opposite next moves, and until the
/// tag existed both arrived as `Error:` followed by prose. The observation now
/// says which it is and puts the instruction in its own text, so the model can
/// read the second half without re-reading the first.
#[test]
fn a_refusal_and_a_runtime_failure_read_differently() {
    let refused = rendered_text(&render(&[failed_step_event(
        "lashlang_step_0",
        "class A {}",
        &crate::feedback::RlmFeedbackKind::Policy.label("TS_CLASS_UNSUPPORTED: classes"),
    )]));
    assert!(
        refused.contains("[POLICY]\nTS_CLASS_UNSUPPORTED: classes"),
        "{refused}"
    );
    assert!(
        refused.contains("sending it again unchanged will be refused again"),
        "{refused}"
    );
    assert!(
        !refused.contains("the defect is in the program"),
        "{refused}"
    );

    let threw = rendered_text(&render(&[failed_step_event(
        "lashlang_step_0",
        "print rows[9]",
        &crate::feedback::RlmFeedbackKind::Error.label("index out of range"),
    )]));
    assert!(threw.contains("[ERROR]\nindex out of range"), "{threw}");
    assert!(threw.contains("the defect is in the program"), "{threw}");
    assert!(!threw.contains("refused"), "{threw}");
}

#[test]
fn a_stop_carrier_never_renders_as_model_feedback() {
    let transcript = rendered_text(&render(&[failed_step_event(
        "lashlang_step_0",
        "value = 1",
        &crate::feedback::RlmFeedbackKind::Stop
            .label("lashlang execution was cancelled by the host"),
    )]));

    assert!(!transcript.contains("[STOP]"), "{transcript}");
    assert!(
        !transcript.contains("cancelled by the host"),
        "{transcript}"
    );
    assert!(!transcript.contains("Next:"), "{transcript}");
}

/// A host-authored System message is not the RLM protocol's to delete.
///
/// `System` is a shared channel: a plugin directive enqueued at a mid-turn
/// checkpoint lands on it exactly where the protocol's own retry feedback would.
/// Scrubbing by role therefore reached past the protocol's own output and
/// deleted a policy reminder the host injected between a failed cell and the
/// next good one — silently, from every later render, with no other copy.
#[test]
fn a_host_system_message_survives_the_scrub() {
    let transcript = rendered_text(&render(&[
        failed_step_event("lashlang_step_0", "print undefined_name", "unknown name"),
        SessionHistoryRecord::Conversation(ConversationRecord {
            id: "h1".to_string(),
            role: MessageRole::System,
            parts: vec![Part::prose(
                "h1.p0".to_string(),
                "Reminder: never write outside the workspace.".to_string(),
                None,
            )]
            .into(),
            origin: Some(MessageOrigin::Plugin {
                plugin_id: "some-other-plugin".to_string(),
                transient: false,
            }),
        }),
        step_event("print 1"),
    ]));

    assert!(!transcript.contains("undefined_name"), "{transcript}");
    assert!(
        transcript.contains("Reminder: never write outside the workspace."),
        "the host's own message must outlive the failure it happened to follow: {transcript}"
    );
}

/// Scrubbing an entry clears the buffered prose, so mis-scrubbing one entry
/// takes the *next* cell's prose with it. With the host message correctly
/// excluded there is nothing to clear, and the surviving cell keeps its prose.
#[test]
fn a_surviving_cells_prose_is_not_taken_by_the_scrub() {
    let transcript = rendered_text(&render(&[
        failed_step_event("lashlang_step_0", "print undefined_name", "unknown name"),
        assistant_reasoning_event(&[], "Second attempt, reading the bound value."),
        SessionHistoryRecord::Conversation(ConversationRecord {
            id: "h1".to_string(),
            role: MessageRole::System,
            parts: vec![Part::prose(
                "h1.p0".to_string(),
                "Reminder: stay in the workspace.".to_string(),
                None,
            )]
            .into(),
            origin: Some(MessageOrigin::Plugin {
                plugin_id: "some-other-plugin".to_string(),
                transient: false,
            }),
        }),
        step_event("print 1"),
    ]));

    assert!(!transcript.contains("undefined_name"), "{transcript}");
    assert!(
        transcript.contains("Second attempt, reading the bound value."),
        "the surviving cell's prose must not be collateral: {transcript}"
    );
    assert!(
        transcript.contains("Reminder: stay in the workspace."),
        "{transcript}"
    );
}

/// Non-vacuity for the identity gate: the protocol's *own* feedback message,
/// on the same channel, still goes.
#[test]
fn the_protocols_own_feedback_is_still_scrubbed() {
    let transcript = rendered_text(&render(&[
        failed_step_event("lashlang_step_0", "print undefined_name", "unknown name"),
        protocol_feedback("s1", "That step failed; retry with a corrected program."),
        step_event("print 1"),
    ]));

    assert!(
        !transcript.contains("retry with a corrected program"),
        "{transcript}"
    );
}
