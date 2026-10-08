//! Provider attempt laws on the durable engine: a facade core over SQLite
//! memory stores runs each turn on its node, and the law reads the turn's
//! call ledger, its activity and what it committed.

use super::*;
use lash_core::llm::transport::{LlmTransportError, TransportRetryVerdict};
use lash_core::llm::types::{LlmUsage, StreamBlockIdentity};
use lash_core::{MessageRole, TurnEvent};

/// Every committed part's content of `output`'s session.
fn committed_contents(output: &crate::TurnOutput) -> Vec<String> {
    output
        .result
        .state
        .read_view()
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .map(|part| part.content().to_string())
        .collect()
}

/// `options` with `attempts` attempts and no backoff.
fn reliability(attempts: u32) -> lash_core::facade_support::ProviderOptions {
    lash_core::facade_support::ProviderOptions {
        reliability: lash_core::provider::ProviderReliability::default()
            .max_attempts(Some(attempts))
            .base_delay_ms(0)
            .max_delay_ms(0),
        ..lash_core::facade_support::ProviderOptions::default()
    }
}

fn core_over(backend: lash_core::Backend, model: ProviderHandle) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(model, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core")
}

async fn created(core: &LashCore, id: &'static str) -> crate::DurableSession {
    core.session(crate::SessionId::from(id))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("created")
}

/// A turn cancelled while its model streams commits none of the streamed
/// text: the partial is observable only as live activity, and the requested
/// cancellation is no model error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_provider_stream_does_not_commit_partial_output() {
    const PARTIAL: &str = "partial provider text";
    let model = crate::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request: LlmRequest| async move {
            let stream = request
                .stream_events
                .expect("a streaming turn requests provider stream events");
            stream.send(LlmStreamEvent::Delta {
                block: StreamBlockIdentity::new("text:0", 0),
                text: PARTIAL.to_string(),
            });
            std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
        })
        .build()
        .into_handle();
    let core = core_over(sqlite_memory_store_backend().await, model);
    let session = created(&core, "cancel-partial-provider-stream").await;
    let held = session
        .send(crate::TurnInput::text("cancel after partial stream"))
        .await
        .expect("accepted");
    // Enqueuing a provider delta does not mean the runtime forwarded it:
    // cancellation wins over queued stream events. Observe the partial on
    // the host's lane before cancelling the still-pending completion.
    let mut events = held.events();
    loop {
        let activity = events
            .next_activity()
            .await
            .expect("the provider stays pending until its partial is observed")
            .expect("the live activity reads");
        if matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text, .. } if text.as_ref() == PARTIAL
        ) {
            break;
        }
    }
    held.cancel().await.expect("cancel");
    while let Some(activity) = events.next_activity().await {
        activity.expect("the cancelled turn's live activity reads");
    }
    let output = held.output().await.expect("the cancelled turn settles");
    assert_eq!(output.status(), crate::TurnStatus::Cancelled);
    assert!(
        output.activities.iter().all(|activity| !matches!(
            &activity.event,
            TurnEvent::Error { message } if message == "LLM error: cancelled"
        )),
        "a requested cancellation emits no user-visible model error"
    );
    assert!(
        output.activities.iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text, .. } if text.as_ref() == PARTIAL
        )),
        "the partial text stays observable as live activity"
    );
    assert!(
        committed_contents(&output)
            .iter()
            .all(|content| !content.contains(PARTIAL)),
        "a cancelled streamed partial is never committed"
    );
    core.shutdown().await.expect("shutdown");
}

