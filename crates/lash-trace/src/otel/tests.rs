use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use super::*;
use crate::telemetry::{
    SpanKey, TraceLinks, TraceRecordIdentity, TraceScopeOwner, TraceTransitionKind, derive_span,
};
use crate::{
    ProducerOutcome, SpanRole, TraceContext, TraceDomainCompletion, TraceIds, TraceTimestamp,
    TraceTurnCompletionReason,
};
use lash_protocol_types::{
    llm::types::AttemptOutcome, llm::types::AttemptRecord, llm::types::ExecutionEvidence,
    llm::types::ProtocolPosition,
};

use opentelemetry::trace::SpanKind;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, RandomIdGenerator, Sampler, SamplingResult, SdkTracerProvider,
    ShouldSample,
};
use registry::{ATTRIBUTES, AttributeType, METRICS, SPANS};

fn providers(
    sampler: impl ShouldSample + 'static,
) -> (
    LashTracerProvider,
    SdkMeterProvider,
    InMemorySpanExporter,
    InMemoryMetricExporter,
) {
    let spans = InMemorySpanExporter::default();
    let metrics = InMemoryMetricExporter::default();
    (
        LashTracerProvider::new(
            SdkTracerProvider::builder().with_simple_exporter(spans.clone()),
            sampler,
            RandomIdGenerator::default(),
        ),
        SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build(),
        spans,
        metrics,
    )
}
fn epoch_ms(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}
fn scope_id() -> TraceScopeId {
    TraceScopeId::admission(TraceScopeOwner::Turn {
        session_id: "session".into(),
        turn_id: "turn".into(),
    })
}
/// A caller's or producer's context with `flags`.
fn context(flags: u8) -> TraceCarrier {
    TraceCarrier::parse_w3c(
        &format!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-{flags:02x}"),
        Some("vendor=one,other=two"),
    )
    .unwrap()
}
/// `carrier` with the span id `span`.
fn with_span(carrier: &TraceCarrier, span: W3cSpanId) -> TraceCarrier {
    TraceCarrier::new(
        carrier.trace_id(),
        span,
        carrier.flags(),
        carrier.tracestate().clone(),
    )
}
/// The sampled producer the tests' turns are caused by.
fn producer() -> TraceCarrier {
    context(1)
}
/// The turn span `scope` derives under `parent`.
fn derived(parent: &TraceCarrier, scope: &TraceScopeId) -> TraceCarrier {
    derive_span(parent, &SpanKey::Scope { scope })
}
/// A `kind` span's position as `role`, under `parent`, owned by `scope`,
/// from `started` to `ended`.
fn position(
    kind: TraceSpanKind,
    role: SpanRole,
    span: TraceCarrier,
    parent: Option<TraceCarrier>,
    scope: &TraceScopeId,
    (started, ended): (u64, u64),
) -> TracePosition {
    TracePosition {
        span,
        parent,
        links: TraceLinks::new(),
        role,
        kind,
        started_at: TraceTimestamp::from_unix_ms(started),
        ended_at: role
            .publishes()
            .then(|| TraceTimestamp::from_unix_ms(ended)),
        scope: Some(scope.clone()),
        attempt: None,
    }
}
/// A record as built: what a host that captures content hands over.
fn captured_record(event: TraceEvent, position: TracePosition) -> TraceRecord {
    let identity = TraceRecordIdentity::Transition {
        scope: position.scope.clone().unwrap_or_else(scope_id),
        transition: TraceTransitionKind::Terminal,
        ordinal: 0,
    };
    let at = position.ended_at.unwrap_or(position.started_at);
    TraceRecord::identified(&identity, TraceContext::default(), event, at)
        .unwrap()
        .at(Some(position))
}
/// A record as the runtime hands it over under the standard content policy.
fn record(event: TraceEvent, position: TracePosition) -> TraceRecord {
    captured_record(event, position).governed(crate::ContentPolicy::standard())
}
/// A `kind` leaf or container's closing record under the turn of
/// [`producer`], from `started` to 9000.
fn closing(
    kind: TraceSpanKind,
    span: TraceCarrier,
    started: u64,
    event: TraceEvent,
) -> TraceRecord {
    let turn = derived(&producer(), &scope_id());
    record(
        event,
        position(
            kind,
            SpanRole::Closes,
            span,
            Some(turn),
            &scope_id(),
            (started, 9000),
        ),
    )
}
/// The completed turn of `scope` under `parent`, closing from 1000 to 9000.
fn completed_under(parent: &TraceCarrier, scope: &TraceScopeId) -> TraceRecord {
    record(
        TraceEvent::TurnCompleted {
            output: Vec::new(),
            input: Vec::new(),
            inference_calls: None,
            tool_calls: None,
            inputs: 1,
            outcome: TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::AssistantMessage,
            },
        },
        position(
            TraceSpanKind::Agent,
            SpanRole::Closes,
            derived(parent, scope),
            Some(parent.clone()),
            scope,
            (1000, 9000),
        ),
    )
}
fn completed() -> TraceRecord {
    completed_under(&producer(), &scope_id())
}
fn settled(kind: ProducerKind, outcome: ProducerOutcome) -> TraceEvent {
    TraceEvent::ProducerSettled {
        producer: kind,
        outcome,
        phases: Vec::new(),
    }
}
fn spec(
    kind: ProducerKind,
    caller: Option<TraceCarrier>,
    link_mode: TraceLinkMode,
) -> ProducerSpec {
    ProducerSpec {
        kind,
        caller,
        link_mode,
        ids: TraceIds::default(),
    }
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
}

/// L11: the exporter builds a span from a sampled record's own position:
/// lash's span id, trace id and tracestate, its parent and links, and its
/// retained start and end. The host's sampler is not asked: lash's decision
/// was made at the producer. An opening record, an event, an unsampled
/// record and a record with no position build nothing.
#[test]
fn a_sampled_closing_record_exports_lash_ids_parent_links_and_times() {
    let (provider, _meter, exporter, _) = providers(Sampler::AlwaysOff);
    let adapter = OtelTelemetry::new(
        &provider,
        opentelemetry::global::meter_provider().as_ref(),
        OtelOptions::default(),
    );
    let producer = producer();
    let turn = derived(&producer, &scope_id());
    let consumed = with_span(&context(1), W3cSpanId::from_bytes([7; 8]).unwrap());
    let mut closes = completed();
    closes
        .position
        .as_mut()
        .unwrap()
        .links
        .push(consumed.clone());
    adapter.append(&closes).unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(span.name, "invoke_agent");
    assert_eq!(
        span.span_context.span_id().to_bytes(),
        turn.span_id().to_bytes()
    );
    assert_eq!(
        span.span_context.trace_id().to_bytes(),
        producer.trace_id().to_bytes()
    );
    assert_eq!(
        span.parent_span_id.to_bytes(),
        producer.span_id().to_bytes()
    );
    assert_eq!(
        span.span_context.trace_state().header(),
        "vendor=one,other=two"
    );
    assert!(span.span_context.is_sampled());
    assert_eq!(span.start_time, epoch_ms(1000));
    assert_eq!(span.end_time, epoch_ms(9000));
    assert_eq!(span.links.len(), 1);
    assert_eq!(span.links[0].span_context, span_context(&consumed).unwrap());
    assert!(
        span.attributes
            .contains(&A::RecordId.value(closes.id.clone()))
    );

    for role in [SpanRole::Opens, SpanRole::Event] {
        let mut record = completed();
        record.position.as_mut().unwrap().role = role;
        adapter.append(&record).unwrap();
    }
    adapter
        .append(&completed_under(&context(0), &scope_id()))
        .unwrap();
    adapter.append(&completed().at(None)).unwrap();
    assert_eq!(exporter.get_finished_spans().unwrap().len(), 1);

    // A producer's whole record is a root at its own preset trace id.
    let root = TraceCarrier::parse_w3c(
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        Some("vendor=x"),
    )
    .unwrap();
    for outcome in [
        ProducerOutcome::Selected,
        ProducerOutcome::Refused {
            error_type: "session_closed".into(),
        },
    ] {
        let refused = matches!(outcome, ProducerOutcome::Refused { .. });
        adapter
            .append(&record(
                settled(ProducerKind::Send, outcome.clone()),
                position(
                    TraceSpanKind::Producer,
                    SpanRole::Whole,
                    root.clone(),
                    None,
                    &scope_id(),
                    (500, 600),
                ),
            ))
            .unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans.last().unwrap();
        assert_eq!(span.name, "lash.send");
        assert_eq!(span.span_kind, SpanKind::Producer);
        assert_eq!(span.parent_span_id, SpanId::INVALID);
        assert_eq!(
            span.span_context.trace_id().to_bytes(),
            root.trace_id().to_bytes()
        );
        assert_eq!(
            span.span_context.span_id().to_bytes(),
            root.span_id().to_bytes()
        );
        assert_eq!(span.span_context.trace_state().header(), "vendor=x");
        assert!(
            span.attributes
                .contains(&A::ProducerOutcome.value(outcome.as_str()))
        );
        assert_eq!(matches!(span.status, Status::Error { .. }), refused);
        assert_eq!(
            span.attributes
                .contains(&KeyValue::new("error.type", "session_closed")),
            refused
        );
    }
}

