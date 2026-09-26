use super::*;
use lash_core::ProcessEventLogTestSupport as _;

#[path = "lease_and_claims/acceptance_window.rs"]
mod acceptance_window;
#[path = "lease_and_claims/attempt_usage.rs"]
mod attempt_usage;
use acceptance_window::{AcceptanceWindowJournalController, LATE_TAB_INPUT};

/// Run one turn whose model call is `complete`, over `controller`'s
/// cancellation-gate watch, and return the turn's result.
async fn run_turn_over_cancel_watch<F, Fut>(
    controller: Arc<super::effect::RecordingEffectController>,
    turn_id: &str,
    complete: F,
) -> Result<AssembledTurn, lash_core::RuntimeError>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<LlmResponse, LlmTransportError>> + Send + 'static,
{
    let backend = memory_backend().await;
    let transport = TestProvider::builder()
        .kind("mock")
        .complete(move |_request| complete())
        .build();
    let clock = Arc::new(CancelWatchTestClock(lash_core::testing::TestClock::new(0)));
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let config = super::effect::runtime_host_config_with_effect_layer(&backend, controller)
        .with_clock(host_clock);
    let host = EmbeddedRuntimeHost::new(config);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(CountingEchoTool {
            executions: Arc::new(AtomicUsize::new(0)),
        }),
        transport,
        host,
    )
    .await;
    let turn = runtime.stream_turn(
        TurnInput::text("run a model call over a failing cancellation watch"),
        TurnOptions::new(
            CancellationToken::new(),
            backend_turn_scope(&backend, &SessionId::from("root"), &TurnId::from(turn_id)),
        ),
    );
    tokio::time::timeout(std::time::Duration::from_secs(20), turn)
        .await
        .expect("the turn settles or aborts")
}

