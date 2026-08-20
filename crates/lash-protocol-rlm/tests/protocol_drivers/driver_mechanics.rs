use super::support::*;

// Focused RLM driver mechanics: malformed options, driver-state ownership, and checkpoint restore internals.

// === Focused RLM White-Box Tests ===
//
// These keep direct `TurnMachine` access because they validate malformed turn
// options, driver-state ownership, and checkpoint restore internals rather
// than reusable protocol scenario behavior.
#[test]
fn malformed_rlm_turn_options_fail_before_llm() {
    let config = test_config_with_protocol_turn_options(
        lash_core::ProtocolTurnOptions::from_payload(serde_json::json!({
            "termination": { "kind": "unknown" }
        })),
    );
    let msgs = vec![user_message("hello")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);

    assert!(find_llm_call(&effects).is_none());
    assert!(effects_include_runtime_error(
        &effects,
        "invalid RLM turn options"
    ));
    assert!(find_done(&effects).is_some());
}

#[test]
fn null_rlm_turn_options_fail_before_llm() {
    let config = test_config_with_protocol_turn_options(
        lash_core::ProtocolTurnOptions::from_payload(serde_json::Value::Null),
    );
    let msgs = vec![user_message("hello")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);

    assert!(find_llm_call(&effects).is_none());
    assert!(effects_include_runtime_error(
        &effects,
        "invalid RLM turn options"
    ));
    assert!(find_done(&effects).is_some());
}

#[test]
fn opaque_reasoning_only_response_stops_as_empty_provider_response() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");

    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(rlm_response(vec![LlmOutputPart::Reasoning {
            text: String::new(),
            replay: Some(lash_sansio::llm::types::ProviderReasoningReplay {
                item_id: Some("opaque-only".to_string()),
                encrypted_content: Some("encrypted-reasoning-blob".to_string()),
                signature: None,
                redacted: false,
                summary: Vec::new(),
                ..Default::default()
            }),
        }])),
    });

    let effects = drain_effects(&mut machine);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::Error {
            envelope: Some(envelope),
            ..
        }) if envelope.code.as_deref() == Some("empty_response")
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::TurnOutcome {
            outcome: lash_sansio::TurnOutcome::Stopped(lash_sansio::TurnStop::ProviderError)
        })
    )));
    assert!(
        find_checkpoint(&effects).is_none(),
        "opaque reasoning without renderable text must not finish through a checkpoint"
    );
}

#[test]
fn native_tool_call_failure_preserves_the_offending_llm_response_event() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");

    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(rlm_response(vec![LlmOutputPart::ToolCall {
            call_id: "native-call".to_string(),
            tool_name: "native_lookup".to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }])),
    });

    let effects = drain_effects(&mut machine);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Emit(SessionStreamEvent::LlmResponse { .. })))
    );
    assert!(effects_include_runtime_error(
        &effects,
        "native provider tool call `native_lookup`"
    ));
}

#[test]
fn provider_stop_evidence_does_not_reconstruct_an_unclosed_cell() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "Before\n<lashlang>\nprint \"hi\"";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            execution_evidence: Some(lash_core::ExecutionEvidence {
                provider_finish_reason: Some("stop_sequence".to_string()),
                ..Default::default()
            }),
            generation_disposition: Some(lash_core::GenerationReceipt {
                stop_sequences: lash_core::GenerationOptionOutcome::Applied,
                ..Default::default()
            }),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(find_checkpoint(&effects).is_some());
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. }))
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::LlmResponse { content, .. })
            if content == "Before"
    )));
    assert!(!machine.messages().iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| part.content.contains("inside multiline source text"))
    }));
}

#[test]
fn natural_stop_without_applied_boundary_does_not_close_or_execute_a_cell() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "Visible plan.\n<lashlang>\nprint \"unfinished\"";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. }))
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::LlmResponse { content, .. })
            if content == "Visible plan."
    )));
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::LlmResponse { content, .. })
            if content.contains("<lashlang>")
    )));
}

#[test]
fn buffered_response_discards_trailing_content_after_the_first_complete_cell() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = concat!(
        "<lashlang>\n",
        "print \"kept\"\n",
        "</lashlang>\n",
        "print \"discarded\"\n",
        "finish \"also discarded\"",
    );
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(
        effects.iter().any(
            |effect| matches!(effect, Effect::ExecCode { code, .. } if code == "print \"kept\"")
        )
    );
    assert!(!effects.iter().any(|effect| {
        matches!(effect, Effect::ExecCode { code, .. } if code.contains("discarded"))
    }));
}

#[test]
fn buffered_response_executes_only_first_of_two_complete_cells_without_retry() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = concat!(
        "<lashlang>\n",
        "print \"first\"\n",
        "</lashlang>\n",
        "<lashlang>\n",
        "finish \"second\"\n",
        "</lashlang>",
    );
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(effects.iter().any(
        |effect| matches!(effect, Effect::ExecCode { code, .. } if code == "print \"first\"")
    ));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::LlmCall { .. }))
    );
    assert!(!effects.iter().any(|effect| {
        matches!(effect, Effect::ExecCode { code, .. } if code.contains("second"))
    }));
}

#[test]
fn illustrative_prose_with_an_unclosed_cell_retries_without_execution() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("how do you run code?")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "You wrap it like:\n<lashlang>\nfiles.delete \"old\"\n";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            generation_disposition: Some(lash_core::GenerationReceipt {
                stop_sequences: lash_core::GenerationOptionOutcome::Applied,
                ..Default::default()
            }),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(find_checkpoint(&effects).is_some());
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. }))
    );
}

#[test]
fn natural_end_turn_with_a_partial_program_retries_without_execution() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("swap the file")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "<lashlang>\nfiles.delete \"old\"";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            generation_disposition: Some(lash_core::GenerationReceipt {
                stop_sequences: lash_core::GenerationOptionOutcome::Applied,
                ..Default::default()
            }),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(find_checkpoint(&effects).is_some());
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. }))
    );
}

