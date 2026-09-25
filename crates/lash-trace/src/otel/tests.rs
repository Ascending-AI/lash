//! Unit tests for the OpenTelemetry bridge.
//!
//! These live in their own file so `otel.rs` carries only the exporter itself.

use lash_sansio::ProcessId;
use lash_sansio::TurnId;
use opentelemetry::trace::noop::NoopTracerProvider;

use super::*;
use crate::{TraceEvent, TraceLlmRequest, TraceRecord};

fn attribute_value<'a>(attrs: &'a [KeyValue], key: &str) -> &'a OtelValue {
    attrs
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| &attribute.value)
        .unwrap_or_else(|| panic!("missing OTel attribute {key}"))
}

#[test]
fn wait_facts_export_closed_otel_attributes() {
    let identity = crate::TraceLanguageExecutionIdentity {
        scope: crate::TraceRuntimeScope::none(),
        subject: crate::TraceRuntimeSubject::Process {
            process_id: ProcessId::from("process-1"),
        },
        source_identity: "source".to_string(),
        module_ref: "module".to_string(),
        entry_kind: "process".to_string(),
        entry_ref: None,
        entry_name: "main".to_string(),
        engine_execution_id: None,
        generation: None,
    };
    let record = |payload| {
        TraceRecord::new(
            TraceContext::default(),
            TraceEvent::LanguageExecution {
                language: "lashlang".to_string(),
                event: crate::TraceLanguageExecution {
                    event_key: "event".to_string(),
                    identity: identity.clone(),
                    payload,
                },
            },
        )
    };
    let waiting = event_attributes(
        &record(crate::TraceLanguageExecutionPayload::NodeWaiting {
            node_id: "node".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::ResourceOperation,
            label: "tool".to_string(),
            occurrence: 2,
            awaited: crate::TraceNodeAwaited::EffectGroup {
                group_key: "group-key".to_string(),
                position: 3,
                wake: lash_sansio::GroupWakePolicy::FirstSuccess,
            },
        }),
        &OtelTraceOptions::default(),
    );
    assert_eq!(
        attribute_value(&waiting, "lash.language_execution.wait_kind"),
        &OtelValue::String("effect_group".into())
    );
    assert_eq!(
        attribute_value(&waiting, "lash.language_execution.awaited_group_key"),
        &OtelValue::String("group-key".into())
    );
    assert_eq!(
        attribute_value(&waiting, "lash.language_execution.awaited_position"),
        &OtelValue::I64(3)
    );
    assert_eq!(
        attribute_value(&waiting, "lash.language_execution.wake_policy"),
        &OtelValue::String("first_success".into())
    );
}

#[test]
fn correlation_fields_are_exported_as_otel_attributes() {
    let identity = crate::TraceLanguageExecutionIdentity {
        scope: crate::TraceRuntimeScope::new("session-1"),
        subject: crate::TraceRuntimeSubject::Process {
            process_id: ProcessId::from("process-1"),
        },
        source_identity: "source-1".to_string(),
        module_ref: "module-1".to_string(),
        entry_kind: "process".to_string(),
        entry_ref: Some("component:0".to_string()),
        entry_name: "main".to_string(),
        engine_execution_id: Some("invocation-1".to_string()),
        generation: Some(crate::TraceLanguageExecutionGeneration::new(2, 3)),
    };
    let language_record = TraceRecord::new(
        TraceContext::default(),
        TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event: crate::TraceLanguageExecution {
                event_key: "process:process-1:node:node-1:1:started".to_string(),
                identity,
                payload: crate::TraceLanguageExecutionPayload::NodeStarted {
                    node_id: "node-1".to_string(),
                    node_kind: lash_sansio::ExecutionNodeKind::ResourceOperation,
                    label: "tool".to_string(),
                    occurrence: 1,
                    call_id: Some("call-1".to_string()),
                },
            },
        },
    );
    let language_attrs = event_attributes(&language_record, &OtelTraceOptions::default());
    assert_eq!(
        attribute_value(&language_attrs, "lash.language_execution.source_identity"),
        &OtelValue::String("source-1".into())
    );
    assert_eq!(
        attribute_value(
            &language_attrs,
            "lash.language_execution.engine_execution_id"
        ),
        &OtelValue::String("invocation-1".into())
    );
    assert_eq!(
        attribute_value(&language_attrs, "lash.language_execution.call_id"),
        &OtelValue::String("call-1".into())
    );
    assert_eq!(
        attribute_value(&language_attrs, "lash.language_execution.attempt"),
        &OtelValue::I64(2)
    );
    assert_eq!(
        attribute_value(&language_attrs, "lash.language_execution.incarnation"),
        &OtelValue::I64(3)
    );

    for event in [
        TraceEvent::ToolCallStarted {
            call_id: Some("call-1".to_string()),
            name: "search".to_string(),
            args: serde_json::json!({}),
            issuing_node_id: Some("node-1".to_string()),
        },
        TraceEvent::ToolCallCompleted {
            call_id: Some("call-1".to_string()),
            name: "search".to_string(),
            args: serde_json::json!({}),
            output: crate::TraceToolCallOutput {
                outcome: crate::TraceToolCallOutcome::Success(serde_json::json!({})),
                control: None,
            },
            duration_ms: 1,
            issuing_node_id: Some("node-1".to_string()),
            attempts: None,
        },
    ] {
        let attrs = event_attributes(
            &TraceRecord::new(TraceContext::default(), event),
            &OtelTraceOptions::default(),
        );
        assert_eq!(
            attribute_value(&attrs, "lash.tool.issuing_node_id"),
            &OtelValue::String("node-1".into())
        );
    }
}