/// A host sampler that samples `lash.send` under its own tracestate,
/// records `lash.process.start` without sampling it, drops everything
/// else, and counts its calls.
#[derive(Clone, Debug)]
struct ByName(Arc<AtomicUsize>);
impl ShouldSample for ByName {
    fn should_sample(
        &self,
        _: Option<&Context>,
        _: TraceId,
        name: &str,
        _: &SpanKind,
        _: &[KeyValue],
        _: &[Link],
    ) -> SamplingResult {
        self.0.fetch_add(1, Ordering::Relaxed);
        SamplingResult {
            decision: match name {
                "lash.send" => SamplingDecision::RecordAndSample,
                "lash.process.start" => SamplingDecision::RecordOnly,
                _ => SamplingDecision::Drop,
            },
            attributes: Vec::new(),
            trace_state: TraceState::from_key_value([("vendor", "x")]).unwrap(),
        }
    }
}

/// D7, L16: the host's sampler decides once per producer, and every lash
/// span of the trace follows its decision without asking it again.
/// `RecordOnly` and `Drop` both leave the trace unsampled. A span lash did
/// not preset still asks the host's sampler.
#[test]
fn the_host_sampler_decides_once_at_the_producer_and_lash_spans_follow() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (provider, meter, exporter, _) = providers(ByName(calls.clone()));
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let open = |kind| adapter.open_producer(&spec(kind, None, TraceLinkMode::Parent));
    let sent = open(ProducerKind::Send);
    assert!(sent.flags().is_sampled());
    assert_eq!(sent.tracestate().as_str(), "vendor=x");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let started = open(ProducerKind::ProcessStart);
    assert!(!started.flags().is_sampled(), "RecordOnly is unsampled");
    let invoked = open(ProducerKind::PluginInvoke);
    assert!(!invoked.flags().is_sampled(), "Drop is unsampled");
    assert_eq!(calls.load(Ordering::Relaxed), 3);

    for producer in [&sent, &started, &invoked] {
        adapter
            .append(&completed_under(producer, &scope_id()))
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::Relaxed), 3, "lash spans never ask");
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1, "only the sampled trace exports");
    assert_eq!(
        spans[0].span_context.trace_id().to_bytes(),
        sent.trace_id().to_bytes()
    );
    assert_eq!(spans[0].span_context.trace_state().header(), "vendor=x");

    provider.tracer("host").start("host-operation").end();
    assert_eq!(calls.load(Ordering::Relaxed), 4, "a host span asks");
    assert_eq!(exporter.get_finished_spans().unwrap().len(), 1);
}

/// A producer in parent mode joins its caller's trace; in link mode it roots
/// a new trace and links its caller. Each id is fresh.
#[test]
fn a_producer_joins_its_callers_trace_or_roots_a_linked_one() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let caller = context(1);
    let joined = adapter.open_producer(&spec(
        ProducerKind::Send,
        Some(caller.clone()),
        TraceLinkMode::Parent,
    ));
    let again = adapter.open_producer(&spec(
        ProducerKind::Send,
        Some(caller.clone()),
        TraceLinkMode::Parent,
    ));
    let linked = adapter.open_producer(&spec(
        ProducerKind::ProcessStart,
        Some(caller.clone()),
        TraceLinkMode::Link,
    ));
    assert_eq!(joined.trace_id(), caller.trace_id());
    assert_ne!(joined.span_id(), caller.span_id());
    assert_ne!(joined.span_id(), again.span_id());
    assert_ne!(linked.trace_id(), caller.trace_id());
    assert!(joined.flags().is_sampled() && linked.flags().is_sampled());

    adapter
        .append(&record(
            settled(ProducerKind::Send, ProducerOutcome::Selected),
            position(
                TraceSpanKind::Producer,
                SpanRole::Whole,
                joined.clone(),
                Some(caller.clone()),
                &scope_id(),
                (500, 600),
            ),
        ))
        .unwrap();
    let mut root = position(
        TraceSpanKind::Producer,
        SpanRole::Whole,
        linked.clone(),
        None,
        &scope_id(),
        (500, 600),
    );
    root.links.push(caller.clone());
    adapter
        .append(&record(
            settled(ProducerKind::ProcessStart, ProducerOutcome::Reused),
            root,
        ))
        .unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans[0].name, "lash.send");
    assert_eq!(
        spans[0].parent_span_id.to_bytes(),
        caller.span_id().to_bytes()
    );
    assert!(spans[0].links.is_empty());
    assert_eq!(spans[1].name, "lash.process.start");
    assert_eq!(spans[1].parent_span_id, SpanId::INVALID);
    assert_eq!(
        spans[1].span_context.trace_id().to_bytes(),
        linked.trace_id().to_bytes()
    );
    assert_eq!(spans[1].links.len(), 1);
    assert_eq!(
        spans[1].links[0].span_context,
        span_context(&caller).unwrap()
    );
}

