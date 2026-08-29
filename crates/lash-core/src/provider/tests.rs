use super::support::*;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::llm::types::{
    LlmContentBlock, LlmMessage, LlmOutputPart, LlmRole, LlmToolChoice, LlmUsage,
    ProviderReasoningReplay,
};
use crate::{GenerationOptions, NonNegativeFiniteF64};

/// Marker `max_output_tokens` value `MutatingProvider` writes into its own
/// options while completing, so a test can prove the handle kept the mutated
/// provider instead of a pre-call clone.
const MUTATED_MAX_OUTPUT_TOKENS: u64 = 4_321;

#[derive(Clone, Debug, Default)]
struct MutatingProvider {
    options: ProviderOptions,
}

#[derive(Clone, Debug)]
struct FailingProvider {
    options: ProviderOptions,
    attempts: Arc<AtomicUsize>,
    fail_until: usize,
    retryable: bool,
}

#[derive(Clone, Debug)]
struct TerminalProvider {
    reason: LlmTerminalReason,
    text: &'static str,
}

#[derive(Clone, Debug)]
struct PartialStreamFailureProvider;

#[derive(Clone, Debug)]
struct CountedPartialStreamFailureProvider {
    attempts: Arc<AtomicUsize>,
}

#[derive(Clone, Debug)]
struct ReplayCaptureProvider;

#[derive(Clone, Debug)]
struct ContradictoryReplayProvider;

#[derive(Clone, Debug)]
struct ContradictoryPartialFailureProvider;

