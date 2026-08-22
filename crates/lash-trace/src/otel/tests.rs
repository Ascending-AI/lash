//! Unit tests for the OpenTelemetry bridge.
//!
//! These live in their own file so `otel.rs` carries only the exporter itself.

use opentelemetry::trace::noop::NoopTracerProvider;

use super::*;
use crate::{TraceLlmRequest, TraceRecord};

#[test]
fn protocol_step_exec_diagnostics_get_distinct_span_names() {
    let diagnostic =
        |phase: &str| serde_json::json!({ "diagnostic": { "phase": phase, "payload": {} } });
    // Exec-code diagnostics collapse into the lash.exec_code family, with
    // the precise phase available as an attribute; other diagnostics keep a
    // phase-scoped name; plain protocol steps stay lash.protocol_step.
    assert_eq!(
        protocol_step_span_name(&diagnostic("exec_code_started")),
        "lash.exec_code"
    );
    assert_eq!(
        protocol_step_span_name(&diagnostic("exec_code_completed")),
        "lash.exec_code"
    );
    assert_eq!(
        protocol_step_span_name(&diagnostic("observation_projection")),
        "lash.observation_projection"
    );
    assert_eq!(
        protocol_step_span_name(&serde_json::json!({ "code": "print 1" })),
        "lash.protocol_step"
    );
    assert_eq!(
        protocol_step_diagnostic_phase(&diagnostic("exec_code_completed")),
        Some("exec_code_completed")
    );
}

#[test]
fn composition_change_projects_fingerprint_counts_and_opt_in_full_payload() {
    let record = TraceRecord::new(
        TraceContext::default().for_session("session-1"),
        TraceEvent::CompositionChanged {
            fingerprint: "composition-sha".to_string(),
            rendered_system_prompt: "system policy".to_string(),
            tool_schemas: vec![crate::TraceToolSpec {
                name: "search".to_string(),
                description: "Search documents".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
                output_schema: serde_json::json!({ "type": "array" }),
            }],
        },
    );
    let attrs = event_attributes(
        &record,
        &OtelTraceOptions {
            include_payload_json: true,
            ..OtelTraceOptions::default()
        },
    );
    let attribute = |key: &str| {
        attrs
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| &attribute.value)
            .unwrap_or_else(|| panic!("missing OTel attribute {key}"))
    };

    assert_eq!(
        attribute("lash.composition.fingerprint"),
        &OtelValue::String("composition-sha".into())
    );
    assert_eq!(
        attribute("lash.composition.prompt_chars"),
        &OtelValue::I64(13)
    );
    assert_eq!(attribute("lash.composition.tool_count"), &OtelValue::I64(1));
    assert!(
        attribute("lash.composition.rendered_system_prompt_json")
            .to_string()
            .contains("system policy")
    );
    assert!(
        attribute("lash.composition.tool_schemas_json")
            .to_string()
            .contains("search")
    );
}

#[test]
fn otel_sink_accepts_turn_and_llm_lifecycle() {
    let tracer = NoopTracerProvider::new().tracer("test");
    let sink = OtelTraceSink::new(tracer);
    let context = TraceContext::default()
        .for_session("session-1")
        .for_llm_call("llm-1");
    let turn_context = TraceContext {
        turn_id: Some("turn-1".to_string()),
        ..context.clone()
    };

    sink.append(&TraceRecord::new(
        turn_context.clone(),
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
    ))
    .unwrap();
    sink.append(&TraceRecord::new(
        turn_context.clone(),
        TraceEvent::LlmCallStarted {
            request: TraceLlmRequest {
                model: "gpt-test".to_string(),
                model_variant: Default::default(),
                messages: Vec::new(),
                attachments: Vec::new(),
                tools: Vec::new(),
                tool_choice: "auto".to_string(),
                output_spec: None,
                stream: true,
            },
        },
    ))
    .unwrap();
    sink.append(&TraceRecord::new(
        turn_context.clone(),
        TraceEvent::LlmCallFailed {
            error: crate::TraceError {
                message: "boom".to_string(),
                retryable: false,
                terminal_reason: None,
                code: Some("test".to_string()),
                raw: None,
            },
            stream_summary: None,
            attempts: None,
        },
    ))
    .unwrap();
    sink.append(&TraceRecord::new(
        turn_context,
        TraceEvent::TurnCompleted {
            outcome: crate::TraceTurnOutcome::Failed {
                done_reason: crate::TraceTurnFailureReason::ProviderError,
            },
        },
    ))
    .unwrap();

    assert!(sink.active.lock_recover().is_empty());
}

