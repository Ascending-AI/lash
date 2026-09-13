use super::*;
use lash_core::{Part, SessionHistoryRecord};
use lash_rlm_types::{RlmProtocolEvent, RlmTrajectoryEntry};
use lash_sansio::TurnId;

fn step(id: &str, error: Option<&str>, terminal: bool) -> RlmTrajectoryEntry {
    RlmTrajectoryEntry {
        id: id.to_string(),
        protocol_iteration: 0,
        code: "finish 1".to_string(),
        output: vec!["observed".to_string()],
        images: Vec::new(),
        calls: Vec::new(),
        calls_omitted: 0,
        error: error.map(str::to_string),
        final_output: terminal.then(|| serde_json::json!(1)),
    }
}
fn pair(step: RlmTrajectoryEntry) -> Vec<SessionHistoryRecord> {
    let parts = vec![Part::tool_call(
        "p0".to_string(),
        r#"{"code":"finish 1"}"#.to_string(),
        step.id.clone(),
        "execute_code".to_string(),
        Some(lash_core::llm::types::ProviderReplayMeta {
            opaque: Some("never-reconstruct-this".to_string()),
            item_id: Some("item".to_string()),
            origin: None,
        }),
    )];
    vec![
        crate::native::transport::execution_event(step.id.clone(), parts),
        SessionHistoryRecord::Protocol(crate::projection::rlm_protocol_event(
            RlmProtocolEvent::RlmTrajectoryEntry(step),
        )),
    ]
}
fn render(events: &[SessionHistoryRecord]) -> Vec<LlmMessage> {
    let dialect = crate::dialect::LashlangDialect::prompt_only(
        lash_lashlang_runtime::LashlangSurface::default(),
    );
    let turn_messages = lash_core::facade_support::MessageSequence::default();
    render_history_messages(&RlmHistoryRenderInput {
        images: true,
        dialect: &dialect,
        events,
        turn_messages: &turn_messages,
        turn_causes: &[],
        max_output_chars: 1000,
        protocol_iteration: 1,
        finalization: "",
        required_output: None,
        final_answer_format: None,
        budget_suffix: None,
        bound_variables: "",
    })
}
fn ids(messages: &[LlmMessage]) -> (Vec<String>, Vec<String>) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for block in messages.iter().flat_map(|message| message.blocks.iter()) {
        match block {
            LlmContentBlock::ToolCall {
                call_id, replay, ..
            } => {
                assert_eq!(
                    replay.as_ref().unwrap().opaque.as_deref(),
                    Some("never-reconstruct-this")
                );
                calls.push(call_id.clone());
            }
            LlmContentBlock::ToolResult { call_id, .. } => results.push(call_id.clone()),
            _ => {}
        }
    }
    assert_eq!(
        calls, results,
        "native history must always contain complete exchanges"
    );
    (calls, results)
}