/// A stream truncated after a partial tool call is retried: the partial call
/// never runs and is never committed, the retry's answer is the turn's, and
/// the failed attempt keeps its interruption and its billed usage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_retry_resets_partial_tool_calls_and_retains_failed_attempt_usage() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let partial_call = || LlmOutputPart::ToolCall {
        call_id: "partial-call".to_string(),
        tool_name: "must_not_run".to_string(),
        input_json: "{\"unfinished\":".to_string(),
        replay: None,
    };
    let model = crate::testing::TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
        .options(reliability(2))
        .complete({
            let attempts = Arc::clone(&attempts);
            move |request: LlmRequest| {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    let stream = request.stream_events.expect("stream events");
                    if attempt == 0 {
                        let usage = LlmUsage {
                            input_tokens: 11,
                            output_tokens: 2,
                            ..LlmUsage::default()
                        };
                        stream.send(LlmStreamEvent::Part(partial_call()));
                        stream.send(LlmStreamEvent::Usage(usage.clone()));
                        return Err(LlmTransportError::new("Stream ended without finish_reason")
                            .with_kind(lash_core::ProviderFailureKind::Stream)
                            .with_lash_code(
                                lash_core::TurnFailureCode::StreamEndedBeforeFinishReason,
                            )
                            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                            .with_partial_response(LlmResponse {
                                parts: vec![partial_call()],
                                usage,
                                provider_usage: Some(serde_json::json!({
                                    "prompt_tokens": 11,
                                    "completion_tokens": 2
                                })),
                                ..LlmResponse::default()
                            }));
                    }
                    stream.send(LlmStreamEvent::Delta {
                        block: StreamBlockIdentity::new("text:0", 0),
                        text: "success".to_string(),
                    });
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let core = core_over(sqlite_memory_store_backend().await, model);
    let session = created(&core, "truncated-stream-retry").await;
    let output = session
        .send(crate::TurnInput::text("retry a truncated stream"))
        .output()
        .await
        .expect("the retry answers");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(output.assistant_message(), Some("success"));
    assert!(output.result.tool_calls.is_empty());
    assert!(
        committed_contents(&output)
            .iter()
            .all(|content| !content.contains("must_not_run"))
    );
    let failed = &output.result.llm_calls[0].attempts[0];
    assert_eq!(failed.outcome, lash_core::AttemptOutcome::Interrupted);
    assert_eq!(
        failed.usage.as_ref().map(|usage| usage.input_tokens),
        Some(11)
    );
    core.shutdown().await.expect("shutdown");
}

/// A provider's retry-after on a throttled first attempt is honoured as a
/// courtesy retry that spends no retry budget, and the host sees exactly one
/// attempt reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn courtesy_retry_after_regeneration_emits_one_host_visible_attempt_reset() {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(reliability(1))
        .complete({
            let calls = Arc::clone(&calls);
            move |request: LlmRequest| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        return Err(LlmTransportError::new("provider requested a retry delay")
                            .with_http_status(429)
                            .with_retry_verdict(TransportRetryVerdict::RetryableThrottle {
                                retry_after: Some(std::time::Duration::from_secs(1)),
                            }));
                    }
                    request
                        .stream_events
                        .expect("stream events")
                        .send(LlmStreamEvent::Delta {
                            block: StreamBlockIdentity::new("text:0", 0),
                            text: "success".to_string(),
                        });
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "success".to_string(),
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let core = core_over(sqlite_memory_store_backend().await, model);
    let session = created(&core, "courtesy-regeneration-reset").await;
    let output = session
        .send(crate::TurnInput::text("defer to a provider retry-after"))
        .output()
        .await
        .expect("the courtesy retry answers");
    assert_eq!(output.assistant_message(), Some("success"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let attempts = &output.result.llm_calls[0].attempts;
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.retry_budget_consumed)
            .count(),
        1
    );
    assert_eq!(
        output
            .activities
            .iter()
            .filter(|activity| matches!(activity.event, TurnEvent::ModelAttemptReset { .. }))
            .count(),
        1
    );
    core.shutdown().await.expect("shutdown");
}

/// A retryable failure after paid output started is not regenerated without
/// the provider's guarantee: the turn fails on its one attempt, its partial
/// stays live activity outside committed history and outside the next
/// turn's request, and the attempt's charge-safety refusal is recorded with
/// its billed usage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retryable_mid_stream_failure_preserves_durable_charge_safety_evidence() {
    let lost_text = std::iter::repeat_n("discarded", 256)
        .collect::<Vec<_>>()
        .join(" ");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let model = crate::testing::TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(reliability(2))
        .complete({
            let requests = Arc::clone(&requests);
            let lost_text = lost_text.clone();
            move |request: LlmRequest| {
                let call = {
                    let mut seen = requests.lock_recover();
                    seen.push(request.messages.clone());
                    seen.len()
                };
                let lost_text = lost_text.clone();
                async move {
                    let stream = request.stream_events.expect("stream events");
                    if call == 1 {
                        stream.send(LlmStreamEvent::Delta {
                            block: StreamBlockIdentity::new("text:0", 0),
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
                        .with_lash_code(
                            lash_core::TurnFailureCode::StreamEndedBeforeTerminalResponse,
                        )
                        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
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
                            ..LlmResponse::default()
                        }));
                    }
                    Ok(text_response("follow-up answer"))
                }
            }
        })
        .build()
        .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let core = core_over(backend.clone(), model);
    let session = created(&core, "paid-output-retry").await;
    let output = session
        .send(crate::TurnInput::text("retry after paid output"))
        .output()
        .await
        .expect("the failed turn settles");
    assert_eq!(requests.lock_recover().len(), 1, "no regeneration");
    assert!(matches!(
        output.result.outcome,
        crate::TurnOutcome::Stopped(crate::TurnStop::ProviderError)
    ));
    assert!(output.activities.iter().any(|activity| matches!(
        &activity.event,
        TurnEvent::AssistantProseDelta { text, .. } if text.as_ref() == lost_text
    )));
    assert!(
        output
            .activities
            .iter()
            .all(|activity| !matches!(activity.event, TurnEvent::ModelAttemptReset { .. }))
    );
    assert!(
        committed_contents(&output)
            .iter()
            .all(|content| !content.contains("discarded")),
        "a failed partial response is preview output, not committed history"
    );
    let [call] = output.result.llm_calls.as_slice() else {
        panic!("one model call: {:?}", output.result.llm_calls);
    };
    let [attempt] = call.attempts.as_slice() else {
        panic!("one attempt: {:?}", call.attempts);
    };
    assert_eq!(
        attempt.protocol_position,
        lash_core::ProtocolPosition::OutputStarted
    );
    assert_eq!(
        attempt.usage.as_ref().map(|usage| usage.output_tokens),
        Some(256)
    );
    let decision = attempt.retry_decision.as_ref().expect("a retry decision");
    assert!(!decision.is_scheduled());
    assert_eq!(
        decision.denial_reason(),
        Some(lash_core::ChargeSafetyDenialReason::GuaranteeRequired)
    );

    let follow_up = session
        .send(crate::TurnInput::text(
            "follow up after the failed generation",
        ))
        .output()
        .await
        .expect("a later turn answers");
    assert!(follow_up.is_success(), "{follow_up:?}");
    {
        let requests = requests.lock_recover();
        assert_eq!(requests.len(), 2);
        assert!(
            !serde_json::to_string(&requests[1])
                .expect("serialize the follow-up request")
                .contains("discarded"),
            "the next request has no path from the failed turn's evidence"
        );
    }

    let session_id = crate::SessionId::from("paid-output-retry");
    let view = lash_core::runtime::live_session_view(&backend.session_store_factory(), &session_id)
        .await
        .expect("the catalog answers")
        .expect("the session is live");
    let evidence = view
        .load_failure_evidence_page(None, std::num::NonZeroU32::new(10).expect("nonzero"))
        .await
        .expect("page the durable failure evidence");
    assert!(evidence.next.is_none());
    let [settlement] = evidence.settlements.as_slice() else {
        panic!("one failure-bearing turn: {:?}", evidence.settlements);
    };
    let [failure] = settlement.evidence.as_slice() else {
        panic!("one sealed attempt's evidence: {:?}", settlement.evidence);
    };
    assert_eq!(
        failure
            .partial_output
            .as_ref()
            .map(|partial| partial.text()),
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
    assert_eq!(
        (
            failure.refusal.attempt_number,
            failure.refusal.attempt_count
        ),
        (1, 1)
    );
    assert!(
        follow_up
            .result
            .state
            .read_view()
            .messages()
            .iter()
            .filter(|message| message.role != MessageRole::System)
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content().contains("discarded")),
        "durable failure evidence stays outside model context"
    );
    core.shutdown().await.expect("shutdown");
}

