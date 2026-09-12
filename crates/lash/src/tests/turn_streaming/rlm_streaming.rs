use super::*;

#[tokio::test]
pub(super) async fn pending_host_tool_completion_parks_turn_and_resolves_through_core_ingress()
-> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(PendingAppTools::new(key_tx)))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("pending-host-tool").open().await?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let mut turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("use async tool"))
            .stream_to(turn_events.as_ref())
            .await
    });

    let key = tokio::time::timeout(std::time::Duration::from_secs(1), key_rx)
        .await
        .expect("pending tool should request completion key")
        .expect("pending tool should send completion key");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut turn)
            .await
            .is_err(),
        "turn completed before external completion resolved"
    );
    assert!(
        !events
            .snapshot()
            .await
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. })),
        "pending launch must not be projected as a completed tool result"
    );

    let resolution = serde_json::json!({ "ok": true, "async": true });
    let accepted = core
        .completions()
        .resolve(key.clone(), lash_core::Resolution::Ok(resolution.clone()))
        .await?;
    assert_eq!(accepted, lash_core::ResolveOutcome::Accepted);

    let result = turn.await.expect("turn task")?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(result.assistant_message(), Some("done"));
    let events = events.snapshot().await;
    assert_eq!(assistant_prose(&events), "done");
    let tool_started = events
        .iter()
        .position(|activity| matches!(&activity.event, TurnEvent::ToolCallStarted { .. }))
        .expect("tool start event");
    let tool_completed = events
        .iter()
        .position(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completion event");
    assert!(tool_started < tool_completed);
    let TurnEvent::ToolCallCompleted { output, .. } = &events[tool_completed].event else {
        unreachable!();
    };
    assert_eq!(output.value_for_projection(), resolution);

    let duplicate = core
        .completions()
        .resolve(
            key,
            lash_core::Resolution::Ok(serde_json::json!({ "ok": false })),
        )
        .await?;
    assert!(matches!(
        duplicate,
        lash_core::ResolveOutcome::AlreadyResolved {
            terminal: lash_core::Resolution::Ok(value)
        } if value == resolution
    ));
    Ok(())
}

