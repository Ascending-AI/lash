use super::*;
use crate::telemetry::{TraceAttemptId, TraceRecordIdentity, TraceScopeOwner, TraceTransitionKind};
use crate::{TraceContext, TraceDomainCompletion, TraceLlmAttempt, TraceTurnCompletionReason};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
use registry::{ATTRIBUTES, AttributeType, METRICS, SPANS};

fn providers(
    sampler: Sampler,
) -> (
    SdkTracerProvider,
    SdkMeterProvider,
    InMemorySpanExporter,
    InMemoryMetricExporter,
) {
    let spans = InMemorySpanExporter::default();
    let metrics = InMemoryMetricExporter::default();
    (
        SdkTracerProvider::builder()
            .with_sampler(sampler)
            .with_simple_exporter(spans.clone())
            .build(),
        SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build(),
        spans,
        metrics,
    )
}
fn scope_id() -> TraceScopeId {
    TraceScopeId::admission(TraceScopeOwner::Turn {
        session_id: "session".into(),
        turn_id: "turn".into(),
    })
}
fn admit(adapter: &OtelTelemetry, cause: TraceCause) -> DurableTraceScope {
    let scope = scope_id();
    let candidate = adapter.propose(&scope, &cause);
    let anchor = candidate.anchor();
    candidate.settle(TraceCandidateOutcome::Selected);
    DurableTraceScope {
        scope,
        cause,
        anchor,
        started_at_ms: 1000,
    }
}
fn record(scope: &DurableTraceScope, event: TraceEvent, terminal_ms: u64) -> TraceRecord {
    let identity = TraceRecordIdentity::Transition {
        scope: scope.scope.clone(),
        transition: TraceTransitionKind::Terminal,
        ordinal: 0,
    };
    TraceRecord::identified(
        &identity,
        TraceContext::default(),
        event,
        chrono::DateTime::from_timestamp_millis(terminal_ms as i64).unwrap(),
    )
    .unwrap()
}
fn completed(scope: &DurableTraceScope) -> TraceRecord {
    record(
        scope,
        TraceEvent::TurnCompleted {
            outcome: TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::AssistantMessage,
            },
        },
        9000,
    )
}
fn live() -> EmissionSource {
    EmissionSource::LiveExecution {
        attempt: TraceAttemptId::new("attempt"),
    }
}
fn context(flags: u8) -> TraceCarrier {
    TraceCarrier::parse_w3c(
        &format!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-{flags:02x}"),
        Some("vendor=one,other=two"),
    )
    .unwrap()
}

#[test]
fn explicit_run_ignores_ambient_context() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let host = provider.tracer("host").start("host");
    let host_id = host.span_context().trace_id();
    let _guard = Context::new().with_span(host).attach();
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    assert_eq!(
        adapter.capture_current().unwrap().trace_id().to_bytes(),
        host_id.to_bytes()
    );
    admit(&adapter, TraceCause::Root);
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert_ne!(
        spans[0].span_context.trace_id(),
        host_id,
        "a root must not inherit ambient host context"
    );
    assert_eq!(spans[0].parent_span_id, SpanId::INVALID);
}

#[test]
fn carrier_sdk_conversion_preserves_unsampled_flags_state_and_remote_provenance() {
    for flags in [0, 1, 3, 255] {
        let original = context(flags);
        let sdk = span_context(&original).unwrap();
        assert!(sdk.is_remote());
        assert_eq!(sdk.is_sampled(), flags & 1 != 0);
        assert_eq!(carrier(&sdk), Some(original));
    }
    assert!(carrier(&SpanContext::empty_context()).is_none());
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOff);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let scope = admit(&adapter, TraceCause::Root);
    let TraceAnchor::Context(anchor) = &scope.anchor else {
        panic!("unsampled context must be retained");
    };
    assert!(!anchor.flags().is_sampled());
    adapter.project(
        &scope,
        None,
        &EmissionSource::NewTransition,
        &completed(&scope),
    );
    assert!(exporter.get_finished_spans().unwrap().is_empty());
}

