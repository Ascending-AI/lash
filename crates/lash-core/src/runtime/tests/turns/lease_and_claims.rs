use super::*;

#[tokio::test]
pub(super) async fn cancellation_watch_exhaustion_tears_down_committed_cancel_and_settles_turn() {
    let controller = Arc::new(
        super::effect::RecordingEffectController::default().with_always_failing_cancel_watch(),
    );
    let controller_for_provider = Arc::clone(&controller);
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let tool_executions = Arc::new(AtomicUsize::new(0));
    let (provider_started_tx, provider_started_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_started_tx = Arc::new(Mutex::new(Some(provider_started_tx)));
    let transport =
        TestProvider::builder()
            .kind("mock")
            .requires_streaming(true)
            .complete(move |request| {
                let controller = Arc::clone(&controller_for_provider);
                let observed_provider_calls = Arc::clone(&observed_provider_calls);
                let provider_started_tx = Arc::clone(&provider_started_tx);
                async move {
                    let call = observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                    match call {
                        0 => {
                            request.stream_events.expect("stream events").send(
                                LlmStreamEvent::Delta("drained before effect abort".to_string()),
                            );
                            if let Some(started) = provider_started_tx.lock_recover().take() {
                                let _ = started.send(());
                            }
                            controller.wait_for_cancel_watch_exhaustion().await;
                            for _ in 0..32 {
                                tokio::task::yield_now().await;
                            }
                            Ok(LlmResponse {
                                parts: vec![LlmOutputPart::ToolCall {
                                    call_id: "post-exhaustion-tool".to_string(),
                                    tool_name: "echo_tool".to_string(),
                                    input_json: serde_json::json!({"value": "zombie"}).to_string(),
                                    replay: None,
                                }],
                                response_metadata: Default::default(),
                                ..LlmResponse::default()
                            })
                        }
                        1 => Ok(LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: "zombie turn completed".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        }),
                        _ => panic!("unexpected provider call {call}"),
                    }
                }
            })
            .build();
    let clock = Arc::new(CancelWatchTestClock(crate::testing::TestClock::new(0)));
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let config = super::effect::runtime_host_config_with_native_controller(controller.clone())
        .with_clock(host_clock);
    let driver_store: Arc<dyn crate::RuntimePersistence> = Arc::new(RecordingStore::default());
    crate::testing::store_fixtures::bind_conformance_session(
        &driver_store,
        &crate::SessionId::from("root"),
    )
    .await;
    let turn_driver = crate::TurnWorkDriver::for_session(
        Arc::clone(&config.control.effect_host),
        "root",
        driver_store,
    );
    let host = EmbeddedRuntimeHost::new(config);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(CountingEchoTool {
            executions: Arc::clone(&tool_executions),
        }),
        transport,
        host,
    )
    .await;
    let turn_id = "bounded-cancel-watch";
    let turn_address = crate::TurnAddress::new("root", turn_id);
    let turn_cancel = CancellationToken::new();
    let observed_turn_cancel = turn_cancel.clone();
    let (turn_events, stream_event_entered_rx) =
        CancellationGatedTurnEvents::new(turn_cancel.clone());
    let turn_events_for_task = turn_events.clone();
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("tear down after the cancellation watcher gives up"),
                TurnOptions::new(
                    turn_cancel,
                    named_turn_scope(&SessionId::from("root"), &TurnId::from(turn_id)),
                )
                .with_turn_events(&turn_events_for_task),
            )
            .await
    });

    provider_started_rx
        .await
        .expect("provider must start before cancellation is committed");
    stream_event_entered_rx
        .await
        .expect("the buffered stream event must reach the gated sink");
    let receipt = turn_driver
        .request_cancel(crate::TurnCancelRequest::new(
            turn_address.clone(),
            "watch-exhaustion-cancel",
            Some("test-user".to_string()),
        ))
        .await
        .expect("commit cancellation receipt");
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == "watch-exhaustion-cancel"
    ));
    controller.release_cancel_watch_failures();

    let turn = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("watch exhaustion must tear down and settle the turn")
        .expect("turn task")
        .expect("committed cancellation remains a successful turn terminal");
    assert_eq!(
        controller.cancel_watch_attempts(),
        crate::runtime::turn_loop::TURN_CANCEL_WATCH_MAX_ATTEMPTS
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "watch exhaustion must abort the in-flight provider call before another can start"
    );
    assert_eq!(
        tool_executions.load(Ordering::SeqCst),
        0,
        "provider output produced after watcher exhaustion must never reach an executor"
    );
    assert!(
        observed_turn_cancel.is_cancelled(),
        "watch exhaustion must cancel the active turn token after {} attempts",
        controller.cancel_watch_attempts()
    );
    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { ref evidence })
            if evidence.request_id == "watch-exhaustion-cancel"
    ));
    assert!(
        turn_events.snapshot().iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text }
                if text.as_ref() == "drained before effect abort"
        )),
        "cooperative teardown must drain the buffered stream event; provider_calls={}, tool_executions={}",
        provider_calls.load(Ordering::SeqCst),
        tool_executions.load(Ordering::SeqCst)
    );

    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        turn_driver.await_terminal(&turn_address),
    )
    .await
    .expect("the cancelled turn must settle its terminal")
    .expect("read settled turn terminal");
    assert!(matches!(
        terminal,
        crate::TurnTerminal::Committed {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled { ref evidence }),
            ..
        } if evidence.request_id == "watch-exhaustion-cancel"
    ));
}

