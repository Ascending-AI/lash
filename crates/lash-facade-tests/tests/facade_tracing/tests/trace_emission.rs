//! Every trace record, span and operational metric lash wires has a
//! production producer (FIG-5658): a host's real run over SQLite memory
//! stores writes the record to the host's `TraceSink` and exports its span or
//! metric through the host's OpenTelemetry SDK providers.
//!
//! The logical records are emitted by the owner whose commit was
//! acknowledged, so each law also resubmits its work and finds no second
//! record.

use super::tracing::{text_call, tool_call};
use super::*;
use lash_facade_tests::ingress::{
    INGRESS_ENGINE_KIND, SCOPE, SESSION, engine_start_intent, ingress_engine_core_with,
    started_process_id,
};

use lash::TurnInput;
use lash_core::testing::runtime_helpers::{EchoTool, mock_provider};
use lash_trace::otel::{OtelOptions, OtelTelemetry};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

const WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

/// What a host installs to observe a core: a record sink and an
/// OpenTelemetry adapter over SDK providers it owns.
pub(super) struct Observed {
    sink: Arc<lash::testing::RecordingTraceSink>,
    spans: InMemorySpanExporter,
    metrics: InMemoryMetricExporter,
    meter: SdkMeterProvider,
    _tracer: lash::tracing::LashTracerProvider,
}

impl Observed {
    /// The observers, and `builder` with them installed.
    pub(super) fn install(builder: lash::LashCoreBuilder) -> (Self, lash::LashCoreBuilder) {
        let sink = Arc::new(lash::testing::RecordingTraceSink::new());
        let spans = InMemorySpanExporter::default();
        let tracer = lash::tracing::LashTracerProvider::new(
            SdkTracerProvider::builder().with_simple_exporter(spans.clone()),
            lash_trace::otel::sdk::trace::Sampler::AlwaysOn,
            lash_trace::otel::sdk::trace::RandomIdGenerator::default(),
        );
        let metrics = InMemoryMetricExporter::default();
        let meter = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build();
        let builder = builder
            .trace_sink(sink.clone())
            .telemetry(OtelTelemetry::new(&tracer, &meter, OtelOptions::default()));
        (
            Self {
                sink,
                spans,
                metrics,
                meter,
                _tracer: tracer,
            },
            builder,
        )
    }

    /// Every record the sink holds of kind `kind`.
    pub(super) fn records(&self, core: &LashCore, kind: &str) -> Vec<lash_trace::TraceRecord> {
        core.flush_trace_sink().expect("flush the trace sink");
        self.sink
            .records()
            .into_iter()
            .filter(|record| record.event.kind().as_str() == kind)
            .collect()
    }

    /// Every exported span.
    pub(super) fn exported(&self) -> Vec<SpanData> {
        self.spans.get_finished_spans().expect("exported spans")
    }

    /// Every exported span whose name is, or starts with, `name`.
    pub(super) fn spans(&self, name: &str) -> Vec<SpanData> {
        self.spans
            .get_finished_spans()
            .expect("exported spans")
            .into_iter()
            .filter(|span| {
                span.name == name
                    || span
                        .name
                        .strip_prefix(name)
                        .is_some_and(|rest| rest.starts_with(' '))
            })
            .collect()
    }

    /// The exported value of counter `name`, summed over its attribute sets.
    fn counter(&self, name: &str) -> u64 {
        self.meter.force_flush().expect("flush the meter provider");
        let exported = self
            .metrics
            .get_finished_metrics()
            .expect("exported metrics");
        // Each flush exports the cumulative sum: the last one is current.
        exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .filter(|metric| metric.name() == name)
            .filter_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                    Some(sum.data_points().map(|point| point.value()).sum::<u64>())
                }
                _ => None,
            })
            .last()
            .unwrap_or(0)
    }

    /// Until `ready` holds, within [`WITHIN`].
    pub(super) async fn until(&self, what: &str, ready: impl Fn(&Self) -> bool + Send + Sync) {
        tokio::time::timeout(WITHIN, async {
            while !ready(self) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
    }
}

pub(super) fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| attribute.value.to_string())
}

