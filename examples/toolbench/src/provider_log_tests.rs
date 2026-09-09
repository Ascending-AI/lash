use super::*;
use lash::{TurnActivitySink as _, TurnEvent};
use std::collections::VecDeque;
use std::time::Duration;

#[derive(Clone, Debug)]
struct Scripted {
    options: ProviderOptions,
    replies: VecDeque<Result<LlmResponse, LlmTransportError>>,
}
#[async_trait::async_trait]
impl Provider for Scripted {
    fn kind(&self) -> &'static str {
        "scripted"
    }
    fn route_identity(&self, model: &str) -> lash::direct::ProviderRouteIdentity {
        lash::direct::ProviderRouteIdentity::new("scripted", "test", model)
    }
    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }
    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }
    fn serialize_config(&self) -> Value {
        json!({})
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
    async fn complete(&mut self, _: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        self.replies.pop_front().expect("unexpected provider retry")
    }
}
fn request() -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: "test".into(),
        messages: vec![],
        resolved_stored: Default::default(),
        tools: Default::default(),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: Default::default(),
        generation: Default::default(),
        scope: LlmRequestScope::new("s", "f", "r"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}
fn handle(
    capture: &Capture,
    retries: u32,
    replies: Vec<Result<LlmResponse, LlmTransportError>>,
) -> ProviderHandle {
    let mut options = ProviderOptions::default();
    options.reliability.retry = retry_policy(retries);
    options.reliability.retry.jitter_ms = 0;
    ProviderHandle::new(capture.wrap(
        ProviderComponents::new(Box::new(Scripted {
            options,
            replies: replies.into(),
        })),
        "test-secret",
    ))
}
fn transient() -> LlmTransportError {
    LlmTransportError::new("connection reset")
        .with_kind(ProviderFailureKind::Transport)
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
}
async fn record(telemetry: &crate::telemetry::Telemetry, event: TurnEvent) {
    telemetry
        .emit(lash::TurnActivity::new(
            lash::TurnActivityId::new("test"),
            event,
        ))
        .await;
}

#[tokio::test]
async fn retries_honor_count_and_exponential_backoff() {
    for retries in [0, 1, 3] {
        let capture = Capture::default();
        let failure = handle(
            &capture,
            retries,
            vec![Err(transient()); retries as usize + 1],
        )
        .complete(request())
        .await
        .unwrap_err();
        assert_eq!(failure.call_record.attempts.len(), retries as usize + 1);
        let delays = failure
            .call_record
            .attempts
            .iter()
            .filter_map(|a| a.retry_decision.as_ref().and_then(|d| d.delay))
            .collect::<Vec<_>>();
        assert_eq!(
            delays,
            (0..retries)
                .map(|n| Duration::from_secs(1 << n))
                .collect::<Vec<_>>()
        );
        assert!(
            !failure
                .call_record
                .attempts
                .last()
                .unwrap()
                .retry_decision
                .as_ref()
                .unwrap()
                .scheduled
        );
    }
    assert_eq!(retry_policy(3).max_delay_ms, 10_000);
    assert_eq!(retry_policy(3).jitter_ms, 500);
}

#[tokio::test]
async fn request_shape_errors_are_not_retried_and_failed_rows_keep_rich_errors() {
    for status in [400, 422] {
        let telemetry = crate::telemetry::Telemetry::default();
        let error = LlmTransportError::new("bad request test-secret")
            .with_status(status)
            .with_raw("provider body verbatim")
            .with_headers([
                ("x-request-id", "req-17"),
                ("Authorization", "Bearer test-secret"),
            ])
            .with_request_body(
                json!({"api_key":"test-secret","text":"é".repeat(5000)}).to_string(),
            );
        let failure = handle(&telemetry.capture, 3, vec![Err(error)])
            .complete(request())
            .await
            .unwrap_err();
        assert_eq!(failure.call_record.attempts.len(), 1);
        record(
            &telemetry,
            TurnEvent::ModelCallRecorded {
                record: *failure.call_record,
            },
        )
        .await;
        let rows = telemetry.rows(&[], true);
        assert_eq!(rows[0]["error"]["status"], status);
        assert_eq!(rows[0]["error"]["raw"], "provider body verbatim");
        assert_eq!(rows[0]["error"]["provider_request_id"], "req-17");
        assert_eq!(rows[0]["retry_decision"]["scheduled"], false);
        // Forensic captures retain the complete redacted request, including Unicode.
        let body: Value =
            serde_json::from_str(rows[0]["error"]["request_body"].as_str().unwrap()).unwrap();
        assert_eq!(body["text"].as_str().unwrap().chars().count(), 5000);
        assert!(!rows[0].to_string().contains("test-secret"));
    }
}

#[tokio::test]
async fn retry_after_is_honored_without_extra_courtesy_attempts() {
    let capture = Capture::default();
    let error = LlmTransportError::new("throttled")
        .with_status(429)
        .with_headers([("retry-after", "1")]);
    let failure = handle(&capture, 1, vec![Err(error.clone()), Err(error)])
        .complete(request())
        .await
        .unwrap_err();
    assert_eq!(failure.call_record.attempts.len(), 2);
    assert_eq!(
        failure.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .unwrap()
            .delay,
        Some(Duration::from_secs(1))
    );
}

