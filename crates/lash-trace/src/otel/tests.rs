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
use lash_sansio::llm::types::{AttemptOutcome, AttemptRecord, ExecutionEvidence, ProtocolPosition};
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
    captured_record(event, position).governed(crate::TelemetryContent::standard())
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
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOff);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
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
    for metric in [
        registry::Metric::PoolAcquireWait,
        registry::Metric::RecoveryLeader,
        registry::Metric::RecoveryTerm,
    ] {
        assert_eq!(metric.definition().ownership, registry::Ownership::Physical);
    }
    let (provider, meter, exporter, metrics) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    adapter.append(&completed()).unwrap();
    let usage = LlmUsage {
        input_tokens: 10,
        output_tokens: 4,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 1,
    };
    let model = closing(
        TraceSpanKind::Model,
        with_span(&producer(), adapter.leaf_span_id()),
        2500,
        TraceEvent::LlmCallCompleted {
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
            attempts: Some(vec![AttemptRecord {
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
            }]),
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
    adapter
        .metrics
        .obligations
        .record_leadership("recovery", true, 1);
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
    let chat = &spans[2];
    assert_eq!(chat.name, "chat model-x");
    assert_eq!(chat.span_kind, SpanKind::Client);
    assert_eq!(chat.start_time, epoch_ms(2500));
    assert_eq!(chat.end_time, epoch_ms(3000));
    assert!(
        chat.attributes
            .contains(&A::ResponseModel.value("observed-model"))
    );
    assert!(chat.attributes.contains(&A::InputTokens.value(10_i64)));
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

struct EnrichmentSpy(Arc<AtomicUsize>);
impl OtelSpanEnricher for EnrichmentSpy {
    fn attributes(&self, _: &TraceRecord, _: &mut Vec<KeyValue>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn unsampled_records_and_bounded_payloads_do_not_serialize_past_their_limit() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let count = Arc::new(AtomicUsize::new(0));
    let adapter = OtelTelemetry::new(
        &provider,
        &meter,
        OtelOptions {
            max_payload_bytes: 32,
            enrich: Some(Arc::new(EnrichmentSpy(count.clone()))),
        },
    );
    let mut unsampled = completed_under(&context(0), &scope_id());
    unsampled.content = crate::TelemetryContent::Captured;
    adapter.append(&unsampled).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 0);
    assert!(exporter.get_finished_spans().unwrap().is_empty());
    struct Huge<'a>(&'a AtomicUsize);
    impl serde::Serialize for Huge<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(Some(1_000_000))?;
            for _ in 0..1_000_000 {
                self.0.fetch_add(1, Ordering::Relaxed);
                sequence.serialize_element("payload")?;
            }
            sequence.end()
        }
    }
    let serializations = AtomicUsize::new(0);
    let mut writer = payload::BoundedWriter::new(32);
    assert!(serde_json::to_writer(&mut writer, &Huge(&serializations)).is_err());
    assert!(writer.truncated);
    assert!(writer.finish().len() <= 32);
    assert!(serializations.load(Ordering::Relaxed) < 10);
}

#[test]
fn payload_limits_are_enforced() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(
        &provider,
        &meter,
        OtelOptions {
            max_payload_bytes: 13,
            enrich: None,
        },
    );
    let mut event = completed();
    event.content = crate::TelemetryContent::Captured;
    adapter.append(&event).unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let bytes: usize = spans[0]
        .attributes
        .iter()
        .filter(|a| a.key.as_str() == A::Payload.definition().key)
        .map(|a| match &a.value {
            opentelemetry::Value::String(v) => v.as_str().len(),
            _ => 0,
        })
        .sum();
    assert!(bytes <= 13);
    assert!(
        spans[0]
            .attributes
            .contains(&A::PayloadTruncated.value(true))
    );
}