#[test]
fn emitted_domain_shape_matches_registry() {
    assert_eq!(
        registry::Metric::PoolAcquireWait.definition().ownership,
        registry::Ownership::Physical
    );
    let (provider, meter, exporter, metrics) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    adapter.append(&completed()).unwrap();
    let usage = LlmUsage {
        input_tokens: 10,
        output_tokens: 4,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 1,
        unknown_calls: 0,
    };
    let model = closing(
        TraceSpanKind::Model,
        with_span(&producer(), adapter.leaf_span_id()),
        2500,
        TraceEvent::LlmCallCompleted {
            model: Some(crate::TraceModelCall {
                provider: Some("openai".into()),
                request_model: "model-x".into(),
                stream: false,
                reasoning: None,
                output_type: None,
                generation: Default::default(),
            }),
            request: Default::default(),
            response_metadata: Default::default(),
            response: crate::TraceLlmResponse {
                text: "answer".into(),
                duration_ms: 500,
                request_model: "model-x".into(),
                terminal_reason: None,
                parts: None,
                generation_disposition: None,
            },
            usage: Some(usage.clone()),
            provider_usage: None,
            stream_summary: None,
            attempts: Some(vec![
                AttemptRecord {
                    ordinal: 1,
                    outcome: AttemptOutcome::Completed,
                    protocol_position: ProtocolPosition::TerminalObserved,
                    retry_budget_consumed: true,
                    retry_decision: None,
                    error: None,
                    evidence: Some(ExecutionEvidence {
                        served_model: Some("observed-model".into()),
                        ..Default::default()
                    }),
                    generation_disposition: None,
                    usage: Some(usage),
                }
                .into(),
            ]),
        },
    );
    adapter.append(&model).unwrap();
    let mut model = model;
    model.position.as_mut().unwrap().ended_at = Some(TraceTimestamp::from_unix_ms(3000));
    adapter.append(&model).unwrap();
    adapter
        .metrics
        .runtime_tuning
        .record_provider_retry("vendor-x", "backoff");
    adapter
        .metrics
        .runtime_tuning
        .record_provider_terminal_delivery_timeout("vendor-x");
    adapter
        .metrics
        .runtime_tuning
        .record_provider_throttle_wait("vendor-x", Duration::from_millis(4));
    adapter
        .metrics
        .runtime_tuning
        .record_pool_acquire_wait(Duration::from_millis(2), "success");
    adapter
        .metrics
        .runtime_tuning
        .record_runtime_commit_budgeted_size(3, "admitted");
    for (label, members) in [("round.outcome", 3), ("turn.commit", 0)] {
        adapter.metrics.runtime_tuning.record_durable_commit(
            label,
            "success",
            crate::telemetry::metrics::DurableCommitCost {
                acquire_wait: Duration::from_micros(123),
                transaction_duration: Duration::from_micros(456),
                sql_statements: 7,
                returned_bytes: 89,
                lock_statement_elapsed: Duration::from_micros(34),
                group_commit_members: members,
            },
        );
    }
    adapter
        .metrics
        .parked_work
        .record_park("session", "pass_loop");
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
    metrics::emit_remaining_metrics(&adapter);
    meter.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    for span in &spans {
        assert_semconv(span);
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
                    | (AttributeType::Double, opentelemetry::Value::F64(_))
                    | (
                        AttributeType::StringArray,
                        opentelemetry::Value::Array(opentelemetry::Array::String(_))
                    )
            ));
        }
    }
    let chat = &spans[2];
    assert_eq!(chat.name, "chat model-x");
    assert_eq!(chat.span_kind, SpanKind::Client);
    assert_eq!(chat.start_time, epoch_ms(2500));
    assert_eq!(chat.end_time, epoch_ms(3000));
    assert!(
        chat.attributes
            .contains(&A::ResponseModel.value("observed-model"))
    );
    assert!(chat.attributes.contains(&A::InputTokens.value(15_i64)));
    assert!(chat.attributes.contains(&A::CacheReadTokens.value(3_i64)));
    assert!(chat.attributes.contains(&A::CacheWriteTokens.value(2_i64)));
    assert!(
        !chat
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
                if metric.name().starts_with("lash.durable.commit.") {
                    let opentelemetry_sdk::metrics::data::AggregatedMetrics::U64(
                        opentelemetry_sdk::metrics::data::MetricData::Histogram(histogram),
                    ) = metric.data()
                    else {
                        panic!("durable cost is a u64 histogram");
                    };
                    let points: Vec<_> = histogram.data_points().collect();
                    assert_eq!(points.len(), 2, "both durable labels have values");
                    for point in points {
                        assert_eq!(point.count(), 1);
                        let label = point
                            .attributes()
                            .find(|attribute| attribute.key.as_str() == "lash.durable.commit.label")
                            .expect("commit label")
                            .value
                            .to_string();
                        assert!(["round.outcome", "turn.commit"].contains(&label.as_str()));
                        let expected = match metric.name() {
                            "lash.durable.commit.acquire_wait.duration" => 123,
                            "lash.durable.commit.transaction.duration" => 456,
                            "lash.durable.commit.sql_statements" => 7,
                            "lash.durable.commit.returned_bytes" => 89,
                            "lash.durable.commit.lock_statement_elapsed" => 34,
                            "lash.durable.commit.group_commit.members" => {
                                if label == "round.outcome" { 3 } else { 0 }
                            }
                            _ => panic!("unrecognized durable cost"),
                        };
                        assert_eq!(point.sum(), expected);
                    }
                }
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

/// V23: both telemetry projections read the same typed terminal subject.
#[test]
fn domain_subject_owns_the_span_and_intent_kind_attribute() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let subjects = [
        TraceDomainSubject::Process {
            kind: "session_turn".into(),
            label: None,
        },
        TraceDomainSubject::Workflow {
            kind: "lashvm".into(),
            name: Some("example".into()),
        },
        TraceDomainSubject::WorkflowNode {
            at: lash_vocabulary::sites::testkit::effect(
                lash_vocabulary::sites::testkit::root_task(),
                lash_vocabulary::sites::Site::new(lash_vocabulary::sites::Unit::Main, [2, 0]),
                1,
                Vec::new(),
            ),
        },
    ]
    .into_iter()
    .chain(
        lash_vocabulary::ToolIntentKind::ALL
            .iter()
            .map(|kind| TraceDomainSubject::ToolIntent { kind: *kind }),
    );
    for subject in subjects {
        let kind = match &subject {
            TraceDomainSubject::Process { .. } => TraceSpanKind::Process,
            TraceDomainSubject::Workflow { .. } => TraceSpanKind::Workflow,
            TraceDomainSubject::WorkflowNode { .. } => TraceSpanKind::WorkflowNode,
            TraceDomainSubject::ToolIntent { .. } => TraceSpanKind::ToolIntent,
        };
        let completion =
            TraceDomainCompletion::new(subject.clone(), 4000, TraceDomainStatus::Completed);
        let span = derived(&producer(), &scope_id());
        adapter
            .append(&closing(
                kind,
                span,
                4000,
                TraceEvent::DomainCompleted { completion },
            ))
            .unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let emitted = spans.last().unwrap();
        assert_semconv(emitted);
        match subject {
            TraceDomainSubject::Process { .. } => {
                assert_eq!(emitted.name, "lash.process session_turn");
                assert!(
                    !emitted
                        .attributes
                        .iter()
                        .any(|attr| attr.key.as_str() == "lash.tool_intent.kind")
                );
            }
            TraceDomainSubject::Workflow { name, .. } => {
                assert_eq!(
                    emitted.name,
                    format!("invoke_workflow {}", name.as_ref().unwrap())
                );
                assert!(
                    emitted
                        .attributes
                        .contains(&A::WorkflowName.value(name.unwrap()))
                );
            }
            TraceDomainSubject::WorkflowNode { at } => {
                // A node is named by its site and keyed by its whole
                // occurrence.
                assert_eq!(
                    emitted.name,
                    format!("lash.workflow.node {}", at.position())
                );
                assert!(
                    emitted
                        .attributes
                        .contains(&A::WorkflowNodeSite.value(at.position().to_string()))
                );
                assert!(emitted.attributes.contains(
                    &A::WorkflowNodeOccurrence.value(serde_json::to_string(&at).unwrap())
                ));
                assert!(emitted.attributes.contains(&A::Outcome.value("completed")));
            }
            TraceDomainSubject::ToolIntent { kind } => {
                assert_eq!(emitted.name, DomainSpan::Intent.definition().name);
                assert!(
                    emitted
                        .attributes
                        .contains(&A::ToolIntentKind.value(kind.as_str()))
                );
            }
        }
        assert_eq!(emitted.start_time, epoch_ms(4000));
        assert_eq!(emitted.end_time, epoch_ms(9000));
    }
}

#[test]
fn tool_and_code_completions_export_their_retained_times_and_the_policys_content() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let turn = derived(&producer(), &scope_id());
    let events = [
        (
            TraceSpanKind::Tool,
            TraceEvent::ToolCallCompleted {
                call_id: lash_protocol_types::ToolCallId::fixture("call"),
                provider_call_id: None,
                name: "unfamiliar-tool".into(),
                args: serde_json::json!({"secret":"hidden"}).into(),
                output: crate::TraceToolCallOutput {
                    outcome: crate::TraceToolCallOutcome::Failure(crate::TraceToolFailure::new(
                        &lash_protocol_types::ToolFailure::io("io_failed", "failed"),
                        crate::ContentPolicy::standard(),
                    )),
                    control: None,
                },
                duration_ms: 55,
                issuing_node_id: None,
                attempts: None,
                span_attributes: Default::default(),
            },
            "execute_tool unfamiliar-tool",
            1000,
        ),
        (
            TraceSpanKind::Cell,
            TraceEvent::ExecCodeCompleted {
                trailing_discarded: 0,
                code: String::new(),
                language: "python".to_string(),
                started_at_ms: 0,
                duration_ms: 300,
                output: "secret".into(),
                output_chars: 6,
                observation_count: 0,
                observation_projections: Vec::new(),
                error: None,
                terminal_finish: None,
                tool_calls: Vec::new(),
            },
            "execute_tool python",
            8700,
        ),
    ];
    // FIG-5530: the adapter exports what the host's content policy left in the
    // record. An omitted record keeps its name, times and status, says its
    // content was omitted, and exports no payload and no content text.
    let text = |span: &opentelemetry_sdk::trace::SpanData| {
        span.attributes
            .iter()
            .map(|a| format!("{}={}", a.key, a.value))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut captured_tool = None;
    for (kind, event, name, start) in events {
        let built = captured_record(
            event,
            position(
                kind,
                SpanRole::Closes,
                derive_span(&turn, &SpanKey::Cell { graph_key: name }),
                Some(turn.clone()),
                &scope_id(),
                (start, 9000),
            ),
        );
        if matches!(built.event, TraceEvent::ToolCallCompleted { .. }) {
            captured_tool = Some(built.clone());
        }
        let event = built.governed(crate::ContentPolicy::omitted());
        adapter.append(&event).unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans.last().unwrap();
        assert_semconv(span);
        assert_eq!(span.name, name);
        assert_eq!(span.start_time, epoch_ms(start));
        assert_eq!(span.end_time, epoch_ms(9000));
        assert_eq!(
            matches!(span.status, Status::Error { .. }),
            event.event.is_failed()
        );
        assert!(
            !span.attributes.iter().any(|a| a
                .key
                .as_str()
                .starts_with("gen_ai.tool.call.arguments")
                || a.key.as_str() == "gen_ai.tool.call.result")
        );
        assert!(
            !text(span).contains("hidden") && !text(span).contains("secret"),
            "{}",
            text(span)
        );
    }
    assert_eq!(exporter.get_finished_spans().unwrap().len(), 2);

    // The same tool completion under a captured policy exports its payload.
    let captured = captured_tool.expect("tool completion");
    adapter
        .append(&captured.governed(crate::ContentPolicy::captured()))
        .unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let span = spans.last().unwrap();
    assert_eq!(span.name, "execute_tool unfamiliar-tool");
    assert!(text(span).contains("hidden"), "{}", text(span));
}