#[async_trait::async_trait]
impl Provider for ContradictoryReplayProvider {
    fn kind(&self) -> &'static str {
        "openai-compatible"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::for_endpoint(self.kind(), "https://gateway-b.example/v1", model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        Ok(LlmResponse {
            parts: vec![LlmOutputPart::Reasoning {
                text: "summary".to_string(),
                replay: Some(ProviderReasoningReplay {
                    signature: Some("opaque".to_string()),
                    origin: Some(ProviderRouteIdentity::for_endpoint(
                        self.kind(),
                        "https://gateway-a.example/v1",
                        "model",
                    )),
                    ..ProviderReasoningReplay::default()
                }),
            }],
            ..LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Provider for ContradictoryPartialFailureProvider {
    fn kind(&self) -> &'static str {
        "openai-compatible"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::for_endpoint(self.kind(), "https://gateway-b.example/v1", model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        Err(LlmTransportError::new("original partial provider failure")
            .with_kind(ProviderFailureKind::Stream)
            .with_status(502)
            .with_code("original_partial_code")
            .with_raw("original raw provider evidence")
            .with_headers([("x-request-id", "original-request")])
            .with_request_body("original request body")
            .with_output_started(true)
            .with_partial_response(LlmResponse {
                parts: vec![LlmOutputPart::Reasoning {
                    text: "partial summary".to_string(),
                    replay: Some(ProviderReasoningReplay {
                        signature: Some("opaque".to_string()),
                        origin: Some(ProviderRouteIdentity::for_endpoint(
                            self.kind(),
                            "https://gateway-a.example/v1",
                            "model",
                        )),
                        ..ProviderReasoningReplay::default()
                    }),
                }],
                ..LlmResponse::default()
            }))
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct GatewayReplayCaptureProvider {
    endpoint: &'static str,
}

#[async_trait::async_trait]
impl Provider for GatewayReplayCaptureProvider {
    fn kind(&self) -> &'static str {
        "openai-compatible"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::for_endpoint(self.kind(), self.endpoint, model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        assert!(matches!(
            request.messages[0].blocks[0],
            LlmContentBlock::Text { ref text, .. } if text.as_ref() == "portable summary"
        ));
        Ok(LlmResponse::default())
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Provider for ReplayCaptureProvider {
    fn kind(&self) -> &'static str {
        "serving-provider"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        assert!(matches!(
            request.messages[0].blocks[0],
            LlmContentBlock::Text { ref text, .. } if text.as_ref() == "neutral summary"
        ));
        Ok(LlmResponse {
            parts: vec![LlmOutputPart::Reasoning {
                text: "new summary".to_string(),
                replay: Some(ProviderReasoningReplay {
                    signature: Some("new-signature".to_string()),
                    ..ProviderReasoningReplay::default()
                }),
            }],
            ..LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Provider for PartialStreamFailureProvider {
    fn kind(&self) -> &'static str {
        "partial-stream-failure"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions {
            reliability: ProviderReliability::default().max_attempts(1),
            ..ProviderOptions::default()
        }
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        Err(LlmTransportError::new("stream truncated")
            .with_kind(ProviderFailureKind::Stream)
            .with_code("stream_ended_before_terminal")
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_partial_response(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "partial".to_string(),
                    response_meta: None,
                }],
                usage: LlmUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..LlmUsage::default()
                },
                provider_usage: Some(serde_json::json!({
                    "prompt_tokens": 7,
                    "completion_tokens": 3
                })),
                execution_evidence: Some(ExecutionEvidence {
                    provider_response_id: Some("resp-partial".to_string()),
                    ..ExecutionEvidence::default()
                }),
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }))
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Provider for CountedPartialStreamFailureProvider {
    fn kind(&self) -> &'static str {
        "counted-partial-stream-failure"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions {
            reliability: ProviderReliability::default().max_attempts(1),
            ..ProviderOptions::default()
        }
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(LlmTransportError::new("stream truncated")
            .with_kind(ProviderFailureKind::Stream)
            .with_code("stream_ended_before_terminal")
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_partial_response(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "paid partial".to_string(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            }))
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Provider for TerminalProvider {
    fn kind(&self) -> &'static str {
        "terminal"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: self.text.to_string(),
                response_meta: None,
            }],
            terminal_reason: self.reason,
            response_metadata: Default::default(),
            ..LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

impl FailingProvider {
    fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }
}

impl MutatingProvider {
    fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }
}

#[async_trait::async_trait]
impl Provider for MutatingProvider {
    fn kind(&self) -> &'static str {
        "mutating"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        self.options.max_output_tokens = Some(MUTATED_MAX_OUTPUT_TOKENS);
        Ok(LlmResponse {
            parts: Vec::new(),
            usage: LlmUsage::default(),
            terminal_reason: crate::LlmTerminalReason::Stop,
            terminal_diagnostic: None,
            provider_usage: None,
            request_body: None,
            http_summary: None,
            execution_evidence: None,
            generation_disposition: None,
            response_metadata: Default::default(),
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Provider for FailingProvider {
    fn kind(&self) -> &'static str {
        "failing"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Object(Default::default())
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= self.fail_until {
            let kind = if self.retryable {
                ProviderFailureKind::Transport
            } else {
                ProviderFailureKind::Validation
            };
            let retry_verdict = if self.retryable {
                TransportRetryVerdict::RetryableTransient
            } else {
                TransportRetryVerdict::Forbidden
            };
            return Err(LlmTransportError::new("temporary failure")
                .with_kind(kind)
                .with_retry_verdict(retry_verdict));
        }
        Ok(LlmResponse {
            parts: Vec::new(),
            usage: LlmUsage::default(),
            terminal_reason: crate::LlmTerminalReason::Stop,
            terminal_diagnostic: None,
            provider_usage: None,
            request_body: None,
            http_summary: None,
            execution_evidence: None,
            generation_disposition: None,
            response_metadata: Default::default(),
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

/// Fails `fail_until` calls with an HTTP-status failure (plus optional
/// `Retry-After`), leaving generic statuses to the classifier. Status 413
/// models the Gemini upload driver's explicit validation verdict.
#[derive(Clone, Debug)]
struct StatusFailingProvider {
    options: ProviderOptions,
    attempts: Arc<AtomicUsize>,
    fail_until: usize,
    status: u16,
    retry_after: Option<Duration>,
    retry_after_header: Option<&'static str>,
}

impl StatusFailingProvider {
    fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }
}

#[async_trait::async_trait]
impl Provider for StatusFailingProvider {
    fn kind(&self) -> &'static str {
        "status-failing"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Object(Default::default())
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= self.fail_until {
            let message = match self.status {
                429 => "throttled by provider",
                413 => "Request too large: attachment exceeds upload limit",
                _ => "upstream unavailable",
            };
            let mut failure = LlmTransportError::new(message).with_status(self.status);
            if self.status == 413 {
                failure = failure
                    .with_kind(ProviderFailureKind::Validation)
                    .with_retry_verdict(TransportRetryVerdict::Forbidden);
            }
            if self.status == 429
                && let Some(retry_after) = self.retry_after
            {
                failure = failure.with_retry_verdict(TransportRetryVerdict::RetryableThrottle {
                    retry_after: Some(retry_after),
                });
            }
            if let Some(retry_after) = self.retry_after_header {
                failure = failure.with_headers([("retry-after", retry_after)]);
            }
            return Err(failure);
        }
        Ok(LlmResponse {
            parts: Vec::new(),
            usage: LlmUsage::default(),
            terminal_reason: crate::LlmTerminalReason::Stop,
            terminal_diagnostic: None,
            provider_usage: None,
            request_body: None,
            http_summary: None,
            execution_evidence: None,
            generation_disposition: None,
            response_metadata: Default::default(),
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

/// Injected [`crate::Clock`] that resolves sleeps immediately while recording
/// the total requested wait, so retry-ladder tests assert real durations
/// without real waits.
#[derive(Debug, Default)]
struct RecordingClock {
    slept_ms: std::sync::atomic::AtomicU64,
}

impl RecordingClock {
    fn slept(&self) -> Duration {
        Duration::from_millis(self.slept_ms.load(Ordering::SeqCst))
    }
}

#[async_trait::async_trait]
impl crate::Clock for RecordingClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        0
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::from(std::time::UNIX_EPOCH)
    }

    async fn sleep(&self, duration: Duration) {
        self.slept_ms
            .fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
    }

    async fn sleep_until(&self, _deadline: std::time::Instant) {}
}

#[derive(Debug)]
struct MetricsTransport {
    inner: Box<dyn Provider>,
    hits: Arc<AtomicUsize>,
}

impl Clone for MetricsTransport {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_boxed(),
            hits: Arc::clone(&self.hits),
        }
    }
}

#[async_trait::async_trait]
impl Provider for MetricsTransport {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        self.inner.route_identity(model)
    }

    fn options(&self) -> ProviderOptions {
        self.inner.options()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.inner.set_options(options);
    }

    fn serialize_config(&self) -> serde_json::Value {
        self.inner.serialize_config()
    }

    async fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        self.inner.complete(request).await
    }

    fn requires_streaming(&self) -> bool {
        self.inner.requires_streaming()
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

fn empty_request() -> LlmRequest {
    LlmRequest {
        model: "model".to_string(),
        messages: Vec::new(),
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::None,
        model_variant: Default::default(),
        model_capability: crate::ModelCapability::default(),
        scope: crate::LlmRequestScope::new(
            "provider-test",
            "provider-test:frame",
            "provider-test:request",
        ),
        output_spec: None,
        stream_events: None,
        generation: GenerationOptions::default(),
        provider_trace: None,
    }
}

#[tokio::test]
async fn provider_handle_records_drop_without_provider_trace_and_stamps_fresh_state() {
    let mut request = empty_request();
    request.model = "serving-model".to_string();
    request.messages = vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::Reasoning {
            text: "neutral summary".to_string(),
            replay: Some(ProviderReasoningReplay {
                signature: Some("foreign-signature".to_string()),
                origin: Some(ProviderRouteIdentity::new(
                    "minting-provider",
                    "minting-route",
                    "minting-model",
                )),
                ..ProviderReasoningReplay::default()
            }),
        }],
    )];
    assert!(request.provider_trace.is_none());
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(ReplayCaptureProvider)));

    let completion = handle.complete(request).await.expect("provider succeeds");

    let replay = match &completion.response.parts[0] {
        LlmOutputPart::Reasoning {
            replay: Some(replay),
            ..
        } => replay,
        other => panic!("expected stamped reasoning replay, got {other:?}"),
    };
    assert_eq!(
        replay.origin.as_ref(),
        Some(&ProviderRouteIdentity::new(
            "serving-provider",
            "serving-provider",
            "serving-model",
        ))
    );
    let drop = &completion.call_record.replay_drops[0];
    assert_eq!(
        drop.minting_route.clone(),
        Some(ProviderRouteIdentity::new(
            "minting-provider",
            "minting-route",
            "minting-model",
        ))
    );
    assert_eq!(
        drop.serving_route,
        ProviderRouteIdentity::new("serving-provider", "serving-provider", "serving-model")
    );
}

#[tokio::test]
async fn same_provider_and_model_on_distinct_gateways_are_foreign_routes() {
    let mut request = empty_request();
    request.model = "shared-model".to_string();
    request.messages = vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::Reasoning {
            text: "portable summary".to_string(),
            replay: Some(ProviderReasoningReplay {
                signature: Some("gateway-a-signature".to_string()),
                origin: Some(ProviderRouteIdentity::new(
                    "openai-compatible",
                    "https://gateway-a.example/v1",
                    "shared-model",
                )),
                ..ProviderReasoningReplay::default()
            }),
        }],
    )];
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(
        GatewayReplayCaptureProvider {
            endpoint: "https://gateway-b.example/v1/",
        },
    )));

    let completion = handle.complete(request).await.expect("provider succeeds");
    assert_eq!(completion.call_record.replay_drops.len(), 1);
    let drop = &completion.call_record.replay_drops[0];
    assert_eq!(drop.reason, crate::ProviderReplayDropReason::ForeignRoute);
    assert_eq!(
        drop.serving_route,
        ProviderRouteIdentity::for_endpoint(
            "openai-compatible",
            "https://gateway-b.example/v1",
            "shared-model",
        )
    );
}

#[tokio::test]
async fn invalid_endpoint_failure_records_a_real_no_response_attempt() {
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(
        GatewayReplayCaptureProvider {
            endpoint: "https://user:secret@gateway.example/v1",
        },
    )));

    let failure = handle
        .complete(empty_request())
        .await
        .expect_err("endpoint userinfo is rejected before transport");

    assert_eq!(
        failure.error.code.as_deref(),
        Some("invalid_provider_endpoint")
    );
    assert_eq!(failure.call_record.attempts.len(), 1);
    assert_eq!(
        failure.call_record.attempts[0].outcome,
        AttemptOutcome::Failed
    );
    assert_eq!(
        failure.call_record.attempts[0].protocol_position,
        ProtocolPosition::NoResponse
    );
}

