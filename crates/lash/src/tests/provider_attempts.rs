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
            .max_attempts(attempts)
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
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created")
}

/// A turn cancelled while its model streams commits none of the streamed
/// text: the partial is observable only as live activity, and the requested
/// cancellation is no model error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_provider_stream_does_not_commit_partial_output() {
    const PARTIAL: &str = "partial provider text";
    let streamed = Arc::new(tokio::sync::Notify::new());
    let model = crate::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete({
            let streamed = Arc::clone(&streamed);
            move |request: LlmRequest| {
                let streamed = Arc::clone(&streamed);
                async move {
                    let stream = request
                        .stream_events
                        .expect("a streaming turn requests provider stream events");
                    stream.send(LlmStreamEvent::Delta {
                        block: StreamBlockIdentity::new("text:0", 0),
                        text: PARTIAL.to_string(),
                    });
                    streamed.notify_one();
                    std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>()
                        .await
                }
            }
        })
        .build()
        .into_handle();
    let core = core_over(sqlite_memory_store_backend().await, model);
    let session = created(&core, "cancel-partial-provider-stream").await;
    let held = session
        .send(crate::TurnInput::text("cancel after partial stream"))
        .await
        .expect("accepted");
    streamed.notified().await;
    held.cancel().await.expect("cancel");
    let output = held.output().await.expect("the cancelled turn settles");
    assert_eq!(output.status(), crate::TurnStatus::Cancelled);
    assert!(output.result.assistant_output.safe_text.is_empty());
    assert!(output.result.assistant_output.raw_text.is_empty());
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
    assert!(output.result.assistant_output.safe_text.is_empty());
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