/// Whether `span` was exported with Error status.
fn failed(span: &SpanData) -> bool {
    format!("{:?}", span.status).starts_with("Error")
}

/// The one span exported for `record`: its `lash.record.id` names it.
fn span_of<'a>(spans: &'a [SpanData], record: &lash_trace::TraceRecord) -> &'a SpanData {
    let exported: Vec<_> = spans
        .iter()
        .filter(|span| attribute(span, "lash.record.id").as_deref() == Some(record.id.as_str()))
        .collect();
    assert_eq!(exported.len(), 1, "one span per record: {exported:#?}");
    exported[0]
}

const TURN_SESSION: &str = "emission-turn-session";
const TURN: &str = "emission-turn";

/// One served turn that calls `echo_tool` and answers, sent twice under one
/// turn id: the second send is answered from the committed run.
async fn served_turn() -> Result<(Observed, LashCore)> {
    let mut calls = vec![
        tool_call("call-echo", "echo_tool", r#"{"value":"emitted"}"#),
        text_call("done"),
    ];
    calls[0].stream_events = vec![lash_protocol_types::llm::types::LlmStreamEvent::Evidence(
        Default::default(),
    )];
    calls[1].stream_events = vec![lash_protocol_types::llm::types::LlmStreamEvent::Part(
        lash_protocol_types::llm::types::LlmOutputPart::Text {
            text: "done".into(),
            response_meta: None,
        },
    )];
    calls[0]
        .response
        .as_mut()
        .expect("successful fixture")
        .usage = lash_protocol_types::llm::types::LlmUsage {
        input_tokens: 10,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 2,
        output_tokens: 4,
        reasoning_output_tokens: 1,
        unknown_calls: 0,
    };
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(calls).into_handle(), mock_llm_profile_spec())
    .tools(lash::tool_provider_key!("tools"), Arc::new(EchoTool));
    let (observed, builder) = Observed::install(builder);
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse(TURN_SESSION).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    for _ in 0..2 {
        let output = session
            .send(TurnInput::text("call the tool"))
            .id(lash::TurnId::parse(TURN).expect("nonblank host identity"))
            .output()
            .await?;
        assert!(
            matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
            "{:?}",
            output.result.outcome
        );
    }
    Ok((observed, core))
}

/// FIG-6431: the committed assistant answer reaches the closer and the
/// invoke_agent span only with OutputMessages consent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_closer_exports_its_answer_only_with_output_consent() -> Result<()> {
    for capture in [lash_trace::Capture::Omitted, lash_trace::Capture::Captured] {
        let builder = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(
            mock_provider(vec![text_call("settled answer")]).into_handle(),
            mock_llm_profile_spec(),
        )
        .content_policy(
            lash_trace::ContentPolicy::omitted()
                .with(lash_trace::ContentCategory::OutputMessages, capture),
        );
        let (observed, builder) = Observed::install(builder);
        let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
        let session = core
            .session(lash::SessionId::parse("turn-answer-consent").unwrap())
            .create(lash::SessionCreation::root(mock_session_spec()))
            .await?;
        let output = session.send(TurnInput::text("answer")).output().await?;
        assert!(output.is_success());
        let records = observed.records(&core, "turn_completed");
        assert_eq!(records.len(), 1);
        let lash_trace::TraceEvent::TurnCompleted { output, .. } = &records[0].event else {
            unreachable!()
        };
        let captured = capture == lash_trace::Capture::Captured;
        assert_eq!(!output.is_empty(), captured);
        let spans = observed.spans("invoke_agent");
        assert_eq!(spans.len(), 1);
        let exported = attribute(&spans[0], "gen_ai.output.messages");
        if captured {
            let answer = serde_json::json!([{"role":"assistant","parts":[{"type":"text","content":"settled answer"}]}]);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&exported.expect("captured output"))
                    .unwrap(),
                answer
            );
            assert_eq!(output[0].role, "assistant");
            assert_eq!(
                serde_json::to_value(output).unwrap()[0]["blocks"][0]["text"],
                "settled answer"
            );
        } else {
            assert!(exported.is_none());
        }
        core.shutdown().await?;
    }
    Ok(())
}