#[test]
fn fig1123_native_history_marks_only_real_turn_inputs_as_segment_boundaries() {
    let events = vec![
        SessionHistoryRecord::Conversation(lash_core::session_model::ConversationRecord {
            id: "real".to_string(),
            role: lash_core::MessageRole::User,
            parts: vec![Part::text("real.p0".to_string(), "real".to_string(), None)].into(),
            origin: Some(lash_core::MessageOrigin::TurnInput {
                turn_id: TurnId::from("turn"),
                input_id: None,
            }),
        }),
        SessionHistoryRecord::Conversation(lash_core::session_model::ConversationRecord {
            id: "synthetic".to_string(),
            role: lash_core::MessageRole::User,
            parts: vec![Part::text(
                "synthetic.p0".to_string(),
                "synthetic".to_string(),
                None,
            )]
            .into(),
            origin: Some(lash_core::MessageOrigin::Plugin {
                plugin_id: "plugin".to_string(),
                transient: false,
            }),
        }),
    ];

    let messages = render(&events);

    assert!(messages[0].starts_user_segment);
    assert!(!messages[1].starts_user_segment);
}
#[test]
fn reload_preserves_replay_and_observation_bytes() {
    let entry = step("reload", None, false);
    let events = pair(entry.clone());
    let reloaded =
        serde_json::from_str::<Vec<SessionHistoryRecord>>(&serde_json::to_string(&events).unwrap())
            .unwrap();
    let messages = render(&reloaded);
    assert_eq!(ids(&messages).0, ["reload"]);
    let dialect = crate::dialect::LashlangDialect::prompt_only(
        lash_lashlang_runtime::LashlangSurface::default(),
    );
    let expected = crate::driver::history::step_output_text(
        crate::dialect::RlmDialect::prompt_vocabulary(&dialect),
        0,
        &entry,
    );
    let output = messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .find_map(|block| match block {
            LlmContentBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .unwrap();
    assert_eq!(output, &expected);
}
#[test]
fn superseded_failure_scrubs_both_sides() {
    let mut events = pair(step("failed", Some("compile error"), false));
    assert_eq!(ids(&render(&events)).0, ["failed"]);
    events.extend(pair(step("repaired", None, false)));
    assert_eq!(ids(&render(&events)).0, ["repaired"]);
}
#[test]
fn terminal_suppression_is_atomic_only_after_transcript_commit() {
    let mut events = pair(step("terminal", None, true));
    assert_eq!(ids(&render(&events)).0, ["terminal"]);
    let message = lash_core::Message {
        id: "answer".to_string(),
        role: lash_core::MessageRole::Assistant,
        parts: vec![Part::prose("answer.p0".to_string(), "1".to_string(), None)].into(),
        origin: Some(lash_core::MessageOrigin::TurnOutput {
            turn_id: TurnId::from("turn"),
            source: lash_core::TurnOutputSource::Runtime,
        }),
    };
    events.push(SessionHistoryRecord::Conversation(
        lash_core::session_model::ConversationRecord::from_message(message),
    ));
    assert!(ids(&render(&events)).0.is_empty());
}
#[test]
fn frame_switch_does_not_reconstruct_old_provider_calls() {
    let old = pair(step("old-frame", None, false));
    assert_eq!(ids(&render(&old)).0, ["old-frame"]);
    let next = pair(step("new-frame", None, false));
    assert_eq!(ids(&render(&next)).0, ["new-frame"]);
    // A seed can contain semantic history without its old transport envelope.
    let seed = render(&old[1..]);
    assert!(ids(&seed).0.is_empty());
    assert!(serde_json::to_string(&seed).unwrap().contains("observed"));
}

#[test]
fn corrupt_envelopes_degrade_individually_after_reload() {
    for payload in [
        serde_json::json!("malformed"),
        serde_json::json!({"schema_version":2}),
    ] {
        let mut events = vec![SessionHistoryRecord::Protocol(
            crate::projection::rlm_protocol_event(RlmProtocolEvent::RlmDiagnostic(
                lash_rlm_types::RlmDiagnosticEvent {
                    phase: "native_transport".into(),
                    payload,
                },
            )),
        )];
        events.extend(pair(step("healthy", None, false)));
        let restored = serde_json::from_str::<Vec<SessionHistoryRecord>>(
            &serde_json::to_string(&events).unwrap(),
        )
        .unwrap();
        let messages = render(&restored);
        assert_eq!(ids(&messages).0, ["healthy"]);
        let rendered = serde_json::to_string(&messages).unwrap();
        assert!(rendered.contains("degraded binding"));
        assert!(rendered.contains("native_transport"));
    }
}

#[test]
fn second_round_history_teaches_images_only_when_enabled() {
    for images in [false, true] {
        let dialect = crate::dialect::lashlang_test_dialect();
        let events = pair(step("previous", None, false));
        let messages = build_rlm_history_messages_from_turn(RlmHistoryRenderInput {
            images,
            dialect: &dialect,
            events: &events,
            turn_messages: &lash_core::facade_support::MessageSequence::default(),
            turn_causes: &[],
            max_output_chars: 1000,
            protocol_iteration: 2,
            finalization: "finish",
            required_output: None,
            final_answer_format: None,
            budget_suffix: None,
            bound_variables: "",
        });
        let tail = messages
            .last()
            .unwrap()
            .blocks
            .iter()
            .filter_map(|block| match block {
                LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(tail.contains("type HistoryItem ="), "{tail}");
        assert_eq!(tail.contains("HistoryImage"), images, "{tail}");
        assert_eq!(tail.contains("images?"), images, "{tail}");
    }
}