#[test]
fn node_failure_provenance_is_exported_as_typed_attributes() {
    let identity = crate::TraceLanguageExecutionIdentity {
        scope: crate::TraceRuntimeScope::new("s1"),
        subject: crate::TraceRuntimeSubject::Process {
            process_id: ProcessId::from("p1"),
        },
        source_identity: "source".into(),
        module_ref: "module".into(),
        entry_kind: "process".into(),
        entry_ref: None,
        entry_name: "main".into(),
        engine_execution_id: None,
        generation: Some(crate::TraceLanguageExecutionGeneration::new(2, 4)),
    };
    let record = |failure| {
        TraceRecord::new(
            TraceContext::default(),
            TraceEvent::LanguageExecution {
                language: "lashlang".into(),
                event: crate::TraceLanguageExecution {
                    event_key: "node-failed".into(),
                    identity: identity.clone(),
                    payload: crate::TraceLanguageExecutionPayload::NodeFailed {
                        node_id: "node".into(),
                        node_kind: "resource_operation".into(),
                        label: "read".into(),
                        occurrence: 1,
                        call_id: Some("effect-1".into()),
                        failure,
                    },
                },
            },
        )
    };
    let effect = record(crate::TraceLanguageExecutionFailure::Effect {
        class: lash_sansio::ToolFailureClass::PermissionDenied,
        code: "approval_denied".into(),
        message: "denied".into(),
        replay_key: "effect-1".into(),
        source: lash_sansio::ToolFailureSource::Policy,
        retry: lash_sansio::ToolRetryStatus::Exhausted { attempts: 3 },
    });
    let attrs = event_attributes(&effect, &OtelTraceOptions::default());
    for (key, expected) in [
        ("lash.language_execution.failure.kind", "effect"),
        ("lash.language_execution.failure.class", "permission_denied"),
        ("lash.language_execution.failure.code", "approval_denied"),
        ("lash.language_execution.failure.message", "denied"),
        ("lash.language_execution.failure.replay_key", "effect-1"),
        ("lash.language_execution.failure.source", "policy"),
        ("lash.language_execution.failure.retry", "exhausted"),
    ] {
        assert_eq!(
            attribute_value(&attrs, key),
            &OtelValue::String(expected.into())
        );
    }
    assert_eq!(
        attribute_value(&attrs, "lash.language_execution.failure.retry_attempts"),
        &OtelValue::I64(3)
    );
    assert_eq!(
        attribute_value(&attrs, "lash.language_execution.attempt"),
        &OtelValue::I64(2)
    );
    assert_eq!(
        attribute_value(&attrs, "lash.language_execution.incarnation"),
        &OtelValue::I64(4)
    );

    let runtime = record(crate::TraceLanguageExecutionFailure::Runtime {
        code: "VmStackUnderflow".into(),
        message: "vm stack underflow".into(),
    });
    let attrs = event_attributes(&runtime, &OtelTraceOptions::default());
    assert_eq!(
        attribute_value(&attrs, "lash.language_execution.failure.kind"),
        &OtelValue::String("runtime".into())
    );
    assert_eq!(
        attribute_value(&attrs, "lash.language_execution.failure.code"),
        &OtelValue::String("VmStackUnderflow".into())
    );
    assert!(
        !attrs
            .iter()
            .any(|attr| attr.key.as_str() == "lash.language_execution.failure.retry")
    );
}