#[tokio::test]
pub(super) async fn cancelled_provider_stream_does_not_commit_partial_output() {
    let (delta_sent_tx, delta_sent_rx) = tokio::sync::oneshot::channel::<()>();
    let delta_sent_tx = Arc::new(Mutex::new(Some(delta_sent_tx)));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete({
            let delta_sent_tx = Arc::clone(&delta_sent_tx);
            move |request| {
                let delta_sent_tx = Arc::clone(&delta_sent_tx);
                async move {
                    let stream = request
                        .stream_events
                        .expect("streaming runtime should request provider stream events");
                    stream.send(LlmStreamEvent::Delta("partial provider text".to_string()));
                    if let Some(tx) = delta_sent_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(transport).await;
    let cancel = CancellationToken::new();
    let turn_cancel = cancel.clone();
    let turn_events = RecordingTurnEvents::default();
    let turn_events_for_task = turn_events.clone();
    let turn = crate::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("cancel after partial stream"),
                TurnOptions::new(
                    turn_cancel,
                    named_turn_scope(
                        &SessionId::from("root"),
                        &TurnId::from("cancel-partial-provider-stream"),
                    ),
                )
                .with_turn_events(&turn_events_for_task),
            )
            .await
    });

    delta_sent_rx
        .await
        .expect("provider should emit the visible partial text");
    cancel.cancel();
    let assembled = turn
        .await
        .expect("turn task")
        .expect("cancelled turn should assemble");

    assert!(matches!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(
        assembled.errors.is_empty(),
        "requested cancellation must not become an llm_provider TurnIssue: {:?}",
        assembled.errors
    );
    assert!(assembled.assistant_output.safe_text.is_empty());
    assert!(assembled.assistant_output.raw_text.is_empty());
    assert!(
        turn_events
            .snapshot()
            .iter()
            .all(|activity| !matches!(&activity.event, TurnEvent::Error { message } if message == "LLM error: cancelled")),
        "requested cancellation must not emit a user-visible LLM error"
    );
    assert!(
        turn_events.snapshot().iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text } if text.as_ref() == "partial provider text"
        )),
        "partial provider text should remain observable only as live turn activity"
    );
    assert!(
        active_conversation_messages(&assembled.state)
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("partial provider text")),
        "cancelled streamed partial must not be committed to read-view history"
    );
}

#[tokio::test]
pub(super) async fn parent_end_failure_after_effect_loop_cancellation_returns_session_for_next_turn()
 {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let (second_call_started_tx, second_call_started_rx) = tokio::sync::oneshot::channel::<()>();
    let second_call_started_tx = Arc::new(Mutex::new(Some(second_call_started_tx)));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let call = observed_calls.fetch_add(1, Ordering::SeqCst);
            let second_call_started_tx = Arc::clone(&second_call_started_tx);
            async move {
                match call {
                    0 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "parent-end-failure-call".to_string(),
                            tool_name: "parent_end_failure_intent".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    1 => {
                        if let Some(tx) = second_call_started_tx.lock_recover().take() {
                            let _ = tx.send(());
                        }
                        std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
                    }
                    2 => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "second turn started".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    }),
                    _ => panic!("unexpected provider call {call}"),
                }
            }
        })
        .build();
    let mut runtime =
        runtime_with_plugins_and_tools(Vec::new(), Arc::new(ParentEndFailureIntentTool), transport)
            .await;
    let effect_controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_local_llm_execution()
            .with_next_tool_parent_end_failure(),
    );
    let cancel = CancellationToken::new();
    let cancel_after_second_call_starts = cancel.clone();
    let canceller = crate::task::spawn(async move {
        second_call_started_rx
            .await
            .expect("the first turn should enter its second provider call");
        cancel_after_second_call_starts.cancel();
    });

    let first_error = runtime
        .run_turn_assembled(
            TurnInput::text("start the parent-end failure witness"),
            cancel,
            crate::ScopedEffectController::shared(
                effect_controller.clone(),
                crate::ExecutionScope::turn("root", "parent-end-failure-first-turn"),
            )
            .expect("first turn scope"),
        )
        .await
        .expect_err("the forced parent-end failure should fail the cancelled turn");
    canceller.await.expect("canceller task");
    assert_eq!(
        first_error.code,
        crate::RuntimeErrorCode::PluginSessionManager
    );
    assert!(first_error.message.contains("forced parent-end failure"));

    let second_turn = runtime
        .run_turn_assembled(
            TurnInput::text("prove the runtime can start another turn"),
            CancellationToken::new(),
            crate::ScopedEffectController::shared(
                effect_controller,
                crate::ExecutionScope::turn("root", "parent-end-failure-second-turn"),
            )
            .expect("second turn scope"),
        )
        .await
        .expect("the second turn should start on the same runtime");

    assert_eq!(
        second_turn.assistant_output.safe_text,
        "second turn started"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
pub(super) async fn truncated_retry_resets_partial_tool_calls_and_retains_failed_attempt_usage() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .generation_retry_guarantee(crate::provider::GenerationRetryGuarantee::Idempotent)
        .options(crate::ProviderOptions {
            reliability: crate::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..crate::ProviderOptions::default()
        })
        .complete({
            let attempts = Arc::clone(&attempts);
            move |request| {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let stream = request.stream_events.expect("stream events");
                    if attempt == 0 {
                        let usage = LlmUsage {
                            input_tokens: 11,
                            output_tokens: 2,
                            ..LlmUsage::default()
                        };
                        stream.send(LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                            call_id: "partial-call".to_string(),
                            tool_name: "must_not_run".to_string(),
                            input_json: "{\"unfinished\":".to_string(),
                            replay: None,
                        }));
                        stream.send(LlmStreamEvent::Usage(usage.clone()));
                        return Err(LlmTransportError::new("Stream ended without finish_reason")
                            .with_kind(crate::ProviderFailureKind::Stream)
                            .with_code("stream_ended_before_finish_reason")
                            .with_retry_verdict(
                                crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                            )
                            .with_partial_response(LlmResponse {
                                parts: vec![LlmOutputPart::ToolCall {
                                    call_id: "partial-call".to_string(),
                                    tool_name: "must_not_run".to_string(),
                                    input_json: "{\"unfinished\":".to_string(),
                                    replay: None,
                                }],
                                usage,
                                provider_usage: Some(serde_json::json!({
                                    "prompt_tokens": 11,
                                    "completion_tokens": 2
                                })),
                                response_metadata: Default::default(),
                                ..LlmResponse::default()
                            }));
                    }

                    stream.send(LlmStreamEvent::Delta("success".to_string()));
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: crate::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(transport).await;

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry a truncated stream"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("truncated-stream-retry"),
                ),
            ),
        )
        .await
        .expect("retry succeeds");

    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(assembled.assistant_output.safe_text, "success");
    assert!(assembled.tool_calls.is_empty());
    assert!(
        active_conversation_messages(&assembled.state)
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("must_not_run"))
    );
    let failed_attempt = &assembled.llm_calls[0].attempts[0];
    assert_eq!(failed_attempt.outcome, crate::AttemptOutcome::Interrupted);
    assert_eq!(
        failed_attempt
            .usage
            .as_ref()
            .map(|usage| usage.input_tokens),
        Some(11)
    );
}

