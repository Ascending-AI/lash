use super::*;

#[test]
fn assembler_uses_assistant_message_outcome_without_recovery_issue_when_no_streamed_prose() {
    let mut assembler = RecordedTurnAssembly::default();
    assembler.record(&SessionStreamEvent::TurnOutcome {
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "settled answer".to_string(),
        }),
    });
    assembler.record(&SessionStreamEvent::Done);

    let out = assembler.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );

    assert_eq!(
        out.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "settled answer".to_string()
        })
    );
    assert!(
        out.errors
            .iter()
            .all(|issue| issue.severity != lash_core::runtime::TurnIssueSeverity::Advisory)
    );
}

#[test]
fn interrupted_assembler_does_not_reuse_assistant_before_latest_user_message() {
    let mut state = default_state();
    append_message(
        &mut state,
        Message {
            id: "a0".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::prose(
                "a0.p0".to_string(),
                "previous assistant answer".to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    );
    append_message(
        &mut state,
        Message {
            id: "u1".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text(
                "u1.p0".to_string(),
                "new prompt".to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    );

    let mut assembler = RecordedTurnAssembly::default();
    assembler.record(&SessionStreamEvent::Done);
    let out = assembler.finish(
        state.to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );

    assert_eq!(
        out.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: String::new()
        })
    );
}

#[test]
fn assembler_prefers_state_output_when_streamed_text_is_a_truncated_prefix() {
    let mut state = default_state();
    append_message(
        &mut state,
        Message {
            id: "m0".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::prose(
                "m0.p0".to_string(),
                "You graduated with a degree in Business Administration.".to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    );
    let mut assembler = RecordedTurnAssembly::default();
    assembler.record(&SessionStreamEvent::TextDelta {
        content: "You graduated with a degree in Business".to_string(),
        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
    });
    assembler.record(&SessionStreamEvent::Done);
    let out = assembler.finish(
        state.to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    assert_eq!(
        out.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "You graduated with a degree in Business Administration.".to_string()
        })
    );
}

#[test]
fn assembler_state_output_excludes_tool_call_payload() {
    // Regression: codex commits an assistant message containing a prose
    // part followed by a tool-call part whose `content` is the raw JSON
    // arguments. On interrupt the assembler falls back to the last
    // assistant message's parts; concatenating EVERY part's content
    // leaks the tool-call JSON into the recovered assistant text. Only
    // Text/Prose/Attachment parts belong to it.
    let mut state = default_state();
    append_message(
        &mut state,
        Message {
            id: "m0".to_string(),
            role: MessageRole::Assistant,
            parts: vec![
                Part::prose(
                    "m0.p0".to_string(),
                    "Searching for the relevant code.".to_string(),
                    None,
                ),
                Part::tool_call(
                    "m0.p1".to_string(),
                    "{\"tool_calls\":[{\"tool\":\"grep\",\"parameters\":{\"query\":\"x\"}}]}"
                        .to_string(),
                    lash_core::ToolCallId::fixture("tc1"),
                    "tc1".to_string(),
                    "batch".to_string(),
                    None,
                ),
            ]
            .into(),
            origin: None,
            reply_marker: None,
        },
    );
    let mut assembler = RecordedTurnAssembly::default();
    assembler.record(&SessionStreamEvent::Done);
    let out = assembler.finish(
        state.to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    assert_eq!(
        out.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "Searching for the relevant code.".to_string()
        })
    );
}

#[test]
fn assembler_treats_any_non_success_record_as_tool_failure() {
    let mut assembler = RecordedTurnAssembly::default();
    assembler.record(&SessionStreamEvent::ToolCall {
        call_id: lash_core::ToolCallId::fixture("tc-cancelled"),
        provider_call_id: None,
        name: "x".to_string(),
        args: serde_json::json!({}),
        output: lash_core::ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime(
            "tool cancelled",
        )),
    });
    assembler.record(&SessionStreamEvent::Error {
        message: "runtime also reported a blocking issue".to_string(),
        envelope: None,
    });
    assembler.record(&SessionStreamEvent::Done);

    let out = assembler.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );

    assert_eq!(out.outcome, TurnOutcome::Stopped(TurnStop::ToolFailure));
}

