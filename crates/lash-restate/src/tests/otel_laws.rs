//! Golden law P3 of the tracing design (FIG-4830): a turn's trace is the same
//! whether its handler runs once or replays its journal from the start at
//! every await.
//!
//! The turn runs through the endpoint's real turn handler on the in-process
//! server double, over a SQLite memory store set. Its shift observes around
//! each recorded step and each step's body observes from inside, the way the
//! engine's turn loop and its effect bodies do. Under always-replay every
//! step suspends the handler, and every resumption replays the journal up to
//! the next step, which then runs live: a new live suffix after each replay.

use super::conformance_harness::{HarnessServer, LiveConformanceHarness};
use super::*;
use lash_core::{AdmittedScope, CancellationToken};

/// How many recorded steps the turn issues.
const STEPS: usize = 3;

fn observation(label: String) -> (lash_trace::TraceContext, lash_trace::TraceEvent) {
    (
        lash_trace::TraceContext::default(),
        lash_trace::TraceEvent::ProtocolStep {
            plugin_id: "golden-tree".to_string(),
            payload: serde_json::json!(label),
        },
    )
}

/// What one run of the turn left behind.
struct Observed {
    /// Each record's label, in emission order.
    labels: Vec<String>,
    /// Each record's id.
    ids: Vec<String>,
    /// How many times the handler ran the turn from its start.
    handler_runs: usize,
    /// How many times a step body recorded its live-class metric.
    metric: usize,
}

async fn run_golden_turn(always_replay: bool) -> Observed {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::InProcess {
        seed,
        always_replay,
    })
    .await;
    let sink = Arc::new(RecordingTraceSink::default());
    let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
    let tracing = lash_core::facade_support::TraceRuntime::default().with_trace_sink(sink_dyn);
    let session_id = SessionId::fixture(format!("golden-tree-{}", harness.run_nonce()));
    let turn_id = TurnId::from("golden-tree-turn");
    let handler_runs = Arc::new(AtomicUsize::new(0));
    let metric = Arc::new(AtomicUsize::new(0));

    let attempt: lash_conformance::ConformanceTurnAttempt = {
        let tracing = tracing.clone();
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        let handler_runs = Arc::clone(&handler_runs);
        let metric = Arc::clone(&metric);
        Arc::new(move |controller| {
            let tracing = tracing.clone();
            let session_id = session_id.clone();
            let turn_id = turn_id.clone();
            let handler_runs = Arc::clone(&handler_runs);
            let metric = Arc::clone(&metric);
            Box::pin(async move {
                handler_runs.fetch_add(1, Ordering::SeqCst);
                let turn = tracing.turn_execution(&controller);
                turn.observe_deferred(move || observation("turn started".to_string()));
                for step in 0..STEPS {
                    turn.observe_deferred(move || observation(format!("step {step} issued")));
                    let effect_id = format!("golden-tree-step-{step}");
                    let envelope = RuntimeEffectEnvelope::new(
                        lash_core::RuntimeEffectInvocation::new(
                            lash_core::EffectAddress::new(
                                ExecutionScope::turn(&session_id, &turn_id),
                                effect_id.clone(),
                            )
                            .expect("valid turn effect address"),
                            lash_core::RuntimeAttribution::for_turn(&session_id, &turn_id, 0, step),
                            effect_id.clone(),
                        ),
                        RuntimeEffectCommand::ToolAttempt {
                            call: Box::new(prepared_tool_call_with(&effect_id, "golden_tree_tool")),
                            execution_grant: None,
                            attempt: 1,
                            max_attempts: 1,
                        },
                    );
                    let body_tracing = tracing.clone();
                    let body_metric = Arc::clone(&metric);
                    controller
                        .execute_effect(
                            envelope,
                            RuntimeEffectLocalExecutor::testing_in_step(
                                move |_envelope, live| async move {
                                    let body = body_tracing.effect_body(&live);
                                    // A live-class metric is recorded under
                                    // the body's permit, which only a body
                                    // that really runs holds.
                                    if body.body_permit().is_some() {
                                        body_metric.fetch_add(1, Ordering::SeqCst);
                                    }
                                    body.observe(|| observation(format!("step {step} ran")));
                                    Ok(restate_segment_tool_attempt_outcome(step as u64))
                                },
                            ),
                        )
                        .await
                        .expect("the recorded step answers");
                    turn.observe_deferred(move || observation(format!("step {step} answered")));
                }
                turn.observe_deferred(move || observation("turn completed".to_string()));
                turn.conclude();
                lash_conformance::ConformanceTurnEnd::Settled
            })
        })
    };
    harness
        .turn_runner()
        .run_turn(
            lash_core::AdmittedScope::turn(&session_id, &turn_id),
            attempt,
        )
        .await;

    let records = sink.records.lock_recover();
    let labels = records
        .iter()
        .map(|record| match &record.event {
            lash_trace::TraceEvent::ProtocolStep { payload, .. } => payload
                .as_str()
                .expect("the law's records carry a label")
                .to_string(),
            other => panic!("the law emits only its own records, got {}", other.kind()),
        })
        .collect();
    let ids = records.iter().map(|record| record.id.clone()).collect();
    Observed {
        labels,
        ids,
        handler_runs: handler_runs.load(Ordering::SeqCst),
        metric: metric.load(Ordering::SeqCst),
    }
}