#[tokio::test]
pub(super) async fn counted_provider_regeneration_emits_one_host_visible_attempt_reset() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(crate::ProviderOptions {
            reliability: crate::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..crate::ProviderOptions::default()
        })
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        return Err(LlmTransportError::new("connection failed before response")
                            .with_kind(crate::ProviderFailureKind::Transport)
                            .with_retry_verdict(
                                crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                            ));
                    }

                    request
                        .stream_events
                        .expect("stream events")
                        .send(LlmStreamEvent::Delta("success".to_string()));
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: crate::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(transport).await;
    let turn_events = RecordingTurnEvents::default();

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry a pre-response transport failure"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("counted-regeneration-reset"),
                ),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("counted retry succeeds");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(assembled.llm_calls[0].attempts.len(), 2);
    assert_eq!(
        assembled.llm_calls[0]
            .attempts
            .iter()
            .filter(|attempt| attempt.retry_budget_consumed)
            .count(),
        2
    );
    let turn_events = turn_events.snapshot();
    let resets = turn_events
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                reasoning_correlation_ids,
            } => Some((assistant_prose_correlation_ids, reasoning_correlation_ids)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(resets.len(), 1);
    assert_eq!(resets[0].0, &Vec::<crate::runtime::TurnActivityId>::new());
    assert_eq!(resets[0].1, &Vec::<crate::runtime::TurnActivityId>::new());
}

#[tokio::test(start_paused = true)]
pub(super) async fn courtesy_retry_after_regeneration_emits_one_host_visible_attempt_reset() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(crate::ProviderOptions {
            reliability: crate::provider::ProviderReliability::default()
                .max_attempts(1)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..crate::ProviderOptions::default()
        })
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        return Err(LlmTransportError::new("provider requested a retry delay")
                            .with_status(429)
                            .with_retry_verdict(
                                crate::llm::transport::TransportRetryVerdict::RetryableThrottle {
                                    retry_after: Some(std::time::Duration::from_secs(1)),
                                },
                            ));
                    }

                    request
                        .stream_events
                        .expect("stream events")
                        .send(LlmStreamEvent::Delta("success".to_string()));
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: crate::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(transport).await;
    let turn_events = RecordingTurnEvents::default();

    let assembled = runtime
        .stream_turn(
            TurnInput::text("defer to a provider retry-after"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("courtesy-regeneration-reset"),
                ),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("courtesy retry succeeds");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(assembled.llm_calls[0].attempts.len(), 2);
    assert_eq!(
        assembled.llm_calls[0]
            .attempts
            .iter()
            .filter(|attempt| attempt.retry_budget_consumed)
            .count(),
        1
    );
    assert_eq!(
        turn_events
            .snapshot()
            .iter()
            .filter(|activity| { matches!(activity.event, TurnEvent::ModelAttemptReset { .. }) })
            .count(),
        1
    );
}