#[tokio::test]
async fn partial_costs_survive_retries_and_charge_safety_refusal_is_visible() {
    let telemetry = crate::telemetry::Telemetry::default();
    let error = transient()
        .with_output_started(true)
        .with_partial_response(LlmResponse {
            provider_usage: Some(json!({"cost":0.02})),
            ..Default::default()
        });
    let mut provider = handle(&telemetry.capture, 1, vec![Err(error.clone())]);
    let failure = provider.complete(request()).await.unwrap_err();
    record(
        &telemetry,
        TurnEvent::ModelCallRecorded {
            record: *failure.call_record,
        },
    )
    .await;
    let rows = telemetry.rows(&[], false);
    assert_eq!(rows[0]["retry_decision"]["scheduled"], false);
    assert_eq!(
        rows[0]["retry_decision"]["charge_safety"]["outcome"],
        "denied"
    );
    assert!(rows[0]["retry_decision"]["reason"].is_string());
    assert_eq!(crate::summary::Usage::from_attempts(&rows).cost, Some(0.02));

    let telemetry = crate::telemetry::Telemetry::default();
    let mut provider = handle(
        &telemetry.capture,
        1,
        vec![
            Err(error),
            Ok(LlmResponse {
                provider_usage: Some(json!({"cost":0.03})),
                ..Default::default()
            }),
        ],
    );
    // Only this simulated accounting test authorizes duplicate billing.
    let completion = provider
        .complete_with_charge_safety(
            request(),
            lash::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: None,
            },
        )
        .await
        .unwrap();
    record(
        &telemetry,
        TurnEvent::ModelCallRecorded {
            record: completion.call_record,
        },
    )
    .await;
    let rows = telemetry.rows(&[], false);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["is_retry"], true);
    assert_eq!(crate::summary::Usage::from_attempts(&rows).cost, Some(0.05));
}

#[test]
fn trace_file_contains_context_and_request_response_pair() {
    let path = std::env::temp_dir().join(format!("toolbench-trace-{}.log", std::process::id()));
    {
        let subscriber = trace_subscriber(&path).unwrap();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "task",
                model = "test-model",
                task = "weather",
                repetition = 8,
                channel = "standard"
            );
            let _guard = span.enter();
            tracing::debug!(target:"toolbench", request="{}", "provider request");
            tracing::debug!(target:"toolbench", response="{}", "provider response");
        });
    }
    let text = std::fs::read_to_string(&path).unwrap();
    for value in [
        "test-model",
        "weather",
        "repetition=8",
        "standard",
        "provider request",
        "provider response",
    ] {
        assert!(text.contains(value), "missing {value}: {text}");
    }
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn cancellation_during_backoff_keeps_the_failed_call_and_its_cost() {
    let telemetry = crate::telemetry::Telemetry::default();
    record(
        &telemetry,
        TurnEvent::ModelRequestStarted {
            protocol_iteration: 1,
        },
    )
    .await;
    assert!(
        telemetry.rows(&[], false).is_empty(),
        "admission is not a provider invocation"
    );
    let failure = transient()
        .with_output_started(true)
        .with_partial_response(LlmResponse {
            provider_usage: Some(json!({"cost":0.02})),
            ..Default::default()
        });
    let mut provider = handle(&telemetry.capture, 3, vec![Err(failure)]);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            provider.complete_with_charge_safety(
                request(),
                lash::ChargeSafetyPolicy::AcceptDuplicateBilling {
                    max_unsafe_retries: 1,
                    max_duplicate_cost_tokens: None
                }
            )
        )
        .await
        .is_err()
    );
    let rows = telemetry.rows(&[], false);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["error"]["message"], "connection reset");
    assert_eq!(rows[0]["is_retry"], false);
    assert!(rows[0]["retry_decision_unavailable"].is_string());
    assert_eq!(crate::summary::Usage::from_attempts(&rows).cost, Some(0.02));
}

#[tokio::test]
async fn empty_response_keeps_cost_from_raw_usage_even_without_partial_response() {
    let telemetry = crate::telemetry::Telemetry::default();
    let error = LlmTransportError::new("OpenAI-compatible empty_response")
        .with_retry_verdict(TransportRetryVerdict::NotRetryable)
        .with_raw(r#"{"id":"gen-empty","usage":{"cost":0.00010212,"prompt_tokens":1098,"completion_tokens":11}}"#);
    let failure = handle(&telemetry.capture, 3, vec![Err(error)])
        .complete(request())
        .await
        .unwrap_err();
    record(
        &telemetry,
        TurnEvent::ModelCallRecorded {
            record: *failure.call_record,
        },
    )
    .await;
    let rows = telemetry.rows(&[], true);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["retry_decision"]["reason"], "not_retryable");
    assert_eq!(rows[0]["cost_unknown"], false);
    assert_eq!(
        crate::summary::Usage::from_attempts(&rows).cost,
        Some(0.00010212)
    );
    assert!(rows[0]["error"]["partial_response"].is_null());
    assert_eq!(rows[0]["error"]["provider_response_id"], "gen-empty");
}