#[tokio::test]
pub(super) async fn stream_returns_terminal_metadata_without_prose() -> Result<()> {
    let core = standard_core();
    let session = core.session("semantic-events").open().await?;
    let events = RecordingEvents::default();

    let result = session
        .turn(TurnInput::text("stream"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    let prose = events
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.to_string()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(prose, "echo: stream");
    assert!(!events.snapshot().await.iter().any(|event| matches!(
        &event.event,
        TurnEvent::FinalValue { .. } | TurnEvent::ToolValue { .. }
    )));
    Ok(())
}

#[tokio::test]
pub(super) async fn stream_emits_chronological_tool_events_without_prose_pollution() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("tool-events").open().await?;
    let events = RecordingEvents::default();

    let collected = session
        .turn(TurnInput::text("use tool"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        collected.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    let events = events.snapshot().await;
    let started = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallStarted { .. }))
        .expect("tool start event");
    let completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completed event");
    assert!(started < completed);
    let TurnEvent::ToolCallCompleted { output, .. } = &events[completed].event else {
        unreachable!();
    };
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
    let prose = events
        .into_iter()
        .filter_map(|event| match event.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.to_string()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(prose, "done");
    assert!(!prose.contains("ok"));
    Ok(())
}

#[tokio::test]
pub(super) async fn interleaved_standard_parts_keep_order_through_store_history_and_anthropic_request()
-> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::<lash_core::LlmRequest>::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("anthropic")
        .complete({
            let requests = Arc::clone(&requests);
            let calls = Arc::clone(&calls);
            move |request| {
                let requests = Arc::clone(&requests);
                let calls = Arc::clone(&calls);
                async move {
                    requests.lock_recover().push(request);
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Ok(LlmResponse {
                            parts: vec![
                                LlmOutputPart::Text {
                                    text: "before".to_string(),
                                    response_meta: None,
                                },
                                LlmOutputPart::Reasoning {
                                    text: "consider".to_string(),
                                    replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                                        signature: Some("signed-consider".to_string()),
                                        ..Default::default()
                                    }),
                                },
                                LlmOutputPart::Text {
                                    text: "after".to_string(),
                                    response_meta: None,
                                },
                                LlmOutputPart::ToolCall {
                                    call_id: "lookup-1".to_string(),
                                    tool_name: "app_lookup".to_string(),
                                    input_json: "{}".to_string(),
                                    replay: None,
                                },
                            ],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        });
                    }
                    Ok(text_response("done"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("interleaved-standard-order").open().await?;

    let result = session
        .turn(TurnInput::text("preserve every part"))
        .run()
        .await?;

    let read_view = result.result.state.read_view();
    let stored_assistant = read_view
        .messages()
        .iter()
        .find(|message| {
            message.role == lash_core::MessageRole::Assistant
                && message
                    .parts
                    .iter()
                    .any(|part| part.tool_call_id.as_deref() == Some("lookup-1"))
        })
        .expect("stored interleaved assistant message");
    assert_eq!(
        stored_assistant
            .parts
            .iter()
            .map(|part| part.kind)
            .collect::<Vec<_>>(),
        [
            lash_core::PartKind::Prose,
            lash_core::PartKind::Reasoning,
            lash_core::PartKind::Prose,
            lash_core::PartKind::ToolCall,
        ]
    );

    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2);
    let history_blocks = requests[1]
        .messages
        .iter()
        .find(|message| {
            message.role == LlmRole::Assistant
                && message.blocks.iter().any(|block| {
                    matches!(
                        block,
                        LlmContentBlock::ToolCall { call_id, .. } if call_id == "lookup-1"
                    )
                })
        })
        .expect("provider request assistant history");
    assert!(
        matches!(
            history_blocks.blocks.as_slice(),
            [
                LlmContentBlock::Text { text: before, .. },
                LlmContentBlock::Reasoning { text: reasoning, .. },
                LlmContentBlock::Text { text: after, .. },
                LlmContentBlock::ToolCall { call_id, .. },
            ] if before.as_ref() == "before"
                && reasoning == "consider"
                && after.as_ref() == "\n\nafter"
                && call_id == "lookup-1"
        ),
        "provider history blocks: {:#?}",
        history_blocks.blocks
    );

    let wire = lash_provider_anthropic::testing::serialize_request(
        &requests[1],
        lash_core::provider::CacheRetention::None,
    )
    .expect("serialize Anthropic request");
    let wire_blocks = wire["messages"]
        .as_array()
        .and_then(|messages| {
            messages.iter().find_map(|message| {
                let blocks = message["content"].as_array()?;
                blocks
                    .iter()
                    .any(|block| block["id"] == "lookup-1")
                    .then_some(blocks)
            })
        })
        .expect("Anthropic assistant content blocks");
    assert_eq!(
        wire_blocks
            .iter()
            .map(|block| block["type"].as_str().expect("block type"))
            .collect::<Vec<_>>(),
        ["text", "text", "text", "tool_use"]
    );
    assert_eq!(wire_blocks[0]["text"], "before");
    assert_eq!(wire_blocks[1]["text"], "consider");
    assert_eq!(wire_blocks[2]["text"], "\n\nafter");
    assert_eq!(wire_blocks[3]["id"], "lookup-1");
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_streamed_lashlang_cell_uses_captured_body_when_final_text_is_raw() -> Result<()> {
    run_async_test_on_stack_budget("rlm-streamed-cell-raw-final-test", || async {
        const RAW_FINAL: &str = "Visible before cell.\n<lashlang>\npayload = r\"\"\"```markdown\ninside\n```\"\"\"\nfinish \"streamed raw final ok\"\n</lashlang>";
        const EXPECTED_CODE: &str =
            "payload = r\"\"\"```markdown\ninside\n```\"\"\"\nfinish \"streamed raw final ok\"";

        let provider = crate::testing::TestProvider::builder()
            .kind("stream-raw-final-test")
            .requires_streaming(true)
            .complete(|request| async move {
                let stream = request
                    .stream_events
                    .expect("RLM streaming turn should request provider stream events");
                for chunk in [
                    "Visible before",
                    " cell.\n<lash",
                    "lang>\npayload = r\"\"\"",
                    "```markdown\ninside\n",
                    "```\"\"\"\nfinish ",
                    "\"streamed raw final ok\"\n</lashlang>",
                ] {
                    stream.send(LlmStreamEvent::Delta(chunk.to_string()));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: RAW_FINAL.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();

        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-streamed-raw-final-cell").open().await?;
        let events = Arc::new(RecordingEvents::default());

        let result = session
            .turn(TurnInput::text("say hi"))
            .stream_to(events.as_ref())
            .await?;

        assert!(matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
        ));
        assert_eq!(
            result.final_value(),
            Some(&serde_json::json!("streamed raw final ok"))
        );

        let events = events.snapshot().await;
        let prose = assistant_prose(&events);
        assert_eq!(prose, "Visible before cell.\n");
        assert!(!prose.contains("<lashlang>"));
        assert!(!prose.contains("finish"));
        assert!(!prose.contains("```markdown"));

        let code_started = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::CodeBlockStarted { .. }))
            .expect("code started");
        let TurnEvent::CodeBlockStarted { language, code, .. } = &code_started.event else {
            unreachable!();
        };
        assert_eq!(language, "lashlang");
        assert_eq!(code, EXPECTED_CODE);
        assert!(!code.contains("<lashlang>"));

        let code_completed = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::CodeBlockCompleted { .. }))
            .expect("code completed");
        let TurnEvent::CodeBlockCompleted { success, error, .. } = &code_completed.event else {
            unreachable!();
        };
        assert!(*success);
        assert!(error.is_none());

        let terminal_output = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::FinalValue { .. }))
            .expect("terminal output");
        let TurnEvent::FinalValue { value } = &terminal_output.event else {
            unreachable!();
        };
        assert_eq!(value, &serde_json::json!("streamed raw final ok"));
        Ok(())
    })
}