#[tokio::test]
pub(super) async fn retryable_mid_stream_failure_preserves_durable_charge_safety_evidence() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let lost_text = std::iter::repeat_n("discarded", 256)
        .collect::<Vec<_>>()
        .join(" ");
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(crate::ProviderOptions {
            reliability: crate::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..crate::ProviderOptions::default()
        })
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let requests = Arc::clone(&requests);
            let lost_text = lost_text.clone();
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                requests.lock_recover().push(request.messages.clone());
                let lost_text = lost_text.clone();
                async move {
                    let stream = request.stream_events.expect("stream events");
                    if call == 0 {
                        stream.send(LlmStreamEvent::Delta(lost_text.clone()));
                        let usage = LlmUsage {
                            input_tokens: 32,
                            output_tokens: 256,
                            ..LlmUsage::default()
                        };
                        stream.send(LlmStreamEvent::Usage(usage.clone()));
                        return Err(LlmTransportError::new(
                            "stream ended before terminal evidence",
                        )
                        .with_kind(crate::ProviderFailureKind::Stream)
                        .with_code("stream_ended_before_terminal_response")
                        .with_retry_verdict(
                            crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                        )
                        .with_partial_response(LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: lost_text,
                                response_meta: None,
                            }],
                            usage,
                            provider_usage: Some(serde_json::json!({
                                "prompt_tokens": 32,
                                "completion_tokens": 256
                            })),
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        }));
                    }

                    stream.send(LlmStreamEvent::Delta("replacement".to_string()));
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "replacement".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: crate::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let store = Arc::new(crate::InMemorySessionStore::new());
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let mut runtime = TestRuntime::new(transport)
        .store(runtime_store)
        .without_process_registry()
        .build()
        .await;
    let turn_events = RecordingTurnEvents::default();

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry after paid output"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(&SessionId::from("root"), &TurnId::from("paid-output-retry")),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("provider failure is returned as an assembled turn");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    assert!(assembled.assistant_output.safe_text.is_empty());
    assert!(assembled.assistant_output.raw_text.is_empty());
    let activities = turn_events.snapshot();
    assert!(activities.iter().any(|activity| matches!(
        &activity.event,
        TurnEvent::AssistantProseDelta { text } if text.as_ref() == lost_text
    )));
    assert!(
        activities
            .iter()
            .all(|activity| !matches!(activity.event, TurnEvent::ModelAttemptReset { .. }))
    );
    assert!(
        active_conversation_messages(&assembled.state)
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("discarded")),
        "a failed partial response remains preview output, not committed history"
    );
    let calls = &assembled.llm_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].attempts.len(), 1);
    let preserved_attempt = &calls[0].attempts[0];
    assert_eq!(
        preserved_attempt.protocol_position,
        crate::ProtocolPosition::OutputStarted
    );
    assert_eq!(
        preserved_attempt
            .usage
            .as_ref()
            .map(|usage| usage.output_tokens),
        Some(256)
    );
    assert_eq!(
        preserved_attempt
            .retry_decision
            .as_ref()
            .map(|decision| decision.scheduled),
        Some(false)
    );
    assert_eq!(
        preserved_attempt
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.reason.as_deref()),
        Some("output_started_without_retry_guarantee")
    );
    let issue = assembled.errors.first().expect("typed provider issue");
    assert_eq!(
        issue.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
    assert_eq!(issue.retryable, Some(false));
    assert!(
        issue.message.contains("already paid for")
            && issue.message.contains("cannot be safely regenerated")
    );
    assert_eq!(assembled.failure_evidence.len(), 1);
    assert!(
        assembled.failure_evidence.len()
            <= assembled
                .llm_calls
                .iter()
                .map(|call| call.attempts.len())
                .sum::<usize>(),
        "durable failure evidence is cardinality-bounded by sealed provider attempts"
    );
    let failure = &assembled.failure_evidence[0];
    assert_eq!(
        failure.partial_output.as_ref().map(|output| output.text()),
        Some(lost_text.as_str())
    );
    assert_eq!(failure.billed_usage.output_tokens, 256);
    assert_eq!(
        failure.refusal.denial_reason,
        crate::ChargeSafetyDenialReason::GuaranteeRequired
    );
    assert_eq!(
        failure.refusal.protocol_position,
        crate::ProtocolPosition::OutputStarted
    );
    assert!(
        serde_json::to_value(failure)
            .expect("serialize durable failure evidence")
            .get("refusal")
            .and_then(|refusal| refusal.get("retry_guarantee"))
            .is_none(),
        "durable refusal evidence must not persist an inferred constant"
    );
    assert_eq!(
        (
            failure.refusal.attempt_number,
            failure.refusal.attempt_count
        ),
        (1, 1)
    );

    runtime
        .stream_turn(
            TurnInput::text("follow up after the failed generation"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("paid-output-follow-up"),
                ),
            ),
        )
        .await
        .expect("a later turn can continue without replaying failure evidence");
    {
        let requests = requests.lock_recover();
        assert_eq!(requests.len(), 2);
        assert!(
            !serde_json::to_string(&requests[1])
                .expect("serialize the follow-up provider request")
                .contains("discarded"),
            "the next provider prompt has no constructional path from turn settlement evidence"
        );
    }

    drop(runtime);
    let reopened = crate::store::load_persisted_session_read_view(store.as_ref())
        .await
        .expect("reopen the failed turn's session")
        .expect("failed turn left a durable session");
    assert_eq!(
        reopened.turn_failure_settlements().len(),
        1,
        "mid-stream failure evidence must survive runtime teardown and reopen"
    );
    assert_eq!(
        reopened.turn_failure_settlements()[0].evidence,
        assembled.failure_evidence
    );
    assert!(
        reopened
            .messages()
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("discarded")),
        "durable failure evidence remains outside model context"
    );
}

// Boundary: execution-lease tests stay in `turns.rs` because they exercise live
// `LashRuntime` lease acquisition, public scheduling, turn phase probes, and
// provider suspension. Runtime Scenarios own persistence-level head-CAS and
// queue/input claim invariants; these tests own the facade scheduler response.
/// A foreground turn is refused while a foreign executor holds the session
/// lane, before provider work or durable input acceptance (ADR 0077).
#[tokio::test]
pub(super) async fn foreground_turn_is_refused_when_session_lane_is_held() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "foreground proceeded".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    let owner = lease_owner("other-runtime");
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &owner,
        "foreground-turn-is-refused-when-session-lane-is-held-executor",
        60_000,
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("foreground must wait"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("foreground-busy-lane-turn"),
            ),
        )
        .await
        .expect_err("a foreign lease holder refuses the foreground turn");

    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert!(
        crate::TurnInputStore::list_pending_turn_inputs(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("read pending turn inputs after refusal")
            .is_empty(),
        "lease refusal must precede durable input acceptance"
    );
    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &held_lease.completion(),
    )
    .await
    .expect("release held session execution lease");
}