/// An authentication failure stops its turn as a classified provider
/// failure on its one attempt, never as a cancellation, and the session's
/// next turn answers under its own input identity (ported by FIG-5308 from
/// the deleted upgrade harness's s27 law).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_failure_stops_unretried_and_permits_the_next_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("openai-compatible")
        .requires_streaming(true)
        .options(reliability(3))
        .complete({
            let calls = Arc::clone(&calls);
            move |_request: LlmRequest| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        return Err(
                            LlmTransportError::new("invalid credentials").with_http_status(401)
                        );
                    }
                    Ok(text_response("recovered"))
                }
            }
        })
        .build()
        .into_handle();
    let core = core_over(sqlite_memory_store_backend().await, model);
    let session = created(&core, "authentication-failure").await;
    let first = session
        .send(crate::TurnInput::text("invalid credentials"))
        .id(crate::TurnId::parse("auth-turn-0").expect("a turn id"))
        .output()
        .await
        .expect("the failed turn settles");
    assert!(
        matches!(
            first.result.outcome,
            crate::TurnOutcome::Stopped(crate::TurnStop::ProviderError)
        ),
        "a 401 stops as a provider failure: {:?}",
        first.result.outcome
    );
    assert!(
        first.result.outcome.cancellation().is_none(),
        "a 401 is no cancellation"
    );
    let [call] = first.result.llm_calls.as_slice() else {
        panic!("one model call: {:?}", first.result.llm_calls);
    };
    let [attempt] = call.attempts.as_slice() else {
        panic!(
            "an authentication failure is not retried: {:?}",
            call.attempts
        );
    };
    // A durable send's report is rebuilt from the store (D1 §1.5 3b): the
    // classification is read from the sealed call ledger.
    assert_eq!(
        attempt.error.as_ref().map(|error| error.class),
        Some(lash_core::ProviderFailureKind::Auth),
        "the failure keeps its authentication classification"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let second = session
        .send(crate::TurnInput::text("fresh valid request"))
        .id(crate::TurnId::parse("auth-turn-1").expect("a turn id"))
        .output()
        .await
        .expect("the next turn answers");
    assert_eq!(second.assistant_message(), Some("recovered"));
    let first_input = first
        .result
        .acceptance
        .as_ref()
        .map(|receipt| &receipt.input_id);
    assert!(
        first_input.is_some(),
        "the failed turn's input was accepted"
    );
    assert_ne!(
        first_input,
        second
            .result
            .acceptance
            .as_ref()
            .map(|receipt| &receipt.input_id),
        "the next turn does not inherit the failed turn's input identity"
    );
    core.shutdown().await.expect("shutdown");
}