/// FIG-6434: control.finish carries its value as one assistant text part,
/// on the committed closer and its span, under OutputMessages consent.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_control_finished_turn_exports_its_value_only_with_output_consent() -> Result<()> {
    for (value, text) in [
        (serde_json::json!("Sent a joke"), "Sent a joke"),
        (serde_json::json!({"a": 1}), r#"{"a":1}"#),
    ] {
        for capture in [lash_trace::Capture::Omitted, lash_trace::Capture::Captured] {
            let response = lash_facade_tests::control_cell_response(
                &lash::rlm::CellDialect::typescript(),
                lash::rlm::RlmChannel::Cell,
                0,
                format!("await control.finish({value});"),
            );
            let builder = explicit_ephemeral_facets(rlm_core_builder_over(
                sqlite_memory_store_backend().await,
            ))
            .serve_test_llm_profile(
                mock_provider(vec![lash_core::testing::runtime_helpers::MockCall {
                    stream_events: Vec::new(),
                    response: Ok(response),
                }])
                .into_handle(),
                mock_llm_profile_spec(),
            )
            .content_policy(
                lash_trace::ContentPolicy::omitted()
                    .with(lash_trace::ContentCategory::OutputMessages, capture),
            );
            let (observed, builder) = Observed::install(builder);
            let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
            let session = core
                .session(lash::SessionId::parse("control-finish-answer").unwrap())
                .create(lash::SessionCreation::root(mock_session_spec()))
                .await?;
            let result = session.send(TurnInput::text("finish")).output().await?;
            assert!(
                matches!(
                    &result.result.outcome,
                    lash::TurnOutcome::Finished(lash::TurnFinish::Finished { value: actual, .. })
                        if actual == &value
                ),
                "{:?}",
                result.result.outcome
            );
            let records = observed.records(&core, "turn_completed");
            assert_eq!(records.len(), 1);
            let lash_trace::TraceEvent::TurnCompleted { output, .. } = &records[0].event else {
                unreachable!()
            };
            let spans = observed.spans("invoke_agent");
            assert_eq!(spans.len(), 1);
            let exported = attribute(&spans[0], "gen_ai.output.messages");
            if capture == lash_trace::Capture::Captured {
                assert_eq!(
                    output,
                    &vec![lash_trace::TraceLlmMessage {
                        role: "assistant".into(),
                        blocks: vec![lash_trace::TraceContentBlock::Text {
                            text: text.to_owned(),
                            cache_breakpoint: false,
                        }],
                    }]
                );
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&exported.expect("captured output"))
                        .unwrap(),
                    serde_json::json!([{"role":"assistant","parts":[{"type":"text","content":text}]}])
                );
            } else {
                assert!(output.is_empty());
                assert!(exported.is_none());
            }
            core.shutdown().await?;
        }
    }
    Ok(())
}

