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

// Boundary: execution-lease tests stay in `turns.rs` because they exercise live
// `LashRuntime` lease acquisition, public scheduling, turn phase probes, and
// provider suspension. Runtime Scenarios own persistence-level head-CAS and
// queue/input claim invariants; these tests own the facade scheduler response.
/// A foreground turn is refused while a foreign executor holds the session
/// lane, before provider work or durable input acceptance (ADR 0077).
#[tokio::test]
pub(super) async fn foreground_turn_is_refused_when_session_lane_is_held() {
    let backend = memory_backend().await;
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
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(&backend, transport).await;
    let owner = lease_owner("other-runtime");
    let held_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
            host_turn_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("foreground-busy-lane-turn"),
            ),
        )
        .await
        .expect_err("a foreign lease holder refuses the foreground turn");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    // Acceptance precedes the drive (FIG-3600): the input stays accepted and
    // pending for the session's next drive.
    assert_eq!(
        lash_core::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("read pending turn inputs after refusal")
        .len(),
        1,
        "the refused drive leaves the accepted input pending"
    );
    lash_core::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &held_lease.completion(),
    )
    .await
    .expect("release held session execution lease");
}

#[tokio::test]
pub(super) async fn idle_queued_work_noops_without_claiming_when_session_lane_is_held() {
    let backend = memory_backend().await;
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
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(&backend, transport).await;
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued while busy",
    )
    .await;
    let owner = lease_owner("foreground-runtime");
    let held_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
            host_queued_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("queued-busy-turn"),
            ),
        ))
        .await
        .expect("busy queued drain should not error")
        .ran();

    assert!(
        busy_result.is_none(),
        "idle queued drain must no-op while another owner holds the session lane"
    );
    assert_eq!(
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queued turn input while busy")
        .len(),
        1,
        "busy drain must not consume queued turn input"
    );

    lash_core::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &held_lease.completion(),
    )
    .await
    .expect("release held session execution lease");
    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            host_queued_scope(
                &runtime.host.core,
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
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
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
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        &backend,
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
    let held_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
    runtime.host.core.control.effect_host =
        super::effect::layered_effect_host(&backend, controller.clone());
    let scope = super::effect::layered_scope(
        &backend,
        controller,
        lash_core::AdmittedScope::queue_drain("root", "queued-failover-wake"),
    );
    let mut drain = lash_core::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
            .await
            .map(lash_core::facade_support::QueuedTurnDrain::ran)
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
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
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
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        &backend,
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
    let held_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
    let scope = super::effect::layered_scope(
        &backend,
        controller,
        lash_core::AdmittedScope::queue_drain("root", "queued-live-holder"),
    );
    let mut drain = lash_core::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
            .await
            .map(lash_core::facade_support::QueuedTurnDrain::ran)
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(5), &mut drain)
            .await
            .is_err(),
        "the drain must still be waiting when the holder renews"
    );

    clock.advance_ms(10);
    let renewed = lash_core::store::SessionExecutionLeaseStore::renew_session_execution_lease(
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
        lash_core::RuntimeErrorCode::SessionExecutionLaneBusy
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

    let holder_after = lash_core::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read the holder row after the drain gave up")
    .lease
    .expect("the live holder still holds the lane");
    assert_eq!(holder_after, renewed);
    assert_eq!(
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
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
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        &backend,
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
    let held_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
    let scope = super::effect::layered_scope(
        &backend,
        controller,
        lash_core::AdmittedScope::queue_drain("root", "queued-cancelled-wait"),
    );
    let cancel = CancellationToken::new();
    let drain_cancel = cancel.clone();
    let drain = lash_core::task::spawn(async move {
        runtime
            .stream_next_queued_work(TurnOptions::new(drain_cancel, scope))
            .await
            .map(lash_core::facade_support::QueuedTurnDrain::ran)
    });
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    cancel.cancel();

    let error = drain
        .await
        .expect("join cancelled durable queued drain")
        .expect_err("cancellation while waiting must remain retryable");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionExecutionLaneBusy
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
        lash_core::store::SessionExecutionLeaseStore::get_session_execution_lease(
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
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
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
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        &backend,
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
    let held_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
    let scope = super::effect::layered_scope(
        &backend,
        controller,
        lash_core::AdmittedScope::queue_drain("root", "queued-frozen-holder"),
    );
    let error = runtime
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
        .await
        .expect_err("the wait budget must end the drain with a typed error");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert!(error.is_retryable());
    assert_eq!(
        error.message,
        "session execution lane for session `root` is held by owner `frozen-worker` \
         incarnation `frozen-worker:incarnation` executor `frozen-holder-executor` \
         (fencing generation 1, expires at 1100); stopped waiting after 200ms \
         because the in-process wait budget elapsed"
    );

    let holder_after = lash_core::store::SessionExecutionLeaseStore::get_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("read the holder row after the wait budget elapsed")
    .lease
    .expect("the frozen holder still holds the lane");
    assert_eq!(holder_after, held_lease);
    assert_eq!(
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
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
    let backend = memory_backend().await;
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(&backend, mock_provider(Vec::new())).await;
    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "queued behind a replay-owning host",
    )
    .await;
    let held_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
    let scope = super::effect::layered_scope(
        &backend,
        controller,
        lash_core::AdmittedScope::queue_drain("root", "queued-replay-owner"),
    );
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
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queued turn input after the one-shot no-op")
        .len(),
        1
    );
    let holder_after = lash_core::store::SessionExecutionLeaseStore::get_session_execution_lease(
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
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_clock(
        &backend,
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
    lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
            host_queued_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("command-before-lease-ttl"),
            ),
        ))
        .await
        .expect("busy command drain should not error")
        .ran();

    assert!(busy_result.is_none());
    assert_eq!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
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
            host_queued_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("command-after-lease-ttl"),
            ),
        ))
        .await
        .expect("command drain after TTL should succeed")
        .ran();

    assert!(after_ttl.is_none(), "a command-only drain returns no turn");
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list command after TTL drain")
        .is_empty(),
        "the durable command should drain after the stale lease expires"
    );
}

