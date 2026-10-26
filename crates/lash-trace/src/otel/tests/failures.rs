//! Failure classification on exported spans.

use super::*;

#[test]
fn failure_attributes_preserve_typed_classes() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let tool = |outcome, attempts| TraceEvent::ToolCallCompleted {
        call_id: lash_protocol_types::ToolCallId::fixture("call"),
        provider_call_id: None,
        name: "tool".into(),
        args: serde_json::Value::Null.into(),
        output: crate::TraceToolCallOutput {
            outcome,
            control: None,
        },
        duration_ms: 1,
        issuing_node_id: None,
        attempts: Some(attempts),
        span_attributes: Default::default(),
    };
    let attempts = vec![
        crate::TraceRetryAttempt {
            ordinal: 1,
            delay_ms: None,
            detail: crate::TraceRetryAttemptDetail::Tool {
                outcome: crate::TraceToolAttemptOutcome::Failed {
                    class: lash_protocol_types::ToolFailureClass::Io,
                    code: "old".into(),
                    message: "private detail".into(),
                    source: lash_protocol_types::ToolFailureSource::Tool,
                    suggested_delay_ms: None,
                },
            },
        },
        crate::TraceRetryAttempt {
            ordinal: 2,
            delay_ms: None,
            detail: crate::TraceRetryAttemptDetail::Tool {
                outcome: crate::TraceToolAttemptOutcome::Failed {
                    class: lash_protocol_types::ToolFailureClass::PermissionDenied,
                    code: "final".into(),
                    message: "private detail".into(),
                    source: lash_protocol_types::ToolFailureSource::Tool,
                    suggested_delay_ms: None,
                },
            },
        },
    ];
    let model = |code| TraceEvent::LlmCallFailed {
        model: None,
        request: Default::default(),
        error: crate::TraceError {
            code: None,
            retryable: false,
            terminal_reason: crate::TraceLlmTerminalReason::ProviderError,
            failure_kind: crate::TraceProviderFailureKind::Http,
        },
        stream_summary: None,
        attempts: Some(vec![
            AttemptRecord {
                ordinal: 1,
                outcome: AttemptOutcome::Failed,
                protocol_position: ProtocolPosition::NoResponse,
                retry_budget_consumed: true,
                retry_decision: None,
                evidence: None,
                generation_disposition: None,
                error: Some(crate::TraceNormalizedError {
                    class: crate::TraceProviderFailureKind::Http,
                    code,
                    http_status: Some(429),
                    provider_request_id: None,
                    retry_after: None,
                }),
                usage: None,
            }
            .into(),
        ]),
    };
    let events = [
        (
            TraceSpanKind::Tool,
            tool(
                crate::TraceToolCallOutcome::Failure(crate::TraceToolFailure::new(
                    &lash_protocol_types::ToolFailure::runtime(
                        lash_protocol_types::ToolFailureClass::Internal,
                        "tool_retry_sleep_failed",
                        "private retry sleep failure",
                    ),
                    crate::ContentPolicy::captured(),
                )),
                attempts.clone(),
            ),
            Some("internal".to_owned()),
        ),
        (
            TraceSpanKind::Tool,
            tool(
                crate::TraceToolCallOutcome::Failure(crate::TraceToolFailure::new(
                    &lash_protocol_types::ToolFailure::io("host_io_failed", "private detail"),
                    crate::ContentPolicy::captured(),
                )),
                Vec::new(),
            ),
            Some("io".to_owned()),
        ),
        (
            TraceSpanKind::Tool,
            tool(
                crate::TraceToolCallOutcome::Success(serde_json::Value::Null),
                attempts,
            ),
            None,
        ),
        (
            TraceSpanKind::Agent,
            TraceEvent::TurnCompleted {
                output: Vec::new(),
                input: Vec::new(),
                inference_calls: None,
                tool_calls: None,
                inputs: 1,
                outcome: TraceTurnOutcome::Failed {
                    done_reason: crate::TraceTurnFailureReason::ContextOverflow,
                    provider_validation: None,
                },
            },
            Some("context_overflow".to_owned()),
        ),
        (TraceSpanKind::Model, model(None), Some("http".to_owned())),
        (
            TraceSpanKind::Model,
            model(Some(crate::TraceFailureCode::provider("rate_limit"))),
            Some(crate::TraceFailureCode::provider("rate_limit").to_string()),
        ),
    ];
    let validation = TraceEvent::TurnCompleted {
        input: Vec::new(),
        output: Vec::new(),
        inference_calls: None,
        tool_calls: None,
        inputs: 1,
        outcome: TraceTurnOutcome::Failed {
            done_reason: crate::TraceTurnFailureReason::ProviderError,
            provider_validation: Some(lash_protocol_types::TurnFailureCode::EmptyResponse),
        },
    };
    for (kind, event, expected) in events.into_iter().chain([(
        TraceSpanKind::Agent,
        validation,
        Some("provider_error".to_owned()),
    )]) {
        let validation = match &event {
            TraceEvent::TurnCompleted {
                outcome:
                    TraceTurnOutcome::Failed {
                        provider_validation,
                        ..
                    },
                ..
            } => provider_validation
                .as_ref()
                .map(|_| "empty_response".to_owned()),
            _ => None,
        };
        let span = with_span(&producer(), adapter.leaf_span_id());
        adapter.append(&closing(kind, span, 1000, event)).unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans.last().unwrap();
        assert_eq!(
            matches!(span.status, Status::Error { .. }),
            expected.is_some()
        );
        let classes: Vec<_> = span
            .attributes
            .iter()
            .filter(|a| a.key.as_str() == "error.type")
            .map(|a| a.value.to_string())
            .collect();
        assert_eq!(classes, expected.into_iter().collect::<Vec<_>>());
        assert_eq!(
            span.attributes
                .iter()
                .find(|a| a.key.as_str() == "lash.provider.validation")
                .map(|a| a.value.to_string()),
            validation,
        );
    }
}
