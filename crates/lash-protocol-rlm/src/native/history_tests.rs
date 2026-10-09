use super::*;
use lash_core::{CellRecord, CellResult};
use lash_core::{Part, SessionHistoryRecord};
use lash_rlm_types::RlmProtocolEvent;
use lash_sansio::TurnId;

fn step(id: &str, error: Option<&str>, terminal: bool) -> CellRecord {
    CellRecord {
        id: id.to_string(),
        language: "typescript".to_string(),
        code: "finish 1".to_string(),
        prints: vec!["observed".to_string().into()],
        result: match (error, terminal) {
            (Some(message), _) => CellResult::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Program,
                message,
            )),
            (None, true) => CellResult::Finished(serde_json::json!(1).into()),
            (None, false) => CellResult::Completed,
        },
        ..CellRecord::default()
    }
}
/// The reply parts of a native `execute_code` call under `call`.
fn call_parts(call: &str) -> Vec<Part> {
    vec![Part::tool_call(
        "p0".to_string(),
        r#"{"code":"finish 1"}"#.to_string(),
        lash_core::ToolCallId::fixture(call),
        call.to_string(),
        "execute_code".to_string(),
        Some(lash_core::llm::types::ProviderReplayMeta {
            opaque: Some("never-reconstruct-this".to_string()),
            item_id: Some("item".to_string()),
            origin: None,
        }),
    )]
}
fn conversation(message: lash_core::Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(lash_core::session_model::ConversationRecord::from_message(
        message,
    ))
}
/// The assistant message of the reply `call` that ran the cell `cell_id`.
fn context(cell_id: &str, call: &str) -> SessionHistoryRecord {
    conversation(crate::native::transport::cell_context_message(
        &TurnId::from("turn"),
        format!("m_{call}"),
        cell_id.to_string(),
        call_parts(call),
    ))
}
fn trajectory(step: CellRecord) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(crate::projection::rlm_protocol_event(
        RlmProtocolEvent::RlmTrajectoryEntry(Box::new(step)),
        lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
            crate::RLM_PROTOCOL_EVENT_VERSION
        )),
    ))
}
fn pair(step: CellRecord) -> Vec<SessionHistoryRecord> {
    vec![context(&step.id, &step.id), trajectory(step)]
}
fn render(events: &[SessionHistoryRecord]) -> Vec<LlmMessage> {
    let dialect = crate::dialect::SessionDialect::prompt_only(
        std::sync::Arc::new(crate::dialect::TypescriptDialect),
        lash_vm_runtime::LashVmSurface::default(),
    );
    let turn_messages = lash_core::facade_support::MessageSequence::default();
    render_history_messages(&RlmHistoryRenderInput {
        dialect: &dialect,
        events,
        turn_messages: &turn_messages,
        max_output_chars: 1000,
        protocol_iteration: 1,
    })
    .expect("valid history fixture")
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
            reply_marker: None,
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
            reply_marker: None,
        }),
    ];

    let messages = render(&events);

    assert!(messages[0].starts_user_segment);
    assert!(!messages[1].starts_user_segment);
}
#[test]
fn native_replay_keeps_captured_result_and_opaque_bytes() {
    let entry = step("reload", None, false);
    let events = pair(entry.clone());
    let reloaded =
        serde_json::from_str::<Vec<SessionHistoryRecord>>(&serde_json::to_string(&events).unwrap())
            .unwrap();
    let messages = render(&reloaded);
    assert_eq!(ids(&messages).0, ["reload"]);
    let expected = "history[0].output[0]:\nobserved".to_string();
    let output = messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .find_map(|block| match block {
            LlmContentBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        output,
        &vec![lash_core::facade_support::ModelToolReturnPart::text(
            expected
        )]
    );
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
            cell_id: None,
        }),
        reply_marker: None,
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
fn duplicate_cell_contexts_keep_the_first_binding() {
    let events = vec![
        context("shared", "first-call"),
        context("shared", "second-call"),
        trajectory(step("shared", None, false)),
    ];
    assert_eq!(ids(&render(&events)).0, ["first-call"]);
}

/// A reply whose call ran nothing is committed as the assistant message it
/// was and the tool result that corrected it (FIG-5527): the prompt replays
/// them as one complete pair, and a later successful cell in the turn scrubs
/// both sides.
#[test]
fn a_refused_call_replays_as_its_pair_until_a_later_cell_supersedes_it() {
    let [call, result] = crate::native::transport::refused_call_messages(
        &TurnId::from("turn"),
        "m_refused".to_string(),
        "m_refused_result".to_string(),
        call_parts("refused"),
        "repair feedback".to_string(),
    );
    let mut events = vec![conversation(call), conversation(result)];
    let events_after_reload =
        serde_json::from_str::<Vec<SessionHistoryRecord>>(&serde_json::to_string(&events).unwrap())
            .unwrap();
    let messages = render(&events_after_reload);
    assert_eq!(ids(&messages).0, ["refused"]);
    assert_eq!(messages.len(), 2, "{messages:?}");
    let correction = messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .find_map(|block| match block {
            LlmContentBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .expect("the refused call is answered");
    assert_eq!(
        correction,
        &vec![lash_core::facade_support::ModelToolReturnPart::text(
            "repair feedback"
        )]
    );
    events.extend(pair(step("execution", None, false)));
    assert_eq!(ids(&render(&events)).0, ["execution"]);
}

#[test]
fn reloaded_null_finish_remains_terminal_in_reconstructed_history() {
    let mut entry = step("null-finish", None, false);
    entry.code = "finish(null)".into();
    entry.result = CellResult::Finished(serde_json::Value::Null.into());
    let events = pair(entry);
    let mut reloaded =
        serde_json::from_str::<Vec<SessionHistoryRecord>>(&serde_json::to_string(&events).unwrap())
            .unwrap();
    let SessionHistoryRecord::Protocol(event) = &reloaded[1] else {
        panic!("trajectory event")
    };
    let Some(RlmProtocolEvent::RlmTrajectoryEntry(restored)) =
        crate::projection::decode_rlm_protocol_event(event).expect("valid history fixture")
    else {
        panic!("decoded trajectory")
    };
    assert_eq!(
        restored.result.finish(),
        Some(&serde_json::Value::Null.into())
    );
    assert_eq!(ids(&render(&reloaded)).0, ["null-finish"]);
    reloaded.push(SessionHistoryRecord::Conversation(
        lash_core::session_model::ConversationRecord::from_message(lash_core::Message {
            id: "answer".into(),
            role: lash_core::MessageRole::Assistant,
            parts: vec![Part::prose("answer.p0".into(), "null".into(), None)].into(),
            origin: Some(lash_core::MessageOrigin::TurnOutput {
                turn_id: TurnId::from("turn"),
                source: lash_core::TurnOutputSource::Runtime,
                cell_id: None,
            }),
            reply_marker: None,
        }),
    ));
    assert!(
        ids(&render(&reloaded)).0.is_empty(),
        "terminal exchange is suppressed after transcript commit"
    );
}