#[test]
fn admission_selection_parent_links_and_dropped_candidates_are_explicit() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let cause = context(1);
    let parented = admit(&adapter, TraceCause::Parent(cause.clone()));
    let TraceAnchor::Context(anchor) = parented.anchor else {
        panic!("anchor");
    };
    assert_eq!(anchor.trace_id(), cause.trace_id());
    admit(&adapter, TraceCause::linked_to(Some(cause.clone())));
    adapter
        .propose(&scope_id(), &TraceCause::Root)
        .settle(TraceCandidateOutcome::Reused);
    drop(adapter.propose(&scope_id(), &TraceCause::Root));
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 4);
    assert_eq!(
        spans[0].parent_span_id.to_bytes(),
        cause.span_id().to_bytes()
    );
    assert_eq!(spans[1].parent_span_id, SpanId::INVALID);
    assert_ne!(
        spans[1].span_context.trace_id().to_bytes(),
        cause.trace_id().to_bytes()
    );
    assert_eq!(spans[1].links.len(), 1);
    assert_eq!(
        spans[1].links[0].span_context,
        span_context(&cause).unwrap()
    );
    assert_eq!(spans[2].name, "lash.admission.attempt");
    assert!(
        spans[2]
            .attributes
            .contains(&A::AdmissionOutcome.value("reused"))
    );
    assert!(
        spans[3]
            .attributes
            .contains(&A::AdmissionOutcome.value("refused"))
    );
}

#[test]
fn completion_uses_retained_anchor_and_times_after_adapter_recreation() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let scope = admit(&adapter, TraceCause::Root);
    let serialized = serde_json::to_string(&scope).unwrap();
    drop(adapter);
    let scope: DurableTraceScope = serde_json::from_str(&serialized).unwrap();
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let attempt = AttemptObservation {
        context: Some(context(1)),
        invocation_id: Some("delivery-2".into()),
    };
    adapter.project(
        &scope,
        Some(&attempt),
        &EmissionSource::NewTransition,
        &completed(&scope),
    );
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[1].parent_span_id, spans[0].span_context.span_id());
    assert_eq!(
        spans[1].span_context.trace_id(),
        spans[0].span_context.trace_id()
    );
    assert_eq!(spans[1].start_time, epoch_ms(1000));
    assert_eq!(spans[1].end_time, epoch_ms(9000));
    assert_eq!(
        spans[1].links[0].span_context,
        span_context(attempt.context.as_ref().unwrap()).unwrap()
    );
    assert!(
        spans[1]
            .attributes
            .contains(&A::AttemptInvocationId.value("delivery-2"))
    );
    let mut untraced = scope.clone();
    untraced.anchor = TraceAnchor::Untraced;
    adapter.project(
        &untraced,
        None,
        &EmissionSource::NewTransition,
        &completed(&untraced),
    );
    assert_eq!(exporter.get_finished_spans().unwrap().len(), 2);
}