#[test]
fn output_limit_unclosed_cell_retries_with_shorten_block_diagnostic() {
    let mut config = test_config();
    config.generation.output_token_cap = std::num::NonZeroUsize::new(4096);
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "<lashlang>\nprint \"too long\"";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::OutputLimit,
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(find_checkpoint(&effects).is_some());
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. }))
    );
    assert!(
        machine
            .messages()
            .iter()
            .any(|message| message.parts.iter().any(|part| {
                part.content.contains("shorter block")
                    && part.content.contains("output limit")
                    && part.content.contains("4096")
                    // The Lashlang reader's noun, from the dialect's own
                    // vocabulary: this fixture runs the default dialect.
                    && part.content.contains("do less per block")
            }))
    );
    assert!(assistant_visible_texts(&machine).is_empty());
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::LlmResponse { content, .. })
            if content.contains("<lashlang>")
    )));
}

#[test]
fn multiple_cells_execute_only_the_first_without_emitting_raw_markup() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "Visible plan.\n<lashlang>\nprint 1\n</lashlang>\n<lashlang>\nprint 2\n</lashlang>";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(rlm_response(vec![text_part(text)])),
    });

    let effects = drain_effects(&mut machine);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { code, .. } if code == "print 1"))
    );
    assert!(
        !effects.iter().any(
            |effect| matches!(effect, Effect::ExecCode { code, .. } if code.contains("print 2"))
        )
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::LlmResponse { content, .. })
            if content == "Visible plan."
    )));
    assert!(!machine.messages().iter().any(|message| {
        message.role == MessageRole::Assistant
            && message
                .parts
                .iter()
                .any(|part| part.content.contains("<lashlang>"))
    }));
}

#[test]
fn output_limit_prose_retries_with_the_request_cap() {
    let mut config = test_config();
    config.generation.output_token_cap = std::num::NonZeroUsize::new(2048);
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("explain at length")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "Here is the long explanation that got cut off mid-sen";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::OutputLimit,
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(find_checkpoint(&effects).is_some());
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Checkpoint { checkpoint, .. }
            if *checkpoint == CheckpointKind::BeforeCompletion
    )));
    assert_eq!(
        single_llm_extraction_payload(&machine)["decision"],
        "retry_output_limit_prose"
    );
    assert!(
        machine
            .messages()
            .iter()
            .any(|message| message.parts.iter().any(|part| part
                .content
                .contains("answer was cut off")
                && part.content.contains("2048")
                && part.content.contains("shorter answer")))
    );
}

#[test]
fn output_limit_prose_cut_at_a_close_tag_mention_retries() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("what closes a cell?")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "A cell is closed by ";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            terminal_reason: lash_core::LlmTerminalReason::OutputLimit,
            generation_disposition: Some(lash_core::GenerationReceipt {
                stop_sequences: lash_core::GenerationOptionOutcome::Applied,
                ..Default::default()
            }),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert_eq!(
        single_llm_extraction_payload(&machine)["decision"],
        "retry_output_limit_prose"
    );
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Checkpoint { checkpoint, .. }
            if *checkpoint == CheckpointKind::BeforeCompletion
    )));
}

#[test]
fn terminal_provider_paths_emit_only_visible_prose() {
    for terminal_reason in [
        lash_core::LlmTerminalReason::ContentFilter,
        lash_core::LlmTerminalReason::ContextOverflow,
        lash_core::LlmTerminalReason::ProviderError,
    ] {
        let mut machine = TurnMachine::new(
            test_config(),
            vec![user_message("respond")],
            Arc::new(Vec::new()),
            0,
        );
        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).expect("llm call");
        let text = "Visible plan.\n<lashlang>\nfiles.delete \"old\"";
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(LlmResponse {
                full_text: text.to_string(),
                parts: vec![text_part(text)],
                terminal_reason,
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Emit(SessionStreamEvent::TextDelta { content })
                if content == "Visible plan."
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Emit(SessionStreamEvent::LlmResponse { content, .. })
                if content == "Visible plan."
        )));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::Emit(SessionStreamEvent::TextDelta { content })
                | Effect::Emit(SessionStreamEvent::LlmResponse { content, .. })
                if content.contains("<lashlang>")
        )));
    }
}

#[test]
fn rlm_driver_state_with_wrong_plugin_id_fails_loudly() {
    let config = test_config();
    let msgs = vec![user_message("run some code")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);
    let effects = drain_effects(&mut machine);
    assert!(find_llm_call(&effects).is_some());

    let mut checkpoint = serde_json::to_value(machine.checkpoint()).expect("checkpoint serializes");
    assert!(
        rewrite_first_rlm_driver_state_owner(&mut checkpoint),
        "checkpoint should contain RLM driver state"
    );
    let checkpoint = serde_json::from_value(checkpoint).expect("checkpoint deserializes");
    let mut restored = TurnMachine::restore_from_checkpoint(test_config(), checkpoint);

    let effects = drain_effects(&mut restored);
    let llm_id = *find_llm_call(&effects).expect("restored llm call");
    restored.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: lashlang_block("print \"hi\""),
            parts: vec![LlmOutputPart::Text {
                text: lashlang_block("print \"hi\""),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut restored);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. })),
        "invalid driver state must not reach code execution"
    );
    assert!(effects_include_runtime_error(
        &effects,
        "driver state belongs to plugin"
    ));
    assert!(find_done(&effects).is_some());
}