#[test]
fn task_join_failure_constructor_records_a_real_interrupted_attempt() {
    let failure = LlmTransportError::new("internal task failed: cancelled")
        .with_kind(ProviderFailureKind::Unknown)
        .with_code("task_join_failed")
        .with_retry_verdict(TransportRetryVerdict::NotRetryable);
    let record = synthetic_terminal_call_record(
        7,
        Duration::from_millis(11),
        AttemptOutcome::Interrupted,
        &failure,
        true,
        ProtocolPosition::OutputStarted,
        Vec::new(),
    );

    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.attempts[0].ordinal, 1);
    assert_eq!(record.attempts[0].outcome, AttemptOutcome::Interrupted);
    assert_eq!(
        record.attempts[0].protocol_position,
        ProtocolPosition::OutputStarted
    );
    assert_eq!(
        record.attempts[0]
            .error
            .as_ref()
            .and_then(|error| error.provider_code.as_deref()),
        Some("task_join_failed")
    );
}

#[tokio::test]
async fn provider_handle_rejects_instead_of_recertifying_foreign_stamped_output() {
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(
        ContradictoryReplayProvider,
    )));

    let failure = handle
        .complete(empty_request())
        .await
        .expect_err("foreign-stamped output is a provider contract violation");

    assert_eq!(
        failure.error.code.as_deref(),
        Some("provider_replay_origin_conflict")
    );
    assert!(!failure.error.is_retryable());
    assert!(failure.error.message.contains("gateway-a.example"));
    assert!(failure.error.message.contains("gateway-b.example"));
}

#[tokio::test]
async fn partial_response_origin_conflict_retains_original_provider_failure_evidence() {
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(
        ContradictoryPartialFailureProvider,
    )));

    let failure = handle
        .complete(empty_request())
        .await
        .expect_err("foreign-stamped partial output is a provider contract violation");

    assert_eq!(
        failure.error.code.as_deref(),
        Some("provider_replay_origin_conflict")
    );
    assert_eq!(failure.error.kind, ProviderFailureKind::Validation);
    assert!(!failure.error.is_retryable());
    assert!(
        failure
            .error
            .message
            .contains("original partial provider failure")
    );
    assert_eq!(failure.error.status, Some(502));
    assert_eq!(
        failure.error.raw.as_deref().map(String::as_str),
        Some("original raw provider evidence")
    );
    assert_eq!(
        failure.error.headers.as_slice(),
        &[("x-request-id".to_string(), "original-request".to_string())]
    );
    assert_eq!(
        failure
            .error
            .request_body
            .as_ref()
            .map(|body| body.as_str()),
        Some("original request body")
    );
    let original = failure.call_record.attempts[0]
        .error
        .as_ref()
        .expect("original provider failure evidence remains attached");
    assert_eq!(original.class, ProviderFailureKind::Stream.code());
    assert_eq!(
        original.provider_code.as_deref(),
        Some("original_partial_code")
    );
    assert_eq!(original.http_status, Some(502));
    assert!(
        original
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("original partial provider failure"))
    );
    let partial = failure
        .error
        .partial_response
        .expect("partial response retained");
    let LlmOutputPart::Reasoning { replay, .. } = &partial.parts[0] else {
        panic!("expected reasoning partial")
    };
    assert_eq!(
        replay.as_ref().and_then(|replay| replay.origin.as_ref()),
        Some(&ProviderRouteIdentity::for_endpoint(
            "openai-compatible",
            "https://gateway-a.example/v1",
            "model",
        ))
    );
}

#[test]
fn provider_options_serialize_only_reliability_shape() {
    let options = ProviderOptions {
        reliability: ProviderReliability::default()
            .request_timeout(Some(RequestTimeout::Millis(1_234)))
            .response_start_timeout_ms(Some(345))
            .stream_chunk_timeout_ms(Some(567))
            .max_attempts(2),
        ..ProviderOptions::default()
    };

    let value = serde_json::to_value(options).expect("serialize");
    assert!(value.get("timeout").is_none());
    assert!(value.get("chunk_timeout").is_none());
    assert_eq!(
        value["reliability"]["request_timeout"],
        serde_json::json!(1234)
    );
    assert_eq!(
        value["reliability"]["response_start_timeout"],
        serde_json::json!(345)
    );
    assert_eq!(
        value["reliability"]["chunk_timeout"],
        serde_json::json!(567)
    );
}

#[test]
fn provider_reliability_resolves_response_start_timeout_independently() {
    let explicit = ProviderReliability::default()
        .request_timeout(Some(RequestTimeout::Millis(300_000)))
        .response_start_timeout_ms(Some(20_000))
        .stream_chunk_timeout_ms(Some(120_000))
        .llm_timeouts();
    assert_eq!(
        explicit.response_start_timeout,
        Duration::from_secs(20),
        "an explicit response-start timeout must not inherit the chunk timeout"
    );
    assert_eq!(explicit.chunk_timeout, Duration::from_secs(120));
}

#[test]
fn provider_reliability_without_response_start_timeout_preserves_derived_bound() {
    let defaults = ProviderReliability::default().llm_timeouts();
    assert_eq!(
        defaults.response_start_timeout,
        Duration::from_millis(DEFAULT_CHUNK_TIMEOUT_MS)
    );

    let request_wins = ProviderReliability::default()
        .request_timeout(Some(RequestTimeout::Millis(5_000)))
        .stream_chunk_timeout_ms(Some(7_500))
        .llm_timeouts();
    assert_eq!(request_wins.response_start_timeout, Duration::from_secs(5));

    let chunk_wins = ProviderReliability::default()
        .request_timeout(Some(RequestTimeout::Disabled))
        .stream_chunk_timeout_ms(Some(7_500))
        .llm_timeouts();
    assert_eq!(
        chunk_wins.response_start_timeout,
        Duration::from_millis(7_500)
    );
}

#[test]
fn provider_options_roundtrip_output_limit_and_cache_retention() {
    let options = ProviderOptions {
        max_output_tokens: Some(16_384),
        cache_retention: CacheRetention::Long,
        ..ProviderOptions::default()
    };

    let value = serde_json::to_value(&options).expect("serialize");
    assert_eq!(value["max_output_tokens"], serde_json::json!(16_384));
    assert_eq!(value["cache_retention"], serde_json::json!("long"));

    let roundtripped: ProviderOptions = serde_json::from_value(value).expect("deserialize");
    assert_eq!(roundtripped, options);
}

#[test]
fn provider_options_roundtrip_sse_buffer_caps() {
    let options = ProviderOptions {
        sse_event_bytes: Some(1_048_576),
        sse_total_bytes: Some(8_388_608),
        ..ProviderOptions::default()
    };

    let value = serde_json::to_value(&options).expect("serialize options");
    assert_eq!(value["sse_event_bytes"], serde_json::json!(1_048_576));
    assert_eq!(value["sse_total_bytes"], serde_json::json!(8_388_608));
    assert_eq!(
        serde_json::from_value::<ProviderOptions>(value).expect("deserialize options"),
        options
    );

    let zero_caps = ProviderOptions {
        sse_event_bytes: Some(0),
        sse_total_bytes: Some(0),
        ..ProviderOptions::default()
    };
    assert!(
        zero_caps.is_default(),
        "zero selects both transport defaults"
    );
}