/// `turn_started`: the turn's admission, once, opening the turn's span at
/// its scope's retained start, where the exported `invoke_agent` starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_turn_writes_one_turn_started_at_its_admission() -> Result<()> {
    let (observed, core) = served_turn().await?;
    let started = observed.records(&core, "turn_started");
    assert_eq!(
        started.len(),
        1,
        "one start across both sends: {started:#?}"
    );
    let record = &started[0];
    assert_eq!(
        record.context.session_id.as_ref().map(|id| id.as_str()),
        Some(TURN_SESSION)
    );
    assert_eq!(
        record.context.turn_id.as_ref().map(|id| id.as_str()),
        Some(TURN)
    );
    assert_eq!(
        record.event,
        lash_trace::TraceEvent::TurnStarted {
            metadata: [("input_count".to_owned(), serde_json::json!(1))].into(),
        }
    );
    let position = record.position.as_ref().expect("a positioned record");
    assert_eq!(position.role, lash_trace::SpanRole::Opens);
    let turns = observed.spans("invoke_agent");
    assert_eq!(turns.len(), 1, "the turn's span is exported once");
    assert_eq!(
        turns[0].span_context.span_id().to_bytes(),
        position.span.span_id().to_bytes(),
        "the start opens the exported span"
    );
    let since_epoch = |at: std::time::SystemTime| {
        at.duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_millis()
    };
    assert_eq!(
        since_epoch(record.timestamp.to_system_time()),
        since_epoch(turns[0].start_time),
        "the span starts at the admission's retained time, in whole milliseconds"
    );
    core.shutdown().await?;
    Ok(())
}

/// `turn_completed` and its `invoke_agent` span: the turn's committed
/// terminal, once, under the send that admitted it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_turn_exports_one_invoke_agent_for_its_committed_terminal() -> Result<()> {
    let (observed, core) = served_turn().await?;
    let completed = observed.records(&core, "turn_completed");
    assert_eq!(
        completed.len(),
        1,
        "one terminal across both sends: {completed:#?}"
    );
    assert_eq!(
        completed[0].event,
        lash_trace::TraceEvent::TurnCompleted {
            output: Vec::new(),
            input: Vec::new(),
            inference_calls: Some(2),
            tool_calls: Some(1),
            inputs: 1,
            outcome: lash_trace::TraceTurnOutcome::Completed {
                done_reason: lash_trace::TraceTurnCompletionReason::AssistantMessage,
            },
        }
    );
    let spans = observed.spans("invoke_agent");
    assert_eq!(spans.len(), 1, "one exported turn: {spans:#?}");
    let span = span_of(&spans, &completed[0]);
    assert_eq!(
        attribute(span, "lash.outcome").as_deref(),
        Some("completed")
    );
    assert_eq!(attribute(span, "lash.turn.id").as_deref(), Some(TURN));
    assert_eq!(
        attribute(span, "lash.session.id").as_deref(),
        Some(TURN_SESSION)
    );
    let sends = observed.spans("lash.send");
    let parent = sends
        .iter()
        .find(|send| send.span_context.span_id() == span.parent_span_id)
        .expect("the turn's parent is an exported send");
    assert_eq!(
        attribute(parent, "lash.producer.outcome").as_deref(),
        Some("selected"),
        "the send whose acceptance admitted the turn parents it: {sends:#?}"
    );
    assert!(span.start_time <= span.end_time);
    // L12 on the production SQLite engine: a resend reads the terminal,
    // so its chats and their metrics are observed only once.
    let chats = observed.spans("chat");
    assert_eq!(chats.len(), 2);
    let seconds: f64 = chats
        .iter()
        .map(|chat| {
            chat.end_time
                .duration_since(chat.start_time)
                .unwrap()
                .as_secs_f64()
        })
        .sum();
    observed.meter.force_flush().expect("flush metrics");
    let exported = observed.metrics.get_finished_metrics().expect("metrics");
    let latest = exported.last().expect("metric export");
    let mut checked = 0;
    for metric in latest.scope_metrics().flat_map(|scope| scope.metrics()) {
        match metric.name() {
            "gen_ai.client.inference.duration" => {
                let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data() else {
                    panic!("duration histogram")
                };
                assert_eq!(h.data_points().map(|point| point.count()).sum::<u64>(), 2);
                assert!(
                    (h.data_points().map(|point| point.sum()).sum::<f64>() - seconds).abs() < 1e-9
                );
                checked += 1;
            }
            "gen_ai.client.inference.usage.input_tokens" => {
                let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                    panic!("token counter")
                };
                assert_eq!(
                    sum.data_points().map(|point| point.value()).sum::<u64>(),
                    15
                );
                let tokens: u64 = chats
                    .iter()
                    .filter_map(|chat| attribute(chat, "gen_ai.usage.input_tokens"))
                    .map(|tokens| tokens.parse::<u64>().unwrap())
                    .sum();
                assert_eq!(tokens, 15);
                checked += 1;
            }
            "gen_ai.client.inference.time_to_first_chunk" => {
                let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data() else {
                    panic!("first chunk histogram")
                };
                assert_eq!(
                    h.data_points().map(|point| point.count()).sum::<u64>(),
                    1,
                    "evidence is not a response chunk"
                );
                checked += 1;
            }
            "gen_ai.invoke_agent.tool_calls" => {
                let AggregatedMetrics::U64(MetricData::Histogram(h)) = metric.data() else {
                    panic!("tool call histogram")
                };
                assert_eq!(h.data_points().map(|point| point.count()).sum::<u64>(), 1);
                assert_eq!(h.data_points().map(|point| point.sum()).sum::<u64>(), 1);
                checked += 1;
            }
            "gen_ai.invoke_agent.inference_calls" => {
                let AggregatedMetrics::U64(MetricData::Histogram(h)) = metric.data() else {
                    panic!("call histogram")
                };
                assert_eq!(h.data_points().map(|point| point.count()).sum::<u64>(), 1);
                assert_eq!(h.data_points().map(|point| point.sum()).sum::<u64>(), 2);
                checked += 1;
            }
            _ => {}
        }
    }
    assert_eq!(checked, 5);
    core.shutdown().await?;
    Ok(())
}