#[test]
fn rlm_checkpoint_redrives_pending_exec_code_with_driver_state() {
    let config = test_config();
    let msgs = vec![user_message("run some code")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: lashlang_block_with_prose("Reason first.", "print \"hi\""),
            parts: vec![LlmOutputPart::Text {
                text: lashlang_block_with_prose("Reason first.", "print \"hi\""),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    let (exec_id, code) = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ExecCode { id, code, .. } => Some((*id, code.clone())),
            _ => None,
        })
        .expect("exec effect");
    assert_eq!(code, "print \"hi\"");

    let checkpoint = roundtrip_turn_checkpoint(machine.checkpoint());
    let mut restored = TurnMachine::restore_from_checkpoint(test_config(), checkpoint);
    let effects = drain_effects(&mut restored);
    let (restored_exec_id, restored_code) = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ExecCode { id, code, .. } => Some((*id, code.clone())),
            _ => None,
        })
        .expect("restored exec effect");
    assert_eq!(restored_exec_id, exec_id);
    assert_eq!(restored_code, "print \"hi\"");

    restored.handle_response(Response::ExecResult {
        id: restored_exec_id,
        result: Ok(lash_sansio::ExecResponse {
            observations: vec!["hi\n".to_string()],
            observation_truncation: Vec::new(),
            tool_calls: vec![lash_core::ToolCallRecord {
                call_id: Some("replayed-call".to_string()),
                tool: "attachment_tool".to_string(),
                args: serde_json::json!({}),
                output: lash_core::ToolCallOutput::success(lash_core::ToolValue::Attachment(
                    lash_core::AttachmentSource::stored(
                        lash_core::facade_support::AttachmentMeta::new(
                            lash_core::AttachmentId::parse("replayed-attachment")
                                .expect("valid attachment id"),
                            lash_core::MediaType::parse("image/png").unwrap(),
                            3,
                            Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
                            Some("replayed".to_string()),
                        )
                        .as_ref(),
                    ),
                )),
                duration_ms: 1,
            }],
            executed_calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 1,
            terminal_finish: None,
        }),
    });

    let effects = drain_effects(&mut restored);
    let replayed_tool_call = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Emit(SessionStreamEvent::ToolCall {
                call_id,
                name,
                output,
                ..
            }) => Some((call_id, name, output)),
            _ => None,
        })
        .expect("replayed exec response emits its tool-call accounting event");
    assert_eq!(replayed_tool_call.0.as_deref(), Some("replayed-call"));
    assert_eq!(replayed_tool_call.1, "attachment_tool");
    assert_eq!(
        replayed_tool_call.2.attachments()[0]
            .stored_ref()
            .expect("stored attachment")
            .id,
        lash_core::AttachmentId::parse("replayed-attachment").expect("valid attachment id")
    );
    let trajectory = machine_trajectory(&restored);
    let entry = trajectory.last().expect("rlm trajectory entry");
    assert_eq!(entry.code, "print \"hi\"");
    assert_eq!(assistant_visible_texts(&restored), vec!["Reason first."]);
    assert_eq!(entry.output, vec!["hi\n".to_string()]);
    let (_, checkpoint) = find_checkpoint(&effects).expect("after-work checkpoint");
    assert_eq!(checkpoint, CheckpointKind::AfterWork);
}

#[test]
fn rlm_checkpoint_after_exec_fanout_tool_outputs_preserves_structured_outcomes() {
    let config = test_config();
    let msgs = vec![user_message("run fanout tools")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: lashlang_block("ok = await tools.ok({})\nfail = await tools.fail({})\nstop = await tools.stop({})\nresults = { a: ok, b: fail, c: stop }"),
            parts: vec![LlmOutputPart::Text {
                text: lashlang_block("ok = await tools.ok({})\nfail = await tools.fail({})\nstop = await tools.stop({})\nresults = { a: ok, b: fail, c: stop }"),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    let exec_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ExecCode { id, .. } => Some(*id),
            _ => None,
        })
        .expect("exec effect");
    machine.handle_response(Response::ExecResult {
        id: exec_id,
        result: Ok(lash_sansio::ExecResponse {
            observations: vec!["fanout done".to_string()],
            observation_truncation: Vec::new(),
            tool_calls: vec![
                lash_core::ToolCallRecord {
                    call_id: Some("fanout-ok".to_string()),
                    tool: "ok".to_string(),
                    args: serde_json::json!({}),
                    output: lash_core::ToolCallOutput::success(serde_json::json!("ok")),
                    duration_ms: 1,
                },
                lash_core::ToolCallRecord {
                    call_id: Some("fanout-fail".to_string()),
                    tool: "fail".to_string(),
                    args: serde_json::json!({}),
                    output: lash_core::ToolCallOutput::failure(lash_core::ToolFailure::tool(
                        lash_core::ToolFailureClass::Execution,
                        "tool_failed",
                        "failed but captured",
                    )),
                    duration_ms: 2,
                },
                lash_core::ToolCallRecord {
                    call_id: Some("fanout-cancel".to_string()),
                    tool: "stop".to_string(),
                    args: serde_json::json!({}),
                    output: lash_core::ToolCallOutput::cancelled(
                        lash_core::ToolCancellation::runtime("cancelled sibling"),
                    ),
                    duration_ms: 3,
                },
            ],
            executed_calls: vec![
                lash_core::ExecutedCallRecord {
                    operation: "module.ok".to_string(),
                    outcome: lash_core::ExecutedCallOutcome::Ok,
                },
                lash_core::ExecutedCallRecord {
                    operation: "module.fail".to_string(),
                    outcome: lash_core::ExecutedCallOutcome::Err,
                },
                lash_core::ExecutedCallRecord {
                    operation: "module.stop".to_string(),
                    outcome: lash_core::ExecutedCallOutcome::Err,
                },
            ],
            printed_images: Vec::new(),
            error: None,
            duration_ms: 3,
            terminal_finish: None,
        }),
    });

    let exec_effects = drain_effects(&mut machine);
    let emitted = exec_effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Emit(SessionStreamEvent::ToolCall {
                call_id,
                name,
                args,
                output,
                duration_ms,
            }) => Some((call_id, name, args, output, duration_ms)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(emitted.len(), 3);
    assert_eq!(emitted[0].0.as_deref(), Some("fanout-ok"));
    assert_eq!(emitted[0].1, "ok");
    assert_eq!(emitted[0].2, &serde_json::json!({}));
    assert!(emitted[0].3.is_success());
    assert_eq!(*emitted[0].4, 1);
    assert_eq!(emitted[1].0.as_deref(), Some("fanout-fail"));
    assert!(!emitted[1].3.is_success());
    assert_eq!(emitted[2].0.as_deref(), Some("fanout-cancel"));
    assert!(!emitted[2].3.is_success());

    let checkpoint = roundtrip_turn_checkpoint(machine.checkpoint());
    let mut restored = TurnMachine::restore_from_checkpoint(test_config(), checkpoint);
    let effects = drain_effects(&mut restored);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Emit(SessionStreamEvent::ToolCall { .. })))
    );
    let trajectory = machine_trajectory(&restored);
    let entry = trajectory.last().expect("rlm trajectory entry");
    assert!(
        serde_json::to_value(entry)
            .unwrap()
            .get("tool_call_ids")
            .is_none()
    );
    assert_eq!(entry.output, vec!["fanout done".to_string()]);
    assert_eq!(
        entry.calls,
        vec![
            lash_rlm_types::RlmExecutedCall {
                operation: "module.ok".to_string(),
                outcome: lash_rlm_types::RlmExecutedCallOutcome::Ok,
            },
            lash_rlm_types::RlmExecutedCall {
                operation: "module.fail".to_string(),
                outcome: lash_rlm_types::RlmExecutedCallOutcome::Err,
            },
            lash_rlm_types::RlmExecutedCall {
                operation: "module.stop".to_string(),
                outcome: lash_rlm_types::RlmExecutedCallOutcome::Err,
            },
        ]
    );
    let (_, checkpoint) = find_checkpoint(&effects).expect("after-work checkpoint");
    assert_eq!(checkpoint, CheckpointKind::AfterWork);
}