#[test]
fn provider_options_default_omits_and_restores_shared_output_fields() {
    let value = serde_json::to_value(ProviderOptions::default()).expect("serialize");
    assert!(value.get("max_output_tokens").is_none());
    assert!(value.get("cache_retention").is_none());

    let restored: ProviderOptions = serde_json::from_value(serde_json::json!({})).expect("default");
    assert_eq!(restored.max_output_tokens, None);
    assert_eq!(restored.cache_retention, CacheRetention::Short);
    assert!(restored.is_default());
}

#[test]
fn provider_retry_default_uses_bounded_nonzero_jitter() {
    let retry = ProviderRetryPolicy::default();

    assert!(
        retry.jitter_ms > 0 && retry.jitter_ms <= retry.base_delay_ms,
        "default jitter must be within (0, base delay], got {} for {}",
        retry.jitter_ms,
        retry.base_delay_ms
    );
}

#[test]
fn provider_retry_jitter_varies_across_samples() {
    let retry = ProviderRetryPolicy::default();
    let samples = (0..64)
        .map(|_| retry.delay_for_attempt(0, None))
        .collect::<Vec<_>>();

    assert!(samples.iter().all(|delay| {
        (Duration::from_millis(2_000)..=Duration::from_millis(2_500)).contains(delay)
    }));
    assert!(
        samples.windows(2).any(|pair| pair[0] != pair[1]),
        "64 retry jitter samples must not all be identical: {samples:?}"
    );
}

#[test]
fn provider_retry_zero_jitter_preserves_deterministic_ladder() {
    let retry = ProviderRetryPolicy {
        jitter_ms: 0,
        ..ProviderRetryPolicy::default()
    };

    assert_eq!(
        (0..4)
            .map(|attempt| retry.delay_for_attempt(attempt, None))
            .collect::<Vec<_>>(),
        [
            Duration::from_millis(2_000),
            Duration::from_millis(4_000),
            Duration::from_millis(8_000),
            Duration::from_millis(10_000),
        ]
    );
}

#[test]
fn generation_policy_prefers_request_then_provider_then_default() {
    let provider_options = ProviderOptions {
        max_output_tokens: Some(8_192),
        cache_retention: CacheRetention::Long,
        expose_thinking: true,
        ..ProviderOptions::default()
    };
    let defaulted = resolve_generation_policy(
        &GenerationOptions::default(),
        &ProviderOptions::default(),
        32_768,
        "thinking",
    );
    assert_eq!(defaulted.max_output_tokens, 32_768);
    assert_eq!(defaulted.cache_retention, CacheRetention::Short);
    assert!(!defaulted.expose_thinking);
    assert_eq!(defaulted.thinking, "thinking");
    assert_eq!(defaulted.temperature, None);
    assert_eq!(defaulted.seed, None);

    let provider_limited =
        resolve_generation_policy(&GenerationOptions::default(), &provider_options, 32_768, ());
    assert_eq!(provider_limited.max_output_tokens, 8_192);
    assert_eq!(provider_limited.cache_retention, CacheRetention::Long);
    assert!(provider_limited.expose_thinking);

    let request_generation = GenerationOptions {
        output_token_cap: NonZeroUsize::new(2_048),
        temperature: Some(NonNegativeFiniteF64::new(0.25).expect("finite temperature")),
        seed: Some(-7),
        stop_sequences: Vec::new(),
        projection_provenance: Default::default(),
    };
    let request_limited =
        resolve_generation_policy(&request_generation, &provider_options, 32_768, ());
    assert_eq!(request_limited.max_output_tokens, 2_048);
    assert_eq!(
        request_limited.temperature.map(|value| value.get()),
        Some(0.25)
    );
    assert_eq!(request_limited.seed, Some(-7));
}

#[test]
fn non_negative_finite_f64_rejects_non_finite_and_negative_values() {
    for rejected in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5] {
        NonNegativeFiniteF64::new(rejected).expect_err("non-finite or negative must be rejected");
    }
    assert_eq!(NonNegativeFiniteF64::new(0.0).expect("zero").get(), 0.0);
    assert_eq!(NonNegativeFiniteF64::new(1.5).expect("positive").get(), 1.5);
}

#[test]
fn non_negative_finite_f64_normalizes_negative_zero() {
    // `-0.0 != 0.0` is false, so a sign test alone lets negative zero through
    // and it would reach the wire spelled `-0.0`.
    let zero = NonNegativeFiniteF64::new(-0.0).expect("negative zero is not a negative number");
    assert!(zero.get().is_sign_positive());
    assert_eq!(
        serde_json::to_value(&zero).expect("serialize"),
        serde_json::json!(0.0)
    );
    let decoded: NonNegativeFiniteF64 =
        serde_json::from_value(serde_json::json!(-0.0)).expect("decode negative zero");
    assert_eq!(
        serde_json::to_value(&decoded).expect("serialize"),
        serde_json::json!(0.0)
    );
}

#[test]
fn non_negative_finite_f64_round_trips_transparently_and_validates_on_decode() {
    let value = NonNegativeFiniteF64::new(0.7).expect("finite");
    let encoded = serde_json::to_value(&value).expect("serialize");
    assert_eq!(encoded, serde_json::json!(0.7));
    let decoded: NonNegativeFiniteF64 = serde_json::from_value(encoded).expect("deserialize");
    assert_eq!(decoded, value);
    serde_json::from_value::<NonNegativeFiniteF64>(serde_json::json!(-1.0))
        .expect_err("negative must not decode");
}

#[test]
fn non_negative_finite_f64_preserves_the_json_spelling_it_decoded() {
    // Decoding must not re-spell the sender's number: an integer stays an
    // integer, a decimal stays a decimal.
    for spelling in ["1", "1.0", "0.7", "2", "9007199254740992"] {
        let decoded: NonNegativeFiniteF64 =
            serde_json::from_str(spelling).expect("decode json number");
        assert_eq!(
            serde_json::to_string(&decoded).expect("serialize"),
            spelling,
            "decoding {spelling} must not change its JSON spelling"
        );
    }
    // Equality is numeric, so the two spellings of one value are one value.
    let integer: NonNegativeFiniteF64 = serde_json::from_str("1").expect("decode integer");
    let decimal: NonNegativeFiniteF64 = serde_json::from_str("1.0").expect("decode decimal");
    assert_eq!(integer, decimal);
    assert_eq!(integer, NonNegativeFiniteF64::new(1.0).expect("finite"));
}

#[test]
fn non_negative_finite_f64_rejects_integers_binary64_cannot_hold() {
    // 2^53 is the last integer with an exact binary64 representation; past it
    // `get()` would hand back a different number than the sender wrote.
    let boundary: NonNegativeFiniteF64 =
        serde_json::from_str("9007199254740992").expect("2^53 decodes");
    assert_eq!(boundary.get(), 9_007_199_254_740_992.0);
    for rejected in ["9007199254740993", "18446744073709551615"] {
        serde_json::from_str::<NonNegativeFiniteF64>(rejected)
            .expect_err("an inexact integer must not decode");
    }
}

#[test]
fn generation_options_round_trip_and_stay_comparable() {
    let options = GenerationOptions {
        output_token_cap: NonZeroUsize::new(2_048),
        temperature: Some(NonNegativeFiniteF64::new(0.0).expect("finite")),
        seed: Some(42),
        stop_sequences: Vec::new(),
        projection_provenance: Default::default(),
    };
    let encoded = serde_json::to_value(&options).expect("serialize");
    assert_eq!(
        encoded,
        serde_json::json!({
            "output_token_cap": 2_048,
            "temperature": 0.0,
            "seed": 42,
        })
    );
    let decoded: GenerationOptions = serde_json::from_value(encoded).expect("deserialize");
    // `Eq`, not just `PartialEq`: every durable envelope and protocol type
    // that carries GenerationOptions depends on it.
    assert!(decoded == options);
    assert_eq!(
        serde_json::to_value(GenerationOptions::default()).expect("serialize default"),
        serde_json::json!({})
    );
}