/// D-DEFAULTS2: facade-configured courtesy count and minimum wait reach the retry owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_courtesy_policy_controls_short_waits_and_call_cap() {
    for (cap, minimum, expected) in [(0, 1, 1), (1, 1, 2), (2, 1, 3), (2, 2, 1)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut options = reliability(1);
        let mut retry = serde_json::to_value(&options.reliability.retry).unwrap();
        retry["courtesy_call_limit"] = serde_json::json!(cap);
        retry["courtesy_min_wait_ms"] = serde_json::json!(minimum);
        options.reliability.retry = serde_json::from_value(retry).unwrap();
        let model = crate::testing::TestProvider::builder()
            .kind("openai-compatible")
            .options(options)
            .complete({
                let calls = Arc::clone(&calls);
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async {
                        Err(LlmTransportError::new("throttled")
                            .with_http_status(429)
                            .with_retry_verdict(TransportRetryVerdict::RetryableThrottle {
                                retry_after: Some(std::time::Duration::from_millis(1)),
                            }))
                    }
                }
            })
            .build()
            .into_handle();
        let core = core_over(sqlite_memory_store_backend().await, model);
        let session = created(&core, "facade-courtesy-policy").await;
        let _output = session
            .send(crate::TurnInput::text("throttle"))
            .output()
            .await
            .unwrap();
        core.shutdown().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), expected);
    }
}

#[cfg(all(feature = "openai", feature = "http-transport"))]
mod facade_settings {
    use crate::http_transport::{HttpRequest, HttpResponse, HttpResponseBody, HttpTransport};
    use crate::provider::{
        LlmRequest, LlmRequestScope, LlmTransportError, NoSlotDeliveries, Provider,
        ProviderOptions, ProviderRateWindow, ProviderReliability, ProviderToken, TokenError,
        TokenPolicy, TokenRequest, TokenRequestReason, TokenSource,
    };
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    // These fixtures intentionally name only facade vocabulary.
    fn request() -> LlmRequest {
        LlmRequest {
            instructions: None,
            model: crate::testing::test_llm_profile_config(
                "facade-policy",
                crate::testing::test_llm_profile_metadata("gpt-5.4"),
            ),
            messages: vec![crate::provider::LlmMessage::text(
                crate::provider::LlmRole::User,
                "hello policy",
            )],
            tools: Arc::new(Vec::new()),
            tool_choice: crate::provider::LlmToolChoice::None,
            attachment_acceptance: Default::default(),
            scope: LlmRequestScope::new("facade-policy", "frame", "request"),
            output_spec: None,
            stream_events: None,
            generation: crate::direct::GenerationOptions::default(),
            provider_trace: None,
        }
    }