/// F1 (FIG-3672 P9): a transient fault watching the turn's cancellation gate
/// during a model call is retried; it never stops the call, so the turn
/// completes as it would have without the fault.
#[tokio::test]
pub(super) async fn a_transient_cancel_watch_fault_never_cancels_the_model_call() {
    let controller = Arc::new(
        super::effect::RecordingEffectController::default().with_transient_cancel_watch_failures(3),
    );
    let watched = Arc::clone(&controller);
    let turn = Box::pin(run_turn_over_cancel_watch(
        Arc::clone(&controller),
        "transient-watch",
        move || {
            let watched = Arc::clone(&watched);
            async move {
                // The call outlives the faults: it ends only after the watch has
                // failed three times and retried past them.
                while watched.cancel_watch_attempts() < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "completed through the watch faults".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        },
    ))
    .await
    .expect("the turn completes");
    assert_eq!(controller.cancel_watch_attempts(), 3);
    assert!(
        matches!(turn.outcome, TurnOutcome::Finished(_)),
        "a watch fault must never become a cancellation: {:?}",
        turn.outcome
    );
}

/// F1 (FIG-3672 P9): a watch that keeps failing ends the attempt with the
/// typed live fault the engine never records — never a `Cancelled` turn and
/// never provider-cancelled evidence.
#[tokio::test]
pub(super) async fn an_exhausted_cancel_watch_fails_the_attempt_closed_not_cancelled() {
    let controller = Arc::new(
        super::effect::RecordingEffectController::default().with_always_failing_cancel_watch(),
    );
    controller.release_cancel_watch_failures();
    let result = Box::pin(run_turn_over_cancel_watch(
        Arc::clone(&controller),
        "exhausted-watch",
        std::future::pending,
    ))
    .await;
    assert_eq!(
        controller.cancel_watch_attempts(),
        8,
        "the watch rides the whole retry ladder (8 attempts) before giving up"
    );
    match result {
        Err(error) => assert_eq!(
            error.code,
            lash_core::RuntimeErrorCode::TransientCancelWatch
        ),
        Ok(turn) => panic!(
            "an exhausted watch must abort the attempt, not settle: {:?}",
            turn.outcome
        ),
    }
}

#[tokio::test]
pub(super) async fn cancelled_provider_stream_does_not_commit_partial_output() {
    let backend = memory_backend().await;
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
                    stream.send(LlmStreamEvent::Delta {
                        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                        text: "partial provider text".to_string(),
                    });
                    if let Some(tx) = delta_sent_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;
    let cancel = CancellationToken::new();
    let turn_cancel = cancel.clone();
    let turn_events = RecordingTurnEvents::default();
    let turn_events_for_task = turn_events.clone();
    let turn = lash_core::task::spawn(async move {
        runtime
            .stream_turn(
                TurnInput::text("cancel after partial stream"),
                TurnOptions::new(
                    turn_cancel,
                    host_turn_scope(
                        &runtime.host.core,
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
            TurnEvent::AssistantProseDelta { text, .. } if text.as_ref() == "partial provider text"
        )),
        "partial provider text should remain observable only as live turn activity"
    );
    assert!(
        active_conversation_messages(&assembled.state)
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content().contains("partial provider text")),
        "cancelled streamed partial must not be committed to read-view history"
    );
}

#[tokio::test]
pub(super) async fn truncated_retry_resets_partial_tool_calls_and_retains_failed_attempt_usage() {
    let backend = memory_backend().await;
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
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
                            .with_kind(lash_core::ProviderFailureKind::Stream)
                            .with_lash_code(TurnFailureCode::StreamEndedBeforeFinishReason)
                            .with_retry_verdict(
                                lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
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

                    stream.send(LlmStreamEvent::Delta { block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0), text: "success".to_string() });
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry a truncated stream"),
            TurnOptions::new(
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
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
            .all(|part| !part.content().contains("must_not_run"))
    );
    let failed_attempt = &assembled.llm_calls[0].attempts[0];
    assert_eq!(
        failed_attempt.outcome,
        lash_core::AttemptOutcome::Interrupted
    );
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
    let backend = memory_backend().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        return Err(LlmTransportError::new("connection failed before response")
                            .with_kind(lash_core::ProviderFailureKind::Transport)
                            .with_retry_verdict(
                                lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                            ));
                    }

                    request
                        .stream_events
                        .expect("stream events")
                        .send(LlmStreamEvent::Delta { block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0), text: "success".to_string() });
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;
    let turn_events = RecordingTurnEvents::default();

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry a pre-response transport failure"),
            TurnOptions::new(
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
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
    assert_eq!(
        resets[0].0,
        &Vec::<lash_core::runtime::TurnActivityId>::new()
    );
    assert_eq!(
        resets[0].1,
        &Vec::<lash_core::runtime::TurnActivityId>::new()
    );
}

#[tokio::test(start_paused = true)]
pub(super) async fn courtesy_retry_after_regeneration_emits_one_host_visible_attempt_reset() {
    let backend = memory_backend().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(1)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        return Err(LlmTransportError::new("provider requested a retry delay")
                            .with_http_status(429)
                            .with_retry_verdict(
                                lash_core::llm::transport::TransportRetryVerdict::RetryableThrottle {
                                    retry_after: Some(std::time::Duration::from_secs(1)),
                                },
                            ));
                    }

                    request
                        .stream_events
                        .expect("stream events")
                        .send(LlmStreamEvent::Delta { block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0), text: "success".to_string() });
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;
    let turn_events = RecordingTurnEvents::default();

    let assembled = runtime
        .stream_turn(
            TurnInput::text("defer to a provider retry-after"),
            TurnOptions::new(
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
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
    let backend = memory_backend().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let lost_text = std::iter::repeat_n("discarded", 256)
        .collect::<Vec<_>>()
        .join(" ");
    let transport = TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
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
                        stream.send(LlmStreamEvent::Delta {
                            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                            text: lost_text.clone(),
                        });
                        let usage = LlmUsage {
                            input_tokens: 32,
                            output_tokens: 256,
                            ..LlmUsage::default()
                        };
                        stream.send(LlmStreamEvent::Usage(usage.clone()));
                        return Err(LlmTransportError::new(
                            "stream ended before terminal evidence",
                        )
                        .with_kind(lash_core::ProviderFailureKind::Stream)
                        .with_lash_code(TurnFailureCode::StreamEndedBeforeTerminalResponse)
                        .with_retry_verdict(
                            lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
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

                    stream.send(LlmStreamEvent::Delta {
                        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                        text: "replacement".to_string(),
                    });
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "replacement".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let store = unbound_recording_store(&backend).await;
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let mut runtime = TestRuntime::new(&backend, transport)
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
                host_turn_scope(
                    &runtime.host.core,
                    &SessionId::from("root"),
                    &TurnId::from("paid-output-retry"),
                ),
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
        TurnEvent::AssistantProseDelta { text, .. } if text.as_ref() == lost_text
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
            .all(|part| !part.content().contains("discarded")),
        "a failed partial response remains preview output, not committed history"
    );
    let calls = &assembled.llm_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].attempts.len(), 1);
    let preserved_attempt = &calls[0].attempts[0];
    assert_eq!(
        preserved_attempt.protocol_position,
        lash_core::ProtocolPosition::OutputStarted
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
        issue.code,
        Some(lash_core::TurnFailureCode::UnsafeRetryAfterOutputStarted.into())
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
        lash_core::ChargeSafetyDenialReason::GuaranteeRequired
    );
    assert_eq!(
        failure.refusal.protocol_position,
        lash_core::ProtocolPosition::OutputStarted
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
                host_turn_scope(
                    &runtime.host.core,
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
    let reopened = lash_core::store::load_persisted_session_read_view(store.as_ref())
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
            .all(|part| !part.content().contains("discarded")),
        "durable failure evidence remains outside model context"
    );
}

fn single_answer_provider(text: &str) -> TestProvider {
    mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }])
}

