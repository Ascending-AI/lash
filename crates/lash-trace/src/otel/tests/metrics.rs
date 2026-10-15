use super::*;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

fn chat(adapter: &OtelTelemetry) -> TraceRecord {
    closing(
        TraceSpanKind::Model,
        with_span(&producer(), adapter.leaf_span_id()),
        2500,
        TraceEvent::LlmCallCompleted {
            request: Default::default(),
            response_metadata: Default::default(),
            model: Some(crate::TraceModelCall {
                provider: Some("openai-compatible".into()),
                request_model: "requested".into(),
                stream: true,
                reasoning: None,
                output_type: None,
                generation: Default::default(),
            }),
            response: crate::TraceLlmResponse {
                text: String::new(),
                duration_ms: 1, // The position, rather than this legacy hint, owns duration.
                request_model: "requested".into(),
                terminal_reason: None,
                parts: None,
                generation_disposition: None,
            },
            usage: None,
            provider_usage: None,
            stream_summary: Some(serde_json::json!({"first_chunk_latency_ms": 125})),
            attempts: Some(vec![
                AttemptRecord {
                    ordinal: 1,
                    outcome: AttemptOutcome::Completed,
                    protocol_position: ProtocolPosition::TerminalObserved,
                    retry_budget_consumed: true,
                    retry_decision: None,
                    error: None,
                    evidence: Some(ExecutionEvidence {
                        served_model: Some("served".into()),
                        ..Default::default()
                    }),
                    generation_disposition: None,
                    usage: Some(LlmUsage {
                        input_tokens: 10,
                        cache_read_input_tokens: 3,
                        cache_write_input_tokens: 2,
                        output_tokens: 4,
                        reasoning_output_tokens: 1,
                    }),
                }
                .into(),
            ]),
        },
    )
}

/// L12: the same closing record supplies span duration and cache-inclusive
/// usage, once. Opening hints and attempt events carry no extra metric sample.
#[test]
fn genai_metrics_equal_their_spans_and_count_only_closing_records() {
    let (provider, meter, spans, metrics) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
    let record = chat(&adapter);
    let mut hint = record.clone();
    hint.position.as_mut().unwrap().role = SpanRole::Opens;
    adapter.append(&hint).unwrap();
    hint.position.as_mut().unwrap().role = SpanRole::Event;
    adapter.append(&hint).unwrap();
    adapter.append(&record).unwrap();
    meter.force_flush().unwrap();
    let spans = spans.get_finished_spans().unwrap();
    let span = &spans[0];
    let duration = span
        .end_time
        .duration_since(span.start_time)
        .unwrap()
        .as_secs_f64();
    let exported = metrics.get_finished_metrics().unwrap();
    let mut names = Vec::new();
    for metric in exported
        .iter()
        .flat_map(|r| r.scope_metrics())
        .flat_map(|s| s.metrics())
    {
        names.push(metric.name());
        match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                let points: Vec<_> = h.data_points().collect();
                assert_eq!(points.len(), 1);
                assert_eq!(points[0].count(), 1);
                assert_eq!(
                    points[0].sum(),
                    if metric.name().ends_with("time_to_first_chunk") {
                        0.125
                    } else {
                        duration
                    }
                );
            }
            AggregatedMetrics::U64(data) => {
                let (value, attributes) = match data {
                    MetricData::Sum(s) => {
                        let points: Vec<_> = s.data_points().collect();
                        assert_eq!(points.len(), 1);
                        (
                            points[0].value(),
                            points[0].attributes().collect::<Vec<_>>(),
                        )
                    }
                    MetricData::Histogram(h) => {
                        let points: Vec<_> = h.data_points().collect();
                        assert_eq!(points.len(), 1);
                        assert_eq!(points[0].count(), 1);
                        (points[0].sum(), points[0].attributes().collect::<Vec<_>>())
                    }
                    _ => panic!("token metric is a sum or histogram"),
                };
                let key = if metric.name().contains("cache_read") {
                    A::CacheReadTokens
                } else if metric.name().contains("cache_write") {
                    A::CacheWriteTokens
                } else if metric.name().contains("reasoning") {
                    A::ReasoningTokens
                } else if metric.name().ends_with("input_tokens") {
                    A::InputTokens
                } else {
                    A::OutputTokens
                };
                assert!(span.attributes.contains(&key.value(value as i64)));
                assert!(attributes.iter().all(|a| {
                    [
                        "gen_ai.operation.name",
                        "gen_ai.provider.name",
                        "gen_ai.request.model",
                        "gen_ai.response.model",
                        "error.type",
                    ]
                    .contains(&a.key.as_str())
                }));
                assert!(attributes.contains(&&A::GenAiProviderName.value("openai")));
            }
            data => panic!("unexpected metric {data:?}"),
        }
    }
    assert_eq!(
        names.len(),
        9,
        "two durations, five counters, two token histograms"
    );
    assert!(span.attributes.contains(&A::InputTokens.value(15_i64)));
}