/// A cell tagged with a registered-but-inactive dialect must be *named*, not
/// silently read as prose.
///
/// This is the mechanism behind the hang the battery found. Extraction only
/// knows the active dialect's tags, so a `<typescript>` cell in a Lashlang
/// session (or the reverse) matched nothing: `lashlang_cell_count` stayed 0,
/// the whole reply counted as prose, and a `FinishRequired` turn asked the
/// model to finish — forever, because the model kept answering with the cell it
/// had been told to write. The execution fence never fires because extraction
/// never yields a cell to fence.
#[test]
fn a_cell_of_the_inactive_dialect_is_named_on_the_first_iteration() {
    let mut machine = TurnMachine::new(
        test_config_with_dialect("lashlang"),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "Here is the answer.\n<typescript>\nfinish(\"ok\");\n</typescript>";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    // Nothing executes: the cell is not this session's.
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. }))
    );
    // And the model is told exactly what is wrong, in its own dialect's words,
    // on this first iteration rather than after an unbounded number of them.
    let told = machine.messages().iter().any(|message| {
        message.parts.iter().any(|part| {
            part.content.contains("<typescript>")
                && part.content.contains("<lashlang>")
                && part.content.contains("does not run")
        })
    });
    assert!(told, "messages: {:#?}", machine.messages());
}

/// The same, in the other direction.
///
/// The driver code is dialect-generic, but "both directions asserted" is this
/// round's own standard everywhere else, and a one-directional fixture cannot
/// tell a generic implementation from one that happens to name Lashlang.
#[test]
fn a_lashlang_cell_in_a_typescript_session_is_named_the_same_way() {
    let mut machine = TurnMachine::new(
        test_config_with_dialect("typescript"),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let text = "Here is the answer.\n<lashlang>\nfinish \"ok\"\n</lashlang>";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            full_text: text.to_string(),
            parts: vec![text_part(text)],
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. }))
    );
    let told = machine.messages().iter().any(|message| {
        message.parts.iter().any(|part| {
            part.content.contains("<lashlang>")
                && part.content.contains("<typescript>")
                && part.content.contains("does not run")
                // The correction is written in the reader's own words: a
                // TypeScript session is told about cells, not blocks.
                && part.content.contains("cell")
        })
    });
    assert!(told, "messages: {:#?}", machine.messages());
}

// === FIG-1407: the no-progress budget ===
//
// A turn whose attempts never commit an error-free execution used to re-call
// the provider for as long as the turn budget allowed, which in a workbench
// running `TurnBudget::Unbounded` was forever: one measured send bought 1,223
// provider calls in 4m36s and committed nothing. These drive the real machine
// to a terminal state and count the calls.

/// Drive `machine` until it is done or `max_llm_calls` provider calls have
/// been answered, answering each call with `reply` and each exec with
/// `exec_result`. Returns the number of provider calls the machine made.
fn drive_stalling_turn(
    machine: &mut TurnMachine,
    reply: &str,
    exec_result: Option<lash_sansio::ExecResponse>,
    max_llm_calls: usize,
) -> StalledTurn {
    let mut llm_calls = 0;
    let mut outcome = None;
    let mut effects = drain_effects(machine);
    loop {
        outcome = outcome.or_else(|| find_turn_outcome(&effects));
        if let Some((messages, _)) = find_done(&effects) {
            return StalledTurn {
                llm_calls,
                outcome,
                messages: messages.iter().cloned().collect(),
            };
        }
        if let Some(llm_id) = find_llm_call(&effects).copied() {
            assert!(
                llm_calls < max_llm_calls,
                "the turn made {llm_calls} provider calls without terminating"
            );
            llm_calls += 1;
            machine.handle_response(Response::LlmComplete {
                id: llm_id,
                text_streamed: false,
                result: Ok(rlm_response(vec![text_part(reply)])),
            });
        } else if let Some(exec_id) = effects.iter().find_map(|effect| match effect {
            Effect::ExecCode { id, .. } => Some(*id),
            _ => None,
        }) {
            let result = exec_result
                .clone()
                .expect("the turn executed a cell the fixture did not script");
            machine.handle_response(Response::ExecResult {
                id: exec_id,
                result: Ok(result),
            });
        } else if let Some((checkpoint_id, _)) = find_checkpoint(&effects) {
            machine.handle_response(Response::Checkpoint {
                id: checkpoint_id,
                delivery: sansio::CheckpointDelivery::default(),
            });
        } else {
            panic!("the turn is neither done nor waiting on anything it can be answered with");
        }
        effects = drain_effects(machine);
    }
}