/// A failed cell reports its own closed cause, independently of a turn's recovery.
#[test]
fn failed_cells_export_their_reason_without_failing_a_recovered_turn() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let cell = derive_span(
        &derived(&producer(), &scope_id()),
        &SpanKey::Cell { graph_key: "cell" },
    );
    let events = [
        (
            TraceEvent::ExecCodeFailed {
                trailing_discarded: 0,
                code: String::new(),
                language: "python".to_string(),
                started_at_ms: 0,
                duration_ms: Some(0),
                reason: crate::ExecCodeFailureReason::RuntimeStopped,
                error: "private runtime detail".into(),
            },
            "runtime_stopped",
        ),
        (
            TraceEvent::ExecCodeCompleted {
                trailing_discarded: 0,
                code: String::new(),
                language: "python".to_string(),
                started_at_ms: 0,
                duration_ms: 10,
                output: "private output".into(),
                output_chars: 14,
                observation_count: 0,
                observation_projections: Vec::new(),
                error: Some(crate::CellFailure::new(
                    crate::CellFailureKind::Program,
                    "bad program",
                )),
                terminal_finish: None,
                tool_calls: Vec::new(),
            },
            "program",
        ),
        (
            TraceEvent::ExecCodeCompleted {
                trailing_discarded: 0,
                code: String::new(),
                language: "python".to_string(),
                started_at_ms: 0,
                duration_ms: 10,
                output: String::new(),
                output_chars: 0,
                observation_count: 0,
                observation_projections: Vec::new(),
                error: Some(
                    lash_protocol_types::ExecCodeFailure::new(
                        crate::ExecCodeFailureReason::ExecutorUnavailable,
                        "missing executor",
                    )
                    .into(),
                ),
                terminal_finish: None,
                tool_calls: Vec::new(),
            },
            "executor_unavailable",
        ),
    ];
    for (event, reason) in events {
        let event = closing(TraceSpanKind::Cell, cell.clone(), 8000, event);
        adapter.append(&event).unwrap();
        adapter.append(&completed()).unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let cell = &spans[spans.len() - 2];
        let turn = spans.last().unwrap();
        assert_eq!(cell.name, "execute_tool python");
        assert!(
            matches!(cell.status, Status::Error { .. }),
            "cell must fail: {reason}"
        );
        assert!(
            cell.attributes
                .contains(&KeyValue::new("error.type", reason))
        );
        assert!(
            event.event.is_failed(),
            "the trace classifier must agree with export"
        );
        assert_eq!(turn.name, "invoke_agent");
        assert!(!matches!(turn.status, Status::Error { .. }));
        assert!(
            !turn
                .attributes
                .iter()
                .any(|a| a.key.as_str() == "error.type")
        );
    }
}

/// §4.5: a cell is an `execute_tool {language}` function tool, and one its
/// turn's terminal discards is not a failure: its status stays unset, with
/// `lash.outcome = discarded`.
#[test]
fn a_discarded_cell_is_a_tool_span_that_did_not_fail() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let cell = derive_span(
        &derived(&producer(), &scope_id()),
        &SpanKey::Cell { graph_key: "cell" },
    );
    let mut event = closing(
        TraceSpanKind::Cell,
        cell,
        8000,
        TraceEvent::ExecCodeDiscarded {
            trailing_discarded: 0,
            code: String::new(),
            language: "typescript".into(),
            started_at_ms: 1000,
            duration_ms: 7000,
        },
    );
    event.context.cell = Some("cell".into());
    adapter.append(&event).unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let span = spans.last().unwrap();
    assert_eq!(span.name, "execute_tool typescript");
    assert!(!event.event.is_failed());
    assert!(!matches!(span.status, Status::Error { .. }));
    for expected in [
        KeyValue::new("gen_ai.operation.name", "execute_tool"),
        KeyValue::new("gen_ai.tool.name", "typescript"),
        KeyValue::new("gen_ai.tool.type", "function"),
        KeyValue::new("lash.cell.id", "cell"),
        KeyValue::new("lash.cell.language", "typescript"),
        KeyValue::new("lash.outcome", "discarded"),
    ] {
        assert!(
            span.attributes.contains(&expected),
            "{expected:?}: {span:#?}"
        );
    }
    assert!(
        !span
            .attributes
            .iter()
            .any(|a| a.key.as_str() == "error.type")
    );
}

/// Known business identities remain joinable when payload export is off.
#[test]
fn correlation_ids_are_attributes_with_payload_export_off() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let process_id = lash_vocabulary::identity::ProcessId::fixture("process");
    let process_value = process_id.to_string();
    let owners = [
        (
            TraceScopeOwner::Turn {
                session_id: "conversation".into(),
                turn_id: "physical-turn".into(),
            },
            None,
            None,
        ),
        (
            TraceScopeOwner::Process {
                process_id: lash_vocabulary::identity::ProcessId::fixture("process"),
            },
            None,
            Some(process_value.as_str()),
        ),
        (
            TraceScopeOwner::Tool {
                owner: TraceToolOwner::Turn {
                    session_id: "conversation".into(),
                    turn_id: "durable-run".into(),
                },
                call_id: "tool".into(),
            },
            Some("durable-run"),
            None,
        ),
        (
            TraceScopeOwner::Tool {
                owner: TraceToolOwner::Process {
                    process_id: lash_vocabulary::identity::ProcessId::fixture("process"),
                },
                call_id: "tool".into(),
            },
            None,
            Some(process_value.as_str()),
        ),
        (
            TraceScopeOwner::ToolIntent {
                owner: lash_vocabulary::identity::RuntimeOwner::Process(
                    lash_vocabulary::identity::ProcessId::fixture("process"),
                ),
                replay_key: "intent".into(),
            },
            None,
            Some(process_value.as_str()),
        ),
        (
            TraceScopeOwner::ToolIntent {
                owner: lash_vocabulary::identity::RuntimeOwner::Session("conversation".into()),
                replay_key: "intent".into(),
            },
            None,
            None,
        ),
    ];
    for (owner, run, process) in owners {
        let id = TraceScopeId::admission(owner);
        let container = derived(&producer(), &id);
        let mut model = record(
            TraceEvent::LlmCallFailed {
                model: None,
                request: Default::default(),
                error: crate::TraceError {
                    code: None,
                    retryable: false,
                    terminal_reason: crate::TraceLlmTerminalReason::ProviderError,
                    failure_kind: crate::TraceProviderFailureKind::Unknown,
                },
                stream_summary: None,
                attempts: None,
            },
            position(
                TraceSpanKind::Model,
                SpanRole::Closes,
                with_span(&container, adapter.leaf_span_id()),
                Some(container),
                &id,
                (2000, 3000),
            ),
        );
        model.context = TraceContext {
            session_id: Some("conversation".into()),
            llm_call_id: Some("model-call".into()),
            ..TraceContext::default()
        };
        adapter.append(&model).unwrap();
        // A scope still supplies conversation identity when the record has
        // no context.
        adapter.append(&completed_under(&producer(), &id)).unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let model = &spans[spans.len() - 2];
        let turn = spans.last().unwrap();
        assert_eq!(model.name, "chat");
        for key_value in [
            KeyValue::new("gen_ai.conversation.id", "conversation"),
            KeyValue::new("lash.session.id", "conversation"),
            KeyValue::new("lash.llm_call.id", "model-call"),
        ] {
            assert!(
                model.attributes.contains(&key_value),
                "missing {key_value:?}"
            );
        }
        for span in [model, turn] {
            for (key, expected) in [("lash.run.id", run), ("lash.process.id", process)] {
                let values: Vec<_> = span
                    .attributes
                    .iter()
                    .filter(|a| a.key.as_str() == key)
                    .map(|a| a.value.to_string())
                    .collect();
                assert_eq!(
                    values,
                    expected.into_iter().map(str::to_owned).collect::<Vec<_>>()
                );
            }
            assert!(
                !span
                    .attributes
                    .iter()
                    .any(|a| a.key.as_str().starts_with("lash.payload"))
            );
        }
        if process.is_none() {
            assert!(
                turn.attributes
                    .contains(&KeyValue::new("gen_ai.conversation.id", "conversation"))
            );
        }
    }
}

/// Export preserves the typed class at each operation boundary, without
/// exporting detail.
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
    for (kind, event, expected) in events {
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
    }
}