#[test]
fn emitted_domain_shape_matches_registry() {
    for metric in [
        registry::Metric::PoolAcquireWait,
        registry::Metric::RecoveryLeader,
        registry::Metric::RecoveryTerm,
    ] {
        assert_eq!(metric.definition().ownership, registry::Ownership::Physical);
    }
    let (provider, meter, exporter, metrics) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let scope = admit(&adapter, TraceCause::Root);
    adapter.project(
        &scope,
        None,
        &EmissionSource::NewTransition,
        &completed(&scope),
    );
    let model = record(
        &scope,
        TraceEvent::LlmAttemptCompleted {
            attempt: TraceLlmAttempt {
                ordinal: 1,
                provider: Some("vendor-x".into()),
                request_model: "model-x".into(),
                response_model: Some("observed-model".into()),
                started_at_ms: Some(2500),
                ended_at_ms: Some(3000),
                outcome: TraceLlmAttemptOutcome::Completed,
                error: None,
                usage: Some(TraceTokenUsage {
                    input_tokens: 10,
                    output_tokens: 4,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 2,
                    reasoning_output_tokens: 1,
                }),
            },
        },
        3000,
    );
    adapter.project(&scope, None, &live(), &model);
    adapter
        .metrics
        .runtime_tuning
        .record_provider_retry("vendor-x", "backoff");
    adapter
        .metrics
        .runtime_tuning
        .record_provider_throttle_wait("vendor-x", Duration::from_millis(4));
    adapter
        .metrics
        .runtime_tuning
        .record_session_lane_contention_wait(Duration::from_millis(1), "acquired");
    adapter
        .metrics
        .runtime_tuning
        .record_session_lane_give_up("busy");
    adapter
        .metrics
        .runtime_tuning
        .record_queued_work_wake_retry();
    adapter
        .metrics
        .runtime_tuning
        .record_postgres_pool_acquire_wait(Duration::from_millis(2), "success");
    adapter
        .metrics
        .runtime_tuning
        .record_runtime_commit_budgeted_size(3, "admitted");
    adapter.metrics.parked_work.record_park("turn", "budget");
    adapter
        .metrics
        .parked_work
        .record_count("turn", "budget", 1);
    adapter.metrics.parked_work.record_oldest_age("turn", 4);
    adapter.metrics.tool_intent.record_executed("start_process");
    adapter
        .metrics
        .tool_intent
        .record_refused("start_process", "refused");
    adapter
        .metrics
        .obligations
        .record_attempt("input", "delivered");
    adapter.metrics.obligations.record_stalled("input", 0);
    adapter
        .metrics
        .obligations
        .record_leadership("recovery", true, 1);
    adapter
        .metrics
        .generation_drain
        .record_work("1", "live_processes", 2);
    meter.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    for span in &spans {
        assert_eq!(span.instrumentation_scope.name(), "lash");
        assert_eq!(span.instrumentation_scope.version(), Some("1.0"));
        assert!(span.instrumentation_scope.schema_url().is_none());
        assert!(
            SPANS
                .iter()
                .any(|definition| span.name.as_ref() == definition.name
                    || span.name.starts_with(&format!("{} ", definition.name)))
        );
        for attr in &span.attributes {
            let definition = ATTRIBUTES
                .iter()
                .find(|definition| definition.key == attr.key.as_str())
                .expect("registered attribute");
            assert!(matches!(
                (definition.value_type, &attr.value),
                (AttributeType::String, opentelemetry::Value::String(_))
                    | (AttributeType::Integer, opentelemetry::Value::I64(_))
                    | (AttributeType::Boolean, opentelemetry::Value::Bool(_))
            ));
        }
    }
    assert_eq!(spans[2].name, "chat model-x");
    assert_eq!(spans[2].span_kind, opentelemetry::trace::SpanKind::Client);
    assert_eq!(spans[2].start_time, epoch_ms(2500));
    assert_eq!(spans[2].end_time, epoch_ms(3000));
    assert!(
        spans[2]
            .attributes
            .contains(&A::ProviderName.value("vendor-x"))
    );
    assert!(
        spans[2]
            .attributes
            .contains(&A::ResponseModel.value("observed-model"))
    );
    assert!(spans[2].attributes.contains(&A::InputTokens.value(10_i64)));
    assert!(
        spans[2]
            .attributes
            .contains(&A::CacheReadTokens.value(3_i64))
    );
    assert!(
        spans[2]
            .attributes
            .contains(&A::CacheWriteTokens.value(2_i64))
    );
    assert!(
        !spans[2]
            .attributes
            .iter()
            .any(|a| a.key.as_str().starts_with("lash.payload"))
    );
    let exported = metrics.get_finished_metrics().unwrap();
    let mut definitions = Vec::new();
    for resource in &exported {
        for scope in resource.scope_metrics() {
            assert_eq!(scope.scope().name(), "lash");
            assert_eq!(scope.scope().version(), Some("1.0"));
            for metric in scope.metrics() {
                definitions.push((metric.name(), metric.unit()));
            }
        }
    }
    let mut expected: Vec<_> = METRICS
        .iter()
        .map(|metric| (metric.name, metric.unit))
        .collect();
    definitions.sort_unstable();
    expected.sort_unstable();
    assert_eq!(definitions, expected);
    assert!(registry::contract_markdown().contains("cache_write.input_tokens"));
}

struct EnrichmentSpy(Arc<std::sync::atomic::AtomicUsize>);
impl OtelSpanEnricher for EnrichmentSpy {
    fn attributes(&self, _: &TraceRecord, _: &mut Vec<KeyValue>) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
#[test]
fn disabled_replay_and_unsampled_paths_do_not_serialize_payloads() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOff);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let adapter = OtelTelemetry::new(
        &provider,
        &meter,
        OtelOptions {
            include_context_metadata: true,
            payloads: OtelPayloadExport::Bounded {
                max_record_bytes: 32,
                max_events: 1,
            },
            enrich: Some(Arc::new(EnrichmentSpy(count.clone()))),
        },
    );
    let scope = admit(&adapter, TraceCause::Root);
    let mut event = completed(&scope);
    event
        .context
        .metadata
        .insert("big".into(), serde_json::Value::String("x".repeat(100_000)));
    adapter.project(&scope, None, &EmissionSource::NewTransition, &event);
    assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(exporter.get_finished_spans().unwrap().is_empty());
    struct Huge<'a>(&'a std::sync::atomic::AtomicUsize);
    impl serde::Serialize for Huge<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(Some(1_000_000))?;
            for _ in 0..1_000_000 {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                sequence.serialize_element("payload")?;
            }
            sequence.end()
        }
    }
    let serializations = std::sync::atomic::AtomicUsize::new(0);
    let mut writer = payload::BoundedWriter::new(32);
    assert!(serde_json::to_writer(&mut writer, &Huge(&serializations)).is_err());
    assert!(writer.truncated);
    assert!(writer.len() <= 32);
    assert!(serializations.load(std::sync::atomic::Ordering::Relaxed) < 10);
}

