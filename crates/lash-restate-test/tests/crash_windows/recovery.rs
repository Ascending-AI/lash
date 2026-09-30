use super::*;

const SIGNAL: &str = "go";

fn worker(core: &lash::LashCore) -> lash::durability::DurableProcessWorker {
    lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config().expect("worker config"),
    )
    .expect("fresh process worker")
}

async fn waiting_request(engine: &Engine, live_bytes: usize) -> lash_core::ProcessStartRequest {
    waiting_request_with_sleep(engine, live_bytes, "20ms").await
}

async fn waiting_request_with_sleep(
    engine: &Engine,
    live_bytes: usize,
    sleep: &str,
) -> lash_core::ProcessStartRequest {
    let call = || b::module_call(&["tools"], TOOL, vec![b::record(Vec::new())]);
    let program = b::module(
        vec![b::process_with_signals(
            PROCESS,
            Vec::new(),
            vec![b::signal(SIGNAL, lashlang::TypeExpr::Any)],
            b::block(vec![
                b::assign("live", b::string(&"x".repeat(live_bytes))),
                b::sleep_for(b::string(sleep)),
                b::assign("first", call()),
                b::assign("signal", b::wait_signal(SIGNAL)),
                b::assign("second", call()),
                b::finish(b::record(vec![
                    ("first", b::var("first")),
                    ("signal", b::var("signal")),
                    ("second", b::var("second")),
                    ("live", b::var("live")),
                ])),
            ]),
        )],
        Vec::new(),
    );
    let input = publish_program(
        engine,
        program,
        lashlang::LashlangAbilities::default().with_sleep(),
    )
    .await;
    lash_core::ProcessStartRequest::new(
        input.into_process_input().expect("process input"),
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_spec(process_env_spec())
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_extra_event_types([lash_core::ProcessEventType {
        name: "signal.go".to_owned(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec::default(),
    }])
}

async fn start(
    engine: &Engine,
    core: &lash::LashCore,
    request: lash_core::ProcessStartRequest,
) -> ProcessId {
    let id = Arc::new(Mutex::new(None));
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let id = Arc::clone(&id);
        Arc::new(move |scoped| {
            let core = core.clone();
            let id = Arc::clone(&id);
            let request = request.clone();
            Box::pin(async move {
                *id.lock().unwrap() = Some(
                    core.processes()
                        .start(request, scoped)
                        .await
                        .expect("start process")
                        .process_id,
                );
            })
        })
    };
    tokio::time::timeout(
        BOUND,
        engine.run_in_handler(
            lash_core::AdmittedScope::runtime_operation(run_tag("start-recovery")),
            attempt,
        ),
    )
    .await
    .expect("start finishes")
    .expect("start result");
    id.lock().unwrap().clone().expect("process id")
}

async fn signal(engine: &Engine, core: &lash::LashCore, id: &ProcessId) {
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let id = id.clone();
        Arc::new(move |scoped| {
            let core = core.clone();
            let id = id.clone();
            Box::pin(async move {
                let event = lash_core::ProcessEventAppendRequest::new(
                    lash_core::facade_support::process_signal_event_type(SIGNAL)
                        .expect("signal type"),
                    json!({"go": 1}),
                )
                .with_replay_key(
                    lash_core::facade_support::process_signal_wait_key(&id, SIGNAL, "signal-1"),
                );
                core.processes()
                    .signal(&id, SIGNAL, "signal-1", event, scoped)
                    .await
                    .expect("deliver signal");
            })
        })
    };
    tokio::time::timeout(
        BOUND,
        engine.run_in_handler(
            lash_core::AdmittedScope::runtime_operation(run_tag("signal-recovery")),
            attempt,
        ),
    )
    .await
    .expect("signal finishes")
    .expect("signal result");
}