#[cfg(feature = "rlm")]
pub(super) fn rlm_abort_drain_core(provider: ProviderHandle) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::rlm_builder(
        lash_core::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(provider)
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_abort_drain_ignores_a_late_attempt_reset() -> Result<()> {
    run_async_test_on_stack_budget("rlm-abort-drain-attempt-reset", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-abort-reset")
            .requires_streaming(true)
            .complete(|request| async move {
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::Delta(
                    "<lashlang>\nfinish \"cell survived reset\"\n</lashlang>\n".to_string(),
                ));
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                stream.send(LlmStreamEvent::AttemptReset);
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            })
            .build()
            .into_handle();
        let core = rlm_abort_drain_core(provider)?;
        let session = core.session("rlm-abort-reset").open().await?;

        let result = session.turn(TurnInput::text("finish")).run().await?;

        assert_eq!(
            result.final_value(),
            Some(&serde_json::json!("cell survived reset"))
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_abort_drain_preserves_late_reasoning_replay_and_usage() -> Result<()> {
    run_async_test_on_stack_budget("rlm-abort-drain-late-events", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-abort-late-events")
            .requires_streaming(true)
            .complete(|request| async move {
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                    response_started: true,
                    request_body: Some("{\"model\":\"rlm-evidence\"}".to_string()),
                    http_summary: Some(
                        "HTTP POST https://provider.test/v1/responses (stream)".to_string(),
                    ),
                    execution_evidence: Some(lash_core::ExecutionEvidence {
                        provider_request_id: Some("request-after-response-start".to_string()),
                        ..Default::default()
                    }),
                    generation_disposition: Some(lash_core::GenerationReceipt {
                        stop_sequences: lash_core::GenerationOptionOutcome::Applied,
                        ..Default::default()
                    }),
                    response_metadata: std::collections::BTreeMap::from([(
                        "header:x-request-cost".to_string(),
                        serde_json::json!("0.04"),
                    )]),
                    ..Default::default()
                }));
                stream.send(LlmStreamEvent::Delta(
                    "<lashlang>\nfinish \"late events survived\"\n</lashlang>\n".to_string(),
                ));
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                stream.send(LlmStreamEvent::Delta("provider suffix".to_string()));
                stream.send(LlmStreamEvent::Part(LlmOutputPart::Reasoning {
                    text: "signed reasoning".to_string(),
                    replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                        item_id: Some("reasoning-after-abort".to_string()),
                        encrypted_content: Some("encrypted-after-abort".to_string()),
                        signature: Some("signature-after-abort".to_string()),
                        redacted: false,
                        summary: vec!["signed reasoning".to_string()],
                        ..Default::default()
                    }),
                }));
                stream.send(LlmStreamEvent::Usage(lash_core::llm::types::LlmUsage {
                    input_tokens: 17,
                    output_tokens: 5,
                    reasoning_output_tokens: 2,
                    ..lash_core::llm::types::LlmUsage::default()
                }));
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            })
            .build()
            .into_handle();
        let recorder = Arc::new(RecordingNativeEffectController::default());
        let effect_controller: Arc<dyn lash_core::RuntimeEffectController> = recorder.clone();
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            lash_core::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .generation(lash_core::GenerationOptions {
            stop_sequences: vec!["caller-owned-stop".to_string()],
            ..Default::default()
        })
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .effect_host(Arc::new(lash_core::facade_support::NativeEffectHost::new(
            effect_controller,
        )))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-abort-late-events").open().await?;

        let result = session.turn(TurnInput::text("finish")).run().await?;

        assert_eq!(result.result.usage.input_tokens, 17);
        assert_eq!(result.result.usage.output_tokens, 5);
        assert!(
            result
                .result
                .state
                .read_view()
                .messages()
                .iter()
                .any(|message| {
                    message.parts.iter().any(|part| {
                        part.reasoning_meta.as_ref().is_some_and(|meta| {
                            meta.signature.as_deref() == Some("signature-after-abort")
                                && meta.encrypted_content.as_deref()
                                    == Some("encrypted-after-abort")
                        })
                    })
                })
        );

        let attempt = result
            .result
            .llm_calls
            .first()
            .and_then(|record| record.attempts.first())
            .expect("persisted aborted attempt");
        assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Aborted);
        assert_eq!(
            attempt
                .generation_disposition
                .expect("attempt disposition")
                .stop_sequences,
            lash_core::GenerationOptionOutcome::SuppressedProtocolOwned
        );
        assert_eq!(
            attempt
                .evidence
                .as_ref()
                .and_then(|evidence| evidence.provider_request_id.as_deref()),
            Some("request-after-response-start")
        );
        assert_eq!(
            attempt
                .evidence
                .as_ref()
                .and_then(|evidence| evidence.collection_interruption),
            Some(lash_core::ExecutionEvidenceCollectionInterruption::ProtocolAbort)
        );
        // Abort-retains-usage law: the late usage landed inside the drain
        // grace, so the aborted attempt is reported, not a hole.
        assert_eq!(
            attempt.usage_disposition,
            lash_core::AttemptUsageDisposition::Reported
        );
        assert_eq!(
            attempt.usage.as_ref().map(|usage| usage.input_tokens),
            Some(17)
        );
        let report = session.usage_report();
        assert_eq!(report.usage.unreported_attempts, 0);
        assert_eq!(report.usage.usage.input_tokens, 17);
        assert!(session.unreported_usage_attempts().await.is_empty());

        let journaled = recorder
            .persisted_outcomes()
            .into_iter()
            .find(|outcome| matches!(outcome, lash_core::RuntimeEffectOutcome::LlmCall { .. }))
            .expect("persisted LLM effect outcome");
        let lash_core::RuntimeEffectOutcome::LlmCall {
            result: journaled_result,
            call_record,
            ..
        } = journaled
        else {
            unreachable!("selected LLM outcome")
        };
        let response = journaled_result
            .as_ref()
            .as_ref()
            .expect("protocol abort is an accepted response");
        assert_eq!(
            response.request_body.as_deref(),
            Some("{\"model\":\"rlm-evidence\"}")
        );
        assert_eq!(
            response.http_summary.as_deref(),
            Some("HTTP POST https://provider.test/v1/responses (stream)")
        );
        assert_eq!(
            response.response_metadata.get("header:x-request-cost"),
            Some(&serde_json::json!("0.04"))
        );
        assert_eq!(
            response
                .generation_disposition
                .expect("response disposition")
                .stop_sequences,
            lash_core::GenerationOptionOutcome::SuppressedProtocolOwned
        );
        assert_eq!(
            response
                .execution_evidence
                .as_ref()
                .and_then(|evidence| evidence.collection_interruption),
            Some(lash_core::ExecutionEvidenceCollectionInterruption::ProtocolAbort)
        );
        let journaled_attempt = call_record
            .as_ref()
            .and_then(|record| record.attempts.first())
            .expect("journaled aborted attempt");
        assert_eq!(journaled_attempt, attempt);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_abort_drain_deadline_proceeds_with_default_usage() -> Result<()> {
    run_async_test_on_stack_budget("rlm-abort-drain-no-usage", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-abort-no-usage")
            .requires_streaming(true)
            .complete(|request| async move {
                request
                    .stream_events
                    .expect("stream events")
                    .send(LlmStreamEvent::Delta(
                        "<lashlang>\nfinish \"deadline survived\"\n</lashlang>\n".to_string(),
                    ));
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            })
            .build()
            .into_handle();
        let core = rlm_abort_drain_core(provider)?;
        let session = core.session("rlm-abort-no-usage").open().await?;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            session.turn(TurnInput::text("finish")).run(),
        )
        .await
        .expect("abort drain deadline must not wedge")?;

        assert_eq!(
            result.final_value(),
            Some(&serde_json::json!("deadline survived"))
        );
        // FIG-2765: the deadline wins, so the attempt has no provider usage.
        // The turn's own counters stay at zero, but the aborted call must
        // never look free: the sealed attempt says its usage is unreported
        // after the abort, and the session ledger carries a typed hole for
        // it even though every counter is zero.
        assert_eq!(result.result.usage, lash_core::TokenUsage::default());
        let attempt = result
            .result
            .llm_calls
            .first()
            .and_then(|record| record.attempts.first())
            .expect("sealed aborted attempt");
        assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Aborted);
        assert_eq!(attempt.usage, None);
        assert_eq!(
            attempt.usage_disposition,
            lash_core::AttemptUsageDisposition::UnreportedAfterAbort
        );

        let report = session.usage_report();
        assert_eq!(report.usage.unreported_attempts, 1);
        assert_eq!(report.usage.reconciled_attempts, 0);
        assert_eq!(report.usage.total_tokens, 0);
        let row = report
            .by_source_model
            .iter()
            .find(|row| row.source == "turn")
            .expect("unreported turn row is written even at zero usage");
        assert_eq!(row.usage.unreported_attempts, 1);
        assert_eq!(row.usage.usage, lash_core::TokenUsage::default());
        let unreported = session.unreported_usage_attempts().await;
        assert_eq!(unreported.len(), 1);
        assert_eq!(unreported[0].call_id, result.result.llm_calls[0].call_id.0);
        assert_eq!(unreported[0].attempt_ordinal, 1);
        assert_eq!(unreported[0].source, "turn");
        // The test provider never named a generation, so reconciliation has
        // nothing to ask for: the hole stays open and is reported as such.
        let reconciliation = session.reconcile_unreported_usage().await?;
        assert!(reconciliation.reconciled.is_empty());
        assert_eq!(reconciliation.unresolved, unreported);
        assert_eq!(session.usage_report().usage.unreported_attempts, 1);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_turn_without_interruption_or_usage_writes_no_ledger_row() -> Result<()> {
    run_async_test_on_stack_budget("rlm-zero-usage-no-row", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-zero-usage")
            .complete(|_request| async move {
                Ok(LlmResponse {
                    parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                        text: "<lashlang>\nfinish \"quiet\"\n</lashlang>\n".to_string(),
                        response_meta: None,
                    }],
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();
        let core = rlm_abort_drain_core(provider)?;
        let session = core.session("rlm-zero-usage").open().await?;
        let result = session.turn(TurnInput::text("finish")).run().await?;
        assert_eq!(result.final_value(), Some(&serde_json::json!("quiet")));
        let attempt = result
            .result
            .llm_calls
            .first()
            .and_then(|record| record.attempts.first())
            .expect("completed attempt");
        assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Completed);
        assert_eq!(
            attempt.usage_disposition,
            lash_core::AttemptUsageDisposition::UnreportedByProvider
        );
        let report = session.usage_report();
        assert_eq!(
            report.entry_count, 0,
            "zero usage with no interruption writes nothing"
        );
        assert_eq!(report.usage.unreported_attempts, 0);
        assert!(session.unreported_usage_attempts().await.is_empty());
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_tool_calls_stream_from_live_exec_boundary() -> Result<()> {
    run_async_test_on_stack_budget("rlm-live-exec-boundary-test", || {
        rlm_tool_calls_stream_from_live_exec_boundary_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_tool_calls_stream_from_live_exec_boundary_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"value = await tools.app_lookup({})?
finish "done""#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-live-tool-events").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("use tool"))
        .stream_to(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
    ));
    assert!(result.execution.had_tool_calls);
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].tool, "app_lookup");
    assert_eq!(result.tool_calls[0].args, serde_json::json!({}));
    assert_eq!(
        result.tool_calls[0].output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
    let events = events.snapshot().await;
    let code_started = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::CodeBlockStarted { .. }))
        .expect("code started");
    let tool_started = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallStarted { .. }))
        .expect("tool started");
    let tool_completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completed");
    let code_completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::CodeBlockCompleted { .. }))
        .expect("code completed");
    let terminal_output = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::FinalValue { .. }))
        .expect("terminal output");
    assert!(code_started < tool_started);
    assert!(tool_started < tool_completed);
    assert!(tool_completed < code_completed);
    assert!(code_completed < terminal_output);
    assert!(!events[code_completed + 1..].iter().any(|event| matches!(
        &event.event,
        TurnEvent::ToolCallStarted { .. } | TurnEvent::ToolCallCompleted { .. }
    )));

    let TurnEvent::ToolCallCompleted {
        call_id,
        output,
        graph_key: tool_completed_graph_key,
        parent_call_id: tool_completed_parent,
        ..
    } = &events[tool_completed].event
    else {
        unreachable!();
    };
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
    let TurnEvent::CodeBlockStarted {
        graph_key: started_graph_key,
        ..
    } = &events[code_started].event
    else {
        unreachable!();
    };
    let encoded_address = started_graph_key
        .as_deref()
        .and_then(|key| key.strip_prefix("effect:"))
        .unwrap_or_else(|| {
            panic!("missing foreground effect address on CodeBlockStarted: {started_graph_key:?}")
        });
    let mut scope =
        serde_json::Deserializer::from_str(encoded_address).into_iter::<serde_json::Value>();
    let scope_value = scope
        .next()
        .transpose()?
        .expect("foreground graph key carries its execution scope");
    let replay_key = encoded_address
        .get(scope.byte_offset()..)
        .and_then(|suffix| suffix.strip_prefix(':'))
        .map(serde_json::from_str::<String>)
        .transpose()?
        .expect("foreground graph key carries its replay key");
    assert_eq!(scope_value["version"], 2);
    assert_eq!(scope_value["kind"], "turn");
    assert_eq!(scope_value["session_id"], "rlm-live-tool-events");
    let execution_id = scope_value["execution_id"]
        .as_str()
        .expect("foreground turn scope carries its execution id");
    assert_eq!(
        replay_key,
        format!("rlm-live-tool-events:{execution_id}:1:0:exec_code:3")
    );
    let TurnEvent::CodeBlockCompleted {
        language,
        success,
        error,
        tool_call_ids,
        graph_key: completed_graph_key,
        ..
    } = &events[code_completed].event
    else {
        unreachable!();
    };
    assert_eq!(language, "lashlang");
    assert!(*success);
    assert!(error.is_none());
    assert_eq!(call_id.as_ref(), tool_call_ids.first());
    assert_eq!(tool_call_ids.len(), 1);
    assert_eq!(completed_graph_key, started_graph_key);
    // Task 4: the RLM tool call carries the enclosing block's graph_key for
    // structural containment, and no batch parent for a top-level call.
    let TurnEvent::ToolCallStarted {
        graph_key: tool_started_graph_key,
        parent_call_id: tool_started_parent,
        ..
    } = &events[tool_started].event
    else {
        unreachable!();
    };
    assert_eq!(tool_started_graph_key, started_graph_key);
    assert_eq!(tool_completed_graph_key, started_graph_key);
    assert_eq!(tool_started_parent, &None);
    assert_eq!(tool_completed_parent, &None);
    let read_view = result.state.read_view();
    assert!(
        read_view.messages().iter().all(|message| message
            .parts
            .iter()
            .all(|part| part.tool_call_id.as_ref() != tool_call_ids.first())),
        "live RLM tool calls should not be persisted as message history"
    );
    assert_eq!(
        read_view
            .session_graph()
            .clone()
            .active_path_nodes()
            .into_iter()
            .filter_map(|node| node.event())
            .filter(|event| matches!(event, lash_core::SessionHistoryRecord::Conversation(_)))
            .count(),
        read_view.messages().len()
    );
    let TurnEvent::FinalValue { value } = &events[terminal_output].event else {
        unreachable!();
    };
    assert_eq!(value, &serde_json::json!("done"));
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_recovered_tool_failure_remains_in_turn_accounting() -> Result<()> {
    run_async_test_on_stack_budget("rlm-recovered-tool-failure-test", || async {
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(queued_text_provider(vec![lashlang_block(
            r#"failure = await tools.app_lookup({})
finish "recovered""#,
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(FailingAppTools))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-recovered-tool-failure").open().await?;

        let result = session
            .turn(TurnInput::text("recover the tool failure"))
            .run()
            .await?
            .result;

        assert!(matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
        ));
        assert_eq!(result.final_value(), Some(&serde_json::json!("recovered")));
        assert!(result.execution.had_tool_calls);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool, "app_lookup");
        assert!(!result.tool_calls[0].output.is_success());
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_code_block_aggregate_lists_every_collected_tool_call() -> Result<()> {
    run_async_test_on_stack_budget("rlm-aggregate-tool-ids-test", || {
        rlm_code_block_aggregate_lists_every_collected_tool_call_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_code_block_aggregate_lists_every_collected_tool_call_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"a = await tools.app_lookup({})?
b = await tools.app_lookup({})?
finish "done""#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-aggregate-tool-ids").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("use tools"))
        .stream_to(events.as_ref())
        .await?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
    ));
    let events = events.snapshot().await;

    // Every collected RLM tool record carries a call_id, so the code block's
    // `tool_call_ids` aggregate (which filters `Some(call_id)`) cannot drop a
    // call.
    let completed_ids = events
        .iter()
        .filter_map(|event| match &event.event {
            TurnEvent::ToolCallCompleted { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_ids.len(), 2, "expected two tool completions");
    assert!(
        completed_ids.iter().all(Option::is_some),
        "every collected RLM tool record must carry a call_id"
    );

    let tool_call_ids = events
        .iter()
        .find_map(|event| match &event.event {
            TurnEvent::CodeBlockCompleted { tool_call_ids, .. } => Some(tool_call_ids.clone()),
            _ => None,
        })
        .expect("code block completed");
    assert_eq!(tool_call_ids.len(), completed_ids.len());
    for call_id in completed_ids.into_iter().flatten() {
        assert!(
            tool_call_ids.contains(&call_id),
            "code block aggregate must list collected tool call {call_id}"
        );
    }
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_tool_calls_emit_typed_trace_pair_and_inline_boundary_protocol_step() -> Result<()>
{
    run_async_test_on_stack_budget("rlm-tool-trace-test", || {
        rlm_tool_calls_emit_typed_trace_pair_and_inline_boundary_protocol_step_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_tool_calls_emit_typed_trace_pair_and_inline_boundary_protocol_step_inner()
-> Result<()> {
    let trace_path = std::env::temp_dir().join(format!(
        "lash-rlm-tool-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"value = await tools.app_lookup({})?
finish "done""#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .trace_jsonl_path(trace_path.clone())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-tool-trace").open().await?;

    let result = session.turn(TurnInput::text("use tool")).run().await?;
    assert!(matches!(
        result.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
    ));
    core.flush_trace_sink()?;

    let logged = std::fs::read_to_string(&trace_path).expect("read trace");
    let entries = logged
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log entry"))
        .collect::<Vec<_>>();

    // The native substrate never persists progress boundaries, but the protocol
    // events returned by those boundaries must still reach the trace sink.
    // Runtime diagnostics use a separate emitter and do not prove this path.
    entries
        .iter()
        .find(|entry| {
            entry.get("type").and_then(|v| v.as_str()) == Some("protocol_step")
                && entry.get("plugin_id").and_then(|v| v.as_str()) == Some("rlm_protocol")
        })
        .expect("inline boundary-sourced RLM protocol step");

    // Task 1: RLM tool calls emit a single typed Started/Completed trace pair,
    // with span identity stamped so each nests under its turn as tool:<call_id>.
    let started = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("tool_call_started"))
        .collect::<Vec<_>>();
    let completed = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("tool_call_completed"))
        .collect::<Vec<_>>();
    assert_eq!(started.len(), 1, "expected one RLM tool start: {entries:?}");
    assert_eq!(
        completed.len(),
        1,
        "expected one RLM tool completion: {entries:?}"
    );
    let call_id = completed[0]
        .get("call_id")
        .and_then(|v| v.as_str())
        .expect("completed tool trace call id");
    assert_eq!(
        completed[0].get("name").and_then(|v| v.as_str()),
        Some("app_lookup")
    );
    assert_eq!(
        completed[0]
            .get("context")
            .and_then(|context| context.get("graph_node_id"))
            .and_then(|v| v.as_str()),
        Some(format!("tool:{call_id}").as_str()),
        "RLM tool span identity must be tool:<call_id>"
    );

    // The typed exec-code completion carries its structured tool-call roll-up.
    let tool_calls = logged
        .lines()
        .find_map(|line| {
            let record = serde_json::from_str::<lash_trace::TraceRecord>(line).ok()?;
            let lash_trace::TraceEvent::ExecCodeCompleted { tool_calls, .. } = record.event else {
                return None;
            };
            Some(tool_calls)
        })
        .expect("typed exec-code completion event");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].call_id.as_deref(), Some(call_id));
    assert_eq!(tool_calls[0].name, "app_lookup");
    assert_eq!(
        tool_calls[0].status,
        lash_trace::TraceToolCallStatus::Success
    );

    let _ = std::fs::remove_file(&trace_path);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_native_provider_tool_call_is_a_traced_non_retryable_turn_issue() -> Result<()> {
    run_async_test_on_stack_budget("rlm-native-tool-contract-test", || async {
        let trace_path = std::env::temp_dir().join(format!(
            "lash-rlm-native-tool-contract-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            lash_core::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(native_tool_call_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .trace_jsonl_path(trace_path.clone())
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-native-tool-contract").open().await?;

        let turn = session
            .turn(TurnInput::text("trigger native provider tool call"))
            .run()
            .await?;

        assert_eq!(
            turn.result.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::RuntimeError)
        );
        let issue = turn
            .result
            .errors
            .iter()
            .find(|issue| issue.code.as_deref() == Some("native_tool_call_not_allowed"))
            .expect("typed RLM native-tool-call issue");
        assert_eq!(issue.kind, "rlm_protocol");
        assert_eq!(issue.retryable, Some(false));
        assert!(issue.message.contains("native_lookup"));
        assert!(issue.message.contains("must flow through Lashlang"));

        core.flush_trace_sink()?;
        let logged = std::fs::read_to_string(&trace_path).expect("read trace");
        let entries = logged
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace JSON"))
            .collect::<Vec<_>>();
        let diagnostic = entries
            .iter()
            .find(|entry| {
                entry.get("type").and_then(|value| value.as_str()) == Some("protocol_step")
                    && entry.get("plugin_id").and_then(|value| value.as_str())
                        == Some(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID)
                    && entry
                        .pointer("/payload/RlmDiagnostic/phase")
                        .and_then(|value| value.as_str())
                        == Some("protocol_contract_violation")
            })
            .expect("RLM protocol-contract trace record");
        assert_eq!(
            diagnostic
                .pointer("/payload/RlmDiagnostic/payload/code")
                .and_then(|value| value.as_str()),
            Some("native_tool_call_not_allowed")
        );
        assert_eq!(
            diagnostic
                .pointer("/payload/RlmDiagnostic/payload/tool_name")
                .and_then(|value| value.as_str()),
            Some("native_lookup")
        );

        let _ = std::fs::remove_file(&trace_path);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_pending_host_tool_completion_resumes_lashlang_await() -> Result<()> {
    run_async_test_on_stack_budget("rlm-pending-host-tool-test", || {
        rlm_pending_host_tool_completion_resumes_lashlang_await_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_pending_host_tool_completion_resumes_lashlang_await_inner() -> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        "value = await tools.app_lookup({})?\nfinish value",
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(PendingAppTools::new(key_tx)))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-pending-host-tool").open().await?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let mut turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("await async app lookup"))
            .stream_to(turn_events.as_ref())
            .await
    });

    let key = tokio::time::timeout(std::time::Duration::from_secs(1), key_rx)
        .await
        .expect("pending RLM tool should request completion key")
        .expect("pending RLM tool should send completion key");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut turn)
            .await
            .is_err(),
        "RLM turn completed before external completion resolved"
    );
    assert!(
        !events
            .snapshot()
            .await
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. })),
        "pending RLM launch must not emit a completed tool result"
    );

    let payload = serde_json::json!({ "ok": true, "async": "rlm" });
    let outcome = core
        .completions()
        .resolve(key, lash_core::Resolution::Ok(payload.clone()))
        .await?;
    assert_eq!(outcome, lash_core::ResolveOutcome::Accepted);

    let result = turn.await.expect("turn task")?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
    ));
    assert_eq!(result.final_value(), Some(&payload));
    let events = events.snapshot().await;
    let terminal_output = events
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::FinalValue { value } => Some(value),
            _ => None,
        })
        .expect("terminal final value");
    assert_eq!(terminal_output, &payload);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_process_pending_host_tool_completion_resumes_process_await() -> Result<()> {
    run_async_test_on_stack_budget("rlm-process-pending-host-tool-test", || {
        rlm_process_pending_host_tool_completion_resumes_process_await_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_process_pending_host_tool_completion_resumes_process_await_inner()
-> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"
process lookup(tools: Tools) {
  value = await tools.app_lookup({})?
  finish value
}
handle = start lookup(tools: tools)
result = (await handle)?
finish result"#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(PendingAppTools::new(key_tx)))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-process-pending-host-tool").open().await?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let mut turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("start process with async app lookup"))
            .stream_to(turn_events.as_ref())
            .await
    });

    let key = tokio::time::timeout(std::time::Duration::from_secs(1), key_rx)
        .await
        .expect("pending process tool should request completion key")
        .expect("pending process tool should send completion key");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut turn)
            .await
            .is_err(),
        "process-backed turn completed before external completion resolved"
    );
    assert!(
        !events
            .snapshot()
            .await
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. })),
        "pending process tool launch must not emit a completed tool result"
    );

    let payload = serde_json::json!({ "ok": true, "async": "process" });
    let outcome = core
        .completions()
        .resolve(key, lash_core::Resolution::Ok(payload.clone()))
        .await?;
    assert_eq!(outcome, lash_core::ResolveOutcome::Accepted);

    let result = turn.await.expect("turn task")?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
    ));
    assert_eq!(result.final_value(), Some(&payload));
    let events = events.snapshot().await;
    let terminal_output = events
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::FinalValue { value } => Some(value),
            _ => None,
        })
        .expect("terminal final value");
    assert_eq!(terminal_output, &payload);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn continue_as_observation_emits_frame_switch_then_commit() -> Result<()> {
    run_async_test_on_stack_budget("continue-as-observation-test", || {
        continue_as_observation_emits_frame_switch_then_commit_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn continue_as_observation_emits_frame_switch_then_commit_inner() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"await control.continue_as({ task: "finish in a fresh frame" })?"#),
        lashlang_block(r#"finish "done after continue_as""#),
    ]))
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("continue-as-observation").open().await?;
    let cursor = session.observe().current_observation().cursor;

    let output = session.turn(TurnInput::text("switch frames")).run().await?;
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after continue_as"))
    );

    let SessionResume::Replayed { events } = session.observe().resume_from_cursor(&cursor)? else {
        panic!("recent cursor should replay continue_as observation events");
    };
    assert!(
        events.windows(2).any(|window| matches!(
            (&window[0].payload, &window[1].payload),
            (
                lash_core::SessionObservationEventPayload::AgentFrameSwitched { .. },
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        )),
        "expected AgentFrameSwitched immediately followed by Committed, got {events:?}"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn lane_less_post_commit_from_plain_turn_does_not_affect_next_turn() -> Result<()> {
    run_async_test_on_stack_budget("lane-less-post-commit-plain-turn-test", || {
        lane_less_post_commit_from_plain_turn_does_not_affect_next_turn_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn lane_less_post_commit_from_plain_turn_does_not_affect_next_turn_inner()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "nested-release-turn-latch";
    let append_count = Arc::new(AtomicUsize::new(0));
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"finish "plain turn complete""#),
        lashlang_block(r#"await control.continue_as({ task: "finish turn two" })?"#),
        lashlang_block(r#"finish "turn two complete""#),
    ]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .plugin(Arc::new(TurnPersistedGraphAppendFactory {
        append_count: Arc::clone(&append_count),
        max_appends: 1,
    }))
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;

    let first = session
        .turn(TurnInput::text("plain finish with nested append"))
        .run()
        .await?;
    assert_eq!(
        first.final_value(),
        Some(&serde_json::json!("plain turn complete"))
    );
    assert_eq!(append_count.load(Ordering::SeqCst), 1);
    let second = session
        .turn(TurnInput::text("continue without another nested append"))
        .run()
        .await?;
    assert_eq!(
        second.final_value(),
        Some(&serde_json::json!("turn two complete"))
    );
    assert_eq!(append_count.load(Ordering::SeqCst), 1);
    // Initial state admission plus main turn 1, its lane-less TurnPersisted
    // append, and main turn 2 each acquire once. No hidden transfer/reacquire
    // occurs at either boundary.
    assert_sqlite_session_lane_free_at_generation(
        store_factory.as_ref(),
        &SessionId::from(session_id),
        4,
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn probe_inprocess_continue_as_survives_post_commit_graph_append() -> Result<()> {
    run_async_test_on_stack_budget("inprocess-continue-as-authority-test", || {
        probe_inprocess_continue_as_survives_post_commit_graph_append_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn probe_inprocess_continue_as_survives_post_commit_graph_append_inner()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "inprocess-continue-as";
    let append_count = Arc::new(AtomicUsize::new(0));
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"await control.continue_as({ task: "finish in process" })?"#),
        lashlang_block(r#"finish "done after in-process handoff""#),
    ]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .plugin(Arc::new(TurnPersistedGraphAppendFactory {
        append_count: Arc::clone(&append_count),
        max_appends: 1,
    }))
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;

    let output = session
        .turn(TurnInput::text("switch frames in process"))
        .run()
        .await?;

    assert_eq!(append_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after in-process handoff")),
        "post-commit graph writes must not strand the in-process frame handoff: {output:?}"
    );
    // Initial state admission and the outer turn each acquire once; the nested
    // post-commit append borrows the outer fence.
    assert_sqlite_session_lane_free_at_generation(
        store_factory.as_ref(),
        &SessionId::from(session_id),
        2,
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn durable_queued_continue_as_survives_post_commit_graph_append() -> Result<()> {
    run_async_test_on_stack_budget("durable-queued-continue-as-authority-test", || {
        durable_queued_continue_as_survives_post_commit_graph_append_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn durable_queued_continue_as_survives_post_commit_graph_append_inner()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "durable-queued-continue-as";
    let append_count = Arc::new(AtomicUsize::new(0));
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"await control.continue_as({ task: "finish from durable handoff" })?"#),
        lashlang_block(r#"finish "done after durable handoff""#),
    ]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .plugin(Arc::new(TurnPersistedGraphAppendFactory {
        append_count: Arc::clone(&append_count),
        max_appends: 1,
    }))
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    session
        .enqueue(TurnInput::text("switch frames from queued work"))
        .id("queued-continue-as")
        .send()
        .await?;

    let output = session
        .queued_turn()
        .run()
        .await?
        .expect("queued turn should run");

    assert_eq!(append_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after durable handoff")),
        "post-commit graph writes must not strand the committed frame handoff: {output:?}"
    );
    // The queued ingress admission and the outer queued turn each acquire
    // once; the nested post-commit append borrows that outer fence.
    assert_sqlite_session_lane_free_at_generation(
        store_factory.as_ref(),
        &SessionId::from(session_id),
        2,
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn durable_queued_continue_as_seed_is_visible_to_follow_turn_linker() -> Result<()> {
    run_async_test_on_stack_budget("durable-queued-continue-as-seed-test", || {
        durable_queued_continue_as_seed_is_visible_to_follow_turn_linker_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn durable_queued_continue_as_seed_is_visible_to_follow_turn_linker_inner()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "durable-queued-continue-as-seed";
    let (first_provider_call_tx, first_provider_call_rx) = tokio::sync::oneshot::channel();
    let first_provider_call_tx = Arc::new(std::sync::Mutex::new(Some(first_provider_call_tx)));
    let release_first_provider_call = Arc::new(tokio::sync::Notify::new());
    let provider_call_count = Arc::new(AtomicUsize::new(0));
    let repair_request = Arc::new(std::sync::Mutex::new(None));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let first_provider_call_tx = Arc::clone(&first_provider_call_tx);
            let release_first_provider_call = Arc::clone(&release_first_provider_call);
            let provider_call_count = Arc::clone(&provider_call_count);
            let repair_request = Arc::clone(&repair_request);
            move |request| {
                let first_provider_call_tx = Arc::clone(&first_provider_call_tx);
                let release_first_provider_call = Arc::clone(&release_first_provider_call);
                let provider_call_count = Arc::clone(&provider_call_count);
                let repair_request = Arc::clone(&repair_request);
                async move {
                    let call = provider_call_count.fetch_add(1, Ordering::SeqCst);
                    let text = match call {
                        0 => {
                            lashlang_block(
                                r#"control = { total: 28 }
finish { established: control.total }"#,
                            )
                        }
                        1 => {
                            if let Some(tx) = first_provider_call_tx
                                .lock_recover()
                                .take()
                            {
                                let _ = tx.send(());
                            }
                            release_first_provider_call.notified().await;
                            lashlang_block(
                                r#"await control.continue_as({ task: "finish from seeded durable handoff", seed: { baton: "seed:durable", session_chars: len(session_projection) } })?"#,
                            )
                        }
                        2 => lashlang_block(
                            r#"finish { seed_visible: baton, session_projection_chars: session_chars }"#,
                        ),
                        _ => {
                            *repair_request.lock_recover() = Some(format!("{request:?}"));
                            lashlang_block(r#"finish { unexpected_repair: true }"#)
                        }
                    };
                    Ok(text_response(&text))
                }
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        lash_core::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(provider)
    .model(mock_model_spec())
    .store_factory(store_factory)
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    let established = session
        .turn(TurnInput::text(
            "establish a durable global that collides with a module root",
        ))
        .run()
        .await?;
    assert_eq!(
        established.final_value(),
        Some(&serde_json::json!({ "established": 28 }))
    );
    session
        .admin()
        .protocol()
        .apply_session_extension(lash_protocol_rlm::rlm_session_projection_extension(
            lash_protocol_rlm::RlmProjectedBindings::new()
                .bind_json("session_projection", serde_json::json!("session:durable"))
                .expect("valid session projection"),
        ))
        .await?;
    session
        .enqueue(TurnInput::text("switch frames with a durable seed"))
        .id("queued-continue-as-seed")
        .send()
        .await?;

    let turn_session = session.clone();
    let turn = tokio::spawn(async move { turn_session.queued_turn().run().await });
    tokio::time::timeout(std::time::Duration::from_secs(1), first_provider_call_rx)
        .await
        .expect("first provider call should start")
        .expect("first provider call signal should arrive");
    session
        .enqueue(TurnInput::text("keep this pending across the frame switch"))
        .id("queued-after-continue-as")
        .send()
        .await?;
    release_first_provider_call.notify_one();
    let output = turn
        .await
        .expect("queued turn task")?
        .expect("queued turn should run");

    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!({
            "seed_visible": "seed:durable",
            "session_projection_chars": 15
        })),
        "the committed frame seed must be installed before the follow turn links: {output:?}; repair_request={:?}",
        repair_request.lock_recover()
    );
    assert_eq!(provider_call_count.load(Ordering::SeqCst), 3);
    Ok(())
}