fn golden_tree() -> Vec<String> {
    let mut tree = vec!["turn started".to_string()];
    for step in 0..STEPS {
        tree.push(format!("step {step} issued"));
        tree.push(format!("step {step} ran"));
        tree.push(format!("step {step} answered"));
    }
    tree.push("turn completed".to_string());
    tree
}

#[tokio::test]
async fn journal_frontier_keeps_only_live_suffixes() {
    let once = run_golden_turn(false).await;
    assert_eq!(
        once.labels,
        golden_tree(),
        "a handler that runs once observes the whole tree in order"
    );
    assert_eq!(once.metric, STEPS, "each body records its metric once");

    let replayed = run_golden_turn(true).await;
    assert!(
        replayed.handler_runs > once.handler_runs,
        "always-replay re-ran the handler from its start ({} runs against {})",
        replayed.handler_runs,
        once.handler_runs
    );
    let mut replayed_tree = golden_tree();
    replayed_tree.truncate(replayed_tree.len() - 2);
    assert_eq!(
        replayed.labels, replayed_tree,
        "a replay observes nothing an earlier attempt observed, and every live suffix after a \
         replay is observed once; a replay-only conclusion grants no live permission"
    );
    assert_eq!(
        replayed.metric, STEPS,
        "a replayed step records no metric; each body recorded its metric once"
    );
    let distinct = replayed.ids.iter().collect::<HashSet<_>>();
    assert_eq!(
        distinct.len(),
        replayed.ids.len(),
        "every record names itself once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golden_tree_survives_replay_and_redrive() {
    use lash_trace::otel::{OtelOptions, OtelTelemetry};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
    let spans = InMemorySpanExporter::default();
    let metrics = InMemoryMetricExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_simple_exporter(spans.clone())
        .build();
    let meter = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(metrics.clone()).build())
        .build();
    let sink = Arc::new(RecordingTraceSink::default());
    let backend = lash_restate_test::backend(
        0x4830,
        lash_restate_test::ServerConfig {
            build_generation: super::replay_corpus::current_generation().await,
            always_replay: true,
            ..Default::default()
        },
    )
    .await
    .expect("always-replay server over SQLite memory");
    let runtime = |clock: Arc<dyn lash_core::Clock>| {
        let adapter = Arc::new(OtelTelemetry::new(
            &provider,
            &meter,
            OtelOptions::default(),
        ));
        lash_core::facade_support::TraceRuntime::new(clock)
            .with_scopes(adapter.clone())
            .with_projector(adapter.clone())
            .with_metrics(adapter.metrics().clone())
            .with_trace_sink(sink.clone())
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(1));
    let core = super::replay_corpus::service_journals::build_core_with_trace(
        backend.lash_backend(),
        &release,
        Some(runtime(backend.test_clock())),
        Some(calls.clone()),
    );
    backend.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("process worker configuration"),
        )
        .expect("build process worker"),
    );
    let session_id = SessionId::fixture("otel-production-golden");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("create session");
    backend
        .ingress()
        .call_object_json::<_, crate::Reply<()>>(
            "LashDurableWaitIndex",
            session_id.as_str(),
            "reinstate",
            &crate::Call::new(()),
        )
        .await
        .expect("initialize wait index");
    let session = core
        .session(session_id.clone())
        .open()
        .await
        .expect("open session");
    let producer = lash_trace::TraceCarrier::parse_w3c(
        "00-00000000000000000000000000000011-0000000000000022-01",
        None,
    )
    .expect("producer context");
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        session
            .send(lash::TurnInput::text("count once"))
            .id(lash::TurnId::parse("golden-first").expect("nonblank host identity"))
            .trace_context(producer.clone())
            .output(),
    )
    .await
    .unwrap_or_else(|error| panic!("first send completes within its bound: {error}; calls={}; paused={:?}; invocations={:?}",
        calls.load(Ordering::SeqCst), backend.paused_session_work(&session_id), backend.server().invocations()))
    .expect("first send");
    backend.settle_session_shift(&session_id).await;
    provider.force_flush().expect("first spans");
    let first_records = sink.records.lock_recover().clone();
    let lifecycle = |records: &[lash_trace::TraceRecord]| {
        records
            .iter()
            .filter(|record| {
                matches!(
                    record.event,
                    lash_trace::TraceEvent::TurnStarted { .. }
                        | lash_trace::TraceEvent::TurnCompleted { .. }
                        | lash_trace::TraceEvent::ToolCallStarted { .. }
                        | lash_trace::TraceEvent::ToolCallCompleted { .. }
                        | lash_trace::TraceEvent::DomainCompleted { .. }
                )
            })
            .count()
    };
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the model actually runs twice, despite replay at every await"
    );
    assert_eq!(
        lifecycle(&first_records),
        7,
        "one segment terminal, one start and terminal for turn/tool, one committed run terminal and awaited process terminal; records: {:?}",
        first_records
            .iter()
            .map(|record| record.event.kind().as_str())
            .collect::<Vec<_>>()
    );
    let first_spans = spans.get_finished_spans().expect("exported first tree");
    let root = first_spans
        .iter()
        .find(|span| span.name == "lash.run.admitted")
        .expect("selected root anchor");
    let send = first_spans
        .iter()
        .find(|span| span.name == "lash.send")
        .expect("committed host send");
    assert_eq!(send.parent_span_id.to_string(), "0000000000000022");
    assert_eq!(root.links.len(), 1);
    assert_eq!(
        root.links[0].span_context.span_id().to_string(),
        send.span_context.span_id().to_string()
    );
    assert_ne!(
        root.span_context.trace_id().to_string(),
        "00000000000000000000000000000011",
        "a send links its own new root"
    );
    let turn = first_records
        .iter()
        .find(|record| matches!(record.event, lash_trace::TraceEvent::TurnCompleted { .. }))
        .expect("retained turn terminal");
    let tool = first_records
        .iter()
        .find(|record| {
            matches!(
                record.event,
                lash_trace::TraceEvent::ToolCallCompleted { .. }
            )
        })
        .expect("retained tool terminal");
    let turn_owner = lash_trace::TraceScopeOwner::Turn {
        session_id: session_id.clone(),
        turn_id: "golden-first".into(),
    };
    let tool_owner = lash_trace::TraceScopeOwner::Tool {
        owner: lash_trace::TraceToolOwner::Turn {
            session_id: session_id.clone(),
            turn_id: "golden-first".into(),
        },
        call_id: match &tool.event {
            lash_trace::TraceEvent::ToolCallCompleted { call_id, .. } => call_id.to_string(),
            _ => unreachable!(),
        },
    };
    // A Run-routed call's receipt is its scope's terminal at ordinal 0, and
    // the projected tool terminal follows it at ordinal 1 (FIG-4830).
    for (record, owner, ordinal) in [(turn, turn_owner, 0), (tool, tool_owner, 1)] {
        let expected = lash_trace::TraceRecordIdentity::Transition {
            scope: lash_trace::TraceScopeId::admission(owner),
            transition: lash_trace::TraceTransitionKind::Terminal,
            ordinal,
        }
        .record_id()
        .expect("terminal identity");
        assert_eq!(record.id, expected);
    }
    let tool_anchor = first_spans
        .iter()
        .find(|span| span.name == "lash.tool.admitted")
        .expect("tool anchor");
    let tool_span = first_spans
        .iter()
        .find(|span| span.name.starts_with("execute_tool"))
        .expect("tool duration");
    assert_eq!(tool_span.parent_span_id, tool_anchor.span_context.span_id());
    let turn_span = first_spans
        .iter()
        .find(|span| span.name.starts_with("invoke_agent"))
        .expect("turn duration");
    assert_eq!(
        tool_anchor.parent_span_id, turn_span.parent_span_id,
        "tool admission and turn duration descend from the retained turn anchor"
    );
    assert_eq!(
        first_spans
            .iter()
            .filter(|span| span.name.starts_with("chat "))
            .count(),
        2
    );
    for model in first_spans
        .iter()
        .filter(|span| span.name.starts_with("chat "))
    {
        assert_eq!(model.parent_span_id, turn_span.parent_span_id);
    }
    let process_anchor = first_spans
        .iter()
        .find(|span| span.name == "lash.process.admitted")
        .expect("the declared child retains its own process anchor");
    assert_eq!(
        process_anchor.parent_span_id,
        tool_anchor.span_context.span_id()
    );
    for (span, started, terminal) in [
        (
            turn_span,
            lash_trace::TraceEventKind::TurnStarted,
            lash_trace::TraceEventKind::TurnCompleted,
        ),
        (
            tool_span,
            lash_trace::TraceEventKind::ToolCallStarted,
            lash_trace::TraceEventKind::ToolCallCompleted,
        ),
    ] {
        let start = first_records
            .iter()
            .find(|record| record.event.kind() == started)
            .expect("retained start");
        let end = first_records
            .iter()
            .find(|record| record.event.kind() == terminal)
            .expect("retained terminal");
        assert_eq!(
            span.start_time,
            std::time::SystemTime::from(start.timestamp)
        );
        assert_eq!(span.end_time, std::time::SystemTime::from(end.timestamp));
    }
    for (operation, name) in [
        (lash_trace::TraceDomainOperation::Process, "lash.process"),
        (
            lash_trace::TraceDomainOperation::ProcessSegment,
            "lash.process.segment",
        ),
    ] {
        let record = first_records.iter().find(|record| matches!(
            &record.event, lash_trace::TraceEvent::DomainCompleted { completion } if completion.operation == operation
        )).expect("committed process completion");
        let lash_trace::TraceEvent::DomainCompleted { completion } = &record.event else {
            unreachable!()
        };
        let span = first_spans
            .iter()
            .find(|span| span.name == name)
            .expect("process duration");
        assert_eq!(span.parent_span_id, process_anchor.span_context.span_id());
        assert_eq!(
            span.start_time,
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(completion.started_at_ms)
        );
        assert_eq!(span.end_time, std::time::SystemTime::from(record.timestamp));
    }
    drop(session);
    drop(core);
    let backend = backend
        .restart()
        .await
        .expect("recreate deployment with retained journals");
    let core = super::replay_corpus::service_journals::build_core_with_trace(
        backend.lash_backend(),
        &release,
        Some(runtime(backend.test_clock())),
        Some(calls.clone()),
    );
    let session = core
        .session(session_id.clone())
        .open()
        .await
        .expect("reopen with recreated adapter");
    session
        .send(lash::TurnInput::text("count once"))
        .id(lash::TurnId::parse("golden-first").expect("nonblank host identity"))
        .output()
        .await
        .expect("redrive same submission");
    assert_eq!(
        lifecycle(&sink.records.lock_recover()),
        lifecycle(&first_records),
        "a redrive emits no retained lifecycle twice"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "redrive does not call a provider"
    );
    session
        .send(lash::TurnInput::text("answer again"))
        .id(lash::TurnId::parse("golden-second").expect("nonblank host identity"))
        .output()
        .await
        .expect("second independent send");
    backend.settle_session_shift(&session_id).await;
    provider.force_flush().expect("final spans");
    meter.force_flush().expect("actual exported metrics");
    let final_records = sink.records.lock_recover();
    assert_eq!(
        lifecycle(&final_records),
        10,
        "second send adds its turn start/terminal and run terminal"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        final_records
            .iter()
            .filter(|record| matches!(
                record.event,
                lash_trace::TraceEvent::LlmAttemptCompleted { .. }
            ))
            .count(),
        3
    );
    let ids = final_records
        .iter()
        .map(|record| &record.id)
        .collect::<HashSet<_>>();
    assert_eq!(
        ids.len(),
        final_records.len(),
        "no logical or actual-attempt identity repeats"
    );
    let final_spans = spans.get_finished_spans().expect("final tree");
    let roots = final_spans
        .iter()
        .filter(|span| span.name == "lash.run.admitted")
        .collect::<Vec<_>>();
    assert_eq!(roots.len(), 2);
    assert_ne!(
        roots[0].span_context.trace_id(),
        roots[1].span_context.trace_id()
    );
    assert!(
        roots[1].links.len() == 1,
        "a fresh send links its independent host send"
    );
    for span in &final_spans {
        if span.parent_span_id.to_bytes() != [0; 8]
            && span.parent_span_id.to_string() != "0000000000000022"
        {
            assert!(
                final_spans
                    .iter()
                    .any(
                        |parent| parent.span_context.span_id() == span.parent_span_id
                            && parent.span_context.trace_id() == span.span_context.trace_id()
                    ),
                "exported parent exists for {}",
                span.name
            );
        }
    }
    let exported = metrics
        .get_finished_metrics()
        .expect("finished metric exports");
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    let responses = final_spans
        .iter()
        .filter(|span| span.name.starts_with("chat "))
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 3, "only actual responses report usage");
    for response in responses {
        for (key, expected) in [
            ("gen_ai.usage.input_tokens", 10),
            ("gen_ai.usage.output_tokens", 4),
        ] {
            assert!(
                response.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == key
                        && attribute.value == lash_trace::otel::api::Value::I64(expected)
                }),
                "{} reports {key}={expected} from its response",
                response.name
            );
        }
    }
    let executed = exported
        .iter()
        .flat_map(|resource| resource.scope_metrics())
        .flat_map(|scope| scope.metrics())
        .filter(|metric| metric.name() == "lash.tool_intent.executed")
        .filter_map(|metric| match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                Some(sum.data_points().map(|point| point.value()).sum::<u64>())
            }
            _ => None,
        })
        .sum::<u64>();
    assert_eq!(
        executed, 1,
        "only the committed tool settlement counts, even after replay and redrive"
    );
    let names = metrics
        .get_finished_metrics()
        .expect("finished metric exports")
        .into_iter()
        .flat_map(|resource| {
            resource
                .scope_metrics()
                .flat_map(|scope| scope.metrics().map(|metric| metric.name().to_string()))
                .collect::<Vec<_>>()
        })
        .collect::<HashSet<_>>();
    assert!(
        names.contains("lash.tool_intent.executed"),
        "committed intent counter exported: {names:?}"
    );
}