#[tokio::test]
pub(super) async fn idle_queued_work_noops_without_claiming_when_session_lane_is_held() {
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "queued answer".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(transport).await;
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued while busy",
    )
    .await;
    let owner = lease_owner("foreground-runtime");
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &owner,
        "idle-queued-work-noops-without-claiming-when-session-lane-is-held-executor",
        60_000,
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    let busy_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(&SessionId::from("root"), &TurnId::from("queued-busy-turn")),
        ))
        .await
        .expect("busy queued drain should not error")
        .ran();

    assert!(
        busy_result.is_none(),
        "idle queued drain must no-op while another owner holds the session lane"
    );
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queued turn input while busy")
        .len(),
        1,
        "busy drain must not consume queued turn input"
    );

    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &held_lease.completion(),
    )
    .await
    .expect("release held session execution lease");
    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("queued-after-busy-turn"),
            ),
        ))
        .await
        .expect("queued drain after release should succeed")
        .ran()
        .expect("queued turn should still be pending after busy no-op");

    assert_eq!(drained.assistant_output.safe_text, "queued answer");
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queued turn input after drain")
        .is_empty()
    );
}

#[tokio::test]
pub(super) async fn durable_controller_waits_for_busy_session_lane_before_draining_queued_input() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    runtime.host.core.clock = clock.clone();
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued during failover",
    )
    .await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &lease_owner("crashed-worker"),
        "durable-controller-waits-for-busy-session-lane-before-draining-queued-input-executor",
        50,
    )
    .await
    .expect("claim crashed worker session execution lease")
    .acquired()
    .expect("crashed worker holds session execution lease");
    assert_eq!(held_lease.expires_at_epoch_ms, 1_050);

    let controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_controller_owned_replay()
            .with_engine_paced_lane(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-failover-wake"),
    )
    .expect("durable queued-turn scope");
    let mut drain = crate::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
            .await
            .map(crate::facade_support::QueuedTurnDrain::ran)
    });

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(5), &mut drain)
            .await
            .is_err(),
        "durable queued drain must remain pending while the foreign lease is live"
    );
    clock.advance_ms(51);
    let drained = drain
        .await
        .expect("join durable queued drain")
        .expect("durable queued drain succeeds")
        .expect("durable queued drain consumes the pending input");
    assert_eq!(drained.assistant_output.safe_text, "finished");
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list pending input after durable drain")
        .is_empty(),
        "durable queued drain must settle the literal pending input"
    );
}

/// The give-up half of the same policy: a holder that keeps renewing is alive,
/// so no amount of in-process waiting can free the lane inside this invocation.
/// The drain reports the typed retryable error naming the live holder, and
/// leaves both the holder row and the queued row exactly as it found them.
#[tokio::test]
pub(super) async fn durable_controller_reports_a_retryable_busy_lane_when_the_holder_is_alive() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    runtime.host.core.clock = clock.clone();
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued behind a live holder",
    )
    .await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &lease_owner("live-worker"),
        "live-holder-executor",
        100,
    )
    .await
    .expect("claim live worker session execution lease")
    .acquired()
    .expect("live worker holds session execution lease");
    assert_eq!(held_lease.expires_at_epoch_ms, 1_100);

    let controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_controller_owned_replay()
            .with_engine_paced_lane(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-live-holder"),
    )
    .expect("durable queued-turn scope");
    let mut drain = crate::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
            .await
            .map(crate::facade_support::QueuedTurnDrain::ran)
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(5), &mut drain)
            .await
            .is_err(),
        "the drain must still be waiting when the holder renews"
    );

    clock.advance_ms(10);
    let renewed = crate::store::SessionExecutionLeaseStore::renew_session_execution_lease(
        store.as_ref(),
        &held_lease.fence(),
        100,
    )
    .await
    .expect("live worker renews its session execution lease");
    assert_eq!(renewed.expires_at_epoch_ms, 1_110);

    let error = drain
        .await
        .expect("join durable queued drain")
        .expect_err("a live holder must end the wait with a typed error");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert!(error.is_retryable());
    assert!(!error.is_terminal());
    assert_eq!(
        error.message,
        "session execution lane for session `root` is held by owner `live-worker` \
         incarnation `live-worker:incarnation` executor `live-holder-executor` \
         (fencing generation 1, expires at 1110); stopped waiting after 25ms \
         because the holder renewed its lease"
    );

    let holder_after = crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read the holder row after the drain gave up")
    .lease
    .expect("the live holder still holds the lane");
    assert_eq!(holder_after, renewed);
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list pending input after the drain gave up")
        .len(),
        1,
        "a drain that gave up must leave the queued row pending"
    );
}

/// Cancellation cannot report an empty queue while a durable queued row is
/// still pending. It returns the same typed retryable lane signal so teardown
/// and redrive leave settlement to the engine.
#[tokio::test]
pub(super) async fn cancelling_a_durable_busy_lane_wait_keeps_the_queued_row_pending() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    runtime.host.core.clock = clock;
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued during cancellation",
    )
    .await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &lease_owner("cancelled-wait-holder"),
        "cancelled-wait-holder-executor",
        100,
    )
    .await
    .expect("claim cancellation test holder lease")
    .acquired()
    .expect("cancellation test holder owns the lane");

    let controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_controller_owned_replay()
            .with_engine_paced_lane(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-cancelled-wait"),
    )
    .expect("durable queued cancellation scope");
    let cancel = CancellationToken::new();
    let drain_cancel = cancel.clone();
    let drain = crate::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(drain_cancel, scope))
            .await
            .map(crate::facade_support::QueuedTurnDrain::ran)
    });
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    cancel.cancel();

    let error = drain
        .await
        .expect("join cancelled durable queued drain")
        .expect_err("cancellation while waiting must remain retryable");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert_eq!(
        error.message,
        "session execution lane for session `root` is held by owner `cancelled-wait-holder` \
         incarnation `cancelled-wait-holder:incarnation` executor \
         `cancelled-wait-holder-executor` (fencing generation 1, expires at 1100); \
         stopped waiting after 25ms because the queued drain was cancelled while waiting"
    );
    assert!(error.is_retryable());
    assert!(!error.is_terminal());
    assert_eq!(
        crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
            store.as_ref(),
            &SessionId::from("root"),
        )
        .await
        .expect("read holder after cancellation")
        .lease
        .expect("holder remains installed"),
        held_lease
    );
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list pending input after cancellation")
        .len(),
        1
    );
}

