use super::*;
use lash_core::{Part, SessionHistoryRecord};
use lash_rlm_types::{CellOutcome, RlmProtocolEvent, RlmTrajectoryEntry};
use lash_sansio::TurnId;

fn step(id: &str, error: Option<&str>, terminal: bool) -> RlmTrajectoryEntry {
    RlmTrajectoryEntry {
        id: id.to_string(),
        protocol_iteration: 0,
        code: "finish 1".to_string(),
        output: vec!["observed".to_string().into()],
        images: Vec::new(),
        calls: Vec::new(),
        calls_omitted: 0,
        outcome: CellOutcome::from_parts(
            error.map(str::to_string),
            terminal.then(|| serde_json::json!(1)),
        ),
    }
}
fn pair(step: RlmTrajectoryEntry) -> Vec<SessionHistoryRecord> {
    let parts = vec![Part::tool_call(
        "p0".to_string(),
        r#"{"code":"finish 1"}"#.to_string(),
        lash_core::ToolCallId::fixture(&step.id),
        step.id.clone(),
        "execute_code".to_string(),
        Some(lash_core::llm::types::ProviderReplayMeta {
            opaque: Some("never-reconstruct-this".to_string()),
            item_id: Some("item".to_string()),
            origin: None,
        }),
    )];
    vec![
        crate::native::transport::execution_event(
            step.id.clone(),
            parts,
            crate::native::transport::NATIVE_TRANSPORT_VERSION,
        ),
        SessionHistoryRecord::Protocol(crate::projection::rlm_protocol_event(
            RlmProtocolEvent::RlmTrajectoryEntry(step),
        )),
    ]
}
fn render(events: &[SessionHistoryRecord]) -> Vec<LlmMessage> {
    let dialect = crate::dialect::TypescriptDialect::prompt_only(
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
    let dialect = crate::dialect::TypescriptDialect::prompt_only(
        lash_lashlang_runtime::LashlangSurface::default(),
    );
    let expected = crate::driver::history::step_output_text(dialect.prompt_vocabulary(), 0, &entry);
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
        let dialect = crate::dialect::typescript_test_dialect();
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

fn native_envelope(payload: serde_json::Value) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(crate::projection::rlm_protocol_event(
        RlmProtocolEvent::RlmDiagnostic(lash_rlm_types::RlmDiagnosticEvent {
            phase: "native_transport".into(),
            payload,
        }),
    ))
}

fn envelope_payload(event: &SessionHistoryRecord) -> serde_json::Value {
    let SessionHistoryRecord::Protocol(event) = event else {
        panic!("expected protocol event");
    };
    let Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) = decode_rlm_protocol_event(event) else {
        panic!("expected diagnostic");
    };
    diagnostic.payload
}

#[test]
fn duplicate_execution_ids_keep_the_first_binding() {
    let first = pair(step("first-call", None, false));
    let second = pair(step("second-call", None, false));
    let mut first_payload = envelope_payload(&first[0]);
    first_payload["step_id"] = "shared".into();
    let mut second_payload = envelope_payload(&second[0]);
    second_payload["step_id"] = "shared".into();
    for later in [
        second_payload,
        serde_json::json!({"schema_version": 2, "step_id": "shared"}),
    ] {
        let mut events = vec![
            native_envelope(first_payload.clone()),
            native_envelope(later),
        ];
        events.push(pair(step("shared", None, false)).remove(1));
        assert_eq!(ids(&render(&events)).0, ["first-call"]);
    }
}

#[test]
fn matching_corruption_refuses_a_later_execution_without_harming_other_steps() {
    for corrupt in [
        serde_json::json!({"kind": "execution", "step_id": "bad", "parts": "invalid"}),
        serde_json::json!({"schema_version": 2, "step_id": "bad"}),
    ] {
        let mut events = pair(step("before", None, false));
        events.push(native_envelope(corrupt));
        events.extend(pair(step("bad", None, false)));
        events.extend(pair(step("after", None, false)));
        let messages = render(&events);
        assert_eq!(ids(&messages).0, ["before", "after"]);
        let text = serde_json::to_string(&messages).unwrap();
        assert_eq!(text.matches("degraded binding").count(), 2);
        assert!(matches!(messages[2].role, LlmRole::User));
        assert!(matches!(messages[3].role, LlmRole::User));
    }
}