/// V23: both telemetry projections read the same typed terminal subject.
#[test]
fn domain_subject_owns_the_span_and_intent_kind_attribute() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let subjects = std::iter::once(TraceDomainSubject::Process {}).chain(
        lash_sansio::ToolIntentKind::ALL
            .iter()
            .map(|kind| TraceDomainSubject::ToolIntent { kind: *kind }),
    );
    for subject in subjects {
        let kind = match subject {
            TraceDomainSubject::Process {} => TraceSpanKind::Process,
            TraceDomainSubject::ToolIntent { .. } => TraceSpanKind::ToolIntent,
        };
        let completion = TraceDomainCompletion::new(subject, 4000, TraceDomainStatus::Completed);
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
        match subject {
            TraceDomainSubject::Process {} => {
                assert_eq!(emitted.name, DomainSpan::Process.definition().name);
                assert!(
                    !emitted
                        .attributes
                        .iter()
                        .any(|attr| attr.key.as_str() == "lash.tool_intent.kind")
                );
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
            "execute_tool unfamiliar-tool",
            1000,
        ),
        (
            TraceSpanKind::Cell,
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
            "lash.exec_code",
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
        let event = built.governed(crate::TelemetryContent::Omitted);
        adapter.append(&event).unwrap();
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
        assert!(span.attributes.contains(&A::ContentOmitted.value(true)));
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
        .append(&captured.governed(crate::TelemetryContent::Captured))
        .unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let span = spans.last().unwrap();
    assert_eq!(span.name, "execute_tool unfamiliar-tool");
    assert!(!span.attributes.contains(&A::ContentOmitted.value(true)));
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
            TraceEvent::ExecCodeCompleted {
                duration_ms: 10,
                output: String::new(),
                output_chars: 0,
                observation_count: 0,
                observation_projections: Vec::new(),
                error: Some(crate::CellFailure::new(
                    crate::CellFailureKind::HelperReleaseNotHeld { release: 7 },
                    "the worker does not hold release 7",
                )),
                terminal_finish: None,
                tool_calls: Vec::new(),
            },
            "helper_release_not_held",
        ),
        (
            TraceEvent::ExecCodeFailed {
                reason: crate::ExecCodeFailureReason::RuntimeStopped,
                error: "private runtime detail".into(),
            },
            "runtime_stopped",
        ),
        (
            TraceEvent::ExecCodeCompleted {
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
                duration_ms: 10,
                output: String::new(),
                output_chars: 0,
                observation_count: 0,
                observation_projections: Vec::new(),
                error: Some(
                    lash_sansio::ExecCodeFailure::new(
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
        assert_eq!(cell.name, "lash.exec_code");
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

/// Known business identities remain joinable when payload export is off.
#[test]
fn correlation_ids_are_attributes_with_payload_export_off() {
    let (provider, meter, exporter, _) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::default());
    let process_id = lash_sansio::ProcessId::fixture("process");
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
                process_id: lash_sansio::ProcessId::fixture("process"),
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
                    process_id: lash_sansio::ProcessId::fixture("process"),
                },
                call_id: "tool".into(),
            },
            None,
            Some(process_value.as_str()),
        ),
        (
            TraceScopeOwner::ToolIntent {
                owner: lash_sansio::RuntimeOwner::Process(lash_sansio::ProcessId::fixture(
                    "process",
                )),
                replay_key: "intent".into(),
            },
            None,
            Some(process_value.as_str()),
        ),
        (
            TraceScopeOwner::ToolIntent {
                owner: lash_sansio::RuntimeOwner::Session("conversation".into()),
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
        call_id: lash_sansio::ToolCallId::fixture("call"),
        provider_call_id: None,
        name: "tool".into(),
        args: serde_json::Value::Null,
        output: crate::TraceToolCallOutput {
            outcome,
            control: None,
        },
        duration_ms: 1,
        issuing_node_id: None,
        attempts: Some(attempts),
    };
    let attempts = vec![
        crate::TraceRetryAttempt {
            ordinal: 1,
            delay_ms: None,
            detail: crate::TraceRetryAttemptDetail::Tool {
                outcome: crate::TraceToolAttemptOutcome::Failed {
                    class: lash_sansio::ToolFailureClass::Io,
                    code: "old".into(),
                    message: "private detail".into(),
                    source: lash_sansio::ToolFailureSource::Tool,
                    suggested_delay_ms: None,
                },
            },
        },
        crate::TraceRetryAttempt {
            ordinal: 2,
            delay_ms: None,
            detail: crate::TraceRetryAttemptDetail::Tool {
                outcome: crate::TraceToolAttemptOutcome::Failed {
                    class: lash_sansio::ToolFailureClass::PermissionDenied,
                    code: "final".into(),
                    message: "private detail".into(),
                    source: lash_sansio::ToolFailureSource::Tool,
                    suggested_delay_ms: None,
                },
            },
        },
    ];
    let model = |code| TraceEvent::LlmCallFailed {
        error: crate::TraceError {
            code: None,
            retryable: false,
            terminal_reason: crate::TraceLlmTerminalReason::ProviderError,
            failure_kind: crate::TraceProviderFailureKind::Http,
        },
        stream_summary: None,
        attempts: Some(vec![AttemptRecord {
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
        }]),
    };
    let events = [
        (
            TraceSpanKind::Tool,
            tool(
                crate::TraceToolCallOutcome::Failure(serde_json::Value::Null),
                attempts.clone(),
            ),
            Some("permission_denied".to_owned()),
        ),
        (
            TraceSpanKind::Tool,
            tool(
                crate::TraceToolCallOutcome::Failure(
                    lash_sansio::ToolFailure::runtime(
                        lash_sansio::ToolFailureClass::Internal,
                        "tool_retry_sleep_failed",
                        "private retry sleep failure",
                    )
                    .to_json_value(),
                ),
                attempts.clone(),
            ),
            Some("internal".to_owned()),
        ),
        (
            TraceSpanKind::Tool,
            tool(
                crate::TraceToolCallOutcome::Failure(
                    lash_sansio::ToolFailure::io("host_io_failed", "private detail")
                        .to_json_value(),
                ),
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
    assert_eq!(points.len(), 1);
    let opentelemetry_sdk::metrics::data::AggregatedMetrics::U64(
        opentelemetry_sdk::metrics::data::MetricData::Histogram(histogram),
    ) = points[0].data()
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