/// The backstop: a holder whose expiry never moves and never lapses (a frozen
/// clock) must not become an unbounded block. Waiting stops at twice the
/// observed TTL with the same typed retryable error.
#[tokio::test]
pub(super) async fn durable_controller_stops_waiting_for_a_busy_lane_at_the_wait_budget() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    runtime.host.core.clock = clock.clone();
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued behind a frozen holder",
    )
    .await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &lease_owner("frozen-worker"),
        "frozen-holder-executor",
        100,
    )
    .await
    .expect("claim frozen worker session execution lease")
    .acquired()
    .expect("frozen worker holds session execution lease");

    let controller = Arc::new(
        super::effect::RecordingEffectController::default()
            .with_controller_owned_replay()
            .with_engine_paced_lane(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-frozen-holder"),
    )
    .expect("durable queued-turn scope");
    let error = runtime
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
        .await
        .expect_err("the wait budget must end the drain with a typed error");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert!(error.is_retryable());
    assert_eq!(
        error.message,
        "session execution lane for session `root` is held by owner `frozen-worker` \
         incarnation `frozen-worker:incarnation` executor `frozen-holder-executor` \
         (fencing generation 1, expires at 1100); stopped waiting after 200ms \
         because the in-process wait budget elapsed"
    );

    let holder_after = crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read the holder row after the wait budget elapsed")
    .lease
    .expect("the frozen holder still holds the lane");
    assert_eq!(holder_after, held_lease);
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list pending input after the wait budget elapsed")
        .len(),
        1,
        "a drain that hit the wait budget must leave the queued row pending"
    );
}

/// The capability gate, not effect-replay ownership, is what selects the busy
/// wait. A controller that owns effect replay but is not a durable workflow
/// controller - every store-backed durable effect host - keeps the ordinary
/// one-shot `Busy -> None` drain contract.
#[tokio::test]
pub(super) async fn controller_owned_replay_alone_keeps_the_one_shot_busy_drain_contract() {
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(mock_provider(Vec::new())).await;
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued behind a replay-owning host",
    )
    .await;
    let held_lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &lease_owner("foreground-runtime"),
        "controller-owned-replay-alone-executor",
        60_000,
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    let controller = Arc::new(
        super::effect::RecordingEffectController::default().with_controller_owned_replay(),
    );
    let scope = crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn("root", "queued-replay-owner"),
    )
    .expect("controller-owned replay queued-turn scope");
    let busy_result = runtime
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
        .await
        .expect("a replay-owning non-workflow controller must not error on Busy")
        .ran();

    assert!(
        busy_result.is_none(),
        "controller-owned effect replay alone must keep the one-shot Busy no-op"
    );
    assert_eq!(
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queued turn input after the one-shot no-op")
        .len(),
        1
    );
    let holder_after = crate::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read the holder row after the one-shot no-op")
    .lease
    .expect("the holder still holds the lane");
    assert_eq!(holder_after.lease_token, held_lease.lease_token);
    assert_eq!(holder_after.fencing_token, held_lease.fencing_token);
}

#[tokio::test]
pub(super) async fn session_command_waits_in_durable_queue_until_session_lease_ttl_expires() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        mock_provider(Vec::new()),
        store_clock,
    )
    .await;
    let command = enqueue_session_command(
        store.as_ref(),
        &SessionId::from("root"),
        "wait for stale lease",
    )
    .await;
    let owner = lease_owner("stale-session-command-owner");
    crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &owner,
        "session-command-waits-in-durable-queue-until-session-lease-ttl-expires-executor",
        50,
    )
    .await
    .expect("claim stale session execution lease")
    .acquired()
    .expect("session execution lease");

    let busy_result = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("command-before-lease-ttl"),
            ),
        ))
        .await
        .expect("busy command drain should not error")
        .ran();

    assert!(busy_result.is_none());
    assert_eq!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("list command while lease is live")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![command.batch_id.as_str()],
        "the command must remain durable while another owner holds the live lease"
    );

    clock.advance_ms(51);
    let after_ttl = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("command-after-lease-ttl"),
            ),
        ))
        .await
        .expect("command drain after TTL should succeed")
        .ran();

    assert!(after_ttl.is_none(), "a command-only drain returns no turn");
    assert!(
        crate::store::QueuedWorkStore::list_queued_work(store.as_ref(), &SessionId::from("root"))
            .await
            .expect("list command after TTL drain")
            .is_empty(),
        "the durable command should drain after the stale lease expires"
    );
}

#[tokio::test]
pub(super) async fn session_command_claim_lease_expiry_surfaces_session_execution_lease_lost() {
    let clock = Arc::new(StepExpiryClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        runtime_store,
    )
    .await;
    let owner = lease_owner("session-command-drain-test");
    let lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &owner,
        "session-command-claim-lease-expiry-surfaces-session-execution-lease-lost-executor",
        crate::LeaseTimings::default().ttl_ms(),
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");
    clock.expire_after_timestamp_calls(0);

    let err = runtime
        .drain_next_session_command(&lease.fence())
        .await
        .expect_err("expired session command claim lease must fail as lease lost");

    assert_eq!(err.code, crate::RuntimeErrorCode::SessionExecutionLeaseLost);
}