async fn record_where(
    engine: &Engine,
    id: &ProcessId,
    matches: impl Fn(&lash_core::ProcessRecord) -> bool,
) -> lash_core::ProcessRecord {
    tokio::time::timeout(BOUND, async {
        loop {
            let record = engine
                .lash_backend()
                .process_registry()
                .get_process(id)
                .await
                .expect("process read")
                .expect("process exists");
            if matches(&record) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("process reaches the required state")
}

fn signal_wait(record: &lash_core::ProcessRecord) -> bool {
    matches!(record.wait.as_ref().map(|wait| &wait.kind), Some(lash_core::WaitKind::Signal { name, .. }) if name == SIGNAL)
}

async fn cold(engine: Engine, executions: &Arc<AtomicUsize>) -> (Engine, lash::LashCore) {
    match engine {
        Engine::Double(backend) => {
            let engine = Engine::Double(backend.clone());
            let core = process_core(&engine, executions);
            engine.install_process_worker(worker(&core));
            let pending: Vec<_> = backend
                .server()
                .invocations()
                .into_iter()
                .filter(|i| {
                    i.target.contains(PROCESS_WORKFLOW)
                        && i.target.ends_with("/run")
                        && i.status != "completed"
                })
                .collect();
            assert!(!pending.is_empty(), "cold replay has a pending segment");
            for invocation in pending {
                backend.server().crash(&invocation.id);
            }
            (engine, core)
        }
        Engine::Live {
            backend, restarts, ..
        } => {
            restarts.abort();
            let rebuilt = backend
                .rebuild()
                .await
                .expect("reconstruct the engine and all services");
            assert!(
                !Arc::ptr_eq(
                    backend.lash_backend().engine(),
                    rebuilt.lash_backend().engine()
                ),
                "a new engine owns the endpoint"
            );
            let engine = Engine::on_live_backend(rebuilt);
            let core = process_core(&engine, executions);
            engine.install_process_worker(worker(&core));
            (engine, core)
        }
    }
}

async fn across_wait(
    engine: Engine,
    live_bytes: usize,
    rebuild: bool,
) -> lash_conformance::SegmentBudgetObservation {
    let executions = Arc::new(AtomicUsize::new(0));
    let core = process_core(&engine, &executions);
    engine.install_process_worker(worker(&core));
    let request = waiting_request(&engine, live_bytes).await;
    let id = start(&engine, &core, request).await;
    let at_wait = record_where(&engine, &id, signal_wait).await;
    assert!(!at_wait.is_terminal());
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "only the pre-wait tool ran"
    );
    let continuation = engine
        .lash_backend()
        .stores()
        .process_continuations()
        .latest_segment_handover(&id)
        .await
        .expect("latest handover");
    let continuation_bytes = continuation
        .as_ref()
        .map_or(0, |handover| handover.handover.engine_state.len());
    let latest = continuation
        .as_ref()
        .map_or(0, |handover| handover.segment_ordinal);
    // A pending durable wait suspends in its segment. It cannot spin a chain
    // of successors, consume the second effect, or publish a terminal.
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        engine
            .lash_backend()
            .stores()
            .process_continuations()
            .latest_segment_handover(&id)
            .await
            .expect("handover while waiting")
            .map_or(0, |h| h.segment_ordinal),
        latest
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let (engine, core) = if rebuild {
        cold(engine, &executions).await
    } else {
        (engine, core)
    };
    signal(&engine, &core, &id).await;
    let output = tokio::time::timeout(BOUND, core.processes().await_output(&id))
        .await
        .expect("terminal finishes")
        .expect("terminal output");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("process did not settle")
    };
    let lash_core::ToolCallOutcome::Success(output) = output.outcome else {
        panic!("process failed: {output:?}")
    };
    let output = output.to_json_value();
    assert_eq!(
        output,
        json!({"first": {"result": "counted"}, "signal": {"go": 1}, "second": {"result": "counted"}, "live": "x".repeat(live_bytes)})
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "each recorded tool result is used once across the wait and replay"
    );
    let settled = engine
        .lash_backend()
        .process_registry()
        .get_process(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        settled.first_started, at_wait.first_started,
        "recovery keeps the original process owner and start marker"
    );
    let runs = engine.completed_process_invocations(&id).await;
    let segments = runs.iter().filter(|i| i.target.ends_with("/run")).count();
    terminal_fact_is_settled(&engine, &id).await;
    engine.finish().await;
    lash_conformance::SegmentBudgetObservation {
        output,
        segments,
        continuation_bytes_at_wait: continuation_bytes,
    }
}

struct SegmentationTier {
    replay: bool,
    live: bool,
}

#[async_trait::async_trait]
impl lash_conformance::SegmentBudgetHarness for SegmentationTier {
    async fn run(
        &self,
        budget: Option<u64>,
        live_bytes: usize,
    ) -> lash_conformance::SegmentBudgetObservation {
        let engine = if self.live {
            Engine::live("segment-budget", budget).await
        } else {
            let config = ServerConfig::default().always_replay(self.replay);
            let backend = match budget {
                Some(budget) => {
                    lash_restate_test::backend_with_segment_budget(0x4145, config, budget).await
                }
                None => lash_restate_test::backend(0x4145, config).await,
            }
            .expect("segmentation tier");
            Engine::Double(backend)
        };
        across_wait(engine, live_bytes, true).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segment_budget_and_continuation_preserve_results_across_waits() {
    for replay in [false, true] {
        lash_conformance::segment_budget_and_continuation_preserve_results_across_waits(
            &SegmentationTier {
                replay,
                live: false,
            },
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the crash-windows Restate suite runs it"]
async fn live_restate_segment_budget_and_continuation_preserve_results_across_waits() {
    lash_conformance::segment_budget_and_continuation_preserve_results_across_waits(
        &SegmentationTier {
            replay: false,
            live: true,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_completion_wait_tracks_only_owned_invocations() {
    use std::future::{Future as _, poll_fn};
    use std::task::Poll;

    for replay in [false, true] {
        let blocked = Arc::new(Mutex::new(None::<String>));
        let refuses = Arc::clone(&blocked);
        let backend = lash_restate_test::backend_with_build(
            0x4243,
            ServerConfig::default()
                .always_replay(replay)
                .time(lash_restate_test::TimeMode::Manual),
            "publication-cut",
            lash_restate_test::DeploymentHooks {
                served: None,
                refuse: Some(Arc::new(move |dispatch| {
                    (dispatch.service == PROCESS_WORKFLOW
                        && dispatch.handler == "run"
                        && refuses.lock().unwrap().as_deref() == dispatch.key.as_deref())
                    .then_some(lash_restate_test::Refusal::Retryable)
                })),
            },
        )
        .await
        .expect("manual recovery backend");
        let on_crash = Arc::clone(&blocked);
        assert!(backend.server().on_crash(Arc::new(move |target| {
            *on_crash.lock().unwrap() = Some(
                target
                    .strip_prefix(&format!("{PROCESS_WORKFLOW}/"))
                    .and_then(|target| target.strip_suffix("/run"))
                    .expect("the cut targets a process run")
                    .to_owned(),
            );
        })));
        let engine = Engine::Double(backend.clone());
        let executions = Arc::new(AtomicUsize::new(0));
        let core = process_core(&engine, &executions);
        engine.install_process_worker(worker(&core));
        engine.crash_on(
            CrashRule::new(CrashPoint::BeforeRun {
                name: "lash.process.terminal.published".to_owned(),
            })
            .service(PROCESS_WORKFLOW)
            .handler("run"),
        );
        let request = publish_process(&engine).await;
        let id = start(&engine, &core, request).await;
        let output = tokio::time::timeout(BOUND, core.processes().await_output(&id))
            .await
            .expect("terminal resolves before the final publication acknowledgment")
            .expect("terminal output");
        assert!(matches!(
            output,
            lash_core::ProcessAwaitOutput::Settled { .. }
        ));
        engine.settle().await;
        assert_eq!(engine.crashes(), 1, "the publication cut was reached");
        let before = engine
            .invocations(&format!("{PROCESS_WORKFLOW}/{id}"))
            .await;
        assert!(before.iter().any(|invocation| {
            invocation.target == format!("{PROCESS_WORKFLOW}/{id}/await_terminal")
                && invocation.status == "completed"
        }));
        assert!(before.iter().any(|invocation| {
            invocation.target == format!("{PROCESS_WORKFLOW}/{id}/run")
                && invocation.status != "completed"
        }));

        let request = waiting_request(&engine, 32).await;
        let unrelated = start(&engine, &core, request).await;
        engine.settle().await;
        let completion = engine.completed_process_invocations(&id);
        tokio::pin!(completion);
        poll_fn(|cx| {
            assert!(
                completion.as_mut().poll(cx).is_pending(),
                "quiescence must not pass for completion of the owned continuation"
            );
            Poll::Ready(())
        })
        .await;

        *blocked.lock().unwrap() = None;
        backend.server().advance(Duration::from_secs(1));
        engine.settle().await;
        let completed = tokio::time::timeout(BOUND, completion)
            .await
            .expect("the owned continuation completes after its retry");
        assert!(
            completed
                .iter()
                .all(|invocation| invocation.status == "completed")
        );
        assert!(
            engine
                .invocations(&format!("{PROCESS_WORKFLOW}/{unrelated}"))
                .await
                .iter()
                .any(|invocation| invocation.status != "completed"),
            "an unrelated process's pending wait does not block owned completion"
        );
        assert_eq!(
            executions.load(Ordering::SeqCst),
            3,
            "recovery reuses the two owned effects"
        );
        terminal_fact_is_settled(&engine, &id).await;
        engine.finish().await;
    }
}

async fn terminal_fact_is_settled(engine: &Engine, id: &lash_core::ProcessId) {
    let registry = engine.lash_backend().process_registry();
    assert_eq!(
        registry
            .count_events_through(id, "process.completed", u64::MAX)
            .await
            .expect("completed facts"),
        1,
        "workflow and outbox redelivery retain one terminal fact"
    );
    assert_eq!(
        registry
            .terminal_publication(id)
            .await
            .expect("publication obligation")
            .expect("terminal arms publication")
            .state,
        lash_core::store::ObligationState::Delivered,
        "terminal promise publication settles its obligation"
    );
}

async fn terminal_case(engine: Engine, point: CrashPoint) {
    let executions = Arc::new(AtomicUsize::new(0));
    let core = process_core(&engine, &executions);
    engine.install_process_worker(worker(&core));
    engine.crash_on(
        CrashRule::new(point)
            .service(PROCESS_WORKFLOW)
            .handler("run")
            .key_ending("#2"),
    );
    let request = publish_process(&engine).await;
    let id = start(&engine, &core, request).await;
    let first = tokio::time::timeout(BOUND, core.processes().await_output(&id))
        .await
        .expect("terminal resolves after the cut")
        .expect("terminal output");
    let again = core
        .processes()
        .await_output(&id)
        .await
        .expect("reattach terminal");
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&again).unwrap()
    );
    let lash_core::ProcessAwaitOutput::Settled { output } = first else {
        panic!("no terminal output")
    };
    assert!(matches!(
        output.outcome,
        lash_core::ToolCallOutcome::Success(_)
    ));
    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "terminal redelivery reuses the effects"
    );
    engine.settle().await;
    assert_eq!(
        engine.crashes(),
        1,
        "the requested terminal cut was reached"
    );
    let runs = engine
        .invocations(&format!("{PROCESS_WORKFLOW}/{id}"))
        .await;
    for ordinal in 0..3 {
        assert_eq!(
            runs.iter()
                .filter(
                    |i| i.target == format!("{PROCESS_WORKFLOW}/{}/run", segment_key(&id, ordinal))
                )
                .count(),
            1,
            "one invocation for segment {ordinal}: {runs:?}"
        );
    }
    terminal_fact_is_settled(&engine, &id).await;
    assert!(
        runs.iter().all(|i| i.status == "completed"),
        "terminal publication settles: {runs:?}"
    );
    engine.finish().await;
}

async fn external_owner_case(engine: Engine) {
    let registry = engine.lash_backend().process_registry();
    let record = registry
        .register_process(lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::External {
                metadata: json!({"owner": "fixture"}),
            },
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register externally owned process");
    let refusal = engine
        .lash_backend()
        .process_work()
        .port()
        .deliver_process_start(&record)
        .await
        .expect_err("Lash refuses to execute an external owner");
    assert!(refusal.to_string().contains("externally owned"));
    let unchanged = registry.get_process(&record.id).await.unwrap().unwrap();
    assert!(unchanged.first_started.is_none());
    assert!(!unchanged.is_terminal());
    assert!(
        engine
            .invocations(&format!("{PROCESS_WORKFLOW}/{}", record.id))
            .await
            .is_empty()
    );
    let expected = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(json!({"owner": "settled"})),
    );
    registry
        .complete_process(
            &record.id,
            expected.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("the owner publishes the terminal");
    let settled = registry.get_process(&record.id).await.unwrap().unwrap();
    assert!(settled.is_terminal());
    assert!(settled.first_started.is_none());
    engine.finish().await;
}

async fn retired_journal_case() {
    let engine = Engine::live("retired-journal", None).await;
    let executions = Arc::new(AtomicUsize::new(0));
    let core = process_core(&engine, &executions);
    engine.install_process_worker(worker(&core));
    let request = waiting_request(&engine, 32).await;
    let id = start(&engine, &core, request).await;
    let at_wait = record_where(&engine, &id, signal_wait).await;
    let generation = engine.lash_backend().build_generation().clone();
    let Engine::Live {
        backend, restarts, ..
    } = engine
    else {
        unreachable!()
    };
    restarts.abort();
    let new = backend
        .rebuild_on_generation(lash_core::engine::BuildGeneration::for_test("t1"))
        .await
        .expect("another build reconstructs the services");
    let engine = Engine::on_live_backend(new);
    let core = process_core(&engine, &executions);
    engine.install_process_worker(worker(&core));
    signal(&engine, &core, &id).await;
    let parked = record_where(&engine, &id, |record| {
        record.park.as_ref().is_some_and(|p| {
            matches!(
                p.reason,
                lash_core::store::ParkReason::RetiredGeneration { .. }
            )
        })
    })
    .await;
    let park = parked.park.as_ref().unwrap();
    assert_eq!(park.build_generation.as_ref(), Some(&generation));
    assert!(park.refusing);
    assert!(!parked.is_terminal());
    assert_eq!(parked.first_started, at_wait.first_started);
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "another build dispatches no fresh effect from this journal"
    );
    let park_id = park.park_id;
    let Engine::Live {
        backend: refused_backend,
        ..
    } = &engine
    else {
        unreachable!()
    };
    let paused = tokio::time::timeout(BOUND, async {
        loop {
            if let Some(invocation) = refused_backend
                .invocations()
                .await
                .unwrap()
                .into_iter()
                .find(|i| {
                    i.target == format!("{PROCESS_WORKFLOW}/{id}/run") && i.status == "paused"
                })
            {
                return invocation;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("generation refusals exhaust the harness's bounded retry policy");
    let retried = engine
        .lash_backend()
        .process_registry()
        .get_process(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retried.park.as_ref().unwrap().park_id,
        park_id,
        "retry extends one refusal rather than opening a second park"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let Engine::Live {
        backend, restarts, ..
    } = engine
    else {
        unreachable!()
    };
    restarts.abort();
    let restored = backend
        .rebuild_on_generation(generation)
        .await
        .expect("restore the recorded build");
    let engine = Engine::on_live_backend(restored);
    let core = process_core(&engine, &executions);
    engine.install_process_worker(worker(&core));
    lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::new(
        std::env::var("RESTATE_ADMIN_URL").expect("the suite's admin endpoint"),
    ))
    .resume_invocation(&lash_restate::RestateInvocationId::new(paused.id))
    .await
    .expect("resume the retained journal after restoring its generation");
    let terminal = tokio::time::timeout(BOUND, core.processes().await_output(&id))
        .await
        .expect("the recorded generation resumes")
        .expect("restored output");
    assert!(matches!(
        terminal,
        lash_core::ProcessAwaitOutput::Settled { .. }
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    engine.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the crash-windows Restate suite runs it"]
async fn live_restate_process_recovery_external_generation_and_terminal_fault_matrix() {
    external_owner_case(Engine::live("external-owner", None).await).await;
    retired_journal_case().await;
    for point in [
        CrashPoint::BeforeRun {
            name: "lash.process.complete".to_owned(),
        },
        CrashPoint::BeforeRunResult {
            name: Some("lash.process.complete".to_owned()),
        },
        CrashPoint::BeforeRun {
            name: "lash.process.terminal.published".to_owned(),
        },
    ] {
        terminal_case(Engine::live("terminal-fault", Some(1)).await, point).await;
    }
}

#[path = "recovery/engine.rs"]
mod engine;