struct StalledTurn {
    llm_calls: usize,
    outcome: Option<lash_sansio::TurnOutcome>,
    messages: Vec<Message>,
}

impl StalledTurn {
    /// The transcript record the turn closed on, if it says why it stopped.
    fn stop_message(&self) -> Option<&Part> {
        self.messages.iter().find_map(|message| {
            message
                .parts
                .iter()
                .find(|part| part.content.contains("no-progress budget is exhausted"))
        })
    }
}

fn find_turn_outcome(effects: &[Effect]) -> Option<lash_sansio::TurnOutcome> {
    effects.iter().find_map(|effect| match effect {
        Effect::Emit(SessionStreamEvent::TurnOutcome { outcome }) => Some(outcome.clone()),
        _ => None,
    })
}

fn config_with_no_progress_budget(max_attempts: usize) -> TurnMachineConfig {
    let mut config = test_config();
    config.no_progress_budget = lash_core::NoProgressBudget::bounded(max_attempts);
    config
}

/// A cell that never closes never executes, so it commits nothing. Before the
/// budget existed this turn had no terminating branch at all.
#[test]
fn a_reply_that_never_yields_a_cell_stops_at_the_no_progress_budget() {
    let mut machine = TurnMachine::new(
        config_with_no_progress_budget(4),
        vec![user_message("do the thing")],
        Arc::new(Vec::new()),
        0,
    );

    let stalled = drive_stalling_turn(&mut machine, "<lashlang>\nfinish \"ok\"", None, 32);

    assert_eq!(
        stalled.llm_calls, 4,
        "the bound is the number of provider calls"
    );
    assert_eq!(
        stalled.outcome,
        Some(lash_core::facade_support::TurnOutcome::Stopped(
            lash_core::facade_support::TurnStop::MaxTurns
        )),
        "exhaustion is terminal for the turn"
    );
    assert!(
        stalled.stop_message().is_some(),
        "the transcript says why the turn stopped: {:#?}",
        stalled.messages
    );
}

/// The same bound, in the other dialect.
#[test]
fn a_typescript_reply_that_never_yields_a_cell_stops_at_the_no_progress_budget() {
    let mut config = test_config_with_dialect("typescript");
    config.no_progress_budget = lash_core::NoProgressBudget::bounded(3);
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("do the thing")],
        Arc::new(Vec::new()),
        0,
    );

    let stalled = drive_stalling_turn(&mut machine, "<typescript>\nfinish(\"ok\");", None, 32);

    assert_eq!(
        stalled.llm_calls, 3,
        "the bound is the number of provider calls"
    );
    assert_eq!(
        stalled.outcome,
        Some(lash_core::facade_support::TurnOutcome::Stopped(
            lash_core::facade_support::TurnStop::MaxTurns
        )),
    );
}

/// The measured `code-failure` shape: the cell parses out of the reply and
/// runs, and raises every time. It commits a trajectory entry per attempt, so
/// "appended something" is not progress — an error-free execution is.
#[test]
fn a_cell_that_only_ever_raises_stops_at_the_no_progress_budget() {
    let mut machine = TurnMachine::new(
        config_with_no_progress_budget(5),
        vec![user_message("do the thing")],
        Arc::new(Vec::new()),
        0,
    );

    let stalled = drive_stalling_turn(
        &mut machine,
        &lashlang_block("fail \"deterministic durable process failure\""),
        Some(exec_response(
            &[],
            Some("`fail` is only valid inside a process"),
            None,
        )),
        32,
    );

    assert_eq!(
        stalled.llm_calls, 5,
        "the bound is the number of provider calls"
    );
    assert_eq!(
        stalled.outcome,
        Some(lash_core::facade_support::TurnOutcome::Stopped(
            lash_core::facade_support::TurnStop::MaxTurns
        )),
    );
}

/// An error-free execution is progress, and progress resets the count. A turn
/// that keeps getting real work done never approaches the bound however many
/// bad attempts are interleaved.
#[test]
fn an_error_free_execution_resets_the_no_progress_count() {
    let mut machine = TurnMachine::new(
        config_with_no_progress_budget(3),
        vec![user_message("do the thing")],
        Arc::new(Vec::new()),
        0,
    );
    let unclosed = "<lashlang>\nfinish \"ok\"";
    let mut effects = drain_effects(&mut machine);

    // Two stalls, one short of the bound.
    for _ in 0..2 {
        let llm_id = *find_llm_call(&effects).expect("llm call");
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(rlm_response(vec![text_part(unclosed)])),
        });
        effects = drain_effects(&mut machine);
        let (checkpoint_id, _) = find_checkpoint(&effects).expect("checkpoint");
        machine.handle_response(Response::Checkpoint {
            id: checkpoint_id,
            delivery: sansio::CheckpointDelivery::default(),
        });
        effects = drain_effects(&mut machine);
    }
    assert!(
        find_done(&effects).is_none(),
        "two stalls must not exhaust a bound of three"
    );

    // One attempt that executes cleanly without finishing the turn.
    let llm_id = *find_llm_call(&effects).expect("llm call");
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(rlm_response(vec![text_part(&lashlang_block(
            "print \"working\"",
        ))])),
    });
    effects = drain_effects(&mut machine);
    let exec_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ExecCode { id, .. } => Some(*id),
            _ => None,
        })
        .expect("exec code");
    machine.handle_response(Response::ExecResult {
        id: exec_id,
        result: Ok(exec_response(&["working"], None, None)),
    });
    effects = drain_effects(&mut machine);
    let (checkpoint_id, _) = find_checkpoint(&effects).expect("checkpoint");
    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: sansio::CheckpointDelivery::default(),
    });
    effects = drain_effects(&mut machine);
    assert!(find_done(&effects).is_none(), "the turn is still running");

    // The count restarted: two more stalls still do not exhaust the bound, and
    // the third does. Six provider calls in a turn bounded at three.
    for _ in 0..2 {
        let llm_id = *find_llm_call(&effects).expect("llm call");
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(rlm_response(vec![text_part(unclosed)])),
        });
        effects = drain_effects(&mut machine);
        let (checkpoint_id, _) = find_checkpoint(&effects).expect("checkpoint");
        machine.handle_response(Response::Checkpoint {
            id: checkpoint_id,
            delivery: sansio::CheckpointDelivery::default(),
        });
        effects = drain_effects(&mut machine);
    }
    assert!(
        find_done(&effects).is_none(),
        "the reset must have restarted the count at the clean execution"
    );

    let llm_id = *find_llm_call(&effects).expect("llm call");
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(rlm_response(vec![text_part(unclosed)])),
    });
    effects = drain_effects(&mut machine);
    assert!(find_done(&effects).is_some(), "the third stall exhausts it");
    assert_eq!(
        find_turn_outcome(&effects),
        Some(lash_core::facade_support::TurnOutcome::Stopped(
            lash_core::facade_support::TurnStop::MaxTurns
        )),
    );
}