#[test]
fn providers_are_isolated_and_sampling_is_decided_for_the_new_child() {
    let (first, first_meter, first_export, first_metrics) = providers(Sampler::AlwaysOn);
    let (second, second_meter, second_export, second_metrics) = providers(Sampler::AlwaysOn);
    let a = OtelTelemetry::new(&first, &first_meter, OtelOptions::default());
    let b = OtelTelemetry::new(&second, &second_meter, OtelOptions::default());
    let (unsampled, unsampled_meter, _, _) = providers(Sampler::AlwaysOff);
    let original = OtelTelemetry::new(&unsampled, &unsampled_meter, OtelOptions::default());
    let scope = admit(&original, TraceCause::Parent(context(0)));
    assert!(!scope.anchor.context().unwrap().flags().is_sampled());
    a.project(
        &scope,
        None,
        &EmissionSource::NewTransition,
        &completed(&scope),
    );
    a.metrics.tool_intent.record_executed("start_process");
    first_meter.force_flush().unwrap();
    second_meter.force_flush().unwrap();
    let recorded = first_export.get_finished_spans().unwrap();
    assert_eq!(
        recorded.len(),
        1,
        "the new child uses the new provider's sampler"
    );
    assert_eq!(
        recorded[0].parent_span_id.to_bytes(),
        scope.anchor.context().unwrap().span_id().to_bytes()
    );
    assert!(second_export.get_finished_spans().unwrap().is_empty());
    assert!(!first_metrics.get_finished_metrics().unwrap().is_empty());
    assert!(
        second_metrics
            .get_finished_metrics()
            .unwrap()
            .iter()
            .all(|r| r.scope_metrics().all(|s| s.metrics().next().is_none()))
    );
    let scope = admit(&b, TraceCause::Root);
    b.project(
        &scope,
        None,
        &EmissionSource::NewTransition,
        &completed(&scope),
    );
    assert_eq!(second_export.get_finished_spans().unwrap().len(), 2);
}

#[test]
fn payload_limits_and_permit_classes_are_enforced() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(
        &provider,
        &meter,
        OtelOptions {
            include_context_metadata: true,
            payloads: OtelPayloadExport::Bounded {
                max_record_bytes: 13,
                max_events: 1,
            },
            enrich: None,
        },
    );
    let scope = admit(&adapter, TraceCause::Root);
    let mut event = completed(&scope);
    event
        .context
        .metadata
        .insert("large".into(), serde_json::Value::String("あ".repeat(1000)));
    adapter.project(&scope, None, &live(), &event);
    assert_eq!(
        exporter.get_finished_spans().unwrap().len(),
        1,
        "a live permit does not authorize a logical terminal"
    );
    adapter.project(&scope, None, &EmissionSource::NewTransition, &event);
    let spans = exporter.get_finished_spans().unwrap();
    let bytes: usize = spans[1]
        .attributes
        .iter()
        .filter(|a| {
            [
                A::Payload.definition().key,
                A::ContextMetadata.definition().key,
            ]
            .contains(&a.key.as_str())
        })
        .map(|a| match &a.value {
            opentelemetry::Value::String(v) => v.as_str().len(),
            _ => 0,
        })
        .sum();
    assert!(bytes <= 13);
    assert!(
        spans[1]
            .attributes
            .contains(&A::PayloadTruncated.value(true))
    );
}