#[test]
fn typed_exec_diagnostics_preserve_the_otel_span_family() {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let sink = OtelTraceSink::new(provider.tracer("test"));

    let events = [
        TraceEvent::ExecCodeStarted {
            code: "print(1)".to_string(),
            code_chars: 8,
        },
        TraceEvent::ExecCodeCompleted {
            duration_ms: 3,
            output: "1".to_string(),
            output_chars: 1,
            observation_count: 1,
            observation_projections: Vec::new(),
            error: None,
            terminal_finish: None,
            tool_calls: Vec::new(),
        },
        TraceEvent::ExecCodeFailed {
            reason: crate::ExecCodeFailureReason::RuntimeStopped,
            error: "boom".to_string(),
        },
        TraceEvent::ObservationProjection {
            projections: Vec::new(),
        },
    ];
    for event in events {
        sink.append(&TraceRecord::new(TraceContext::default(), event))
            .unwrap();
    }

    let spans = exporter.get_finished_spans().unwrap();
    let names = spans
        .iter()
        .map(|span| span.name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "lash.exec_code",
            "lash.exec_code",
            "lash.exec_code",
            "lash.observation_projection"
        ]
    );
}

#[test]
fn typed_exec_diagnostic_attributes_keep_the_protocol_otel_contract() {
    let record = TraceRecord::new(
        TraceContext::default(),
        TraceEvent::ExecCodeCompleted {
            duration_ms: 3,
            output: "1".to_string(),
            output_chars: 1,
            observation_count: 1,
            observation_projections: Vec::new(),
            error: None,
            terminal_finish: None,
            tool_calls: Vec::new(),
        },
    );
    let attrs = event_attributes(
        &record,
        &OtelTraceOptions {
            include_payload_json: true,
            ..OtelTraceOptions::default()
        },
    );
    let value = |key: &str| {
        attrs
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| attribute.value.to_string())
            .unwrap_or_else(|| panic!("missing OTel attribute {key}"))
    };

    assert_eq!(value("lash.protocol.plugin_id"), "runtime");
    assert_eq!(value("lash.protocol.diagnostic_phase"), record.event.kind());
    let payload = value("lash.protocol.payload_json");
    assert!(payload.contains(record.event.kind()));
    assert!(!payload.contains("tool_call_count"));
    assert!(!payload.contains("terminal_finish_present"));
}

/// FIG-2362: the closed failure reason survives into the OTel payload beside
/// the human error text.
#[test]
fn exec_code_failed_otel_payload_carries_the_typed_reason() {
    let record = TraceRecord::new(
        TraceContext::default(),
        TraceEvent::ExecCodeFailed {
            reason: crate::ExecCodeFailureReason::ExecutorUnavailable,
            error: "code execution is not available in this session".to_string(),
        },
    );
    let attrs = event_attributes(
        &record,
        &OtelTraceOptions {
            include_payload_json: true,
            ..OtelTraceOptions::default()
        },
    );
    let payload = attrs
        .iter()
        .find(|attribute| attribute.key.as_str() == "lash.protocol.payload_json")
        .map(|attribute| attribute.value.to_string())
        .expect("missing OTel payload attribute");
    assert!(payload.contains("\"reason\":\"executor_unavailable\""));
    assert!(payload.contains("\"error\":\"code execution is not available in this session\""));
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
        turn_id: Some(TurnId::from("turn-1")),
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
                failure_kind: None,
                code: Some("test".to_string()),
                code_namespace: None,
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

/// FIG-3435: OTel `error.type` carries the failure kind, not the terminal
/// reason — a timeout exports `timeout` even though its terminal reason is
/// `provider_error`, and an unknown kind demotes to `_OTHER`. The
/// `lash.error.code` attribute carries the code's spelling alone.
#[test]
fn llm_call_failed_exports_failure_kind_and_spelling_only_code() {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let sink = OtelTraceSink::new(provider.tracer("test"));
    let context = TraceContext::default().for_session("session-1");

    sink.append(&TraceRecord::new(
        context.clone(),
        TraceEvent::LlmCallFailed {
            error: crate::TraceError {
                message: "timed out".to_string(),
                retryable: true,
                terminal_reason: Some("provider_error".to_string()),
                failure_kind: Some("timeout".to_string()),
                code: Some("insufficient_quota".to_string()),
                code_namespace: Some("provider".to_string()),
                raw: None,
            },
            stream_summary: None,
            attempts: None,
        },
    ))
    .unwrap();
    sink.append(&TraceRecord::new(
        context,
        TraceEvent::LlmCallFailed {
            error: crate::TraceError {
                message: "no kind".to_string(),
                retryable: false,
                terminal_reason: Some("provider_error".to_string()),
                failure_kind: None,
                code: None,
                code_namespace: None,
                raw: None,
            },
            stream_summary: None,
            attempts: None,
        },
    ))
    .unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 2);
    let attribute = |span: &opentelemetry_sdk::trace::SpanData, key: &str| {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.clone())
            .unwrap_or_else(|| panic!("missing OTel attribute {key}"))
    };
    assert_eq!(
        attribute(&spans[0], "error.type"),
        OtelValue::String("timeout".into()),
        "a timeout must export its kind, not the provider_error terminal reason"
    );
    assert_eq!(
        attribute(&spans[0], "lash.error.code"),
        OtelValue::String("insufficient_quota".into()),
        "the code attribute carries the spelling alone, not provider:insufficient_quota"
    );
    assert_eq!(
        attribute(&spans[1], "error.type"),
        OtelValue::String("_OTHER".into()),
        "an absent failure kind demotes to _OTHER"
    );
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
            process_id: ProcessId::from("p1".to_string()),
        },
        source_identity: "source".to_string(),
        module_ref: "module".to_string(),
        entry_kind: "process".to_string(),
        entry_ref: Some("component:0".to_string()),
        entry_name: "main".to_string(),
        engine_execution_id: None,
        generation: None,
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
                    node_kind: lash_sansio::ExecutionNodeKind::ResourceOperation,
                    label: "eval".to_string(),
                    occurrence: 1,
                    call_id: None,
                    failure: crate::TraceLanguageExecutionFailure::Runtime {
                        code: "InvalidJson".to_string(),
                        message: "syntax error".to_string(),
                    },
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