/// A host may still opt a deployment out of the bound.
#[test]
fn an_unbounded_no_progress_budget_keeps_re_asking() {
    let mut config = test_config();
    config.no_progress_budget = lash_core::NoProgressBudget::Unbounded;
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("do the thing")],
        Arc::new(Vec::new()),
        0,
    );
    let unclosed = "<lashlang>\nfinish \"ok\"";
    let mut effects = drain_effects(&mut machine);
    for _ in 0..20 {
        let llm_id = *find_llm_call(&effects).expect("llm call");
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(rlm_response(vec![text_part(unclosed)])),
        });
        effects = drain_effects(&mut machine);
        let (checkpoint_id, _) = find_checkpoint(&effects).expect("checkpoint");
        machine.handle_response(Response::Checkpoint {
            id: checkpoint_id,
            delivery: sansio::CheckpointDelivery::default(),
        });
        effects = drain_effects(&mut machine);
    }
    assert!(
        find_done(&effects).is_none(),
        "an explicit opt-out is honored"
    );
}

/// The count is scoped to one turn, and the driver's history is not.
///
/// `DriverContextView::events` is the whole active session path, so a forward
/// scan that resets only on an error-free execution reads every earlier turn's
/// extraction diagnostics as this turn's stalls. A turn that stopped on the
/// no-progress budget leaves exactly that trail, so the session would be
/// bricked: every later turn gets one provider call and stops.
#[test]
fn a_no_progress_stop_does_not_spend_the_next_turns_budget() {
    let mut first = TurnMachine::new(
        config_with_no_progress_budget(3),
        vec![user_message("do the thing")],
        Arc::new(Vec::new()),
        0,
    );
    let stalled = drive_stalling_turn(&mut first, "<lashlang>\nfinish \"ok\"", None, 32);
    assert_eq!(stalled.llm_calls, 3, "the first turn spends its own budget");
    let carried = first.events().to_vec();
    assert!(
        !carried.is_empty(),
        "the stopped turn leaves its diagnostics on the session path"
    );

    // A second turn, on the same session path, with its own turn id.
    let mut config = config_with_no_progress_budget(3);
    config.turn_id = "second-turn".to_string();
    let mut second = TurnMachine::new(
        config,
        vec![user_message("try again")],
        Arc::new(carried),
        0,
    );
    let stalled = drive_stalling_turn(&mut second, "<lashlang>\nfinish \"ok\"", None, 32);

    assert_eq!(
        stalled.llm_calls, 3,
        "the second turn gets its whole budget, not the leftovers of the first"
    );
}

/// Prose-only turns are the other leak: each commits one extraction diagnostic
/// and no execution at all, so a chat session accumulates a trail that would
/// exhaust the next turn's budget on its first stalled attempt.
#[test]
fn prose_only_turns_do_not_accumulate_into_the_next_turns_count() {
    let prose_turns = 12;
    let mut carried: Vec<lash_core::SessionHistoryRecord> = Vec::new();
    for index in 0..prose_turns {
        let mut config = config_with_no_progress_budget(3);
        config.turn_id = format!("prose-turn-{index}");
        let mut machine = TurnMachine::new(
            config,
            vec![user_message("just talk to me")],
            Arc::new(carried.clone()),
            0,
        );
        let mut effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).expect("llm call");
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(rlm_response(vec![text_part(
                "No code needed; here is the answer.",
            )])),
        });
        effects = drain_effects(&mut machine);
        let (checkpoint_id, _) = find_checkpoint(&effects).expect("completion checkpoint");
        machine.handle_response(Response::Checkpoint {
            id: checkpoint_id,
            delivery: sansio::CheckpointDelivery::default(),
        });
        drain_effects(&mut machine);
        carried = machine.events().to_vec();
    }
    let prose_diagnostics = carried
        .iter()
        .filter(|record| match record {
            lash_core::SessionHistoryRecord::Protocol(event) => matches!(
                lash_protocol_rlm::decode_rlm_protocol_event(event),
                Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                    if diagnostic.phase == "llm_extraction"
            ),
            _ => false,
        })
        .count();
    assert_eq!(
        prose_diagnostics, prose_turns,
        "each prose-only turn leaves one extraction diagnostic behind"
    );

    let mut config = config_with_no_progress_budget(3);
    config.turn_id = "code-turn".to_string();
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("now run something")],
        Arc::new(carried),
        0,
    );
    let mut effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(rlm_response(vec![text_part("<lashlang>\nfinish \"ok\"")])),
    });
    effects = drain_effects(&mut machine);

    assert!(
        find_done(&effects).is_none(),
        "one stalled attempt after {prose_turns} prose turns is the first, not the {}th",
        prose_turns + 1
    );
    assert!(
        find_checkpoint(&effects).is_some(),
        "the turn asks the model again instead of stopping"
    );
}