/// FIG-5728: host resource identity and Lash source identity accompany both
/// exported signals, independently of the instrumentation schema version.
#[test]
fn host_and_lash_versions_accompany_exported_spans_and_metric_points() {
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};

    #[derive(Debug)]
    struct Exporter {
        resource: Resource,
        exported: Arc<Mutex<Vec<(Resource, SpanData)>>>,
    }
    impl SpanExporter for Exporter {
        async fn export(&self, batch: Vec<SpanData>) -> opentelemetry_sdk::error::OTelSdkResult {
            self.exported
                .lock()
                .unwrap()
                .extend(batch.into_iter().map(|span| (self.resource.clone(), span)));
            Ok(())
        }
        fn set_resource(&mut self, resource: &Resource) {
            self.resource = resource.clone();
        }
    }
    let resource = Resource::builder_empty()
        .with_attributes([KeyValue::new("service.version", "host-release-42")])
        .build();
    let spans = Arc::new(Mutex::new(Vec::new()));
    let provider = LashTracerProvider::new(
        SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_simple_exporter(Exporter {
                resource: resource.clone(),
                exported: spans.clone(),
            }),
        Sampler::AlwaysOn,
        RandomIdGenerator::default(),
    );
    let metrics = InMemoryMetricExporter::default();
    let meter = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(PeriodicReader::builder(metrics.clone()).build())
        .build();
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
    adapter.append(&completed()).unwrap();
    adapter
        .metrics()
        .runtime_tuning
        .record_pool_acquire_wait(Duration::from_secs(30), "success");
    meter.force_flush().unwrap();
    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 1);
    let host_version = Some(opentelemetry::Value::from("host-release-42"));
    let lash_version = KeyValue::new("lash.version", env!("CARGO_PKG_VERSION"));
    for (resource, span) in spans.iter() {
        assert_eq!(resource.get(&"service.version".into()), host_version);
        assert!(
            span.instrumentation_scope
                .attributes()
                .any(|attr| attr == &lash_version)
        );
        assert_eq!(
            span.instrumentation_scope.version(),
            Some(LASH_INSTRUMENTATION_CONTRACT)
        );
        let revision = span
            .instrumentation_scope
            .attributes()
            .find(|attr| attr.key.as_str() == "lash.build.revision");
        let expected_revision = option_env!("LASH_BUILD_REVISION")
            .filter(|value| !value.is_empty())
            .map(|value| KeyValue::new("lash.build.revision", value));
        assert_eq!(revision, expected_revision.as_ref());
    }
    let exported = metrics.get_finished_metrics().unwrap();
    let resource = exported.last().unwrap();
    assert_eq!(
        resource.resource().get(&"service.version".into()),
        host_version
    );
    let scopes: Vec<_> = resource.scope_metrics().collect();
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].scope(), &spans[0].1.instrumentation_scope);
    let points: Vec<_> = scopes[0].metrics().collect();
    assert_eq!(points.len(), 2);
    let point = points
        .iter()
        .find(|point| point.name() == "lash.store.pool.acquire_wait.duration")
        .unwrap();
    let opentelemetry_sdk::metrics::data::AggregatedMetrics::U64(
        opentelemetry_sdk::metrics::data::MetricData::Histogram(histogram),
    ) = point.data()
    else {
        panic!("latency histogram");
    };
    assert_eq!(histogram.data_points().next().unwrap().sum(), 30_000);
}

/// FIG-5728: each latency histogram retains the 30-second tail separately
/// from 5 seconds, with dense subsecond buckets in its declared unit.
#[test]
fn latency_histograms_distinguish_thirty_seconds_from_five_seconds() {
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    let (provider, meter, _, metrics) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
    for seconds in [5, 30] {
        let wait = Duration::from_secs(seconds);
        adapter
            .metrics()
            .runtime_tuning
            .record_provider_throttle_wait("fixture", wait);
        adapter
            .metrics()
            .runtime_tuning
            .record_pool_acquire_wait(wait, "success");
        adapter.metrics().runtime_tuning.record_durable_commit(
            "turn.commit",
            "success",
            crate::telemetry::metrics::DurableCommitCost {
                acquire_wait: wait,
                transaction_duration: wait,
                lock_statement_elapsed: wait,
                ..Default::default()
            },
        );
    }
    meter.force_flush().unwrap();
    let exported = metrics.get_finished_metrics().unwrap();
    let mut latencies = 0;
    for metric in exported
        .last()
        .unwrap()
        .scope_metrics()
        .flat_map(|scope| scope.metrics())
    {
        let scale = match metric.unit() {
            "ms" => 1.0,
            "us" => 1000.0,
            _ => continue,
        };
        let AggregatedMetrics::U64(MetricData::Histogram(histogram)) = metric.data() else {
            panic!("latency histogram");
        };
        let point = histogram.data_points().next().unwrap();
        let bounds: Vec<_> = point.bounds().collect();
        assert!(
            bounds.last().unwrap() >= &(60_000.0 * scale),
            "{} retains a 60s tail",
            metric.name()
        );
        assert!(
            bounds
                .iter()
                .filter(|bound| **bound < 1000.0 * scale)
                .count()
                >= 10
        );
        let bucket = |value| bounds.iter().position(|bound| value <= *bound).unwrap();
        let five = bucket(5000.0 * scale);
        let thirty = bucket(30_000.0 * scale);
        assert_ne!(five, thirty, "{} separates 5s and 30s", metric.name());
        let counts: Vec<_> = point.bucket_counts().collect();
        assert_eq!(counts[five], 1);
        assert_eq!(counts[thirty], 1);
        assert_eq!(point.count(), 2);
        latencies += 1;
    }
    assert_eq!(latencies, 5);

    // The host's provider view is authoritative over instrument hints, and
    // can start from the recommended helper before choosing its own bounds.
    use opentelemetry_sdk::metrics::{Aggregation, Instrument, Stream};
    let overridden = InMemoryMetricExporter::default();
    let host_meter = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(overridden.clone()).build())
        .with_view(|instrument: &Instrument| {
            if instrument.scope().name() != registry::LASH_INSTRUMENTATION_NAME {
                return None;
            }
            let mut boundaries = recommended_latency_histogram_boundaries(instrument.name())?;
            boundaries.push(
                120_000.0
                    * if instrument.unit() == "us" {
                        1000.0
                    } else {
                        1.0
                    },
            );
            Some(
                Stream::builder()
                    .with_aggregation(Aggregation::ExplicitBucketHistogram {
                        boundaries,
                        record_min_max: true,
                    })
                    .build()
                    .unwrap(),
            )
        })
        .build();
    TelemetryMetrics::from_provider(&host_meter)
        .runtime_tuning
        .record_pool_acquire_wait(Duration::from_secs(90), "success");
    host_meter.force_flush().unwrap();
    let exported = overridden.get_finished_metrics().unwrap();
    let metric = exported
        .last()
        .unwrap()
        .scope_metrics()
        .next()
        .unwrap()
        .metrics()
        .next()
        .unwrap();
    let AggregatedMetrics::U64(MetricData::Histogram(histogram)) = metric.data() else {
        panic!("host latency histogram");
    };
    let point = histogram.data_points().next().unwrap();
    assert_eq!(point.bounds().last(), Some(120_000.0));
    assert_eq!(
        point.bucket_counts().nth(point.bounds().count() - 1),
        Some(1)
    );
}

/// What [`Refining`] saw its writer answer.
#[derive(Debug, Default)]
struct Refusals {
    provider_set: Option<Result<(), crate::TraceOmission>>,
    reserved_set: Option<Result<(), crate::TraceOmission>>,
    host_set: Option<Result<(), crate::TraceOmission>>,
}

/// An enricher that replaces the provider name, tries to set it and a
/// `lash.` key directly, and adds a host key the registry does not declare.
struct Refining(Arc<Mutex<Refusals>>);
impl SpanEnricher for Refining {
    fn enrich(&self, _: &SpanView<'_>, out: &mut SpanEnrichment) {
        let replaced = out.replace(crate::RefinableKey::GenAiProviderName, "openrouter");
        assert_eq!(replaced, Ok(()));
        let mut refusals = self.0.lock().unwrap();
        refusals.provider_set = Some(out.set("gen_ai.provider.name", "anthropic"));
        refusals.reserved_set = Some(out.set("lash.turn.id", "forged"));
        refusals.host_set = Some(out.set("gen_ai.usage.cost", 0.25));
    }
}

/// L10's replacement clause (FIG-6300): the enricher's `replace` wins over
/// the semantic default lash set, once; `set` of a refinable key is refused
/// `UseReplace` and of a `lash.` key `ReservedKey`, and neither changes the
/// span; a host key the registry does not declare is kept unchecked.
#[test]
fn the_enrichers_replacement_wins_over_lashs_default_and_lash_keys_are_refused() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let refusals = Arc::new(Mutex::new(Refusals::default()));
    let ledger = Arc::new(crate::TraceOmissionLedger::default());
    let adapter = OtelTelemetry::new(
        &provider,
        &meter,
        OtelOptions {
            enricher: Some(Arc::new(Refining(refusals.clone()))),
            ..OtelOptions::standard()
        },
    )
    .bound(crate::TracePolicySnapshot::standard(), ledger.clone());
    let mut record = closing(
        TraceSpanKind::Model,
        with_span(&producer(), adapter.leaf_span_id()),
        4000,
        TraceEvent::LlmCallCompleted {
            request: Default::default(),
            response_metadata: Default::default(),
            model: Some(crate::TraceModelCall {
                provider: Some("openai-compatible".into()),
                request_model: "m".into(),
                stream: true,
                reasoning: None,
                output_type: None,
                generation: Default::default(),
            }),
            response: crate::TraceLlmResponse {
                text: String::new(),
                duration_ms: 5000,
                request_model: "m".into(),
                terminal_reason: None,
                parts: None,
                generation_disposition: None,
            },
            usage: None,
            provider_usage: None,
            stream_summary: None,
            attempts: None,
        },
    );
    record.context.turn_id = Some("lash-turn".into());
    adapter.append(&record).unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    let span = spans.last().unwrap();
    let values = |key: &str| -> Vec<String> {
        span.attributes
            .iter()
            .filter(|attribute| attribute.key.as_str() == key)
            .map(|attribute| attribute.value.to_string())
            .collect()
    };
    assert_eq!(values("gen_ai.provider.name"), ["openrouter"]);
    assert_eq!(values("gen_ai.usage.cost"), ["0.25"]);
    assert_eq!(values("lash.turn.id"), ["lash-turn"]);
    let refusals = refusals.lock().unwrap();
    assert_eq!(
        refusals.provider_set,
        Some(Err(crate::TraceOmission::UseReplace))
    );
    assert_eq!(
        refusals.reserved_set,
        Some(Err(crate::TraceOmission::ReservedKey))
    );
    assert_eq!(refusals.host_set, Some(Ok(())));
    let report = ledger.report();
    let site = crate::TraceOmissionSiteKind::Span;
    assert_eq!(report.get(site, crate::TraceOmission::UseReplace), 1);
    assert_eq!(report.get(site, crate::TraceOmission::ReservedKey), 1);
    assert_eq!(report.total(), 2);
    let omitted = span
        .attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == "lash.trace.omitted")
        .map(|attribute| attribute.value.clone());
    assert_eq!(
        omitted,
        Some(opentelemetry::Value::Array(opentelemetry::Array::String(
            vec!["reserved_key".into(), "use_replace".into()]
        )))
    );
}