#[tokio::test]
pub(super) async fn session_command_claim_lease_expiry_surfaces_session_execution_lease_lost() {
    let backend = memory_backend().await;
    let clock = Arc::new(StepExpiryClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    let owner = lease_owner("session-command-drain-test");
    let lease = lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &owner,
        "session-command-claim-lease-expiry-surfaces-session-execution-lease-lost-executor",
        lash_core::facade_support::LeaseTimings::default().ttl_ms(),
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

    assert_eq!(
        err.code,
        lash_core::RuntimeErrorCode::SessionExecutionLeaseLost
    );
}

#[tokio::test]
pub(super) async fn idle_queued_work_claim_lease_expiry_retains_pending_admission() {
    let backend = memory_backend().await;
    let clock = Arc::new(StepExpiryClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    clock.expire_after_timestamp_calls(3);

    let err = runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            host_queued_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("idle-claim-lease-expiry-turn"),
            ),
        ))
        .await
        .expect_err("expired idle queued-work claim leaves durable recovery pending");

    assert_eq!(err.code, lash_core::RuntimeErrorCode::QueuedRunPending);
    assert!(
        store
            .pending_queued_run(&SessionId::from("root"))
            .await
            .unwrap()
            .is_some(),
        "the replacement worker must resume the admitted run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
pub(super) async fn concurrent_real_turn_commits_record_product_admission_waits() {
    let backend = memory_backend().await;
    const SESSION_ID: &str = "concurrent-real-turn-admission";

    let session_id = SESSION_ID;
    let _ = lash_core::runtime::commit_admission::take_product_commit_admission_observations(
        &SessionId::from(session_id),
    );
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
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
        let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
        let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
        let backend = Arc::clone(&backend);
        async move {
            TestRuntime::new(&backend, transport)
                .tools(Arc::new(EmptyTools))
                .host(lash_core::facade_support::EmbeddedRuntimeHost::new(
                    lash_core::facade_support::RuntimeHostConfig::new(
                        std::sync::Arc::clone(&backend),
                        lash_core::CommitBudget::bounded(1024 * 1024, 512),
                        lash_core::QueuedWorkBatchingConfig::new(1),
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

    let first = lash_core::task::spawn(async move {
        first_runtime
            .run_turn_assembled(
                TurnInput::text("first concurrent commit"),
                CancellationToken::new(),
                host_turn_scope(
                    &first_runtime.host.core,
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
    clock.advance_ms(lash_core::facade_support::LeaseTimings::default().ttl_ms() + 1);
    let second = lash_core::task::spawn(async move {
        second_runtime
            .run_turn_assembled(
                TurnInput::text("second concurrent commit"),
                CancellationToken::new(),
                host_turn_scope(
                    &second_runtime.host.core,
                    &SessionId::from(session_id),
                    &TurnId::from("product-admission-second"),
                ),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while lash_core::runtime::commit_admission::process_commit_admission_queue_depth(
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
    // The session drive serializes the two turns (FIG-3600): the second is
    // admitted, sealed and claimed under the lease the first released, so it
    // commits on the first one's head instead of racing it to a refused CAS.
    assert!(
        first_result.is_ok() && second_result.is_ok(),
        "both admitted turns advance the head in turn: first={first_result:?}, second={second_result:?}"
    );

    let observations =
        lash_core::runtime::commit_admission::take_product_commit_admission_observations(
            &SessionId::from(session_id),
        );
    assert!(
        observations.iter().any(|observation| {
            // The second runtime's drive takes over the first turn's root
            // (its lease lapsed) and waits behind the first's commit.
            observation.path == "turn_final_commit"
                && observation.queue_depth > 0
                && !observation.waited.is_zero()
        }),
        "real runtime turn commits must record a nonzero product admission wait: {observations:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
pub(super) async fn committed_intent_survives_takeover_and_head_cas_loss_in_the_same_runtime_turn()
{
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let registry = backend.process_registry();
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                "cas-survivor-intent-target",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "intent.survivor.committed".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from("root")],
        )
        .await
        .expect("register same-turn CAS survivor target");
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(CasSurvivorIntentTools {
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
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let config = lash_core::facade_support::RuntimeHostConfig::new(
        std::sync::Arc::clone(&backend),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    let mut runtime = TestRuntime::new(&backend, transport)
        .plugins(Vec::new())
        .tools(tools)
        .host(lash_core::facade_support::EmbeddedRuntimeHost::new(config))
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
    let first = lash_core::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("emit evidence before losing CAS"),
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
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
            .full_event_window(&ProcessId::from("cas-survivor-intent-target"), 0)
            .await
            .expect("read committed pre-CAS intent")
            .iter()
            .filter(|event| event.event_type == "intent.survivor.committed")
            .count(),
        1,
        "the same runtime turn executes the intent before its head CAS"
    );

    clock.advance_ms(lash_core::facade_support::LeaseTimings::default().ttl_ms() + 1);
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
    let successor_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let successor_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let successor_config = lash_core::facade_support::RuntimeHostConfig::new(
        std::sync::Arc::clone(&backend),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(successor_clock);
    let mut successor = TestRuntime::new(&backend, successor_transport)
        .plugins(Vec::new())
        .host(lash_core::facade_support::EmbeddedRuntimeHost::new(
            successor_config,
        ))
        .store(successor_store)
        .process_registry(registry.clone())
        .build()
        .await;
    successor
        .run_turn_assembled(
            TurnInput::text("take over and win the head"),
            CancellationToken::new(),
            host_turn_scope(
                &successor.host.core,
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
    // The successor's drive admitted the stale turn's own root first and
    // committed it (FIG-3600), so the stale runtime's final commit of that
    // root is refused as different content under the same commit identity.
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::StoreCommitFailed,
        "{error:?}"
    );
    assert!(
        error
            .message
            .contains("retried with different commit content"),
        "the same-turn loser must retain typed CAS diagnostics: {error:?}"
    );
    assert_eq!(
        registry
            .full_event_window(&ProcessId::from("cas-survivor-intent-target"), 0)
            .await
            .expect("read intent after CAS loss")
            .iter()
            .filter(|event| event.event_type == "intent.survivor.committed")
            .count(),
        1,
        "the intent survives the enclosing turn's failing CAS without duplication"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
pub(super) async fn activated_successor_loses_head_cas_after_predecessor_publication_without_stranding_turn()
 {
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let mut predecessor_final = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(standard_test_policy())
    };
    append_message(
        &mut predecessor_final,
        Message {
            id: "activated-overlap-predecessor-final".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::text(
                "activated-overlap-predecessor-final.p0".to_string(),
                "predecessor publishes after successor activation".to_string(),
                None,
            )]
            .into(),
            origin: None,
        },
    );
    let predecessor_commit =
        lash_core::RuntimeCommit::persisted_state_for_test(&predecessor_final, &[]);

    let successor_transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "stale successor publication".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let successor_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let successor_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let successor_config = lash_core::facade_support::RuntimeHostConfig::new(
        std::sync::Arc::clone(&backend),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(successor_clock);
    let mut successor_runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        successor_transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(successor_config),
        successor_store,
    )
    .await;
    let successor_prepared = Arc::new(AtomicBool::new(false));
    let release_successor = Arc::new(AtomicBool::new(false));
    successor_runtime.set_turn_phase_probe(Arc::new(PauseAtPreparedTurn {
        entered: Arc::clone(&successor_prepared),
        release: Arc::clone(&release_successor),
    }));
    let successor = lash_core::task::spawn(async move {
        let result = successor_runtime
            .run_turn_assembled(
                TurnInput::text("activate before the predecessor publishes"),
                CancellationToken::new(),
                host_turn_scope(
                    &successor_runtime.host.core,
                    &SessionId::from("root"),
                    &TurnId::from("activated-overlap-successor"),
                ),
            )
            .await;
        (successor_runtime, result)
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !successor_prepared.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("successor completes activation recovery and holds its lease before commit");

    let commits_before_publication = *store.runtime_commit_count.lock_recover();
    lash_core::store::SessionCommitStore::commit_runtime_state(store.as_ref(), predecessor_commit)
        .await
        .expect("the predecessor final publishes under its current-head CAS");
    let commits_after_predecessor = *store.runtime_commit_count.lock_recover();
    assert_eq!(commits_after_predecessor, commits_before_publication + 1);

    release_successor.store(true, Ordering::SeqCst);
    let (successor_runtime, successor_result) =
        tokio::time::timeout(std::time::Duration::from_secs(5), successor)
            .await
            .expect("successor commit must resolve after the predecessor advances the head")
            .expect("successor task");
    let successor_error = successor_result
        .expect_err("activated successor must lose the head CAS it loaded before publication");
    assert_eq!(
        successor_error.code,
        lash_core::RuntimeErrorCode::StoreCommitSuperseded
    );
    assert!(
        successor_error.message.contains("head revision conflict"),
        "the authorization mismatch must retain typed HeadRevisionConflict diagnostics: {successor_error:?}"
    );
    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        commits_after_predecessor,
        "the losing successor must not publish a second head"
    );
    drop(successor_runtime);

    let pending_inputs = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from("root"),
        ),
    )
    .await
    .expect("durable input read must not remain blocked after the successor exits")
    .expect("read durable input after the successor loses the head CAS");
    assert_eq!(
        pending_inputs.len(),
        1,
        "only the rejected successor input remains unsettled"
    );
    let successor_input = &pending_inputs[0];
    assert_eq!(
        successor_input.input.state,
        lash_core::TurnInputState::DeferredNextTurn,
        "the CAS loser must return its input to the durable next-turn queue"
    );
    assert_eq!(
        successor_input.input.ingress(),
        lash_core::TurnInputIngress::NextTurn
    );
    assert!(
        successor_input.input.accepted_input().is_some(),
        "the CAS loser must retain canonical accepted-input evidence for redrive"
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
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
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
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        std::sync::Arc::clone(&backend),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;

    let turn = lash_core::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("lease can be lost"),
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
                    &SessionId::from("root"),
                    &TurnId::from("lease-loss-turn"),
                ),
            )
            .await
    });
    provider_started_rx
        .await
        .expect("provider should start after session lease acquisition");

    clock.advance_ms(lash_core::facade_support::LeaseTimings::default().ttl_ms() + 1);
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
    let successor_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let successor_host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let successor_config = lash_core::facade_support::RuntimeHostConfig::new(
        std::sync::Arc::clone(&backend),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(successor_host_clock);
    let mut successor_runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        successor_transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(successor_config),
        successor_store,
    )
    .await;
    let successor_owner = successor_runtime.runtime_lease_owner.clone();
    let stolen = lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
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
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease_with_token(
            store.as_ref(),
            &SessionId::from("root"),
            &successor_owner,
            &stolen.executor_id,
            &lash_core::LeaseClaimNonce::for_testing("successor-reentry-token"),
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
            host_turn_scope(
                &successor_runtime.host.core,
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
            .filter(|part| part.content() == "committed under head CAS")
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