#[test]
fn rlm_step_spans_keep_diagnostics_and_existing_llm_span() {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    // Default options deliberately omit payload JSON: the diagnostic must
    // remain visible to ordinary production collectors.
    let sink = OtelTraceSink::new(provider.tracer("test"));
    let context = TraceContext::default().for_session("s1").for_turn("t1");
    sink.append(&TraceRecord::new(
        context.clone(),
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
    ))
    .unwrap();
    let llm_context = context.clone().for_llm_call("llm-1");
    sink.append(&TraceRecord::new(
        llm_context.clone(),
        TraceEvent::LlmCallStarted {
            request: TraceLlmRequest {
                model: "test-model".into(),
                model_variant: None,
                messages: vec![],
                tools: vec![],
                tool_choice: "auto".into(),
                output_spec: None,
                stream: true,
            },
        },
    ))
    .unwrap();
    sink.append(&TraceRecord::new(
        llm_context,
        TraceEvent::LlmCallCompleted {
            response: crate::TraceLlmResponse {
                text: "inbox.send_item({})".into(),
                duration_ms: 5,
                request_model: "test-model".into(),
                terminal_reason: None,
                parts: None,
                generation_disposition: None,
            },
            usage: None,
            provider_usage: None,
            stream_summary: None,
            attempts: None,
        },
    ))
    .unwrap();
    for (step_index, outcome) in [
        (
            1,
            crate::TraceRlmStepOutcome::Failure {
                diagnostic: "operation send_item expects { body: str }, got {}".into(),
            },
        ),
        (2, crate::TraceRlmStepOutcome::Ok),
    ] {
        sink.append(&TraceRecord::new(
            context.clone(),
            TraceEvent::RlmStep {
                step_index,
                outcome,
            },
        ))
        .unwrap();
    }
    sink.append(&TraceRecord::new(
        context,
        TraceEvent::TurnCompleted {
            outcome: crate::TraceTurnOutcome::Failed {
                done_reason: crate::TraceTurnFailureReason::MaxTurns,
            },
        },
    ))
    .unwrap();
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let turn = spans.iter().find(|s| s.name == "lash.turn").unwrap();
    // The current exporter names the existing LLM span `lash.llm`.
    let llm: Vec<_> = spans.iter().filter(|s| s.name == "lash.llm").collect();
    assert_eq!(llm.len(), 1);
    assert_eq!(llm[0].parent_span_id, turn.span_context.span_id());
    assert!(!matches!(llm[0].status, Status::Error { .. }));
    let steps: Vec<_> = spans.iter().filter(|s| s.name == "lash.rlm.step").collect();
    assert_eq!(steps.len(), 2);
    for (index, step) in steps.iter().enumerate() {
        assert_eq!(step.parent_span_id, turn.span_context.span_id());
        let attribute = |key: &str| {
            &step
                .attributes
                .iter()
                .find(|a| a.key.as_str() == key)
                .unwrap()
                .value
        };
        assert_eq!(
            attribute("lash.rlm.step.index"),
            &OtelValue::I64(index as i64 + 1)
        );
        assert_eq!(
            attribute("lash.rlm.step.outcome").to_string(),
            if index == 0 { "failure" } else { "ok" }
        );
        if index == 0 {
            assert_eq!(
                attribute("error.message").to_string(),
                "operation send_item expects { body: str }, got {}"
            );
            assert!(matches!(step.status, Status::Error { .. }));
        } else {
            assert!(!matches!(step.status, Status::Error { .. }));
        }
    }
}