    #[derive(Debug)]
    struct ReplyTransport {
        body: &'static str,
        sse: bool,
        timeouts: Arc<Mutex<Vec<Option<Duration>>>>,
    }

    #[crate::async_trait]
    impl HttpTransport for ReplyTransport {
        async fn send(
            &self,
            _: HttpRequest,
            timeout: Option<Duration>,
        ) -> Result<HttpResponse, LlmTransportError> {
            self.timeouts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(timeout);
            Ok(HttpResponse {
                status: 200,
                headers: if self.sse {
                    vec![("content-type".into(), "text/event-stream".into())]
                } else {
                    Vec::new()
                },
                body: HttpResponseBody::buffered(self.body.as_bytes().to_vec()),
            })
        }
    }

    const CHAT: &str = r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}]}"#;
    const SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    /// D-DEFAULTS2: adapter constructors keep a preset and execute overrides of it.
    #[tokio::test]
    async fn facade_reliability_and_transport_byte_budgets_reach_execution() {
        for (sse, body_limit, event_limit, total_limit, expected) in [
            (false, Some(1), None, None, "body"),
            (true, None, Some(1), None, "event"),
            (true, None, None, Some(1), "total"),
            (false, Some(1024), None, None, "ok"),
        ] {
            let timeouts = Arc::new(Mutex::new(Vec::new()));
            let mut options = ProviderOptions::standard();
            options.reliability = ProviderReliability::standard()
                .response_start_timeout_ms(Some(7))
                .request_timeout_ms(Some(7));
            options.response_body_bytes = body_limit;
            options.sse_event_bytes = event_limit;
            options.sse_total_bytes = total_limit;
            let mut provider =
                crate::openai::OpenAiCompatibleProvider::new("key", "https://provider.test")
                    .with_options(options)
                    .with_request_work_policy(crate::openai::RequestWorkPolicy { inline_bytes: 1 })
                    .with_transport(Arc::new(ReplyTransport {
                        body: if sse { SSE } else { CHAT },
                        sse,
                        timeouts: timeouts.clone(),
                    }));
            let result = provider.complete(request(), &NoSlotDeliveries).await;
            match expected {
                "body" => assert!(matches!(
                    result.unwrap_err().context.as_ref(),
                    crate::provider::HttpFailureContext::ResponseBodyTooLarge { limit: 1, .. }
                )),
                "event" => assert_eq!(
                    result.unwrap_err().code.unwrap().spelling(),
                    "sse_event_too_large"
                ),
                "total" => assert_eq!(
                    result.unwrap_err().code.unwrap().spelling(),
                    "sse_response_too_large"
                ),
                _ => assert_eq!(result.unwrap().full_text(), "done"),
            }
            assert_eq!(*timeouts.lock().unwrap(), [Some(Duration::from_millis(7))]);
        }
    }

    #[derive(Debug)]
    struct ExpiringSource(Arc<Mutex<Vec<TokenRequestReason>>>);
    #[crate::async_trait]
    impl TokenSource for ExpiringSource {
        async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
            self.0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(request.reason);
            Ok(ProviderToken::new("key").expiring_at(SystemTime::now() + Duration::from_secs(300)))
        }
    }

    /// D-DEFAULTS2: facade token skew reaches the gate that calls the host source.
    #[tokio::test]
    async fn facade_token_expiry_skew_controls_proactive_renewal() {
        for (skew, expected) in [
            (Duration::ZERO, vec![TokenRequestReason::Current]),
            (
                Duration::from_secs(600),
                vec![TokenRequestReason::Current, TokenRequestReason::Expiring],
            ),
        ] {
            let reasons = Arc::new(Mutex::new(Vec::new()));
            let mut provider = crate::openai::OpenAiCompatibleProvider::with_token_source(
                Arc::new(ExpiringSource(reasons.clone())),
                "https://provider.test",
            )
            .with_token_policy(TokenPolicy { expiry_skew: skew })
            .with_transport(Arc::new(ReplyTransport {
                body: CHAT,
                sse: false,
                timeouts: Default::default(),
            }));
            assert_eq!(
                provider
                    .complete(request(), &NoSlotDeliveries)
                    .await
                    .unwrap()
                    .full_text(),
                "done"
            );
            assert_eq!(*reasons.lock().unwrap(), expected);
        }
    }

    /// D-DEFAULTS2: a rate cannot invent its window, and a chosen short window is honored.
    #[tokio::test]
    async fn facade_rate_counts_require_windows_and_use_them() {
        for malformed in [
            serde_json::json!({"requests_per_window": {"count": 1}}),
            serde_json::json!({"tokens_per_window": {"count": 1, "window_ms": 0}}),
            serde_json::json!({"requests_per_window": {"count": 0, "window_ms": 20}}),
        ] {
            assert!(
                serde_json::from_value::<crate::provider::ProviderRateLimitPolicy>(malformed)
                    .is_err()
            );
        }
        use futures_util::FutureExt as _;
        let mut unrestricted = crate::testing::TestProvider::builder().build();
        let template = unrestricted.lower(&request()).await.unwrap();
        let limiter = crate::provider::ProviderRateLimiter::new();
        for _ in 0..2 {
            assert!(
                limiter
                    .admit(&unrestricted, &template)
                    .now_or_never()
                    .is_some()
            );
        }
        for tokens in [false, true] {
            let rate = ProviderRateWindow {
                count: 1.try_into().unwrap(),
                window_ms: 20.try_into().unwrap(),
            };
            let reliability = if tokens {
                ProviderReliability::standard().tokens_per_window(Some(rate))
            } else {
                ProviderReliability::standard().requests_per_window(Some(rate))
            };
            let mut provider = crate::testing::TestProvider::builder()
                .options(ProviderOptions {
                    reliability,
                    ..ProviderOptions::standard()
                })
                .build();
            let limiter = crate::provider::ProviderRateLimiter::new();
            let template = provider.lower(&request()).await.unwrap();
            let start = Instant::now();
            drop(limiter.admit(&provider, &template).await);
            drop(
                tokio::time::timeout(Duration::from_secs(2), limiter.admit(&provider, &template))
                    .await
                    .unwrap(),
            );
            assert!(start.elapsed() >= Duration::from_millis(20));
        }
    }

    /// D-DEFAULTS2: disabling reuse through either cache bound opens a new stream each call.
    #[tokio::test]
    async fn facade_websocket_cache_policy_controls_stream_reuse() {
        use crate::openai::codex::ws_testing::{ScriptedWsAction, spawn_scripted_websocket};
        for (ttl, cap) in [(Duration::ZERO, 32), (Duration::from_secs(300), 0)] {
            let server = spawn_scripted_websocket(vec![
                ScriptedWsAction::Complete {
                    response_id: "r1",
                    message_id: "m1",
                    text: "first",
                },
                ScriptedWsAction::Complete {
                    response_id: "r2",
                    message_id: "m2",
                    text: "second",
                },
            ])
            .await;
            let mut provider =
                crate::openai::CodexProvider::new(Arc::new(ProviderToken::new("key")))
                    .with_endpoint_urls("https://provider.test", server.url.clone())
                    .force_websocket_transport()
                    .with_websocket_cache_policy(crate::openai::WebSocketCachePolicy {
                        idle_ttl: ttl,
                        max_entries: cap,
                        ..crate::openai::WebSocketCachePolicy::standard()
                    });
            for expected in ["first", "second"] {
                assert_eq!(
                    provider
                        .complete(request(), &NoSlotDeliveries)
                        .await
                        .unwrap()
                        .full_text(),
                    expected
                );
            }
            assert_eq!(server.handshakes().len(), 2);
            provider.close().await.unwrap();
        }
    }

    /// D-DEFAULTS2: a non-default HTTP pool policy is consumed by the facade-built client.
    #[tokio::test]
    async fn facade_http_pool_policy_disables_idle_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            // Keep both sockets alive. A client reusing the first cannot complete the second request.
            let mut sockets = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
                sockets.push(socket);
            }
            sockets
        });
        let policy = crate::http_transport::HttpTransportPolicy {
            pool_max_idle_per_host: 0,
            ..crate::http_transport::HttpTransportPolicy::standard()
        };
        let client = crate::http_transport::http_client_builder_with(&policy)
            .build()
            .unwrap();
        for _ in 0..2 {
            let body = tokio::time::timeout(Duration::from_secs(2), async {
                client.get(&url).send().await.unwrap().text().await.unwrap()
            })
            .await
            .unwrap();
            assert_eq!(body, "ok");
        }
        assert_eq!(server.await.unwrap().len(), 2);
    }
}