/// `execute_tool`: the call's live completion is exported as its span,
/// once per execution that ran it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_tool_call_exports_one_execute_tool_span() -> Result<()> {
    let (observed, core) = served_turn().await?;
    let completed = observed.records(&core, "tool_call_completed");
    assert_eq!(completed.len(), 1, "one completion: {completed:#?}");
    let spans = observed.spans("execute_tool");
    assert_eq!(spans.len(), 1, "one exported call: {spans:#?}");
    let span = span_of(&spans, &completed[0]);
    assert_eq!(span.name, "execute_tool echo_tool");
    assert_eq!(
        attribute(span, "gen_ai.tool.name").as_deref(),
        Some("echo_tool")
    );
    assert!(!failed(span), "a successful call is no error");
    let turns = observed.spans("invoke_agent");
    assert_eq!(
        span.parent_span_id,
        turns[0].span_context.span_id(),
        "the call is a child of its turn"
    );
    core.shutdown().await?;
    Ok(())
}

/// A core serving the ingress engine, observed.
async fn observed_engine_core() -> Result<(Observed, LashCore, Arc<dyn ProcessRegistry>)> {
    let mut observed = None;
    let (core, registry) =
        ingress_engine_core_with(sqlite_memory_store_backend().await, |builder| {
            let (installed, builder) = Observed::install(builder);
            observed = Some(installed);
            builder
        })
        .await?;
    Ok((observed.expect("installed observers"), core, registry))
}