#[test]
fn repair_with_raw_step_id_is_skipped_only_when_valid() {
    let execution = pair(step("execution", None, false));
    let repair = crate::native::transport::repair_event(
        &TurnId::from("turn"),
        0,
        serde_json::from_value(envelope_payload(&execution[0])["parts"].clone()).unwrap(),
        "repair feedback".into(),
        crate::native::transport::NATIVE_TRANSPORT_VERSION,
    );
    let mut payload = envelope_payload(&repair);
    payload["step_id"] = "execution".into();
    for version in [1, 2] {
        let mut payload = payload.clone();
        payload["schema_version"] = version.into();
        let mut events = vec![native_envelope(payload)];
        events.extend(execution.clone());
        let messages = render(&events);
        if version == 1 {
            // The later successful trajectory scrubs the earlier repair pair.
            assert_eq!(ids(&messages).0, ["execution"]);
        } else {
            assert!(ids(&messages).0.is_empty());
            assert_eq!(
                serde_json::to_string(&messages)
                    .unwrap()
                    .matches("degraded binding")
                    .count(),
                2
            );
        }
    }
    let mut malformed = envelope_payload(&repair);
    malformed["step_id"] = "execution".into();
    malformed["parts"] = "invalid".into();
    let mut events = vec![native_envelope(malformed)];
    events.extend(execution);
    assert!(ids(&render(&events)).0.is_empty());
}

#[test]
fn unbound_malformed_envelopes_degrade_at_their_chronological_positions() {
    let mut events = pair(step("before", None, false));
    for payload in [
        serde_json::json!("malformed"),
        serde_json::json!({"schema_version": 2}),
        serde_json::json!({"kind": "execution", "parts": []}),
        serde_json::json!({"kind": "execution", "step_id": 42, "parts": []}),
    ] {
        events.push(native_envelope(payload));
    }
    events.extend(pair(step("after", None, false)));
    let messages = render(&events);
    assert_eq!(ids(&messages).0, ["before", "after"]);
    assert_eq!(messages.len(), 8);
    for message in &messages[2..6] {
        assert!(matches!(message.role, LlmRole::User));
        assert!(
            serde_json::to_string(message)
                .unwrap()
                .contains("degraded binding")
        );
    }
}

#[test]
fn many_step_projection_matches_bytes_with_one_transport_pass_and_decode() {
    let dialect = crate::dialect::TypescriptDialect::prompt_only(
        lash_lashlang_runtime::LashlangSurface::default(),
    );
    for count in [128, 16, 1] {
        let mut events = Vec::new();
        let mut expected = Vec::new();
        for index in 0..count {
            let entry = step(&format!("step-{index}"), None, false);
            let exchange = pair(entry.clone());
            let parts: Vec<Part> =
                serde_json::from_value(envelope_payload(&exchange[0])["parts"].clone()).unwrap();
            crate::native::transport::append_pair(
                &mut expected,
                &parts,
                &crate::driver::history::step_output_text(
                    dialect.prompt_vocabulary(),
                    index,
                    &entry,
                ),
            );
            events.extend(exchange);
        }
        let repair = crate::native::transport::repair_event(
            &TurnId::from("turn"),
            count,
            serde_json::from_value(envelope_payload(&events[0])["parts"].clone()).unwrap(),
            "repair feedback".into(),
            crate::native::transport::NATIVE_TRANSPORT_VERSION,
        );
        let parts: Vec<Part> =
            serde_json::from_value(envelope_payload(&repair)["parts"].clone()).unwrap();
        crate::native::transport::append_pair(&mut expected, &parts, "repair feedback");
        events.push(repair);
        crate::native::transport::work::reset();
        let actual = render(&events);
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(
            crate::native::transport::work::counts(),
            (events.len(), count + 1),
            "transport must visit history once and decode each native envelope once per render"
        );
    }
}

#[test]
fn reloaded_null_finish_remains_terminal_in_reconstructed_history() {
    let mut entry = step("null-finish", None, false);
    entry.code = "finish(null)".into();
    entry.outcome = CellOutcome::Finished(serde_json::Value::Null);
    let events = pair(entry);
    let mut reloaded =
        serde_json::from_str::<Vec<SessionHistoryRecord>>(&serde_json::to_string(&events).unwrap())
            .unwrap();
    let SessionHistoryRecord::Protocol(event) = &reloaded[1] else {
        panic!("trajectory event")
    };
    let Some(RlmProtocolEvent::RlmTrajectoryEntry(restored)) =
        crate::projection::decode_rlm_protocol_event(event)
    else {
        panic!("decoded trajectory")
    };
    assert_eq!(
        restored.outcome.terminal_value(),
        Some(&serde_json::Value::Null)
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
            }),
        }),
    ));
    assert!(
        ids(&render(&reloaded)).0.is_empty(),
        "terminal exchange is suppressed after transcript commit"
    );
}