/// FIG-6415: an attempt span's view names the attempt it decorates, and the
/// logical span's view carries the same summed usage its `gen_ai.usage.*`
/// attributes export; attempt views carry no logical usage.
#[test]
fn enrichers_see_the_attempt_they_decorate_and_the_usage_lash_exported() {
    #[derive(Debug)]
    struct Seen {
        kind: TraceSpanKind,
        ordinal: Option<u32>,
        usage: Option<LlmUsage>,
        event: &'static str,
    }
    struct Recorder(Arc<Mutex<Vec<Seen>>>);
    impl SpanEnricher for Recorder {
        fn enrich(&self, view: &SpanView<'_>, _: &mut SpanEnrichment) {
            self.0.lock().unwrap().push(Seen {
                kind: view.kind,
                ordinal: view.attempt.map(|attempt| attempt.attempt.ordinal),
                usage: view.usage().cloned(),
                event: view.record.event.kind().as_str(),
            });
        }
    }
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let adapter = OtelTelemetry::new(
        &provider,
        &meter,
        OtelOptions {
            enricher: Some(Arc::new(Recorder(seen.clone()))),
            ..OtelOptions::standard()
        },
    )
    .bound(crate::TracePolicySnapshot::standard(), Arc::default());
    let mut record = model_fixture(&adapter, "openai-compatible");
    let TraceEvent::LlmCallCompleted {
        attempts: Some(attempts),
        ..
    } = &mut record.event
    else {
        unreachable!()
    };
    attempts[0].attempt.outcome = AttemptOutcome::Failed;
    attempts[0].attempt.usage = Some(LlmUsage {
        input_tokens: 7,
        output_tokens: 2,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 5,
        reasoning_output_tokens: 0,
        unknown_calls: 0,
    });
    attempts[1].attempt.usage = Some(LlmUsage {
        input_tokens: 11,
        output_tokens: 13,
        cache_read_input_tokens: 17,
        cache_write_input_tokens: 19,
        reasoning_output_tokens: 3,
        unknown_calls: 0,
    });
    adapter.append(&record).unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "one chat view and one per attempt");
    assert_eq!(seen[0].kind, TraceSpanKind::Model);
    assert_eq!(seen[0].ordinal, None);
    assert_eq!(
        seen[0].usage,
        Some(LlmUsage {
            input_tokens: 18,
            output_tokens: 15,
            cache_read_input_tokens: 17,
            cache_write_input_tokens: 24,
            reasoning_output_tokens: 3,
            unknown_calls: 0,
        })
    );
    for (view, ordinal) in seen[1..].iter().zip([1, 2]) {
        assert_eq!(view.kind, TraceSpanKind::ModelAttempt);
        assert_eq!(view.ordinal, Some(ordinal));
        assert_eq!(view.usage, None);
        assert_eq!(view.event, record.event.kind().as_str());
    }
    drop(seen);

    let spans = exporter.get_finished_spans().unwrap();
    let chat = &spans[0];
    assert!(chat.attributes.contains(&A::CacheWriteTokens.value(24_i64)));
    assert!(chat.attributes.contains(&A::InputTokens.value(59_i64)));
}

/// L12's token half: cache reads and writes are billed input, and reasoning
/// is an output subset. Overflow must never wrap or publish a false total.
#[test]
fn semconv_token_usage_includes_cache_and_marks_overflow() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
    let mut model = closing(
        TraceSpanKind::Model,
        with_span(&producer(), adapter.leaf_span_id()),
        2500,
        TraceEvent::LlmCallCompleted {
            request: Default::default(),
            response_metadata: Default::default(),
            model: None,
            response: crate::TraceLlmResponse {
                text: String::new(),
                duration_ms: 6500,
                request_model: "model-x".into(),
                terminal_reason: None,
                parts: None,
                generation_disposition: None,
            },
            usage: Some(LlmUsage {
                input_tokens: 10,
                output_tokens: 8,
                cache_read_input_tokens: 3,
                cache_write_input_tokens: 2,
                reasoning_output_tokens: 4,
                unknown_calls: 0,
            }),
            provider_usage: None,
            stream_summary: None,
            attempts: None,
        },
    );
    adapter.append(&model).unwrap();
    if let TraceEvent::LlmCallCompleted { usage, .. } = &mut model.event {
        usage.as_mut().unwrap().input_tokens = i64::MAX;
    }
    model.position.as_mut().unwrap().span = with_span(&producer(), adapter.leaf_span_id());
    adapter.append(&model).unwrap();
    provider.provider().force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let value = |index: usize, key: &str| {
        spans[index]
            .attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| attribute.value.clone())
    };
    assert_eq!(value(0, "gen_ai.usage.input_tokens"), Some(15_i64.into()));
    assert_eq!(value(0, "gen_ai.usage.output_tokens"), Some(8_i64.into()));
    assert_eq!(
        value(0, "gen_ai.usage.reasoning.output_tokens"),
        Some(4_i64.into())
    );
    assert_eq!(value(0, "lash.usage.reasoning_output_tokens"), None);
    assert_eq!(value(1, "gen_ai.usage.input_tokens"), None);
    assert_eq!(value(1, "lash.usage.overflow"), Some(true.into()));
}