/// `lash.tool_intent`: a host-submitted intent's committed settlement is
/// exported once, executed or refused, and a redelivery of its key exports
/// nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_submitted_tool_intent_exports_one_span_for_its_settlement() -> Result<()> {
    let (observed, core, _registry) = observed_engine_core().await?;
    let ingress = core.tool_intents(
        lash::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let intent =
        || engine_start_intent(INGRESS_ENGINE_KIND, serde_json::json!({"program": "known"}));
    for replayed in [false, true] {
        let outcome = ingress
            .submit(
                ingress.key("emission-executed", 0).expect("a key"),
                intent(),
            )
            .await;
        assert!(
            matches!(
                &outcome,
                lash::tools::ToolIntentIngressOutcome::Admitted {
                    outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. },
                    replayed: was,
                } if *was == replayed
            ),
            "{outcome:?}"
        );
    }
    let refused = ingress
        .submit(
            ingress.key("emission-refused", 0).expect("a key"),
            engine_start_intent("emission-engine-never-registered", serde_json::json!({})),
        )
        .await;
    assert!(
        matches!(
            &refused,
            lash::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Refused { .. },
                ..
            }
        ),
        "{refused:?}"
    );

    let completions: Vec<_> = observed
        .records(&core, "domain_completed")
        .into_iter()
        .filter(|record| {
            matches!(
                &record.event,
                lash_trace::TraceEvent::DomainCompleted { completion }
                    if matches!(completion.subject, lash_trace::TraceDomainSubject::ToolIntent { .. })
            )
        })
        .collect();
    assert_eq!(
        completions.len(),
        2,
        "one settlement per key, none for the redelivery: {completions:#?}"
    );
    let spans = observed.spans("lash.tool_intent");
    assert_eq!(spans.len(), 2, "{spans:#?}");
    let producers = observed.spans("lash.tool_intent.submit");
    for span in &spans {
        assert!(
            producers
                .iter()
                .any(|producer| producer.span_context.span_id() == span.parent_span_id),
            "each settlement is a child of its submission: {producers:#?}"
        );
    }
    let mut outcomes: Vec<_> = completions
        .iter()
        .map(|record| {
            let span = span_of(&spans, record);
            assert_eq!(
                attribute(span, "lash.tool_intent.kind").as_deref(),
                Some("start_process")
            );
            (
                attribute(span, "lash.outcome").expect("an outcome"),
                failed(span),
            )
        })
        .collect();
    outcomes.sort();
    assert_eq!(
        outcomes,
        [("completed".to_owned(), false), ("failed".to_owned(), true)]
    );
    assert_eq!(observed.counter("lash.tool_intent.executed"), 1);
    assert_eq!(observed.counter("lash.tool_intent.refused"), 1);
    core.shutdown().await?;
    Ok(())
}

/// `lash.process`: a process's committed terminal is exported once, from
/// its registration's retained start to its terminal's retained time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_process_exports_one_lash_process_span() -> Result<()> {
    let (observed, core, registry) = observed_engine_core().await?;
    let ingress = core.tool_intents(
        lash::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let started = ingress
        .submit(
            ingress.key("emission-process", 0).expect("a key"),
            engine_start_intent(INGRESS_ENGINE_KIND, serde_json::json!({"program": "known"})),
        )
        .await;
    let process = started_process_id(&started);
    observed
        .until("the process's terminal is exported", |observed| {
            !observed.spans("lash.process").is_empty()
        })
        .await;
    let record = registry
        .get_process(&process)
        .await?
        .expect("the started process");
    let lash_core::ProcessLifecycleState::Terminal { occurred_at_ms, .. } = record.lifecycle else {
        panic!("the process ended: {:?}", record.lifecycle);
    };

    let completions: Vec<_> = observed
        .records(&core, "domain_completed")
        .into_iter()
        .filter(|record| {
            matches!(
                &record.event,
                lash_trace::TraceEvent::DomainCompleted { completion }
                    if matches!(completion.subject, lash_trace::TraceDomainSubject::Process { .. })
            )
        })
        .collect();
    assert_eq!(completions.len(), 1, "one terminal: {completions:#?}");
    assert_eq!(
        Some(completions[0].timestamp.unix_ms()),
        Some(occurred_at_ms),
        "the record's time is the terminal's retained time"
    );
    let spans = observed.spans("lash.process");
    assert_eq!(spans.len(), 1, "{spans:#?}");
    let span = span_of(&spans, &completions[0]);
    assert_eq!(
        attribute(span, "lash.outcome").as_deref(),
        Some("completed")
    );
    assert_eq!(
        attribute(span, "lash.process.id"),
        Some(process.to_string())
    );
    assert!(span.start_time <= span.end_time);
    core.shutdown().await?;
    Ok(())
}

