//! Every trace record, span and operational metric lash wires has a
//! production producer (FIG-5658): a host's real run over SQLite memory
//! stores writes the record to the host's `TraceSink` and exports its span or
//! metric through the host's OpenTelemetry SDK providers.
//!
//! The logical records are emitted by the owner whose commit was
//! acknowledged, so each law also resubmits its work and finds no second
//! record.

use super::tool_intent_ingress::{
    INGRESS_ENGINE_KIND, SCOPE, SESSION, engine_start_intent, ingress_engine_core_with,
    started_process_id,
};
use super::tracing::{text_call, tool_call};
use super::*;

use crate::support::TurnInput;
use lash_core::testing::runtime_helpers::{EchoTool, mock_provider};
use lash_trace::otel::{OtelOptions, OtelTelemetry};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

const WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

/// What a host installs to observe a core: a JSONL record sink and an
/// OpenTelemetry adapter over SDK providers it owns.
struct Observed {
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
    spans: InMemorySpanExporter,
    metrics: InMemoryMetricExporter,
    meter: SdkMeterProvider,
    _tracer: SdkTracerProvider,
}

impl Observed {
    /// The observers, and `builder` with them installed.
    fn install(builder: crate::core::LashCoreBuilder) -> (Self, crate::core::LashCoreBuilder) {
        let dir = tempfile::tempdir().expect("trace directory");
        let path = dir.path().join("trace.jsonl");
        let spans = InMemorySpanExporter::default();
        let tracer = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let metrics = InMemoryMetricExporter::default();
        let meter = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build();
        let builder = builder
            .trace_jsonl_path(path.clone())
            .telemetry(OtelTelemetry::new(&tracer, &meter, OtelOptions::default()));
        (
            Self {
                path,
                _dir: dir,
                spans,
                metrics,
                meter,
                _tracer: tracer,
            },
            builder,
        )
    }

    /// Every record the sink holds of kind `kind`.
    #[allow(
        clippy::disallowed_methods,
        reason = "the law reads back the trace file its own core wrote"
    )]
    fn records(&self, core: &LashCore, kind: &str) -> Vec<lash_trace::TraceRecord> {
        core.flush_trace_sink().expect("flush the trace sink");
        lash_trace::parse_jsonl_records::<lash_trace::TraceRecord>(
            &std::fs::read_to_string(&self.path).unwrap_or_default(),
        )
        .expect("trace records")
        .into_iter()
        .filter(|record| record.event.kind().as_str() == kind)
        .collect()
    }

    /// Every exported span whose name is, or starts with, `name`.
    fn spans(&self, name: &str) -> Vec<SpanData> {
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
    async fn until(&self, what: &str, ready: impl Fn(&Self) -> bool) {
        tokio::time::timeout(WITHIN, async {
            while !ready(self) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
    }
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
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
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        mock_provider(vec![
            tool_call("call-echo", "echo_tool", r#"{"value":"emitted"}"#),
            text_call("done"),
        ])
        .into_handle(),
        mock_llm_profile_spec(),
    )
    .tools(Arc::new(EchoTool));
    let (observed, builder) = Observed::install(builder);
    let core = builder.build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(TURN_SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    for _ in 0..2 {
        let output = session
            .send(TurnInput::text("call the tool"))
            .id(crate::TurnId::parse(TURN).expect("nonblank host identity"))
            .output()
            .await?;
        assert!(
            matches!(output.result.outcome, crate::TurnOutcome::Finished(_)),
            "{:?}",
            output.result.outcome
        );
    }
    Ok((observed, core))
}

/// `turn_started`: the turn's admission, once, as the logical start of its
/// scope, at the scope's retained start. It has no span of its own: the
/// turn's admission exports `lash.turn.admitted`.
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
    let admitted = observed.spans("lash.turn.admitted");
    assert_eq!(admitted.len(), 1, "the turn's admission is exported once");
    let since_epoch = |at: std::time::SystemTime| {
        at.duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_millis()
    };
    assert_eq!(
        since_epoch(record.timestamp.into()),
        since_epoch(admitted[0].start_time),
        "the start is the admission's retained time, in whole milliseconds"
    );
    core.shutdown().await?;
    Ok(())
}

/// `turn_completed` and its `invoke_agent` span: the turn's committed
/// terminal, once, under the admission's anchor.
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
    let admitted = observed.spans("lash.turn.admitted");
    assert_eq!(
        span.parent_span_id,
        admitted[0].span_context.span_id(),
        "the completion is a child of the turn's admission anchor"
    );
    assert!(span.start_time <= span.end_time);
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
    assert_eq!(observed.spans("lash.tool.admitted").len(), 1);
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
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
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
                crate::tools::ToolIntentIngressOutcome::Admitted {
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
            crate::tools::ToolIntentIngressOutcome::Admitted {
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
                    if completion.operation == lash_trace::TraceDomainOperation::ToolIntent
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
    assert_eq!(
        observed.spans("lash.tool_intent.admitted").len(),
        2,
        "each first submission's admission is exported once"
    );
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
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
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
                    if completion.operation == lash_trace::TraceDomainOperation::Process
            )
        })
        .collect();
    assert_eq!(completions.len(), 1, "one terminal: {completions:#?}");
    assert_eq!(
        u64::try_from(completions[0].timestamp.timestamp_millis()).ok(),
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
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
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
            if let Ok(Some(snapshot)) = core.backend.durable().actor(&actor).await
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