/// FIG-3078: a `next_turn` input admitted after a turn's journaled acceptance
/// never joins that turn's message block, so the replacement worker replays the
/// identical block instead of aborting, and the late input is delivered exactly
/// once by the next turn.
#[tokio::test]
pub(super) async fn a_next_turn_input_admitted_after_the_acceptance_waits_for_the_next_turn() {
    let backend = memory_backend().await;
    let turn_id = &TurnId::from("claim-window-worker-replacement");
    let store = unbound_recording_store(&backend).await;
    let controller = Arc::new(AcceptanceWindowJournalController::new(Arc::clone(&store)));
    let shared: Arc<dyn lash_core::testing::EffectLayer> = controller.clone();
    let input = TurnInput::text("first tab input");

    let mut first_worker = Box::pin(runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        single_answer_provider("journaled answer"),
        journal_replay_host(&backend, Arc::clone(&shared)),
        Arc::clone(&store) as Arc<dyn lash_core::RuntimePersistence>,
    ))
    .await;
    let first_error = first_worker
        .stream_turn(
            input.clone(),
            TurnOptions::new(
                CancellationToken::new(),
                super::effect::layered_scope(
                    &backend,
                    Arc::clone(&shared),
                    lash_core::AdmittedScope::turn("root", turn_id),
                ),
            ),
        )
        .await
        .expect_err("the first worker is replaced after it journals the message block");
    assert!(
        first_error.code == lash_core::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
        "the staged failure must be the worker replacement itself: {first_error:?}"
    );
    let late_input_id = controller.late_input_id();
    drop(first_worker);

    let mut replacement = Box::pin(runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        journal_replay_host(&backend, Arc::clone(&shared)),
        Arc::clone(&store) as Arc<dyn lash_core::RuntimePersistence>,
    ))
    .await;
    let replayed = Box::pin(replacement.stream_turn(
        input,
        TurnOptions::new(
            CancellationToken::new(),
            super::effect::layered_scope(
                &backend,
                Arc::clone(&shared),
                lash_core::AdmittedScope::turn("root", turn_id),
            ),
        ),
    ))
    .await
    .expect("the replacement must replay the journaled message block, not a re-claimed one");
    let acceptance = replayed
        .turn_input_acceptance
        .expect("the replayed direct turn exposes its journaled acceptance");
    assert_ne!(acceptance.input_id, late_input_id);

    let after_replacement = lash_core::store::TurnInputStore::list_turn_input_applications(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read applications after the replacement committed");
    assert_eq!(
        after_replacement
            .iter()
            .map(|application| application.input_id.clone())
            .collect::<Vec<_>>(),
        vec![acceptance.input_id.clone()],
        "the replacement turn must apply only the row its journaled acceptance admitted"
    );

    controller.retire_journal();
    let mut next_turn_worker = Box::pin(runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        single_answer_provider("answer for the second tab"),
        journal_replay_host(&backend, Arc::clone(&shared)),
        Arc::clone(&store) as Arc<dyn lash_core::RuntimePersistence>,
    ))
    .await;
    let drained = Box::pin(next_turn_worker.stream_next_queued_work(TurnOptions::new(
        CancellationToken::new(),
        super::effect::layered_scope(
            &backend,
            Arc::clone(&shared),
            lash_core::AdmittedScope::queue_drain("root", "claim-window-late-input-drain"),
        ),
    )))
    .await
    .expect("the deferred second-tab input must drain on the next turn")
    .ran();
    assert!(
        drained.is_some(),
        "the late input must be claimable by the next turn, not stranded"
    );

    let settled = lash_core::store::TurnInputStore::list_turn_input_applications(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read applications after the next turn");
    let delivered = settled
        .iter()
        .map(|application| application.input_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        delivered,
        vec![acceptance.input_id.clone(), late_input_id.clone()],
        "each admitted input is delivered exactly once, in admission order"
    );
    let late_application = settled
        .iter()
        .find(|application| application.input_id == late_input_id)
        .expect("the late input is applied by some turn");
    assert_ne!(
        &late_application.turn_id, turn_id,
        "the late input must ride the next turn, never the block the replaced worker journaled"
    );
}