#[tokio::test]
pub(super) async fn idle_queued_work_claim_lease_expiry_surfaces_session_execution_lease_lost() {
    let clock = Arc::new(StepExpiryClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        runtime_store,
    )
    .await;
    clock.expire_after_timestamp_calls(3);

    let err = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("idle-claim-lease-expiry-turn"),
            ),
        ))
        .await
        .expect_err("expired idle queued-work claim lease must fail as lease lost");

    assert_eq!(err.code, crate::RuntimeErrorCode::SessionExecutionLeaseLost);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
pub(super) async fn concurrent_real_turn_commits_record_product_admission_waits() {
    const SESSION_ID: &str = "concurrent-real-turn-admission";

    let session_id = SESSION_ID;
    let _ = crate::runtime::commit_admission::take_product_commit_admission_observations(
        &SessionId::from(session_id),
    );
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let build_runtime = |answer: &'static str| {
        let transport = mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: answer.to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]);
        let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
        let host_clock: Arc<dyn crate::Clock> = clock.clone();
        async move {
            TestRuntime::new(transport)
                .tools(Arc::new(EmptyTools))
                .host(crate::EmbeddedRuntimeHost::new(
                    crate::RuntimeHostConfig::in_memory(
                        crate::CommitBudget::bounded(1024 * 1024, 512),
                        crate::QueuedWorkBatchingConfig::new(1),
                    )
                    .with_clock(host_clock),
                ))
                .store(runtime_store)
                .with_session_id(SESSION_ID)
                .build()
                .await
        }
    };
    let mut first_runtime = build_runtime("first committed turn").await;
    let mut second_runtime = build_runtime("second stale turn").await;
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let probe = Arc::new(PauseFirstProductCommitAttempt {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        attempts: AtomicUsize::new(0),
    });
    first_runtime.set_turn_phase_probe(probe.clone());
    second_runtime.set_turn_phase_probe(probe);

    let first = crate::task::spawn(async move {
        first_runtime
            .run_turn_assembled(
                TurnInput::text("first concurrent commit"),
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from(session_id),
                    &TurnId::from("product-admission-first"),
                ),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first real turn entered the admitted product commit attempt");

    // Let a second same-process runtime take over the advisory execution lease
    // while the first remains current at the store head. This reaches two real
    // final-commit attempts without weakening the store CAS authority.
    clock.advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
    let second = crate::task::spawn(async move {
        second_runtime
            .run_turn_assembled(
                TurnInput::text("second concurrent commit"),
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from(session_id),
                    &TurnId::from("product-admission-second"),
                ),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while crate::runtime::commit_admission::process_commit_admission_queue_depth(
            &SessionId::from(session_id),
        ) == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second real turn queued behind product commit admission");
    release.store(true, Ordering::SeqCst);

    let first_result = first.await.expect("first product turn task");
    let second_result = second.await.expect("second product turn task");
    assert!(
        first_result.is_ok() || second_result.is_ok(),
        "one admitted real turn must advance the store head: first={first_result:?}, second={second_result:?}"
    );
    assert!(
        first_result.is_err() || second_result.is_err(),
        "the stale/superseded real turn must still be refused by durable authority"
    );

    let observations = crate::runtime::commit_admission::take_product_commit_admission_observations(
        &SessionId::from(session_id),
    );
    assert!(
        observations.iter().any(|observation| {
            observation.path == "turn_final_commit"
                && observation.work_identity == "product-admission-second"
                && observation.queue_depth > 0
                && !observation.waited.is_zero()
        }),
        "real runtime turn commits must record a nonzero product admission wait: {observations:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
pub(super) async fn committed_intent_survives_takeover_and_head_cas_loss_in_the_same_runtime_turn()
{
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                "cas-survivor-intent-target",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
            )
            .with_extra_event_types([crate::ProcessEventType {
                name: "intent.survivor.committed".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from("root")],
        )
        .await
        .expect("register same-turn CAS survivor target");
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(CasSurvivorIntentTools {
        calls: Arc::clone(&tool_calls),
    });
    let model_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            move |_| {
                let model_calls = Arc::clone(&model_calls);
                async move {
                    Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                        0 => LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "cas-survivor-call".to_string(),
                                tool_name: "cas_survivor_intent".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        },
                        1 => LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: "stale conversational tail".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        },
                        index => panic!("unexpected CAS survivor model call {index}"),
                    })
                }
            }
        })
        .build();
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    let mut runtime = TestRuntime::new(transport)
        .plugins(Vec::new())
        .tools(tools)
        .host(crate::EmbeddedRuntimeHost::new(config))
        .store(runtime_store)
        .process_registry(registry.clone())
        .build()
        .await;
    let effect_loop_ended = Arc::new(AtomicBool::new(false));
    let release_effect_loop = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAfterEffectLoop {
        entered: Arc::clone(&effect_loop_ended),
        release: Arc::clone(&release_effect_loop),
    }));
    let first = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("emit evidence before losing CAS"),
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("cas-survivor-stale-turn"),
                ),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !effect_loop_ended.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the intent-owning runtime turn reaches the pre-CAS boundary");
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        registry
            .events_after(&ProcessId::from("cas-survivor-intent-target"), 0)
            .await
            .expect("read committed pre-CAS intent")
            .iter()
            .filter(|event| event.event_type == "intent.survivor.committed")
            .count(),
        1,
        "the same runtime turn executes the intent before its head CAS"
    );

    clock.advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
    let successor_transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "successor wins".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let successor_store: Arc<dyn crate::RuntimePersistence> = store.clone();
    let successor_clock: Arc<dyn crate::Clock> = clock.clone();
    let successor_config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(successor_clock);
    let mut successor = TestRuntime::new(successor_transport)
        .plugins(Vec::new())
        .host(crate::EmbeddedRuntimeHost::new(successor_config))
        .store(successor_store)
        .process_registry(registry.clone())
        .build()
        .await;
    successor
        .run_turn_assembled(
            TurnInput::text("take over and win the head"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("cas-survivor-successor-turn"),
            ),
        )
        .await
        .expect("successor wins the shared store head CAS");
    release_effect_loop.store(true, Ordering::SeqCst);
    let error = first
        .await
        .expect("stale runtime task joins")
        .expect_err("the intent-owning stale conversational tail loses head CAS");
    assert_eq!(error.code, crate::RuntimeErrorCode::StoreCommitFailed);
    assert!(
        error.message.contains("head revision conflict"),
        "the same-turn loser must retain typed CAS diagnostics: {error:?}"
    );
    assert_eq!(
        registry
            .events_after(&ProcessId::from("cas-survivor-intent-target"), 0)
            .await
            .expect("read intent after CAS loss")
            .iter()
            .filter(|event| event.event_type == "intent.survivor.committed")
            .count(),
        1,
        "the intent survives the enclosing turn's failing CAS without duplication"
    );
}