#[tokio::test]
async fn transport_mutations_are_visible_after_completion_returns() {
    let mut handle = ProviderHandle::new(MutatingProvider::default().into_components());

    let completion = handle.complete(empty_request()).await.expect("complete");

    assert_eq!(
        handle.options().max_output_tokens,
        Some(MUTATED_MAX_OUTPUT_TOKENS)
    );
    assert_eq!(completion.call_record.attempts.len(), 1);
    assert_eq!(
        completion.call_record.attempts[0].outcome,
        AttemptOutcome::Completed
    );
    assert_eq!(
        completion.call_record.attempts[0].protocol_position,
        ProtocolPosition::TerminalObserved
    );
}

#[tokio::test]
async fn provider_handle_records_aborted_and_interrupted_outcomes() {
    for (reason, text, expected_outcome, expected_position) in [
        (
            LlmTerminalReason::Cancelled,
            "partial",
            AttemptOutcome::Aborted,
            ProtocolPosition::OutputStarted,
        ),
        (
            LlmTerminalReason::Unknown,
            "",
            AttemptOutcome::Interrupted,
            ProtocolPosition::ResponseObserved,
        ),
    ] {
        let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(TerminalProvider {
            reason,
            text,
        })));
        let completion = handle.complete(empty_request()).await.expect("completion");
        assert_eq!(completion.call_record.attempts[0].outcome, expected_outcome);
        assert_eq!(
            completion.call_record.attempts[0].protocol_position,
            expected_position
        );
    }
}

#[tokio::test]
async fn failed_stream_attempt_retains_observed_usage_and_evidence_in_ledger() {
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(
        PartialStreamFailureProvider,
    )));

    let failure = handle
        .complete(empty_request())
        .await
        .expect_err("truncated stream must fail");
    let attempt = &failure.call_record.attempts[0];

    assert_eq!(attempt.outcome, AttemptOutcome::Interrupted);
    assert_eq!(attempt.protocol_position, ProtocolPosition::OutputStarted);
    assert_eq!(
        attempt.usage.as_ref().expect("observed usage").input_tokens,
        7
    );
    assert_eq!(
        attempt
            .usage
            .as_ref()
            .expect("observed usage")
            .output_tokens,
        3
    );
    assert_eq!(
        attempt
            .evidence
            .as_ref()
            .and_then(|evidence| evidence.provider_response_id.as_deref()),
        Some("resp-partial")
    );
    assert_eq!(
        failure
            .partial_response
            .as_deref()
            .map(LlmResponse::full_text),
        Some("partial".to_string())
    );
}

#[tokio::test]
async fn output_started_failure_is_typed_non_retryable_when_max_attempts_is_one() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(
        CountedPartialStreamFailureProvider {
            attempts: Arc::clone(&attempts),
        },
    )));

    let failure = handle
        .complete(empty_request())
        .await
        .expect_err("paid output cannot be safely retried by the host");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        failure.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
    assert!(!failure.is_retryable());
    assert_eq!(
        failure.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.reason.as_deref()),
        Some("output_started_without_retry_guarantee")
    );
}

#[test]
fn raw_provider_usage_requires_a_reported_quantity() {
    let empty_usage = LlmResponse {
        provider_usage: Some(serde_json::json!({})),
        ..LlmResponse::default()
    };
    assert!(!response_has_output_evidence(&empty_usage));

    let zero_quantities = LlmResponse {
        provider_usage: Some(serde_json::json!({ "input_tokens": 0 })),
        ..LlmResponse::default()
    };
    assert!(response_has_output_evidence(&zero_quantities));
}

#[test]
fn automatic_retry_classes_are_explicit_and_output_requires_a_guarantee() {
    let no_response = LlmTransportError::new("connect failed")
        .with_kind(ProviderFailureKind::Transport)
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient);
    assert_eq!(
        automatic_retry_class(
            &no_response,
            failure_protocol_position(&no_response),
            GenerationRetryGuarantee::None,
        ),
        Some(AutomaticRetryClass::NoResponse)
    );

    let rejected = LlmTransportError::new("throttled")
        .with_retry_verdict(TransportRetryVerdict::RetryableThrottle {
            retry_after: Some(Duration::from_secs(1)),
        })
        .with_status(429);
    let rejected = DefaultProviderFailureClassifier.classify(rejected);
    assert_eq!(
        automatic_retry_class(
            &rejected,
            failure_protocol_position(&rejected),
            GenerationRetryGuarantee::None,
        ),
        Some(AutomaticRetryClass::RejectedHttpResponse)
    );

    // Response-observed transient statuses cannot prove that an upstream
    // generation never started, so they must not authorize another purchase.
    for status in [408, 409, 425, 500, 502, 503, 504] {
        let failure = DefaultProviderFailureClassifier
            .classify(LlmTransportError::new("ambiguous rejection").with_status(status));
        assert_eq!(
            automatic_retry_class(
                &failure,
                failure_protocol_position(&failure),
                GenerationRetryGuarantee::None,
            ),
            None,
            "HTTP {status} cannot prove generation did not start"
        );
    }
    let retry_after_absent = DefaultProviderFailureClassifier
        .classify(LlmTransportError::new("throttled").with_status(429));
    assert_eq!(
        automatic_retry_class(
            &retry_after_absent,
            failure_protocol_position(&retry_after_absent),
            GenerationRetryGuarantee::None,
        ),
        Some(AutomaticRetryClass::RejectedHttpResponse),
        "typed throttling is sufficient rejection proof without Retry-After"
    );

    let empty_partial = LlmTransportError::new("stream disconnected")
        .with_kind(ProviderFailureKind::Stream)
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
        .with_partial_response(LlmResponse::default());
    assert_eq!(
        automatic_retry_class(
            &empty_partial,
            failure_protocol_position(&empty_partial),
            GenerationRetryGuarantee::None,
        ),
        Some(AutomaticRetryClass::EmptyStreamPartial)
    );

    let output_started = LlmTransportError::new("stream disconnected")
        .with_kind(ProviderFailureKind::Stream)
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
        .with_partial_response(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "paid output".to_string(),
                response_meta: None,
            }],
            ..LlmResponse::default()
        });
    let position = failure_protocol_position(&output_started);
    assert_eq!(position, ProtocolPosition::OutputStarted);
    assert_eq!(
        automatic_retry_class(&output_started, position, GenerationRetryGuarantee::None,),
        None
    );

    let usage_only = LlmTransportError::new("stream disconnected")
        .with_kind(ProviderFailureKind::Stream)
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
        .with_partial_response(LlmResponse {
            usage: LlmUsage {
                output_tokens: 1,
                ..LlmUsage::default()
            },
            ..LlmResponse::default()
        });
    let usage_position = failure_protocol_position(&usage_only);
    assert_eq!(usage_position, ProtocolPosition::OutputStarted);
    assert_eq!(
        automatic_retry_class(&usage_only, usage_position, GenerationRetryGuarantee::None,),
        None
    );
    assert_eq!(
        automatic_retry_class(
            &output_started,
            position,
            GenerationRetryGuarantee::Idempotent,
        ),
        Some(AutomaticRetryClass::ProviderGuarantee(
            GenerationRetryGuarantee::Idempotent
        ))
    );
}