#[test]
fn failed_language_execution_yields_error_span() {
    use crate::{
        TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload,
        TraceLanguageExecutionStatus, TraceRuntimeScope, TraceRuntimeSubject,
    };
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let tracer = provider.tracer("test");
    let sink = OtelTraceSink::new(tracer);

    let identity = TraceLanguageExecutionIdentity {
        scope: TraceRuntimeScope::new("s1"),
        subject: TraceRuntimeSubject::Process {
            process_id: "p1".to_string(),
        },
        module_ref: "module".to_string(),
        entry_kind: "process".to_string(),
        entry_ref: Some("component:0".to_string()),
        entry_name: "main".to_string(),
    };

    // 1. Failed node execution
    let failed_node = TraceRecord::new(
        TraceContext::default().for_session("s1"),
        TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event: TraceLanguageExecution {
                event_key: "process:p1:node:n1:1:failed".to_string(),
                identity: identity.clone(),
                payload: TraceLanguageExecutionPayload::NodeFailed {
                    node_id: "n1".to_string(),
                    node_kind: "resource_operation".to_string(),
                    label: "eval".to_string(),
                    occurrence: 1,
                    error: "syntax error".to_string(),
                },
            },
        },
    );
    sink.append(&failed_node).unwrap();

    // 2. Failed execution finished
    let failed_execution = TraceRecord::new(
        TraceContext::default().for_session("s1"),
        TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event: TraceLanguageExecution {
                event_key: "process:p1:finished".to_string(),
                identity,
                payload: TraceLanguageExecutionPayload::ExecutionFinished {
                    status: TraceLanguageExecutionStatus::Failed,
                    error: Some("execution crashed".to_string()),
                },
            },
        },
    );
    sink.append(&failed_execution).unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 2);

    assert_eq!(
        spans[0].status,
        opentelemetry::trace::Status::error("syntax error")
    );
    assert_eq!(
        spans[1].status,
        opentelemetry::trace::Status::error("execution crashed")
    );
}

/// FIG-1758: a cancelled turn is a deliberate stop, not a failure. The
/// exporter's failure predicate matches on the typed outcome, so the turn
/// span closes `Ok` and carries its cancellation evidence, while a failed
/// turn on the same path still closes `Error`.
#[test]
fn cancelled_turn_is_not_exported_as_failed() {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let sink = OtelTraceSink::new(provider.tracer("test"));

    let cancelled_outcome = crate::TraceTurnOutcome::Cancelled {
        evidence: crate::TraceTurnCancellationEvidence {
            request_id: "cancel-req-1".to_string(),
            origin: Some("host-console".to_string()),
            reason: Some("operator stopped the turn".to_string()),
        },
    };
    assert!(
        !TraceEvent::TurnCompleted {
            outcome: cancelled_outcome.clone(),
        }
        .is_failed(),
        "a cancelled turn must not satisfy the shared failure predicate"
    );

    let cancelled_context = TraceContext::default()
        .for_session("session-cancel")
        .for_turn("turn-cancel");
    sink.append(&TraceRecord::new(
        cancelled_context.clone(),
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
    ))
    .unwrap();
    sink.append(&TraceRecord::new(
        cancelled_context,
        TraceEvent::TurnCompleted {
            outcome: cancelled_outcome,
        },
    ))
    .unwrap();

    let failed_context = TraceContext::default()
        .for_session("session-failed")
        .for_turn("turn-failed");
    sink.append(&TraceRecord::new(
        failed_context.clone(),
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
    ))
    .unwrap();
    sink.append(&TraceRecord::new(
        failed_context,
        TraceEvent::TurnCompleted {
            outcome: crate::TraceTurnOutcome::Failed {
                done_reason: crate::TraceTurnFailureReason::ProviderError,
            },
        },
    ))
    .unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 2, "one span per completed turn");

    assert_eq!(
        spans[0].status,
        opentelemetry::trace::Status::Unset,
        "cancelled turn span must not be exported with an error status"
    );
    assert!(
        !matches!(spans[0].status, opentelemetry::trace::Status::Error { .. }),
        "cancelled turn span must not carry an error status"
    );
    let attribute = |span: &opentelemetry_sdk::trace::SpanData, key: &str| {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.clone())
            .unwrap_or_else(|| panic!("missing OTel attribute {key}"))
    };
    assert_eq!(
        attribute(&spans[0], "lash.turn.status"),
        OtelValue::String("cancelled".into())
    );
    assert_eq!(
        attribute(&spans[0], "lash.turn.cancellation.request_id"),
        OtelValue::String("cancel-req-1".into())
    );
    assert_eq!(
        attribute(&spans[0], "lash.turn.cancellation.origin"),
        OtelValue::String("host-console".into())
    );

    assert_eq!(
        spans[1].status,
        opentelemetry::trace::Status::error("turn failed: provider_error"),
        "a genuinely failed turn still exports as an error"
    );
    assert_eq!(
        attribute(&spans[1], "lash.turn.done_reason"),
        OtelValue::String("provider_error".into())
    );
}
