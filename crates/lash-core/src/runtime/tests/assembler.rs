use super::*;

#[test]
fn assembler_ignores_streamed_text_without_durable_output() {
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::TextDelta {
        content: "streamed but not committed".to_string(),
    });
    assembler.push(&SessionStreamEvent::Done);

    let out = assembler.finish(
        default_state().to_snapshot(),
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
    assert!(out.assistant_output.safe_text.is_empty());
    assert!(out.assistant_output.raw_text.is_empty());
    assert_eq!(out.assistant_output.state, OutputState::EmptyOutput);
}

#[test]
fn cancelled_assembler_with_only_streamed_text_has_empty_assistant_output() {
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::TextDelta {
        content: "partial answer".to_string(),
    });

    let out = assembler.finish(
        default_state().to_snapshot(),
        Some(crate::TurnCancellationEvidence::internal("assembler-test")),
        None,
        &TerminationPolicy::default(),
    );

    assert!(matches!(
        out.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(out.assistant_output.safe_text.is_empty());
    assert!(out.assistant_output.raw_text.is_empty());
    assert_eq!(out.assistant_output.state, OutputState::EmptyOutput);
}

#[test]
fn assembler_preserves_explicit_assistant_message_outcome() {
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::TurnOutcome {
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "first\n\nsecond".to_string(),
        }),
    });
    assembler.push(&SessionStreamEvent::Done);

    let out = assembler.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );

    assert_eq!(
        out.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "first\n\nsecond".to_string()
        })
    );
    assert_eq!(out.assistant_output.safe_text, "first\n\nsecond");
}

#[test]
fn assembler_uses_assistant_message_outcome_without_recovery_issue_when_no_streamed_prose() {
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::TurnOutcome {
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "settled answer".to_string(),
        }),
    });
    assembler.push(&SessionStreamEvent::Done);

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
    assert_eq!(out.assistant_output.safe_text, "settled answer");
    assert!(
        out.errors
            .iter()
            .all(|issue| issue.code.as_deref() != Some("assistant_output_recovered_from_state"))
    );
}

#[test]
fn assembler_uses_final_value_for_assistant_output() {
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::TurnOutcome {
        outcome: TurnOutcome::Finished(TurnFinish::FinalValue {
            value: serde_json::json!({ "ok": true }),
        }),
    });
    assembler.push(&SessionStreamEvent::Done);

    let out = assembler.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );

    assert_eq!(
        out.outcome,
        TurnOutcome::Finished(TurnFinish::FinalValue {
            value: serde_json::json!({ "ok": true })
        })
    );
    assert_eq!(out.assistant_output.safe_text, "{\n  \"ok\": true\n}");
}

#[test]
fn assembler_uses_tool_value_for_assistant_output() {
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::TurnOutcome {
        outcome: TurnOutcome::Finished(TurnFinish::ToolValue {
            tool_name: "finish".to_string(),
            value: serde_json::json!("done"),
        }),
    });
    assembler.push(&SessionStreamEvent::Done);

    let out = assembler.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );

    assert_eq!(
        out.outcome,
        TurnOutcome::Finished(TurnFinish::ToolValue {
            tool_name: "finish".to_string(),
            value: serde_json::json!("done")
        })
    );
    assert_eq!(out.assistant_output.safe_text, "done");
}

#[test]
fn assembler_falls_back_to_last_assistant_message_when_stream_output_is_empty() {
    let mut state = default_state();
    append_message(
        &mut state,
        Message {
            id: "m0".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::prose("m0.p0".to_string(), "stored".to_string(), None)].into(),
            origin: None,
        },
    );
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::Done);
    let out = assembler.finish(
        state.to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    assert!(matches!(
        &out.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(matches!(
        &out.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(out.assistant_output.safe_text, "stored");
    assert_eq!(out.assistant_output.raw_text, "stored");
    assert_eq!(out.assistant_output.state, OutputState::Usable);
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
        },
    );

    let out = TurnAssembler::default().finish(
        state.to_snapshot(),
        Some(crate::TurnCancellationEvidence::internal("assembler-test")),
        None,
        &TerminationPolicy::default(),
    );

    assert!(matches!(
        &out.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(out.assistant_output.safe_text.is_empty());
    assert!(out.assistant_output.raw_text.is_empty());
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
        },
    );
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::TextDelta {
        content: "You graduated with a degree in Business".to_string(),
    });
    assembler.push(&SessionStreamEvent::Done);
    let out = assembler.finish(
        state.to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    assert_eq!(
        out.assistant_output.safe_text,
        "You graduated with a degree in Business Administration."
    );
    assert_eq!(
        out.assistant_output.raw_text,
        "You graduated with a degree in Business Administration."
    );
    assert_eq!(out.assistant_output.state, OutputState::Usable);
}

#[test]
fn assembler_state_output_excludes_tool_call_payload() {
    // Regression: codex commits an assistant message containing a prose
    // part followed by a tool-call part whose `content` is the raw JSON
    // arguments. On interrupt the assembler falls back to the last
    // assistant message's parts; concatenating EVERY part's content
    // leaks the tool-call JSON into safe_text and the UI then renders it
    // as a literal AssistantText block. Only Text/Prose/Image parts
    // should appear in safe_text.
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
                    "tc1".to_string(),
                    "batch".to_string(),
                    None,
                ),
            ]
            .into(),
            origin: None,
        },
    );
    let assembler = TurnAssembler::default();
    let out = assembler.finish(
        state.to_snapshot(),
        Some(crate::TurnCancellationEvidence::internal("assembler-test")),
        None,
        &TerminationPolicy::default(),
    );
    assert!(matches!(
        &out.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert_eq!(
        out.assistant_output.safe_text,
        "Searching for the relevant code."
    );
    assert!(!out.assistant_output.raw_text.contains("tool_calls"));
}

#[test]
fn assembler_derives_tool_failure_from_assembled_records() {
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::ToolCall {
        call_id: Some("tc1".to_string()),
        name: "x".to_string(),
        args: serde_json::json!({}),
        output: crate::ToolCallOutput::failure(crate::ToolFailure::tool(
            crate::ToolFailureClass::Execution,
            "tool_error",
            serde_json::json!({"error": true}).to_string(),
        )),
        duration_ms: 1,
    });
    assembler.push(&SessionStreamEvent::Error {
        message: "tool failed".to_string(),
        envelope: None,
    });
    assembler.push(&SessionStreamEvent::Done);
    let out = assembler.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    assert!(matches!(&out.outcome, TurnOutcome::Stopped(_)));
    assert!(matches!(
        &out.outcome,
        TurnOutcome::Stopped(TurnStop::ToolFailure)
    ));
    assert_eq!(out.tool_calls.len(), 1);
}