#[test]
fn forbidden_is_terminal_even_with_retry_after_and_status_noise() {
    let failure = LlmTransportError::new("content policy rejection")
        .with_kind(ProviderFailureKind::Quota)
        .with_terminal_reason(LlmTerminalReason::ContentFilter)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
        .with_status(429)
        .with_headers([("retry-after", "1")]);

    assert_eq!(
        automatic_retry_class(
            &failure,
            failure_protocol_position(&failure),
            GenerationRetryGuarantee::None,
        ),
        None,
        "content-policy failures must be terminal regardless of retry/status noise"
    );
}

#[test]
fn attempt_diagnostics_are_redacted_and_bounded() {
    let message = format!("Bearer sk-secret api_key=also-secret {}", "x".repeat(2_000));
    let diagnostic = bounded_redacted_diagnostic(&message).expect("diagnostic");
    assert!(!diagnostic.contains("sk-secret"));
    assert!(!diagnostic.contains("also-secret"));
    assert!(diagnostic.chars().count() <= MAX_ATTEMPT_DIAGNOSTIC_CHARS);
}

#[tokio::test]
async fn map_provider_installs_transport_decorator() {
    let hits = Arc::new(AtomicUsize::new(0));
    let components = MutatingProvider::default().into_components().map_provider({
        let hits = Arc::clone(&hits);
        move |inner| Box::new(MetricsTransport { inner, hits })
    });
    let mut handle = ProviderHandle::new(components);

    handle.complete(empty_request()).await.expect("complete");

    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_handle_retries_retryable_failures_in_shared_executor() {
    #[cfg(feature = "otel-trace")]
    let metrics = crate::operational_metrics::TestMetrics::install();
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = FailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(3)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 2,
        retryable: true,
    };
    let mut handle = ProviderHandle::new(provider.into_components());

    let completion = handle
        .complete(empty_request())
        .await
        .expect("eventual success");

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(completion.call_record.attempts.len(), 3);
    assert_eq!(
        completion
            .call_record
            .attempts
            .iter()
            .map(|attempt| attempt.outcome)
            .collect::<Vec<_>>(),
        vec![
            AttemptOutcome::Failed,
            AttemptOutcome::Failed,
            AttemptOutcome::Completed,
        ]
    );
    assert!(
        completion
            .call_record
            .attempts
            .iter()
            .all(|attempt| attempt.retry_budget_consumed)
    );
    assert_eq!(completion.call_record.attempts[2].evidence, None);
    #[cfg(feature = "otel-trace")]
    assert_eq!(metrics.counter_value("lash.provider.retries"), 2);
}

/// A non-OpenAI provider kind that reports both usage and execution evidence
/// on a successful call, the way Anthropic, Google and Codex do.
#[derive(Clone, Debug)]
struct ReportingProvider;

#[async_trait::async_trait]
impl Provider for ReportingProvider {
    fn kind(&self) -> &'static str {
        "reporting-vendor"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions {
            reliability: ProviderReliability::default().max_attempts(1),
            ..ProviderOptions::default()
        }
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "ok".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 11,
                output_tokens: 5,
                ..LlmUsage::default()
            },
            provider_usage: Some(serde_json::json!({ "input": 11, "output": 5 })),
            execution_evidence: Some(crate::ExecutionEvidence {
                served_model: Some("vendor-model-1".to_string()),
                ..crate::ExecutionEvidence::default()
            }),
            ..LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

// The ledger records what the provider reported, whatever its kind. Gating it
// on a provider-kind allowlist made an absent value mean "not allowlisted"
// rather than "not reported", which is the distinction ADR 0031 rests on.
#[tokio::test]
async fn provider_handle_records_usage_and_evidence_for_any_provider_kind() {
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(ReportingProvider)));

    let completion = handle
        .complete(empty_request())
        .await
        .expect("reporting provider succeeds");

    let attempt = &completion.call_record.attempts[0];
    assert_eq!(
        attempt.usage.as_ref().map(|usage| usage.input_tokens),
        Some(11),
        "usage reported by a non-OpenAI provider must reach the ledger"
    );
    assert_eq!(
        attempt
            .evidence
            .as_ref()
            .and_then(|evidence| evidence.served_model.as_deref()),
        Some("vendor-model-1"),
        "execution evidence reported by a non-OpenAI provider must reach the ledger"
    );
}

#[tokio::test]
async fn provider_handle_stops_on_non_retryable_failure() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = FailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(3)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 3,
        retryable: false,
    };
    let mut handle = ProviderHandle::new(provider.into_components());

    let err = handle
        .complete(empty_request())
        .await
        .expect_err("non retryable");

    assert!(!err.is_retryable());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_handle_set_options_affects_retry_behavior() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = FailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::disabled(),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 1,
        retryable: true,
    };
    let mut handle = ProviderHandle::new(provider.into_components());
    handle.set_options(ProviderOptions {
        reliability: ProviderReliability::default()
            .max_attempts(2)
            .base_delay_ms(0)
            .max_delay_ms(0),
        ..ProviderOptions::default()
    });

    handle
        .complete(empty_request())
        .await
        .expect("retry after set_options");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn provider_handle_throttle_with_retry_after_does_not_consume_attempts() {
    #[cfg(feature = "otel-trace")]
    let metrics = crate::operational_metrics::TestMetrics::install();
    let attempts = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(RecordingClock::default());
    let provider = StatusFailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        // Three throttles in a row: more failures than the two-attempt
        // budget could ever absorb if throttles consumed attempts.
        fail_until: 3,
        status: 429,
        retry_after: Some(Duration::from_secs(5)),
        retry_after_header: None,
    };
    let mut handle =
        ProviderHandle::new(provider.into_components()).with_clock(Arc::clone(&clock) as _);

    let completion = handle
        .complete(empty_request())
        .await
        .expect("success after deferred throttle waits");

    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    // Each deference honored the provider-stated 5s Retry-After.
    assert_eq!(clock.slept(), Duration::from_secs(15));
    assert_eq!(completion.call_record.attempts.len(), 4);
    assert!(
        completion.call_record.attempts[..3]
            .iter()
            .all(|attempt| !attempt.retry_budget_consumed)
    );
    assert!(completion.call_record.attempts[3].retry_budget_consumed);
    #[cfg(feature = "otel-trace")]
    {
        assert_eq!(metrics.counter_value("lash.provider.retries"), 3);
        assert_eq!(
            metrics.histogram_count("lash.provider.throttle_wait.duration"),
            3
        );
    }
}

#[tokio::test]
async fn provider_handle_throttle_with_future_http_date_retries_for_free() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(RecordingClock::default());
    let provider = StatusFailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(1)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 1,
        status: 429,
        retry_after: None,
        retry_after_header: Some("Sat, 06 Nov 2094 08:49:37 GMT"),
    };
    let mut handle =
        ProviderHandle::new(provider.into_components()).with_clock(Arc::clone(&clock) as _);

    let result = handle.complete(empty_request()).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let completion =
        result.expect("valid future HTTP-date defers without consuming the only attempt");
    assert!(clock.slept() > Duration::ZERO);
    assert_eq!(completion.call_record.attempts.len(), 2);
    assert!(!completion.call_record.attempts[0].retry_budget_consumed);
}