#[test]
fn typed_domain_completions_cover_operations_times_and_permit_classes() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let scope = admit(&adapter, TraceCause::Root);
    let operations = [
        (TraceDomainOperation::Run, DomainSpan::Run, false),
        (TraceDomainOperation::Process, DomainSpan::Process, false),
        (
            TraceDomainOperation::ProcessSegment,
            DomainSpan::ProcessSegment,
            false,
        ),
        (TraceDomainOperation::Send, DomainSpan::Send, true),
        (TraceDomainOperation::ToolIntent, DomainSpan::Intent, true),
    ];
    for (operation, span, is_live) in operations {
        let mut completion = TraceDomainCompletion::new(operation, 4000, TraceDomainStatus::Failed);
        completion.error_code = Some(crate::TraceFailureCode::provider("refused"));
        completion.intent_kind = Some("custom_intent".into());
        completion.tool_call_id = Some("call".into());
        completion.provider = Some("custom-provider".into());
        completion.model = Some("custom-model".into());
        completion.tool_name = Some("custom-tool".into());
        let event = record(&scope, TraceEvent::DomainCompleted { completion }, 8000);
        let wrong_source = if is_live {
            EmissionSource::NewTransition
        } else {
            live()
        };
        let before = exporter.get_finished_spans().unwrap().len();
        adapter.project(&scope, None, &wrong_source, &event);
        assert_eq!(exporter.get_finished_spans().unwrap().len(), before);
        let source = if is_live {
            live()
        } else {
            EmissionSource::NewTransition
        };
        adapter.project(&scope, None, &source, &event);
        let spans = exporter.get_finished_spans().unwrap();
        let emitted = spans.last().unwrap();
        assert_eq!(emitted.name, span.definition().name);
        assert_eq!(emitted.span_kind, span.definition().kind);
        assert_eq!(emitted.start_time, epoch_ms(4000));
        assert_eq!(emitted.end_time, epoch_ms(8000));
        assert_eq!(emitted.parent_span_id, spans[0].span_context.span_id());
        assert!(matches!(emitted.status, Status::Error { .. }));
        assert!(
            emitted
                .attributes
                .contains(&A::ErrorType.value("provider:refused"))
        );
        assert!(
            emitted
                .attributes
                .contains(&A::ProviderName.value("custom-provider"))
        );
        assert!(emitted.attributes.contains(&A::ToolCallId.value("call")));
        assert!(
            !emitted
                .attributes
                .iter()
                .any(|attr| attr.key.as_str().starts_with("gen_ai.usage."))
        );
    }
    assert_eq!(exporter.get_finished_spans().unwrap().len(), 6);
}

#[test]
fn provider_attempts_use_reported_identity_and_never_project_aggregate_calls() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let scope = admit(&adapter, TraceCause::Root);
    for (ordinal, served_by, outcome) in [
        (1, Some("provider-a"), TraceLlmAttemptOutcome::Failed),
        (2, Some("provider-b"), TraceLlmAttemptOutcome::Completed),
        (3, None, TraceLlmAttemptOutcome::Interrupted),
    ] {
        let event = record(
            &scope,
            TraceEvent::LlmAttemptCompleted {
                attempt: TraceLlmAttempt {
                    ordinal,
                    provider: served_by.map(str::to_owned),
                    request_model: "same-alias".into(),
                    response_model: None,
                    started_at_ms: None,
                    ended_at_ms: None,
                    outcome,
                    error: None,
                    usage: None,
                },
            },
            9000,
        );
        adapter.project(&scope, None, &EmissionSource::NewTransition, &event);
        assert_eq!(
            exporter.get_finished_spans().unwrap().len(),
            ordinal as usize
        );
        adapter.project(&scope, None, &live(), &event);
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans.last().unwrap();
        assert_eq!(span.name, "chat same-alias");
        assert_eq!(span.start_time, span.end_time);
        assert_eq!(span.start_time, epoch_ms(9000));
        assert_eq!(
            matches!(span.status, Status::Error { .. }),
            outcome == TraceLlmAttemptOutcome::Failed
        );
        let reported = span
            .attributes
            .iter()
            .find(|a| a.key.as_str() == A::ProviderName.definition().key);
        assert_eq!(
            reported.map(|a| &a.value),
            served_by
                .map(|v| opentelemetry::Value::String(v.into()))
                .as_ref()
        );
        assert!(
            !span
                .attributes
                .iter()
                .any(|a| a.key.as_str().starts_with("gen_ai.usage."))
        );
    }
    let aggregate = record(
        &scope,
        TraceEvent::LlmCallFailed {
            error: crate::TraceError {
                code: None,
                retryable: false,
                terminal_reason: crate::TraceLlmTerminalReason::ProviderError,
                failure_kind: crate::TraceProviderFailureKind::Unknown,
            },
            stream_summary: None,
            attempts: None,
        },
        9000,
    );
    adapter.project(&scope, None, &live(), &aggregate);
    assert_eq!(exporter.get_finished_spans().unwrap().len(), 4);
}