// === FIG-1475: one-line cells, named fence rules, and repeated replies ===
//
// The measured shape (finale3 row `workbench-continue-as/typescript`,
// reproduced twice): a reply that was a single `<typescript>…</typescript>`
// line. Extraction refused it, the whole reply counted as prose, the
// `FinishRequired` driver answered "please finish" — which corrects nothing —
// and a frontier model re-sent the identical reply until the turn's no-progress
// budget died. Twelve billed provider calls, one dead turn, no signal.

/// Answer the completion checkpoint a finishing turn asks for, and return the
/// effects that follow it — where `Done` lives.
fn complete_through_checkpoint(machine: &mut TurnMachine, effects: &[Effect]) -> Vec<Effect> {
    let (checkpoint_id, _) = find_checkpoint(effects).expect("completion checkpoint");
    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: sansio::CheckpointDelivery::default(),
    });
    drain_effects(machine)
}

fn finish_required_options() -> lash_core::ProtocolTurnOptions {
    lash_core::ProtocolTurnOptions::typed(RlmCreateExtras {
        dialect: None,
        termination: Some(RlmTermination::FinishRequired { schema: None }),
        final_answer_format: None,
    })
    .expect("valid rlm turn options")
}

/// A one-line cell executes.
///
/// Red before the fix: `first_cell_span` required the open tag to be alone on
/// its line, so this reply yielded no cell and nothing ran.
#[test]
fn a_one_line_cell_executes_in_both_dialects() {
    for (dialect, reply, code) in [
        (
            "lashlang",
            "<lashlang>finish \"ok\"</lashlang>",
            "finish \"ok\"",
        ),
        (
            "typescript",
            "<typescript>finish(\"ok\");</typescript>",
            "finish(\"ok\");",
        ),
    ] {
        let mut machine = TurnMachine::new(
            test_config_with_dialect(dialect),
            vec![user_message("respond")],
            Arc::new(Vec::new()),
            0,
        );
        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).expect("llm call");
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(rlm_response(vec![text_part(reply)])),
        });

        let effects = drain_effects(&mut machine);
        let executed = effects.iter().find_map(|effect| match effect {
            Effect::ExecCode { code, .. } => Some(code.clone()),
            _ => None,
        });
        assert_eq!(
            executed.as_deref(),
            Some(code),
            "{dialect}: a one-line cell must execute its source"
        );
    }
}

/// Prose that only *mentions* both tags on one line is still prose.
///
/// The widened grammar must not turn a sentence about cells into a cell: the
/// line has to start with the open tag and end with the close tag.
#[test]
fn a_one_line_tag_mention_still_finishes_as_prose() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("respond")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    let reply = "Write code between <lashlang> and </lashlang> tags, like this one did.";
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(rlm_response(vec![text_part(reply)])),
    });

    let effects = drain_effects(&mut machine);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExecCode { .. })),
        "a tag mention is not executable source"
    );
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::Emit(SessionStreamEvent::LlmResponse { content, .. })
                if content == reply
        )),
        "the whole line is delivered as the answer: {effects:#?}"
    );
    let effects = complete_through_checkpoint(&mut machine, &effects);
    assert!(
        find_done(&effects).is_some(),
        "and the turn finishes on it rather than asking again"
    );
}

/// Prose *about* the tags is an answer, not a fence to correct.
///
/// A line that opens with the tag is indistinguishable from an attempted cell,
/// so the fence correction is asked for only where a cell is required. On a
/// `Natural` turn this reply is exactly what the user wanted; answering it with
/// grammar guidance would bury the answer and spend the turn's attempts.
#[test]
fn a_natural_turn_answering_about_the_tags_is_not_corrected() {
    let mut machine = TurnMachine::new(
        test_config(),
        vec![user_message("what are the tags?")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call");
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(rlm_response(vec![text_part(
            "<lashlang> and </lashlang> are the tags you asked about.",
        )])),
    });

    let effects = drain_effects(&mut machine);
    let effects = complete_through_checkpoint(&mut machine, &effects);
    assert!(
        find_done(&effects).is_some(),
        "the turn finishes on its answer"
    );
    assert_eq!(
        single_llm_extraction_payload(&machine)["decision"],
        "finish_prose",
        "prose about the tags finishes; it is not a refused fence"
    );
}

