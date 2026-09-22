//! Per-attempt usage ledgering tests, extracted from `lease_and_claims.rs` —
//! the parent sits on the 2500-line test budget
//! `scripts/check-production-file-size.py` enforces. A real module rather
//! than an `include!`, so `cargo fmt` keeps walking it.

use super::*;

#[tokio::test]
pub(super) async fn failed_attempt_partial_usage_is_ledgered() {
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
                        stream.send(LlmStreamEvent::Usage(usage.clone()));
                        return Err(LlmTransportError::new("Stream ended without finish_reason")
                            .with_kind(lash_core::ProviderFailureKind::Stream)
                            .with_lash_code(TurnFailureCode::StreamEndedBeforeFinishReason)
                            .with_retry_verdict(
                                lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                            )
                            .with_partial_response(LlmResponse {
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
                        usage: LlmUsage {
                            input_tokens: 20,
                            output_tokens: 5,
                            ..LlmUsage::default()
                        },
                        provider_usage: Some(serde_json::json!({
                            "prompt_tokens": 20,
                            "completion_tokens": 5
                        })),
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let store = Arc::new(lash_core::facade_support::InMemorySessionStore::new());
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let mut runtime = TestRuntime::new(transport)
        .store(runtime_store)
        .without_process_registry()
        .build()
        .await;

    let assembled = runtime
        .stream_turn(
            TurnInput::text("retry a truncated stream"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("failed-attempt-usage-ledgered"),
                ),
            ),
        )
        .await
        .expect("retry succeeds");

    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(assembled.assistant_output.safe_text, "success");

    // The failed attempt's billed usage and the successful retry's usage are
    // two facts: the durable journal holds one delta each, and the report
    // sums both.
    let deltas = store.raw_usage_deltas_for_testing();
    assert_eq!(
        deltas.len(),
        2,
        "one delta per reported attempt: {deltas:?}"
    );
    assert_eq!(
        deltas
            .iter()
            .map(|entry| entry.usage.input_tokens)
            .sum::<i64>(),
        31
    );
    assert_eq!(
        deltas
            .iter()
            .map(|entry| entry.usage.output_tokens)
            .sum::<i64>(),
        7
    );
    let report = runtime.usage_report();
    assert_eq!(report.usage.usage.input_tokens, 31);
    assert_eq!(report.usage.usage.output_tokens, 7);
}

#[tokio::test]
pub(super) async fn all_attempts_failed_partial_usage_is_ledgered() {
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
                    let usage = LlmUsage {
                        input_tokens: 10 + attempt as i64,
                        output_tokens: 2,
                        ..LlmUsage::default()
                    };
                    request
                        .stream_events
                        .expect("stream events")
                        .send(LlmStreamEvent::Usage(usage.clone()));
                    Err(LlmTransportError::new("Stream ended without finish_reason")
                        .with_kind(lash_core::ProviderFailureKind::Stream)
                        .with_lash_code(TurnFailureCode::StreamEndedBeforeFinishReason)
                        .with_retry_verdict(
                            lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                        )
                        .with_partial_response(LlmResponse {
                            usage: usage.clone(),
                            provider_usage: Some(serde_json::json!({
                                "prompt_tokens": usage.input_tokens,
                                "completion_tokens": usage.output_tokens
                            })),
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        }))
                }
            }
        })
        .build();
    let store = Arc::new(lash_core::facade_support::InMemorySessionStore::new());
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let mut runtime = TestRuntime::new(transport)
        .store(runtime_store)
        .without_process_registry()
        .build()
        .await;

    let assembled = runtime
        .stream_turn(
            TurnInput::text("every attempt fails"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from("root"),
                    &TurnId::from("all-attempts-failed-usage-ledgered"),
                ),
            ),
        )
        .await
        .expect("provider failure is returned as an assembled turn");

    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(matches!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    assert_eq!(assembled.llm_calls.len(), 1);
    assert_eq!(assembled.llm_calls[0].attempts.len(), 2);

    // No response was ever counted into the turn's cumulative usage, so each
    // failed attempt's reported partial usage lands as its own delta.
    let deltas = store.raw_usage_deltas_for_testing();
    assert_eq!(
        deltas.len(),
        2,
        "one delta per reported attempt: {deltas:?}"
    );
    assert_eq!(
        deltas
            .iter()
            .map(|entry| entry.usage.input_tokens)
            .sum::<i64>(),
        21
    );
    let report = runtime.usage_report();
    assert_eq!(report.usage.usage.input_tokens, 21);
    assert_eq!(report.usage.usage.output_tokens, 4);
}