#[tokio::test]
async fn provider_handle_throttle_with_past_http_date_consumes_attempt() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(RecordingClock::default());
    let provider = StatusFailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(1)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 1,
        status: 429,
        retry_after: None,
        retry_after_header: Some("Sun, 06 Nov 1994 08:49:37 GMT"),
    };
    let mut handle =
        ProviderHandle::new(provider.into_components()).with_clock(Arc::clone(&clock) as _);

    let failure = handle
        .complete(empty_request())
        .await
        .expect_err("past HTTP-date is not an attempt-free deferral");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(clock.slept(), Duration::ZERO);
    assert_eq!(failure.call_record.attempts.len(), 1);
    assert!(failure.call_record.attempts[0].retry_budget_consumed);
}

#[tokio::test]
async fn provider_handle_repeated_past_http_dates_are_attempt_bounded() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(RecordingClock::default());
    let provider = StatusFailingProvider {
        options: ProviderOptions::default(),
        attempts: Arc::clone(&attempts),
        fail_until: 100,
        status: 429,
        retry_after: None,
        retry_after_header: Some("Sun, 06 Nov 1994 08:49:37 GMT"),
    };
    let mut handle =
        ProviderHandle::new(provider.into_components()).with_clock(Arc::clone(&clock) as _);

    let failure = handle
        .complete(empty_request())
        .await
        .expect_err("past-date throttle storm must exhaust the attempt ladder");

    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    assert_eq!(failure.call_record.attempts.len(), 4);
    assert!(
        failure
            .call_record
            .attempts
            .iter()
            .all(|attempt| attempt.retry_budget_consumed)
    );
    let slept = clock.slept();
    assert!(
        (Duration::from_secs(14)..=Duration::from_secs(16)).contains(&slept),
        "retry sleep must stay within the bounded jitter envelope, got {slept:?}"
    );
}

#[tokio::test]
async fn provider_handle_throttle_budget_exhaustion_degrades_to_attempt_counting() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(RecordingClock::default());
    let provider = StatusFailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0)
                // Room for exactly two 4s deferences; the third throttle
                // overflows the budget and counts as a normal failure.
                .throttle_wait_budget_ms(10_000),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 100,
        status: 429,
        retry_after: Some(Duration::from_secs(4)),
        retry_after_header: None,
    };
    let mut handle =
        ProviderHandle::new(provider.into_components()).with_clock(Arc::clone(&clock) as _);

    let err = handle
        .complete(empty_request())
        .await
        .expect_err("throttle storm outlives budget and attempts");

    // Two free deferences, then the two-attempt ladder (one counted retry
    // that still waits the provider-stated Retry-After before re-calling).
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    assert_eq!(clock.slept(), Duration::from_secs(12));
    assert!(err.is_retryable());
    assert_eq!(err.kind, ProviderFailureKind::Quota);
}

#[tokio::test]
async fn provider_handle_one_second_throttle_storm_has_a_total_call_bound() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(RecordingClock::default());
    let provider = StatusFailingProvider {
        options: ProviderOptions::default(),
        attempts: Arc::clone(&attempts),
        fail_until: 100,
        status: 429,
        retry_after: Some(Duration::from_secs(1)),
        retry_after_header: None,
    };
    let mut handle =
        ProviderHandle::new(provider.into_components()).with_clock(Arc::clone(&clock) as _);

    let failure = handle
        .complete(empty_request())
        .await
        .expect_err("throttle storm must hit the total provider-call bound");

    assert_eq!(attempts.load(Ordering::SeqCst), 12);
    assert_eq!(failure.call_record.attempts.len(), 12);
    assert_eq!(
        failure
            .call_record
            .attempts
            .iter()
            .filter(|attempt| !attempt.retry_budget_consumed)
            .count(),
        8
    );
    assert_eq!(
        failure
            .call_record
            .attempts
            .iter()
            .filter(|attempt| attempt.retry_budget_consumed)
            .count(),
        4
    );
}

#[tokio::test]
async fn provider_handle_throttle_without_retry_after_uses_counted_backoff_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = StatusFailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 100,
        status: 429,
        retry_after: None,
        retry_after_header: None,
    };
    let mut handle = ProviderHandle::new(provider.into_components());

    let err = handle
        .complete(empty_request())
        .await
        .expect_err("no server-stated wait, so the normal ladder applies");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(err.kind, ProviderFailureKind::Quota);
    assert_eq!(err.code.as_deref(), Some("429"));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn provider_handle_throttle_with_malformed_retry_after_uses_counted_backoff_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = StatusFailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 100,
        status: 429,
        retry_after: None,
        retry_after_header: Some("not an HTTP date"),
    };
    let mut handle = ProviderHandle::new(provider.into_components());

    let err = handle
        .complete(empty_request())
        .await
        .expect_err("malformed Retry-After cannot prove safe resubmission");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(err.kind, ProviderFailureKind::Quota);
    assert_eq!(err.code.as_deref(), Some("429"));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn provider_handle_server_error_with_retry_after_is_not_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = StatusFailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 100,
        status: 503,
        retry_after: Some(Duration::from_secs(1)),
        retry_after_header: None,
    };
    let mut handle = ProviderHandle::new(provider.into_components())
        .with_clock(Arc::new(RecordingClock::default()) as _);

    let err = handle
        .complete(empty_request())
        .await
        .expect_err("5xx is a failure, not a throttle");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(!err.is_retryable());
    assert_eq!(err.kind, ProviderFailureKind::Http);
    assert_eq!(
        err.code.as_deref(),
        Some("unsafe_retry_after_response_observed")
    );
}

#[tokio::test]
async fn provider_handle_attachment_413_remains_plain_non_retryable_validation() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = StatusFailingProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default().max_attempts(3),
            ..ProviderOptions::default()
        },
        attempts: Arc::clone(&attempts),
        fail_until: 100,
        status: 413,
        retry_after: None,
        retry_after_header: None,
    };
    let mut handle = ProviderHandle::new(provider.into_components());

    let error = handle
        .complete(empty_request())
        .await
        .expect_err("attachment 413 is terminal validation");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(error.kind, ProviderFailureKind::Validation);
    assert!(!error.is_retryable());
    assert_eq!(error.code.as_deref(), Some("413"));
    assert_eq!(error.terminal_reason, LlmTerminalReason::ProviderError);
    assert_eq!(
        error.message,
        "Request too large: attachment exceeds upload limit"
    );
}

#[test]
fn default_failure_classifier_classifies_429_as_retryable_throttle() {
    let classifier = DefaultProviderFailureClassifier;
    let failure = classifier.classify(
        ProviderFailure::new("Rate limit reached for requests")
            .with_status(429)
            .with_retry_verdict(TransportRetryVerdict::RetryableThrottle {
                retry_after: Some(Duration::from_secs(7)),
            }),
    );
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(failure.is_retryable());
    assert_eq!(failure.retry_after(), Some(Duration::from_secs(7)));
}

#[test]
fn default_failure_classifier_keeps_quota_exhaustion_non_retryable() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "insufficient_quota",
        "usage_limit_reached",
        "usage_not_included in your plan",
    ] {
        let failure = classifier.classify(ProviderFailure::new(message).with_status(429));
        assert_eq!(failure.kind, ProviderFailureKind::Quota);
        assert!(!failure.is_retryable());
    }
}