#[test]
fn assembler_classifies_failure_omitted_beyond_128_call_horizon() {
    let mut assembler = RecordedTurnAssembly::default();
    for index in 0..128 {
        assembler.record(&SessionStreamEvent::ToolCall {
            call_id: lash_core::ToolCallId::fixture(&format!("call-{index}")),
            provider_call_id: None,
            name: "successful_tool".to_string(),
            args: serde_json::json!({ "index": index }),
            output: lash_core::ToolCallOutput::success(serde_json::json!(index)),
        });
    }
    assembler.record(&SessionStreamEvent::ToolCallsOmitted {
        summary: lash_core::OmittedToolCalls {
            count: 1,
            failures: 1,
            attachments: Vec::new(),
        },
    });
    assembler.record(&SessionStreamEvent::Error {
        message: "runtime also reported a blocking issue".to_string(),
        envelope: None,
    });
    assembler.record(&SessionStreamEvent::Done);

    let out = assembler.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );

    assert_eq!(out.outcome, TurnOutcome::Stopped(TurnStop::ToolFailure));
    assert_eq!(out.tool_calls.len(), 128);
    assert_eq!(out.omitted.expect("typed omission").failures, 1);
}

#[test]
fn assembler_marks_missing_done_as_failure() {
    let mut assembler = RecordedTurnAssembly::default();
    assembler.record(&SessionStreamEvent::TextDelta {
        content: "partial".to_string(),
        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
    });
    let out = assembler.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    assert!(matches!(&out.outcome, TurnOutcome::Stopped(_)));
    assert!(matches!(
        &out.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
}

#[tokio::test]
async fn normalize_items_merges_adjacent_text_items() {
    let items = vec![
        InputItem::Text {
            text: "before ".to_string(),
        },
        InputItem::Text {
            text: "[file: host-prepared.txt]".to_string(),
        },
    ];
    let out = normalize_input_items(
        &items,
        &lash_core::facade_support::RuntimeAttachmentStore::unavailable(),
    )
    .await
    .expect("normalized");
    assert_eq!(out.len(), 1);
    match &out[0] {
        NormalizedItem::Text(text) => {
            assert_eq!(text, "before [file: host-prepared.txt]");
        }
        _ => panic!("expected merged text item"),
    }
}

#[test]
fn producer_severity_controls_completion_independently_of_issue_code() {
    use lash_core::runtime::TurnIssueSeverity;
    for severity in [TurnIssueSeverity::Advisory, TurnIssueSeverity::Blocking] {
        let mut assembler = RecordedTurnAssembly::default();
        assembler.record(&SessionStreamEvent::Done);
        let issue = TurnIssue {
            severity,
            kind: lash_core::TurnFailureKind::Runtime,
            code: Some(lash_core::TurnFailureCode::from_wire("arbitrary-new-code").into()),
            terminal_reason: None,
            message: "evidence".into(),
            raw: None,
            retryable: None,
            provider_failure_kind: None,
            plugin_failures: Vec::new(),
        };
        let out = assembler.finish(
            default_state().to_snapshot(),
            None,
            Some(issue),
            &TerminationPolicy::default(),
        );
        assert_eq!(out.errors[0].severity, severity);
        assert_eq!(
            matches!(out.outcome, TurnOutcome::Stopped(TurnStop::RuntimeError)),
            severity == TurnIssueSeverity::Blocking
        );
    }
}

#[test]
fn recovered_output_producer_emits_advisory_severity() {
    let mut state = default_state();
    append_message(
        &mut state,
        Message {
            id: "recovered".into(),
            role: MessageRole::Assistant,
            parts: vec![Part::text(
                "recovered.p0".into(),
                "saved answer".into(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    );
    let mut assembler = RecordedTurnAssembly::default();
    assembler.record(&SessionStreamEvent::Done);
    let out = assembler.finish(
        state.to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    assert_eq!(out.errors.len(), 1);
    assert_eq!(
        out.errors[0].severity,
        lash_core::runtime::TurnIssueSeverity::Advisory
    );
    assert!(matches!(out.outcome, TurnOutcome::Finished(_)));
}