// Regression (FIG-862): a foreground turn that has not observed takeover may
// still publish its current-head tail afterward.
// ManualClock advances store time only, while its sleep uses real Tokio time, so
// the default 10s renewal never fires in this millisecond-scale test. The
// observed-loss path is covered by
// `renewal_failure_mid_turn_does_not_select_a_durable_branch`.
#[tokio::test]
pub(super) async fn unobserved_lease_loss_does_not_stop_foreground_turn_before_final_commit() {
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn crate::Clock> = clock.clone();
    let store = Arc::new(RecordingStore::with_clock(store_clock));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let (provider_started_tx, provider_started_rx) = tokio::sync::oneshot::channel();
    let (provider_continue_tx, provider_continue_rx) = tokio::sync::oneshot::channel();
    let provider_started_tx = Arc::new(Mutex::new(Some(provider_started_tx)));
    let provider_continue_rx = Arc::new(Mutex::new(Some(provider_continue_rx)));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete({
            let provider_started_tx = Arc::clone(&provider_started_tx);
            let provider_continue_rx = Arc::clone(&provider_continue_rx);
            move |_request| {
                let provider_started_tx = Arc::clone(&provider_started_tx);
                let provider_continue_rx = Arc::clone(&provider_continue_rx);
                async move {
                    if let Some(tx) = provider_started_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    let rx = provider_continue_rx
                        .lock_recover()
                        .take()
                        .expect("provider continue receiver available");
                    let _ = rx.await;
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "committed under head CAS".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let host_clock: Arc<dyn crate::Clock> = clock.clone();
    let mut config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        transport.clone().into_handle(),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        crate::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;

    let turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("lease can be lost"),
                CancellationToken::new(),
                named_turn_scope(&SessionId::from("root"), &TurnId::from("lease-loss-turn")),
            )
            .await
    });
    provider_started_rx
        .await
        .expect("provider should start after session lease acquisition");

    clock.advance_ms(crate::LeaseTimings::default().ttl_ms() + 1);
    let successor_transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "successor continued from landed tail".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let successor_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let successor_host_clock: Arc<dyn crate::Clock> = clock.clone();
    let successor_config = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(successor_host_clock);
    let mut successor_runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        successor_transport,
        crate::EmbeddedRuntimeHost::new(successor_config),
        successor_store,
    )
    .await;
    let successor_owner = successor_runtime.runtime_lease_owner.clone();
    let stolen = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &successor_owner,
        &successor_runtime.runtime_lease_executor_id,
        60_000,
    )
    .await
    .expect("steal expired session execution lease")
    .acquired()
    .expect("expired session execution lease should be claimable");
    let commits_before_lease_loss = *store.runtime_commit_count.lock_recover();
    provider_continue_tx
        .send(())
        .expect("provider should still be waiting");

    let assembled = turn
        .await
        .expect("foreground turn task")
        .expect("unobserved lease loss must not reject the turn");
    assert_eq!(
        assembled.assistant_output.safe_text,
        "committed under head CAS"
    );
    assert!(
        *store.runtime_commit_count.lock_recover() > commits_before_lease_loss,
        "the current-head turn must checkpoint and commit despite advisory lease loss"
    );
    let still_owned =
        crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease_with_token(
            store.as_ref(),
            &SessionId::from("root"),
            &successor_owner,
            &stolen.executor_id,
            &crate::LeaseClaimNonce::for_testing("successor-reentry-token"),
            60_000,
        )
        .await
        .expect("reclaim successor lease with the same owner")
        .acquired()
        .expect("the predecessor commit must leave the successor lease live");
    assert_eq!(
        still_owned.fencing_token, stolen.fencing_token,
        "the predecessor's final commit must not release the successor lease"
    );

    let successor_turn = successor_runtime
        .run_turn_assembled(
            TurnInput::text("continue after predecessor tail"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("successor-after-landed-tail"),
            ),
        )
        .await
        .expect("the successor should continue from the newly committed head");
    assert_eq!(
        active_conversation_messages(&successor_turn.state)
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| part.content == "committed under head CAS")
            .count(),
        1,
        "the successor must reload exactly one predecessor tail that landed after takeover"
    );
    assert_eq!(
        successor_turn.assistant_output.safe_text,
        "successor continued from landed tail"
    );
    assert!(
        successor_turn.state.turn_index > assembled.state.turn_index,
        "the successor must advance from the predecessor's landed head"
    );
}