// Per-minute throttling reads as "quota" at several providers but is exactly
// the case the retry ladder exists for. Treating it as exhaustion both fails
// the turn and skips throttle deference, which needs `Quota` + retryable.
#[test]
fn default_failure_classifier_keeps_rate_throttling_retryable() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "Quota exceeded for quota metric 'Generate requests per model per minute' \
         and limit 'GenerateRequestsPerDayPerProjectPerModel' of service \
         'generativelanguage.googleapis.com'",
        "Resource has been exhausted (e.g. check quota).",
        "429 RESOURCE_EXHAUSTED: Quota exceeded for aiplatform.googleapis.com",
    ] {
        let failure = classifier.classify(ProviderFailure::new(message).with_status(429));
        assert_eq!(
            failure.kind,
            ProviderFailureKind::Quota,
            "throttling stays Quota: {message}"
        );
        assert!(
            failure.is_retryable(),
            "per-minute throttling must stay retryable: {message}"
        );
    }
}

#[test]
fn default_failure_classifier_uses_context_overflow_text_for_unclassified_failures() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "Anthropic request failed: prompt is too long",
        "context_length_exceeded",
        "This model's maximum context length is 128000 tokens",
        "Google says input is too long",
        "OpenRouter error: too many tokens",
        "Together: request too large",
        "Copilot: exceeds the maximum number of tokens",
        "local model: context window exceeded",
        "Anthropic: request_too_large",
        "OpenAI: Your input exceeds the context window of this model",
        "Google: The input token count (1196265) exceeds the maximum number of tokens allowed",
        "xAI: This model's maximum prompt length is 131072 but the request contains more",
        "Groq: Please reduce the length of the messages",
        "OpenRouter: This endpoint's maximum context length is 128000 tokens",
        "Together: The input (150000 tokens) is longer than the model's context length (128000 tokens).",
        "llama.cpp: the request exceeds the available context size",
        "LM Studio: tokens to keep from the initial prompt is greater than the context length",
        "MiniMax: invalid params, context window exceeds limit",
        "Kimi: Your request exceeded model token limit: 131072 (requested: 150000)",
        "Mistral: Prompt contains too many tokens; too large for model with 128000 maximum context length",
        "z.ai: model_context_window_exceeded",
    ] {
        let failure = classifier.classify(
            ProviderFailure::new(message)
                .with_kind(ProviderFailureKind::Http)
                .with_status(400),
        );
        assert_eq!(failure.kind, ProviderFailureKind::Validation);
        assert_eq!(
            failure.terminal_reason,
            crate::LlmTerminalReason::ContextOverflow
        );
        assert!(!failure.is_retryable());
    }
}

#[test]
fn generic_anthropic_and_google_http_overflow_envelopes_use_the_text_fallback() {
    let classifier = DefaultProviderFailureClassifier;
    for raw in [
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
        r#"{"error":{"code":400,"message":"The input token count (1200000) exceeds the maximum number of tokens allowed"}}"#,
    ] {
        let failure = classifier.classify(
            ProviderFailure::new("provider request failed with 400")
                .with_kind(ProviderFailureKind::Http)
                .with_status(400)
                .with_raw(raw),
        );

        assert_eq!(failure.kind, ProviderFailureKind::Validation, "{raw}");
        assert!(!failure.is_retryable(), "{raw}");
        assert_eq!(
            failure.terminal_reason,
            LlmTerminalReason::ContextOverflow,
            "{raw}"
        );
    }
}

#[test]
fn default_failure_classifier_fails_open_when_unclassified_text_is_uncertain() {
    let failure = DefaultProviderFailureClassifier.classify(
        ProviderFailure::new("upstream request failed")
            .with_raw(r#"{"error":{"message":"ambiguous provider failure"}}"#),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Unknown);
    assert!(failure.is_retryable());
    assert_eq!(
        failure.terminal_reason,
        crate::LlmTerminalReason::ProviderError
    );
}

#[test]
fn default_failure_classifier_preserves_explicit_non_retryability() {
    let failure = DefaultProviderFailureClassifier.classify(
        ProviderFailure::new("Anthropic stream error: invalid request")
            .with_retry_verdict(TransportRetryVerdict::NotRetryable),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Unknown);
    assert!(!failure.is_retryable());
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
}

#[test]
fn default_failure_classifier_makes_structured_validation_forbidden_without_scraping_echo() {
    // Deliberately, the more-specific provider-kind semantics take precedence
    // over an explicitly classified but conflicting transport verdict.
    let failure = DefaultProviderFailureClassifier.classify(
        ProviderFailure::new("request rejected")
            .with_kind(ProviderFailureKind::Validation)
            .with_code("invalid_request_error")
            .with_raw(
                r#"{"error":{"message":"The user wrote: context length is a useful phrase"}}"#,
            )
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert_eq!(failure.retry_verdict, TransportRetryVerdict::Forbidden);
    assert!(!failure.is_retryable());
    assert_eq!(failure.code.as_deref(), Some("invalid_request_error"));
    assert_eq!(
        failure.terminal_reason,
        crate::LlmTerminalReason::ProviderError
    );
}

#[test]
fn default_failure_classifier_does_not_override_structured_hard_quota_echo() {
    let failure = DefaultProviderFailureClassifier.classify(
        ProviderFailure::new("request rejected")
            .with_kind(ProviderFailureKind::Validation)
            .with_code("invalid_request_error")
            .with_raw(r#"{"echo":"insufficient_quota"}"#),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert!(!failure.is_retryable());
    assert_eq!(failure.code.as_deref(), Some("invalid_request_error"));
}

#[test]
fn default_failure_classifier_does_not_override_structured_content_filter_echo() {
    let failure = DefaultProviderFailureClassifier.classify(
        ProviderFailure::new("request rejected")
            .with_kind(ProviderFailureKind::Validation)
            .with_code("invalid_request_error")
            .with_raw(r#"{"echo":"the user asked about safety"}"#),
    );

    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
}

#[test]
fn default_failure_classifier_does_not_override_structured_unsupported_model_echo() {
    let failure = DefaultProviderFailureClassifier.classify(
        ProviderFailure::new("request rejected")
            .with_kind(ProviderFailureKind::Validation)
            .with_code("invalid_request_error")
            .with_raw(r#"{"echo":"the example model does not exist"}"#),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert!(!failure.is_retryable());
    assert_eq!(failure.code.as_deref(), Some("invalid_request_error"));
}

#[test]
fn default_failure_classifier_marks_content_filter() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "Provider finish_reason: content_filter",
        "blocked by prohibited_content",
        "safety filter tripped",
        "sensitive content refused",
    ] {
        let failure = classifier.classify(ProviderFailure::new(message).with_status(400));
        assert_eq!(
            failure.terminal_reason,
            crate::LlmTerminalReason::ContentFilter
        );
    }
}

#[test]
fn default_failure_classifier_does_not_treat_rate_limits_as_context_overflow() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "rate limit: too many tokens per minute",
        "Too many requests",
        "throttling because token rate exceeded",
        "insufficient_quota",
    ] {
        let failure = classifier.classify(ProviderFailure::new(message).with_status(429));
        assert_ne!(
            failure.terminal_reason,
            crate::LlmTerminalReason::ContextOverflow
        );
    }
}