/// A fence the grammar refuses is answered by naming the rule.
///
/// Red before the fix: this reply produced no cell, so it fell through to the
/// finish reminder, which says nothing about fences. The model has no way to
/// learn what to change, which is what made the loop repeat.
#[test]
fn a_malformed_fence_is_answered_by_naming_the_rule() {
    for (dialect, reply, open, close) in [
        (
            "lashlang",
            "<lashlang>finish \"ok\"\n</lashlang>",
            "<lashlang>",
            "</lashlang>",
        ),
        (
            "typescript",
            "<typescript >\nfinish(\"ok\");\n</typescript>",
            "<typescript>",
            "</typescript>",
        ),
    ] {
        let mut config = test_config_with_dialect(dialect);
        config.termination = finish_required_options();
        let mut machine = TurnMachine::new(
            config,
            vec![user_message("respond")],
            Arc::new(Vec::new()),
            0,
        );
        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).expect("llm call");
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(rlm_response(vec![text_part(reply)])),
        });

        let effects = drain_effects(&mut machine);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ExecCode { .. })),
            "{dialect}: a refused fence executes nothing"
        );
        let told = machine.messages().iter().any(|message| {
            message.parts.iter().any(|part| {
                part.content.contains("grammar could not read")
                    && part.content.contains("stand alone on its own line")
                    && part.content.contains("stand alone on a later line")
                    && part.content.contains(open)
                    && part.content.contains(close)
            })
        });
        assert!(
            told,
            "{dialect}: the retry must name the fence rule: {:#?}",
            machine.messages()
        );
        let decision = machine
            .events()
            .iter()
            .filter_map(|record| match record {
                lash_core::SessionHistoryRecord::Protocol(event) => {
                    match lash_protocol_rlm::decode_rlm_protocol_event(event) {
                        Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                            if diagnostic.phase == "llm_extraction" =>
                        {
                            diagnostic.payload["decision"].as_str().map(str::to_string)
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
            .next_back();
        assert_eq!(
            decision.as_deref(),
            Some("retry_malformed_cell_fence"),
            "{dialect}: the diagnostic names the fence, not a finish request"
        );
    }
}

/// An identical reply is *evidence*, not a stop condition.
///
/// The measured FIG-1475 turn spent twelve provider calls on twelve identical
/// replies, and that bound is the host's: the no-progress budget it configured.
/// Lash records a fingerprint of each reply so a host can see the repetition —
/// a provider sending the same bytes twice is legitimate output, and lash draws
/// no conclusion from it.
#[test]
fn identical_replies_are_fingerprinted_and_run_to_the_hosts_budget() {
    let mut machine = TurnMachine::new(
        config_with_no_progress_budget(6),
        vec![user_message("do the thing")],
        Arc::new(Vec::new()),
        0,
    );

    let stalled = drive_stalling_turn(&mut machine, "<lashlang>\nfinish \"ok\"", None, 32);

    assert_eq!(
        stalled.llm_calls, 6,
        "a repeat is bounded by the host's budget and nothing else"
    );
    assert!(
        stalled.stop_message().is_some(),
        "and stops for the budget's reason: {:#?}",
        stalled.messages
    );
    let fingerprints: Vec<String> = machine
        .events()
        .iter()
        .filter_map(|record| match record {
            lash_core::SessionHistoryRecord::Protocol(event) => {
                match lash_protocol_rlm::decode_rlm_protocol_event(event) {
                    Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                        if diagnostic.phase == "llm_extraction" =>
                    {
                        diagnostic.payload["reply_fingerprint"]
                            .as_str()
                            .map(str::to_string)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();
    assert_eq!(fingerprints.len(), 6, "every attempt names its reply");
    assert!(
        fingerprints.windows(2).all(|pair| pair[0] == pair[1]),
        "identical replies fingerprint identically: {fingerprints:?}"
    );
    assert!(
        !machine.events().iter().any(|record| matches!(
            record,
            lash_core::SessionHistoryRecord::Protocol(event)
                if matches!(
                    lash_protocol_rlm::decode_rlm_protocol_event(event),
                    Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                        if diagnostic.phase == "repeated_reply"
                )
        )),
        "lash stops no turn for repeating itself"
    );
}

/// Leg 1 of the retry-hygiene triple, driven through the real machine.
///
/// Within one iteration a failing cell keeps whatever it printed before it
/// failed — that output is real and the model should see it. What must not
/// happen is any of it surviving into the *next* attempt, where it would read
/// as output of a program that never ran, and where an `error` left in place
/// would mark a cell that succeeded as failed.
///
/// The driver gets this by rebuilding the whole driver state on every protocol
/// iteration rather than clearing fields, so there is no field to forget. That
/// is a property of one line in `prepare_protocol_iteration`, which is why this
/// exercises the iteration rather than asserting that `Default` is empty.
#[test]
fn a_repair_iteration_carries_no_accumulation_from_the_failed_one() {
    let mut config = test_config_with_termination(RlmTermination::FinishRequired { schema: None });
    config.turn_budget = lash_core::TurnBudget::bounded(8);
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("run it")],
        Arc::new(Vec::new()),
        0,
    );

    let run_cell = |machine: &mut TurnMachine, code: &str, result: lash_sansio::ExecResponse| {
        let mut effects = drain_effects(machine);
        // Between iterations the machine parks on a checkpoint; acknowledge
        // it so the next LLM call is issued.
        if find_llm_call(&effects).is_none()
            && let Some((checkpoint_id, _)) = find_checkpoint(&effects)
        {
            machine.handle_response(Response::Checkpoint {
                id: checkpoint_id,
                delivery: lash_sansio::CheckpointDelivery::default(),
            });
            effects = drain_effects(machine);
        }
        let llm_id = *find_llm_call(&effects).expect("an llm call opens the iteration");
        let text = format!("<lashlang>\n{code}\n</lashlang>");
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(LlmResponse {
                full_text: text.clone(),
                parts: vec![text_part(&text)],
                terminal_reason: lash_core::LlmTerminalReason::Stop,
                ..LlmResponse::default()
            }),
        });
        let effects = drain_effects(machine);
        let exec_id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::ExecCode { id, .. } => Some(*id),
                _ => None,
            })
            .expect("the cell is executed");
        machine.handle_response(Response::ExecResult {
            id: exec_id,
            result: Ok(result),
        });
    };

    // A cell that printed something and then failed.
    run_cell(
        &mut machine,
        "print \"partial\"",
        exec_response(
            &["partial output before the failure"],
            Some("unknown variable `missing_name`"),
            None,
        ),
    );
    // The repair, which runs clean.
    run_cell(
        &mut machine,
        "print \"repaired\"",
        exec_response(&["repaired output"], None, None),
    );

    let trajectory = machine_trajectory(&machine);
    assert_eq!(trajectory.len(), 2, "one entry per executed cell");

    let failed = &trajectory[0];
    assert_eq!(failed.output, vec!["partial output before the failure"]);
    assert!(failed.error.is_some(), "the failure keeps its own error");

    let repaired = &trajectory[1];
    assert_eq!(
        repaired.output,
        vec!["repaired output"],
        "the repair iteration must not inherit the failed cell's output"
    );
    assert_eq!(
        repaired.error, None,
        "a clean cell must not inherit the previous iteration's error"
    );
    assert_eq!(repaired.code, "print \"repaired\"");
}
