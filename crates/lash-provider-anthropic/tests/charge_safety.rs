use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::llm::types::{
    LlmMessage, LlmOutputPart, LlmRequest, LlmRole, LlmToolChoice, LlmToolSpec, LlmUsage,
};
use lash_core::provider::{
    ProviderCompletionError, ProviderHandle, ProviderOptions, ProviderReliability,
    StreamTermination,
};
use lash_core::session_model::ChargeSafetyPolicy;
use lash_core::{
    ChargeSafetyDecision, ChargeSafetyDenialReason, GenerationOptions, LlmRequestScope,
    ProtocolPosition,
};
use lash_provider_anthropic::AnthropicProvider;

#[derive(Debug)]
struct CountingSseTransport {
    body: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_llm_transport::LlmHttpTransport for CountingSseTransport {
    async fn send(
        &self,
        _request: lash_llm_transport::LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::facade_support::LlmTransportError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: lash_llm_transport::LlmHttpBody::buffered(self.body),
        })
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: "claude-sonnet-4-6".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "hello")],
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: Default::default(),
        scope: LlmRequestScope::new(
            "charge-safety",
            "charge-safety:frame",
            "charge-safety:request",
        ),
        output_spec: None,
        stream_events: None,
        generation: GenerationOptions::default(),
        provider_trace: None,
    }
}

fn handle(body: &'static str) -> (ProviderHandle, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let reliability = ProviderReliability::default()
        .max_attempts(2)
        .base_delay_ms(0)
        .max_delay_ms(0);
    let provider = AnthropicProvider::new("test-key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability {
                retry: lash_core::provider::ProviderRetryPolicy {
                    jitter_ms: 0,
                    ..reliability.retry
                },
                ..reliability
            },
            ..ProviderOptions::default()
        })
        .with_stream_termination(StreamTermination::RequireTerminalEvidence)
        .with_transport(Arc::new(CountingSseTransport {
            body,
            calls: Arc::clone(&calls),
        }));
    let handle = ProviderHandle::new(provider.into_components()).with_clock(Arc::new(
        lash_core::testing::TestClock::new(1_700_000_000_000),
    ));
    (handle, calls)
}

fn output_started_refusal(body: &'static str, tokens_at_stake: u64) -> ProviderCompletionError {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let (mut handle, calls) = handle(body);
    let failure = runtime
        .block_on(
            handle.complete_with_charge_safety(request(), ChargeSafetyPolicy::RequireGuarantee),
        )
        .expect_err("escaped provider output must stop the retry ladder");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        failure.error.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
    let attempt = &failure.call_record.attempts[0];
    assert_eq!(attempt.protocol_position, ProtocolPosition::OutputStarted);
    let retry = attempt.retry_decision.as_ref().expect("retry decision");
    assert!(!retry.scheduled);
    assert_eq!(
        retry.reason.as_deref(),
        Some("output_started_without_retry_guarantee")
    );
    assert_eq!(
        retry.charge_safety,
        Some(ChargeSafetyDecision::Denied {
            tokens_at_stake,
            attempt_number: 1,
            reason: ChargeSafetyDenialReason::GuaranteeRequired,
        })
    );
    failure
}

fn empty_stream_partial_retry(body: &'static str) -> ProviderCompletionError {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let (mut handle, calls) = handle(body);
    let failure = runtime
        .block_on(
            handle.complete_with_charge_safety(request(), ChargeSafetyPolicy::RequireGuarantee),
        )
        .expect_err("two truncated responses exhaust the retry budget");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(failure.call_record.attempts.len(), 2);
    let first = &failure.call_record.attempts[0];
    assert_eq!(first.protocol_position, ProtocolPosition::ResponseObserved);
    let retry = first.retry_decision.as_ref().expect("retry decision");
    assert!(retry.scheduled);
    assert_eq!(retry.delay, Some(std::time::Duration::ZERO));
    assert_eq!(
        retry.reason.as_deref(),
        Some("empty_stream_partial_before_output")
    );
    assert_eq!(retry.charge_safety, None);

    let exhausted = &failure.call_record.attempts[1];
    assert_eq!(
        exhausted.protocol_position,
        ProtocolPosition::ResponseObserved
    );
    assert_eq!(
        exhausted
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.reason.as_deref()),
        Some("retry_budget_exhausted")
    );
    failure
}

#[test]
fn charge_safety_anthropic_escaped_content_refuses_retry_without_usage() {
    let body = concat!(
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
    );
    let failure = output_started_refusal(body, 0);
    let partial = failure
        .error
        .partial_response
        .as_deref()
        .expect("Anthropic content failure carries a partial response");
    assert_eq!(partial.full_text(), "partial");
    assert_eq!(partial.usage, LlmUsage::default());
    assert_eq!(partial.provider_usage, None);
}

#[test]
fn charge_safety_anthropic_quantity_bearing_message_start_usage_refuses_retry_without_content() {
    let body =
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n";
    let failure = output_started_refusal(body, 7);
    let partial = failure
        .error
        .partial_response
        .as_deref()
        .expect("Anthropic usage failure carries a partial response");
    assert!(partial.parts.is_empty());
    assert_eq!(partial.usage.input_tokens, 7);
    assert_eq!(
        partial
            .provider_usage
            .as_ref()
            .and_then(|usage| usage.get("input_tokens"))
            .and_then(serde_json::Value::as_u64),
        Some(7)
    );
}

#[test]
fn charge_safety_anthropic_tool_start_refuses_retry_without_message_start_usage_or_content() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",\"name\":\"lookup\",\"input\":{}}}\n\n",
    );
    let failure = output_started_refusal(body, 0);
    let partial = failure
        .error
        .partial_response
        .as_deref()
        .expect("Anthropic tool-start failure carries a partial response");
    assert_eq!(partial.full_text(), "");
    assert!(matches!(
        partial.parts.as_slice(),
        [LlmOutputPart::ToolCall {
            tool_name,
            input_json,
            ..
        }] if tool_name == "lookup" && input_json == "{}"
    ));
    assert_eq!(partial.provider_usage, None);
}

#[test]
fn charge_safety_anthropic_absent_or_quantity_free_message_start_usage_retries_empty_partial() {
    for (body, raw_usage_expected) in [
        ("", false),
        (
            "data: {\"type\":\"message_start\",\"message\":{}}\n\n",
            false,
        ),
        (
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"service_tier\":\"standard\"}}}\n\n",
            true,
        ),
    ] {
        let failure = empty_stream_partial_retry(body);
        let partial = failure
            .error
            .partial_response
            .as_deref()
            .expect("Anthropic empty-stream failure carries a partial response");
        assert_eq!(partial.full_text(), "");
        assert!(partial.parts.is_empty());
        assert_eq!(partial.usage, LlmUsage::default());
        assert_eq!(partial.provider_usage.is_some(), raw_usage_expected);
    }
}