#[tokio::test]
async fn a_replay_only_wait_resolution_emits_once_from_its_sql_receipt() {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory"),
    );
    let sink = Arc::new(RecordingTraceSink::default());
    let host = lash_core::facade_support::RuntimeHostConfig::new(
        lash_conformance::recording_backend_over(stores),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    let tracing = host.tracing.with_trace_sink(sink.clone());
    let session = SessionId::fixture("replayed-wait-session");
    let turn = TurnId::from("replayed-wait-turn");
    let scope = ExecutionScope::turn(&session, &turn);
    let key = test_restate_await_event_key(
        &scope,
        lash_core::AwaitEventWaitIdentity::Custom {
            key: "last-wait".into(),
        },
    )
    .expect("wait key");
    let context = Arc::new(ReplayableRecordingContext::default());
    let resolution = Resolution::Ok(serde_json::json!({"accepted":true}));
    context
        .events
        .resolve_durable_event(RestateDurableWaitResolveRequest {
            key: key.clone(),
            resolution: resolution.clone(),
        });
    let priming = RestateRuntimeEffectController::new_for_test(context.clone());
    let invocation = lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(scope.clone(), "last-wait").expect("address"),
        lash_core::RuntimeAttribution::for_turn(&session, &turn, 0, 0),
        "last-wait",
    );
    priming
        .execute_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::AwaitEvent { key: key.clone() },
            ),
            RuntimeEffectLocalExecutor::await_event(CancellationToken::new())
                .with_turn_cancel_scope(scope.clone()),
        )
        .await
        .expect("prime native journal");
    let journal_size = context.records.lock_recover().len();
    context.replaying.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let controller = RestateRuntimeEffectController::new_for_test(context.clone());
        let scoped = controller
            .scoped_effect_controller(AdmittedScope::turn(&session, &turn))
            .expect("scope")
            .with_trace_scope(lash_trace::DurableTraceScope {
                scope: lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
                    session_id: session.clone(),
                    turn_id: turn.clone(),
                }),
                cause: lash_trace::TraceCause::Root,
                anchor: lash_trace::TraceAnchor::Untraced,
                started_at_ms: 1,
            });
        let standing = tracing.shift(scoped.trace_scope().cloned(), &scoped);
        let invocation = lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope.clone(), "last-wait").expect("address"),
            lash_core::RuntimeAttribution::for_turn(&session, &turn, 0, 0),
            "last-wait",
        );
        let outcome = scoped
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::AwaitEvent { key: key.clone() },
                ),
                RuntimeEffectLocalExecutor::await_event(CancellationToken::new())
                    .with_turn_cancel_scope(scope.clone()),
            )
            .await
            .expect("replayed resolution");
        assert!(
            matches!(outcome, RuntimeEffectOutcome::AwaitEvent { resolution: ref result } if result == &resolution)
        );
        assert!(
            !scoped.frontier().is_crossed(),
            "the native wait executed no journaled body"
        );
        standing.conclude();
        assert!(
            !scoped.frontier().is_crossed(),
            "conclusion grants no permission"
        );
    }
    assert_eq!(
        context.records.lock_recover().len(),
        journal_size,
        "observed calls added no journaled body"
    );
    let records = sink.records.lock_recover();
    assert_eq!(
        records.len(),
        2,
        "one SQL request and one SQL resolution across replay"
    );
    assert!(matches!(
        records[0].event,
        lash_trace::TraceEvent::DurableWaitParked { .. }
    ));
    assert!(matches!(
        records[1].event,
        lash_trace::TraceEvent::DurableWaitResolved {
            resolution: lash_trace::TraceDurableWaitResolution::Ok,
            ..
        }
    ));
    assert_ne!(records[0].id, records[1].id);
    assert!(records[0].timestamp <= records[1].timestamp);
}