#[test]
fn assembler_treats_any_non_success_record_as_tool_failure() {
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::ToolCall {
        call_id: Some("tc-cancelled".to_string()),
        name: "x".to_string(),
        args: serde_json::json!({}),
        output: crate::ToolCallOutput::cancelled(crate::ToolCancellation::runtime(
            "tool cancelled",
        )),
        duration_ms: 1,
    });
    assembler.push(&SessionStreamEvent::Error {
        message: "runtime also reported a blocking issue".to_string(),
        envelope: None,
    });
    assembler.push(&SessionStreamEvent::Done);

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
    let mut assembler = TurnAssembler::default();
    for index in 0..128 {
        assembler.push(&SessionStreamEvent::ToolCall {
            call_id: Some(format!("call-{index}")),
            name: "successful_tool".to_string(),
            args: serde_json::json!({ "index": index }),
            output: crate::ToolCallOutput::success(serde_json::json!(index)),
            duration_ms: 1,
        });
    }
    assembler.push(&SessionStreamEvent::ToolCallsOmitted {
        summary: crate::OmittedToolCalls {
            count: 1,
            failures: 1,
            attachments: Vec::new(),
        },
    });
    assembler.push(&SessionStreamEvent::Error {
        message: "runtime also reported a blocking issue".to_string(),
        envelope: None,
    });
    assembler.push(&SessionStreamEvent::Done);

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
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::TextDelta {
        content: "partial".to_string(),
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

#[test]
fn assembler_detects_max_turn_message() {
    let mut state = default_state();
    append_message(
        &mut state,
        Message {
            id: "m0".to_string(),
            role: MessageRole::System,
            parts: vec![Part::text(
                "m0.p0".to_string(),
                "Turn limit reached (5).".to_string(),
                None,
            )]
            .into(),
            origin: None,
        },
    );
    let mut assembler = TurnAssembler::default();
    assembler.push(&SessionStreamEvent::Done);
    let out = assembler.finish(
        state.to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    assert!(matches!(
        &out.outcome,
        TurnOutcome::Stopped(TurnStop::MaxTurns)
    ));
}

#[test]
fn output_state_empty_output() {
    assert_eq!(classify_output_state("", "", &[]), OutputState::EmptyOutput);
}

#[test]
fn output_state_traceback_only() {
    let raw = "Runtime error: Traceback (most recent call last):\nFile \"frame_1.py\", line 2, in <module>\nNameError: name 'now' is not defined";
    assert_eq!(
        classify_output_state(raw, "", &[]),
        OutputState::TracebackOnly
    );
}

#[test]
fn output_state_recovered_from_error() {
    let issues = vec![TurnIssue {
        kind: "runtime".to_string(),
        code: Some("example".to_string()),
        terminal_reason: None,
        message: "something failed".to_string(),
        raw: None,
        retryable: None,
        provider_failure_kind: None,
    }];
    assert_eq!(
        classify_output_state("raw", "usable", &issues),
        OutputState::RecoveredFromError
    );
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
        &crate::SessionAttachmentStore::in_memory(),
        &crate::OpenAttachmentSourcePolicy,
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

#[derive(Debug)]
struct DenyBorrowedIngress;

impl crate::AttachmentSourcePolicy for DenyBorrowedIngress {
    fn authorize(
        &self,
        producer: &crate::AttachmentProducer,
        source: &crate::AttachmentSource,
    ) -> Result<(), crate::test_support::AttachmentSourcePolicyError> {
        if matches!(producer, crate::AttachmentProducer::TurnIngress)
            && matches!(source, crate::AttachmentSource::ExternalUrl { .. })
        {
            return Err(crate::test_support::AttachmentSourcePolicyError {
                producer: producer.clone(),
                reason: "borrowed ingress disabled".to_string(),
            });
        }
        Ok(())
    }
}

#[tokio::test]
async fn attachment_source_policy_can_deny_borrowed_turn_ingress() {
    let items = vec![InputItem::Attachment {
        source: crate::AttachmentSource::external_url(
            crate::MediaType::parse("application/pdf").unwrap(),
            "https://example.test/document.pdf",
        ),
    }];

    let error = normalize_input_items(
        &items,
        &crate::SessionAttachmentStore::in_memory(),
        &DenyBorrowedIngress,
    )
    .await
    .expect_err("policy denial must stop ingress");

    assert!(error.contains("TurnIngress"));
    assert!(error.contains("borrowed ingress disabled"));
}