/// L9: taxonomy must be declared before checking attributes. Otherwise a
/// missing type can make the conformance check vacuous.
#[test]
fn default_projection_conforms_to_the_pinned_genai_taxonomy() {
    let definitions: Vec<serde_json::Value> = [
        include_str!("../../tests/semconv/gen-ai/metrics.yaml"),
        include_str!("../../tests/semconv/gen-ai/token-metrics.yaml"),
    ]
    .into_iter()
    .map(|source| serde_yaml::from_str(source).unwrap())
    .collect();
    for metric in METRICS
        .iter()
        .filter(|metric| metric.name.starts_with("gen_ai."))
    {
        let pinned = definitions
            .iter()
            .flat_map(|model| model["metrics"].as_array().unwrap())
            .find(|definition| definition["name"] == metric.name)
            .expect("metric is pinned in semconv");
        assert_eq!(pinned["unit"], metric.unit, "{} unit", metric.name);
        assert_eq!(
            pinned["instrument"],
            match metric.kind {
                registry::MetricKind::Counter => "counter",
                registry::MetricKind::Histogram => "histogram",
                registry::MetricKind::Gauge => "gauge",
            },
            "{} instrument",
            metric.name
        );
    }
    for (span, expected) in [
        (DomainSpan::Model, "gen_ai.client.inference"),
        (DomainSpan::Turn, "gen_ai.invoke_agent.internal"),
        (DomainSpan::Tool, "gen_ai.execute_tool.internal"),
        (DomainSpan::ExecCode, "gen_ai.execute_tool.internal"),
        (DomainSpan::Workflow, "gen_ai.invoke_workflow.internal"),
    ] {
        assert_eq!(span.definition().semconv_type, Some(expected));
    }
    let contract = registry::contract_markdown();
    for type_id in [
        "gen_ai.client.inference",
        "gen_ai.execute_tool.internal",
        "gen_ai.invoke_agent.internal",
        "gen_ai.invoke_workflow.internal",
    ] {
        assert!(contract.contains(type_id), "missing semconv type {type_id}");
    }
    assert_eq!(
        GEN_AI_SEMCONV_SNAPSHOT,
        "open-telemetry/semantic-conventions-genai@07794021bfe46e1c241408b6f55cb4354bd28318"
    );
    for (kind, expected) in [
        ("openai", "openai"),
        ("codex", "openai"),
        ("anthropic", "anthropic"),
        ("google_oauth", "gcp.gen_ai"),
        ("openai-compatible", "openai"),
    ] {
        let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
        let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
        let record = model_fixture(&adapter, kind);
        adapter.append(&record).unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 3, "one logical chat and two real attempts");
        let chat = &spans[0];
        assert_eq!(chat.name, "chat model-x");
        for attribute in [
            A::GenAiProviderName.value(expected),
            A::InputTokens.value(30_i64),
            A::OutputTokens.value(16_i64),
            A::ReasoningTokens.value(8_i64),
            A::RequestStream.value(true),
            A::ProviderTerminalDeliveryTimedOut.value(true),
            A::ResponseId.value("response-2"),
            A::ReasoningLevel.value("high"),
            A::OutputType.value("json"),
        ] {
            assert!(
                chat.attributes.contains(&attribute),
                "missing {attribute:?}"
            );
        }
        assert_eq!(
            chat.parent_span_id,
            span_context(record.position.as_ref().unwrap().parent.as_ref().unwrap())
                .unwrap()
                .span_id()
        );
        for (index, attempt) in spans[1..].iter().enumerate() {
            assert_eq!(attempt.name, "lash.model.attempt");
            assert_eq!(attempt.parent_span_id, chat.span_context.span_id());
            assert_eq!(attempt.start_time, epoch_ms(4000 + index as u64 * 1000));
            assert_eq!(attempt.end_time, epoch_ms(4500 + index as u64 * 1000));
            assert_eq!(
                attempt
                    .attributes
                    .contains(&A::ProviderTerminalDeliveryTimedOut.value(true)),
                index == 0
            );
            assert!(
                attempt
                    .attributes
                    .contains(&A::AttemptInputTokens.value(10_i64))
            );
            assert!(!attempt.attributes.iter().any(|attribute| {
                attribute.key.as_str().starts_with("gen_ai.usage.")
                    || attribute.key.as_str() == "gen_ai.operation.name"
            }));
        }
        for span in &spans {
            assert_semconv(span);
        }
        let TraceEvent::LlmCallCompleted {
            model, attempts, ..
        } = model_fixture(&adapter, kind).event
        else {
            unreachable!()
        };
        let failed = closing(
            TraceSpanKind::Model,
            with_span(&producer(), adapter.leaf_span_id()),
            2500,
            TraceEvent::LlmCallFailed {
                request: Default::default(),
                model,
                attempts,
                error: crate::TraceError {
                    retryable: false,
                    terminal_reason: crate::TraceLlmTerminalReason::ProviderError,
                    failure_kind: crate::TraceProviderFailureKind::Http,
                    code: None,
                },
                stream_summary: None,
            },
        );
        adapter.append(&failed).unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert!(spans[3].attributes.contains(&A::InputTokens.value(30_i64)));
        assert_semconv(&spans[3]);
    }
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
    let mut cancelled = model_fixture(&adapter, "openai");
    let TraceEvent::LlmCallCompleted {
        model, attempts, ..
    } = cancelled.event
    else {
        unreachable!()
    };
    cancelled.event = TraceEvent::LlmCallFailed {
        request: Default::default(),
        model,
        attempts,
        error: crate::TraceError {
            retryable: false,
            terminal_reason: crate::TraceLlmTerminalReason::Cancelled,
            failure_kind: crate::TraceProviderFailureKind::Unknown,
            code: None,
        },
        stream_summary: None,
    };
    adapter.append(&cancelled).unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans[0].status, Status::Unset);
    assert!(spans[0].attributes.contains(&A::Outcome.value("cancelled")));
    assert!(
        !spans[0]
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "error.type")
    );
    for export_skip in [false, true] {
        let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
        let mut options = OtelOptions::standard();
        let mut policy = crate::TracePolicySnapshot::standard();
        if export_skip {
            options.export = TraceExportPolicy::builder()
                .skip(TraceSpanKind::ModelAttempt)
                .build()
                .unwrap();
        } else {
            policy.detail = crate::TraceDetailPolicy::builder()
                .set(TraceSpanKind::ModelAttempt, crate::TraceDetail::Off)
                .build()
                .unwrap();
        }
        let adapter = OtelTelemetry::new(&provider, &meter, options).bound(policy, Arc::default());
        adapter
            .append(&model_fixture(&adapter, "openai-compatible"))
            .unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        assert!(
            spans[0].attributes.contains(&A::InputTokens.value(30_i64)),
            "attempt policy cannot change billed usage"
        );
    }
}