/// `lash.parked_work.parks`: a process whose engine refuses its first
/// transition parks, and its committed park is counted once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_process_counts_one_park() -> Result<()> {
    let (observed, core, _registry) = observed_engine_core().await?;
    assert_eq!(observed.counter("lash.parked_work.parks"), 0);
    let ingress = core.tool_intents(
        lash::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let started = ingress
        .submit(
            ingress.key("emission-park", 0).expect("a key"),
            engine_start_intent(
                INGRESS_ENGINE_KIND,
                serde_json::json!({"program": "refuse"}),
            ),
        )
        .await;
    let process = started_process_id(&started);
    let actor =
        lash_core::durable_port::ActorKey::process(process.as_str()).expect("a process actor key");
    tokio::time::timeout(WITHIN, async {
        loop {
            if let Ok(Some(snapshot)) = core.backend().durable().actor(&actor).await
                && snapshot.state == lash_core::durable_port::ActorState::Parked
            {
                assert!(
                    snapshot
                        .park
                        .as_deref()
                        .is_some_and(|reason| reason.contains("advance_refused")),
                    "{:?}",
                    snapshot.park
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the process parks on its engine's refusal");
    observed
        .until("the park is counted", |observed| {
            observed.counter("lash.parked_work.parks") > 0
        })
        .await;
    assert_eq!(observed.counter("lash.parked_work.parks"), 1);
    assert!(
        observed.spans("lash.process").is_empty(),
        "a parked process has no terminal"
    );
    core.shutdown().await?;
    Ok(())
}

/// L9: a cancellation on a node without the engine uses the process's
/// recorded classification, so an admitted workflow remains a workflow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workflow_cancelled_without_its_engine_keeps_its_recorded_trace_class() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref()).await;
    let registry = backend.process_registry();
    let (observed, builder) = Observed::install(explicit_ephemeral_facets(
        LashCore::standard_builder(backend),
    ));
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let parent = lash_trace::TraceCarrier::parse_w3c(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        None,
    )
    .unwrap();
    let mut registration = lash_core::ProcessRegistration::new(
        lash_core::ProcessInput::Engine {
            kind: "absent-workflow-engine".into(),
            payload: serde_json::json!({}),
            launch: lash_core::EngineLaunch::new(
                lash_core::testing::process_execution_env_fixture_ref(),
                None,
            ),
        },
        lash_core::ProcessProvenance::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .with_trace_class(lash_core::ProcessTraceClass::Workflow)
    .with_trace(lash_trace::TraceScopeOffer::new(
        lash_trace::TraceCause::Parent(parent),
        lash_trace::TraceLinks::new(),
        None,
    ));
    registration.identity.label = Some("retained-workflow".into());
    let record = registry.register_process(registration).await?;
    registry
        .request_process_cancel(
            &record.id,
            lash_core::CancelOrigin::OperatorRequested,
            "trace-law".into(),
            None,
        )
        .await?;
    observed
        .until("the terminal is published", |observed| {
            observed
                .records(&core, "domain_completed")
                .iter()
                .any(|record| record.context.process_id.is_some())
                || !observed.spans("invoke_workflow").is_empty()
                || !observed.spans("lash.process").is_empty()
        })
        .await;
    let spans = observed.spans("invoke_workflow retained-workflow");
    assert_eq!(
        spans.len(),
        1,
        "the retained workflow must not become an internal process"
    );
    assert_eq!(
        attribute(&spans[0], "gen_ai.operation.name").as_deref(),
        Some("invoke_workflow")
    );
    assert_eq!(
        attribute(&spans[0], "lash.outcome").as_deref(),
        Some("cancelled")
    );
    assert!(!failed(&spans[0]));
    assert_eq!(
        registry.get_process(&record.id).await?.unwrap().trace_class,
        lash_core::ProcessTraceClass::Workflow
    );
    core.shutdown().await?;
    Ok(())
}