/// §9.3: span sampling and export selection do not sample metrics, and
/// §9.2: each metric family has an independent no-op meter.
#[test]
fn metric_families_are_independent_of_sampling_and_span_export() {
    for sampled in [false, true] {
        for family in [
            TraceMetrics::Off,
            TraceMetrics::Runtime,
            TraceMetrics::GenAi,
            TraceMetrics::All,
        ] {
            let (provider, meter, spans, metrics) = providers(Sampler::AlwaysOff);
            let adapter = OtelTelemetry::new(
                &provider,
                &meter,
                OtelOptions {
                    metrics: family,
                    export: TraceExportPolicy::builder()
                        .skip(TraceSpanKind::ModelAttempt)
                        .skip(TraceSpanKind::Model)
                        .build()
                        .unwrap(),
                    ..OtelOptions::standard()
                },
            );
            let mut record = chat(&adapter);
            record.position.as_mut().unwrap().span = context(u8::from(sampled));
            adapter.append(&record).unwrap();
            adapter
                .metrics()
                .runtime_tuning
                .record_provider_retry("openai-compatible", "backoff");
            meter.force_flush().unwrap();
            assert!(spans.get_finished_spans().unwrap().is_empty());
            let exported = metrics.get_finished_metrics().unwrap();
            let names: Vec<_> = exported
                .iter()
                .flat_map(|r| r.scope_metrics())
                .flat_map(|s| s.metrics())
                .map(|m| m.name())
                .collect();
            assert_eq!(
                names.iter().filter(|n| n.starts_with("gen_ai.")).count(),
                if family.records_gen_ai() { 9 } else { 0 }
            );
            assert_eq!(
                names.contains(&"lash.provider.retries"),
                family.records_runtime()
            );
        }
    }
}

/// §9.3: omission metrics count at the ledger's source, including refusals
/// with no record, and contain only the reason, under Runtime selection.
#[test]
fn omission_metrics_follow_the_runtime_family_at_the_ledger() {
    for family in [
        TraceMetrics::Off,
        TraceMetrics::Runtime,
        TraceMetrics::GenAi,
        TraceMetrics::All,
    ] {
        let (provider, meter, _, metrics) = providers(Sampler::AlwaysOff);
        let ledger = Arc::new(TraceOmissionLedger::default());
        let _adapter = OtelTelemetry::new(
            &provider,
            &meter,
            OtelOptions {
                metrics: family,
                ..OtelOptions::standard()
            },
        )
        .bound(TracePolicySnapshot::standard(), ledger.clone());
        ledger.record_one(
            &TraceOmissionSite::Shift { scope: None },
            crate::TraceOmission::HeldObservations,
        );
        meter.force_flush().unwrap();
        let exported = metrics.get_finished_metrics().unwrap();
        let all: Vec<_> = exported
            .iter()
            .flat_map(|r| r.scope_metrics())
            .flat_map(|s| s.metrics())
            .collect();
        assert_eq!(all.len(), usize::from(family.records_runtime()));
        for metric in all {
            assert_eq!(metric.name(), "lash.trace.omissions");
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                panic!("omissions are a counter")
            };
            let points: Vec<_> = sum.data_points().collect();
            assert_eq!(points[0].value(), 1);
            assert_eq!(
                points[0].attributes().collect::<Vec<_>>(),
                [&KeyValue::new("reason", "held_observations")]
            );
        }
    }
}