/// Resolve the pinned model's attribute groups, including requirement levels.
fn semconv_attributes(
    definition: &serde_json::Value,
    model: &serde_json::Value,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut attributes = std::collections::BTreeMap::new();
    for attribute in definition["attributes"].as_array().unwrap() {
        if let Some(group) = attribute["ref_group"].as_str() {
            let group = model["attribute_groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["id"] == group)
                .expect("pinned attribute group");
            attributes.extend(semconv_attributes(group, model));
        } else {
            let key = attribute["ref"].as_str().unwrap().to_string();
            let previous = attributes
                .entry(key)
                .or_insert_with(|| serde_json::json!({}));
            for (key, value) in attribute.as_object().unwrap() {
                previous[key] = value.clone();
            }
        }
    }
    attributes
}

fn assert_semconv(span: &opentelemetry_sdk::trace::SpanData) {
    let registry: serde_json::Value =
        serde_yaml::from_str(include_str!("../../tests/semconv/gen-ai/registry.yaml")).unwrap();
    let model: serde_json::Value =
        serde_yaml::from_str(include_str!("../../tests/semconv/gen-ai/spans.yaml")).unwrap();
    let definition = SPANS
        .iter()
        .find(|definition| {
            span.name == definition.name
                || span
                    .name
                    .strip_prefix(definition.name)
                    .is_some_and(|suffix| suffix.starts_with(' '))
        })
        .expect("declared span kind");
    let value = |key: &str| {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
    };
    if let Some(type_id) = definition.semconv_type {
        let typed = model["spans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|typed| typed["type"] == type_id)
            .unwrap_or_else(|| panic!("unknown semconv type {type_id}"));
        for (key, attribute) in semconv_attributes(typed, &model) {
            if attribute["requirement_level"] == "required" {
                assert!(value(&key).is_some(), "{} requires {key}", span.name);
            }
        }
        assert_eq!(
            typed["kind"].as_str().unwrap(),
            format!("{:?}", span.span_kind).to_lowercase()
        );
    }
    for attribute in &span.attributes {
        if !attribute.key.as_str().starts_with("gen_ai.") {
            continue;
        }
        let registered = registry["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|registered| registered["key"] == attribute.key.as_str())
            .unwrap_or_else(|| panic!("unregistered semconv key {}", attribute.key));
        match registered["type"].as_str().unwrap_or("enum") {
            "string" | "enum" => {
                assert!(matches!(attribute.value, opentelemetry::Value::String(_)))
            }
            "int" => assert!(matches!(attribute.value, opentelemetry::Value::I64(_))),
            "double" => assert!(matches!(attribute.value, opentelemetry::Value::F64(_))),
            "boolean" => assert!(matches!(attribute.value, opentelemetry::Value::Bool(_))),
            "string[]" => assert!(matches!(
                attribute.value,
                opentelemetry::Value::Array(opentelemetry::Array::String(_))
            )),
            other => panic!("unhandled semconv type {other}"),
        }
    }
}

fn model_fixture(adapter: &OtelTelemetry, provider: &str) -> TraceRecord {
    let attempts = (1..=2)
        .map(|ordinal| crate::TraceModelAttempt {
            attempt: AttemptRecord {
                ordinal,
                outcome: if ordinal == 1 {
                    AttemptOutcome::Interrupted
                } else {
                    AttemptOutcome::Completed
                },
                protocol_position: ProtocolPosition::TerminalObserved,
                retry_budget_consumed: true,
                retry_decision: None,
                error: (ordinal == 1).then(|| lash_protocol_types::llm::types::NormalizedError {
                    class: crate::TraceProviderFailureKind::Http,
                    code: None,
                    http_status: Some(503),
                    provider_request_id: Some("retry-request".into()),
                    retry_after: None,
                }),
                evidence: (ordinal == 2).then(|| ExecutionEvidence {
                    served_model: Some("served-model".into()),
                    provider_response_id: Some("response-2".into()),
                    provider_finish_reason: Some("stop".into()),
                    ..Default::default()
                }),
                generation_disposition: None,
                usage: Some(LlmUsage {
                    input_tokens: 10,
                    output_tokens: 8,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 2,
                    reasoning_output_tokens: 4,
                    unknown_calls: 0,
                }),
            },
            span: Some(crate::TraceModelAttemptSpan {
                span_id: adapter.leaf_span_id(),
                provider: provider.into(),
                terminal_delivery_timed_out: ordinal == 1,
                response_metadata_keys: Vec::new(),
                started_at_ms: 3000 + u64::from(ordinal) * 1000,
                ended_at_ms: 3500 + u64::from(ordinal) * 1000,
            }),
        })
        .collect();
    closing(
        TraceSpanKind::Model,
        with_span(&producer(), adapter.leaf_span_id()),
        2500,
        TraceEvent::LlmCallCompleted {
            request: Default::default(),
            response_metadata: Default::default(),
            model: Some(crate::TraceModelCall {
                provider: Some(provider.into()),
                request_model: "model-x".into(),
                stream: true,
                reasoning: Some("high".into()),
                output_type: Some("json".into()),
                generation: Default::default(),
            }),
            response: crate::TraceLlmResponse {
                text: String::new(),
                duration_ms: 6500,
                request_model: "model-x".into(),
                terminal_reason: None,
                parts: None,
                generation_disposition: None,
            },
            usage: Some(LlmUsage {
                input_tokens: 10,
                ..Default::default()
            }),
            provider_usage: None,
            stream_summary: None,
            attempts: Some(attempts),
        },
    )
}

/// L7: content is valid structured semconv JSON, including a non-object result.
#[test]
fn content_attributes_are_whole_schema_valid_json() {
    let (traces, meters, spans, _) = providers(Sampler::AlwaysOn);
    let exporter = OtelTelemetry::new(&traces, &meters, OtelOptions::standard());
    let event = TraceEvent::ToolCallCompleted {
        call_id: lash_protocol_types::ToolCallId::fixture("content-law"),
        provider_call_id: None,
        name: "read".into(),
        args: serde_json::json!([1, 2]).into(),
        output: crate::TraceToolCallOutput {
            outcome: crate::TraceToolCallOutcome::Success(serde_json::json!("é".repeat(20_000))),
            control: None,
        },
        duration_ms: 1,
        issuing_node_id: None,
        attempts: None,
        span_attributes: Default::default(),
    };
    let turn = derived(&producer(), &scope_id());
    let captured = captured_record(
        event,
        position(
            TraceSpanKind::Tool,
            SpanRole::Closes,
            turn,
            Some(producer()),
            &scope_id(),
            (1, 2),
        ),
    )
    .governed(crate::ContentPolicy::captured());
    exporter.append(&captured).unwrap();
    traces.provider().force_flush().unwrap();
    let exported = spans.get_finished_spans().unwrap();
    let attrs = &exported[0].attributes;
    assert!(
        !attrs
            .iter()
            .any(|kv| kv.key.as_str() == "lash.payload.json")
    );
    for key in ["gen_ai.tool.call.arguments", "gen_ai.tool.call.result"] {
        let value = attrs
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .expect("semconv content attribute");
        let json: serde_json::Value = serde_json::from_str(&value.value.as_str()).unwrap();
        assert!(json.is_object());
    }
}

/// L6 and L22(a): all consumers see redaction and references consume the span budget before enrichment.
#[test]
fn content_hooks_reach_sinks_and_spans_and_references_share_the_enrichment_budget() {
    struct Hook;
    impl crate::ContentHook for Hook {
        fn on_content(&self, _: &crate::ContentView<'_>, content: &mut crate::RecordContent<'_>) {
            content.for_each(|field| match field.value() {
                crate::ContentValue::Text(value) => *value = value.replace("secret", "REDACTED"),
                crate::ContentValue::Json(value) => {
                    if let Some(object) = value.as_object_mut() {
                        for value in object.values_mut() {
                            if let Some(text) = value.as_str() {
                                *value = serde_json::json!(text.replace("secret", "REDACTED"));
                            }
                        }
                    }
                }
            });
            for i in 0..100 {
                let _ = content.reference(format!("app.ref.{i}").into(), "uploaded".into());
            }
        }
    }
    struct Budget(Arc<Mutex<Option<crate::WriterRemaining>>>);
    impl SpanEnricher for Budget {
        fn enrich(&self, _: &SpanView<'_>, out: &mut SpanEnrichment) {
            *self.0.lock().unwrap() = Some(out.remaining());
        }
    }
    let (traces, meters, spans, _) = providers(Sampler::AlwaysOn);
    let remaining = Arc::new(Mutex::new(None));
    let policy = crate::TracePolicySnapshot {
        content: crate::ContentPolicy::captured(),
        ..Default::default()
    };
    let adapter = OtelTelemetry::new(
        &traces,
        &meters,
        OtelOptions {
            enricher: Some(Arc::new(Budget(remaining.clone()))),
            ..Default::default()
        },
    )
    .bound(policy.clone(), Arc::default());
    let mut record = captured_record(
        crate::content::laws::model_event(),
        position(
            TraceSpanKind::Model,
            SpanRole::Closes,
            context(1),
            Some(producer()),
            &scope_id(),
            (1, 2),
        ),
    );
    assert!(record.apply_content(&policy, Some(&Hook)).is_none());
    let sink = crate::RecordingTraceSink::new();
    sink.append(&record).unwrap();
    adapter.append(&record).unwrap();
    assert!(
        !serde_json::to_string(&sink.records())
            .unwrap()
            .contains("secret")
    );
    assert_eq!(record.content.references.len(), 16);
    assert_eq!(record.omissions.get(crate::TraceOmission::References), 84);
    let span = spans.get_finished_spans().unwrap().remove(0);
    assert!(!format!("{:?}", span.attributes).contains("secret"));
    assert!(
        span.attributes
            .contains(&KeyValue::new("app.ref.0", "uploaded"))
    );
    assert!(
        span.attributes
            .iter()
            .any(|a| a.key.as_str() == "lash.trace.omitted")
    );
    let initial = TraceAttributeWriter::new(TraceSpanKind::Model, &policy.limits).remaining();
    assert_eq!(
        remaining.lock().unwrap().unwrap().attributes,
        initial.attributes - 16
    );
}

/// An exporter on a closing owner has never received this turn's opener.
#[test]
fn a_turn_closer_exports_input_with_the_global_meter_provider() {
    let (provider, _meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(
        &provider,
        opentelemetry::global::meter_provider().as_ref(),
        OtelOptions::default(),
    );
    let mut closer = completed();
    if let TraceEvent::TurnCompleted { input, .. } = &mut closer.event {
        input.push(crate::TraceLlmMessage {
            role: "user".into(),
            blocks: vec![crate::TraceContentBlock::Text {
                text: "turn input".into(),
                cache_breakpoint: false,
            }],
        });
    }
    let closer = closer.governed(crate::ContentPolicy::captured());
    adapter.append(&closer).unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    let input = spans[0]
        .attributes
        .iter()
        .find(|attr| attr.key.as_str() == "gen_ai.input.messages")
        .expect("the closer alone supplies the span's input")
        .value
        .to_string();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&input).unwrap(),
        serde_json::json!([{"role":"user","parts":[{"type":"text","content":"turn input"}]}])
    );
}

mod metrics;

/// S1: a synthetic terminal has unavailable arguments, unlike a real JSON null.
#[test]
fn abandoned_tool_spans_omit_unavailable_arguments_but_keep_real_null() {
    let (traces, meters, spans, _) = providers(Sampler::AlwaysOn);
    let exporter = OtelTelemetry::new(&traces, &meters, OtelOptions::standard());
    for (index, outcome) in [
        crate::TraceToolCallOutcome::Discarded,
        crate::TraceToolCallOutcome::Cancelled(crate::TraceToolCancellation::new(
            &lash_protocol_types::ToolCancellation::runtime("the turn ended")
                .with_origin(lash_protocol_types::CancelOrigin::TurnStopped),
            crate::ContentPolicy::standard(),
        )),
        crate::TraceToolCallOutcome::Success(serde_json::Value::Null),
    ]
    .into_iter()
    .enumerate()
    {
        let event = TraceEvent::ToolCallCompleted {
            call_id: lash_protocol_types::ToolCallId::fixture(&format!("abandoned-{index}")),
            provider_call_id: None,
            name: if index == 0 {
                crate::TraceToolName::Identity {
                    tool_id: lash_vocabulary::tool_contract::ToolId::new("tool:read"),
                }
            } else {
                "read".into()
            },
            args: if index < 2 {
                crate::TraceToolArgs::Unavailable
            } else {
                crate::TraceToolArgs::Value(serde_json::Value::Null)
            },
            output: crate::TraceToolCallOutput {
                outcome,
                control: None,
            },
            duration_ms: 5,
            issuing_node_id: None,
            attempts: None,
            span_attributes: Default::default(),
        };
        let record = captured_record(
            event,
            position(
                TraceSpanKind::Tool,
                SpanRole::Closes,
                with_span(&producer(), exporter.leaf_span_id()),
                Some(producer()),
                &scope_id(),
                (1, 6),
            ),
        )
        .governed(crate::ContentPolicy::captured());
        exporter.append(&record).unwrap();
    }
    traces.provider().force_flush().unwrap();
    let exported = spans.get_finished_spans().unwrap();
    assert_eq!(exported.len(), 3);
    for (span, expected) in exported.iter().zip(["discarded", "cancelled", "completed"]) {
        let attribute = |key| span.attributes.iter().find(|kv| kv.key.as_str() == key);
        assert_eq!(attribute("lash.outcome").unwrap().value.as_str(), expected);
        if expected == "discarded" {
            assert_eq!(span.name.as_ref(), "execute_tool tool:read");
            assert!(attribute("gen_ai.tool.name").is_none());
        }
        if expected == "completed" {
            let args: serde_json::Value = serde_json::from_str(
                &attribute("gen_ai.tool.call.arguments")
                    .unwrap()
                    .value
                    .as_str(),
            )
            .unwrap();
            assert_eq!(args, serde_json::json!({"value":null}));
        } else {
            assert!(
                attribute("gen_ai.tool.call.arguments").is_none(),
                "{:?}",
                span.attributes
            );
        }
    }
}