#[test]
fn every_admission_kind_has_registered_name_kind_and_first_writer_outcome() {
    use crate::telemetry::TraceScopeOwner;
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let owners = [
        TraceScopeOwner::Run {
            session_id: "s".into(),
            run: "r".into(),
        },
        TraceScopeOwner::Turn {
            session_id: "s".into(),
            turn_id: "t".into(),
        },
        TraceScopeOwner::Tool {
            session_id: "s".into(),
            turn_id: "t".into(),
            call_id: "c".into(),
        },
        TraceScopeOwner::ToolIntent {
            owner: lash_sansio::RuntimeOwner::Session("s".into()),
            replay_key: "key".into(),
        },
        TraceScopeOwner::Process {
            process_id: lash_sansio::ProcessId::fixture("p"),
        },
        TraceScopeOwner::TriggerOccurrence {
            occurrence_id: "fire".into(),
        },
    ];
    for owner in owners {
        let id = TraceScopeId::admission(owner);
        let selected = adapter.propose(&id, &TraceCause::Root);
        let retained = selected.anchor();
        selected.settle(TraceCandidateOutcome::Selected);
        let losing = adapter.propose(&id, &TraceCause::Root);
        assert_ne!(retained, losing.anchor());
        losing.settle(TraceCandidateOutcome::Reused);
        let spans = exporter.get_finished_spans().unwrap();
        let pair = &spans[spans.len() - 2..];
        assert_eq!(pair[0].name, admitted(id.kind()).definition().name);
        assert_eq!(pair[0].span_kind, admitted(id.kind()).definition().kind);
        assert_eq!(pair[1].name, "lash.admission.attempt");
        assert!(
            pair[1]
                .attributes
                .contains(&A::AdmissionOutcome.value("reused"))
        );
    }
    assert_eq!(exporter.get_finished_spans().unwrap().len(), 12);
}

#[test]
fn tool_wait_and_code_completions_use_explicit_scope_and_local_leaf_durations() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let scope = admit(&adapter, TraceCause::Root);
    let events = [
        (
            TraceEvent::ToolCallCompleted {
                call_id: lash_sansio::ToolCallId::fixture("call"),
                provider_call_id: None,
                name: "unfamiliar-tool".into(),
                args: serde_json::json!({"secret":"hidden"}),
                output: crate::TraceToolCallOutput {
                    outcome: crate::TraceToolCallOutcome::Failure(serde_json::Value::Null),
                    control: None,
                },
                duration_ms: 55,
                issuing_node_id: None,
                attempts: None,
            },
            EmissionSource::NewTransition,
            "execute_tool unfamiliar-tool",
            1000,
        ),
        (
            TraceEvent::DurableWaitResolved {
                wait_kind: "custom-wait".into(),
                resolution: crate::TraceDurableWaitResolution::Failed,
            },
            EmissionSource::NewTransition,
            "lash.wait",
            1000,
        ),
        (
            TraceEvent::DurableTimerResolved {
                duration_ms: 200,
                status: crate::TraceDurableTimerStatus::Failed,
            },
            EmissionSource::NewTransition,
            "lash.wait",
            8800,
        ),
        (
            TraceEvent::ExecCodeCompleted {
                duration_ms: 300,
                output: "secret".into(),
                output_chars: 6,
                observation_count: 0,
                observation_projections: Vec::new(),
                error: None,
                terminal_finish: None,
                tool_calls: Vec::new(),
            },
            live(),
            "lash.exec_code",
            8700,
        ),
    ];
    for (event, source, name, start) in events {
        let event = record(&scope, event, 9000);
        adapter.project(&scope, None, &source, &event);
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans.last().unwrap();
        assert_eq!(span.name, name);
        assert_eq!(span.start_time, epoch_ms(start));
        assert_eq!(span.end_time, epoch_ms(9000));
        assert_eq!(
            matches!(span.status, Status::Error { .. }),
            event.event.is_failed()
        );
        assert!(
            !span
                .attributes
                .iter()
                .any(|a| a.key.as_str() == A::Payload.definition().key)
        );
    }
    assert_eq!(exporter.get_finished_spans().unwrap().len(), 5);
}