pub(super) fn emit_remaining_metrics(adapter: &OtelTelemetry) {
    adapter.append(&chat(adapter)).unwrap();
    let mut agent = completed();
    if let TraceEvent::TurnCompleted {
        inference_calls,
        tool_calls,
        ..
    } = &mut agent.event
    {
        *inference_calls = Some(2);
        *tool_calls = Some(3);
    }
    adapter.append(&agent).unwrap();
    for (kind, event) in [
        (
            TraceSpanKind::Tool,
            TraceEvent::ToolCallCompleted {
                call_id: lash_sansio::ToolCallId::fixture("call"),
                provider_call_id: None,
                name: "search".into(),
                args: serde_json::Value::Null,
                output: crate::TraceToolCallOutput {
                    outcome: crate::TraceToolCallOutcome::Success(serde_json::Value::Null),
                    control: None,
                },
                duration_ms: 1,
                issuing_node_id: None,
                attempts: None,
            },
        ),
        (
            TraceSpanKind::Cell,
            TraceEvent::ExecCodeDiscarded {
                code: String::new(),
                language: "typescript".into(),
                started_at_ms: 1,
                duration_ms: 1,
            },
        ),
        (
            TraceSpanKind::Workflow,
            TraceEvent::DomainCompleted {
                completion: TraceDomainCompletion::new(
                    TraceDomainSubject::Workflow {
                        kind: "workflow".into(),
                        name: Some("research".into()),
                    },
                    1,
                    TraceDomainStatus::Completed,
                ),
            },
        ),
    ] {
        adapter
            .append(&closing(
                kind,
                with_span(&producer(), adapter.leaf_span_id()),
                4000,
                event,
            ))
            .unwrap();
    }
    adapter.omissions.record_one(
        &TraceOmissionSite::Shift { scope: None },
        crate::TraceOmission::HeldObservations,
    );
}

/// §9.2: agents, tools (cells included) and workflows use their closing
/// position and only their documented dimensions; unknown counts stay absent.
#[test]
fn agent_tool_and_workflow_metrics_use_their_closing_positions() {
    let (provider, meter, _, metrics) = providers(Sampler::AlwaysOn);
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
    emit_remaining_metrics(&adapter);
    meter.force_flush().unwrap();
    let exported = metrics.get_finished_metrics().unwrap();
    let mut found = 0;
    for metric in exported
        .iter()
        .flat_map(|r| r.scope_metrics())
        .flat_map(|s| s.metrics())
    {
        match metric.name() {
            "gen_ai.invoke_agent.inference_calls" | "gen_ai.invoke_agent.tool_calls" => {
                let AggregatedMetrics::U64(MetricData::Histogram(h)) = metric.data() else {
                    panic!("count histogram")
                };
                let points: Vec<_> = h.data_points().collect();
                assert_eq!(points.len(), 1);
                assert_eq!(points[0].count(), 1);
                assert_eq!(
                    points[0].sum(),
                    if metric.name().ends_with("inference_calls") {
                        2
                    } else {
                        3
                    }
                );
                found += 1;
            }
            "gen_ai.invoke_agent.duration"
            | "gen_ai.execute_tool.duration"
            | "gen_ai.invoke_workflow.duration" => {
                let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data() else {
                    panic!("seconds histogram")
                };
                for point in h.data_points() {
                    assert_eq!(point.count(), 1);
                    assert_eq!(
                        point.sum(),
                        if metric.name().contains("invoke_agent") {
                            8.0
                        } else {
                            5.0
                        }
                    );
                    assert!(point.attributes().all(|a| {
                        [
                            "gen_ai.operation.name",
                            "gen_ai.tool.name",
                            "gen_ai.workflow.name",
                            "error.type",
                        ]
                        .contains(&a.key.as_str())
                    }));
                    found += 1;
                }
            }
            _ => {}
        }
    }
    assert_eq!(found, 6);
}
