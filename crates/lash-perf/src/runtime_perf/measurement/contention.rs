use super::*;
use lash_sansio::SessionId;

#[derive(Clone, Copy)]
enum WriterContentionOperation {
    Configure,
    AppendMessages,
    SecondTurn,
}

impl WriterContentionOperation {
    const ALL: [Self; 3] = [Self::Configure, Self::AppendMessages, Self::SecondTurn];

    fn name(self) -> &'static str {
        match self {
            Self::Configure => "configure",
            Self::AppendMessages => "append_messages",
            Self::SecondTurn => "second_turn",
        }
    }
}

struct ContentionWave {
    execution_ms: Vec<f64>,
    wait_ms: Vec<f64>,
    release_latency_ms: f64,
}

async fn run_writer_operation(
    turn_entry: &TurnEntry,
    session: lash::LashSession,
    scenario: RuntimePerfScenario,
    operation: WriterContentionOperation,
    ordinal: usize,
) -> anyhow::Result<()> {
    match operation {
        WriterContentionOperation::Configure => {
            // A config transaction takes the writer and settles through the
            // command lane. Concurrent writers read the same revision, so a
            // later one settles stale; either settlement is the measured
            // operation, and only an owner refusal is a failure.
            let config = session.admin().config();
            let revision = config.revision().await?;
            let outcome = config
                .apply(
                    lash::config::ConfigWrite::new(
                        format!("perf-contention:{ordinal}:{}", uuid::Uuid::new_v4()),
                        revision,
                    ),
                    lash::config::ConfigTransaction::of(lash::config::SetTurnBudget {
                        turn_budget: session.policy_snapshot().turn_budget,
                    }),
                )
                .await?
                .await_outcome(&config)
                .await?;
            anyhow::ensure!(
                !matches!(
                    outcome,
                    lash::config::ConfigTransactionOutcome::Refused { .. }
                ),
                "writer contention config transaction was refused: {outcome:?}"
            );
        }
        WriterContentionOperation::AppendMessages => {
            session
                .admin()
                .state()
                .append_messages(
                    vec![lash_core::PluginMessage::text(
                        lash_core::MessageRole::User,
                        "writer contention append",
                    )],
                    format!("writer-contention-append:{ordinal}"),
                )
                .await?
                .settle_with(
                    &session.admin().commands(),
                    lash::testing::admin_fixture_outcome,
                )
                .await?;
        }
        WriterContentionOperation::SecondTurn => {
            let turn = turn_entry
                .run(
                    &session,
                    TurnInput::text(format!(
                        "writer contention operation {ordinal}: reply with exactly: runtime perf benchmark ok"
                    )),
                    None,
                    CancellationToken::new(),
                )
                .await?;
            validate_runtime_perf_turn(scenario, ordinal, &turn)?;
        }
    }
    Ok(())
}

async fn measure_writer_operation(
    turn_entry: &TurnEntry,
    session: lash::LashSession,
    scenario: RuntimePerfScenario,
    operation: WriterContentionOperation,
    ordinal: usize,
) -> anyhow::Result<f64> {
    let started = Instant::now();
    run_writer_operation(turn_entry, session, scenario, operation, ordinal).await?;
    Ok(elapsed_ms(started))
}

async fn run_contention_wave(
    turn_entry: &TurnEntry,
    scenario: RuntimePerfScenario,
    operation: WriterContentionOperation,
    holder_session: lash::LashSession,
    target_sessions: &[lash::LashSession],
    control: Arc<crate::runtime_perf::providers::BenchmarkProviderControl>,
    expect_contention: bool,
) -> anyhow::Result<ContentionWave> {
    let mut execution_ms = Vec::with_capacity(target_sessions.len());
    for (ordinal, session) in target_sessions.iter().enumerate() {
        execution_ms.push(
            measure_writer_operation(turn_entry, session.clone(), scenario, operation, ordinal)
                .await?,
        );
    }

    control.arm();
    let provider_started = control.provider_started.notified();
    let holder_entry = turn_entry.clone();
    let holder = tokio::spawn(async move {
        holder_entry
            .run(
                &holder_session,
                TurnInput::text(
                    "hold the runtime writer at the provider gate, then reply with exactly: runtime perf benchmark ok",
                ),
                None,
                CancellationToken::new(),
            )
            .await
    });
    provider_started.await;
    let release_latency_started = Instant::now();

    let waiter_barrier = Arc::new(tokio::sync::Barrier::new(target_sessions.len() + 1));
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut waiters = tokio::task::JoinSet::new();
    for (ordinal, session) in target_sessions.iter().cloned().enumerate() {
        let waiter_barrier = Arc::clone(&waiter_barrier);
        let completed = Arc::clone(&completed);
        let turn_entry = turn_entry.clone();
        waiters.spawn(async move {
            waiter_barrier.wait().await;
            let result =
                measure_writer_operation(&turn_entry, session, scenario, operation, ordinal).await;
            completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            result
        });
    }
    waiter_barrier.wait().await;
    tokio::task::yield_now().await;
    let mut contended_ms = Vec::with_capacity(target_sessions.len());
    if expect_contention {
        assert_eq!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "writer contention witness: a waiter completed before the held writer was released"
        );
    } else {
        while let Some(result) = waiters.join_next().await {
            contended_ms.push(result.map_err(anyhow::Error::from)??);
        }
        assert_eq!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            target_sessions.len(),
            "writer contention witness: peer-session waiters must complete while the writer is held"
        );
    }
    let release_latency_ms = elapsed_ms(release_latency_started);
    control.release_provider.notify_one();

    while let Some(result) = waiters.join_next().await {
        contended_ms.push(result.map_err(anyhow::Error::from)??);
    }
    holder.await.map_err(anyhow::Error::from)??;
    contended_ms.sort_by(f64::total_cmp);
    execution_ms.sort_by(f64::total_cmp);
    // Latencies are benchmark evidence only. The same-session assertion witnesses
    // blocking; the peer-session drain witnesses progress while the writer is held.
    let wait_ms = contended_ms
        .iter()
        .zip(&execution_ms)
        .map(|(contended, execution)| round3(contended - execution))
        .collect();

    Ok(ContentionWave {
        execution_ms,
        wait_ms,
        release_latency_ms,
    })
}

fn push_contention_wave_metrics(
    metrics: &mut BTreeMap<String, Vec<f64>>,
    scope: &str,
    operation: WriterContentionOperation,
    wave: ContentionWave,
) {
    let prefix = format!("writer_contention.{scope}");
    metrics
        .entry(format!("{prefix}.execution_ms"))
        .or_default()
        .extend(wave.execution_ms.iter().copied());
    metrics
        .entry(format!("{prefix}.wait_ms"))
        .or_default()
        .extend(wave.wait_ms.iter().copied());
    metrics
        .entry(format!("{prefix}.release_latency_ms"))
        .or_default()
        .push(wave.release_latency_ms);
    metrics.insert(
        format!("{prefix}.{}.execution_ms", operation.name()),
        wave.execution_ms,
    );
    metrics.insert(
        format!("{prefix}.{}.wait_ms", operation.name()),
        wave.wait_ms,
    );
}

fn metric_phase(samples: &[f64]) -> RuntimePerfPhaseRunResult {
    RuntimePerfPhaseRunResult {
        samples: samples.len(),
        duration_ms: round3(samples.iter().sum()),
        allocations: zero_allocation_delta(),
        rss_growth_kb: None,
    }
}

fn contention_phase_profile(
    metrics: &BTreeMap<String, Vec<f64>>,
) -> BTreeMap<String, RuntimePerfPhaseRunResult> {
    metrics
        .iter()
        .filter(|(key, _)| {
            !key.contains(".configure.")
                && !key.contains(".process_refresh.")
                && !key.contains(".second_turn.")
        })
        .map(|(key, samples)| {
            (
                key.trim_end_matches("_ms").to_string(),
                metric_phase(samples),
            )
        })
        .collect()
}

/// Measures facade-operation latency under same-session and many-session waves.
///
/// Known caveat: each contended `second_turn` wave advances its sessions by
/// `workers` turns, so those samples run at greater history depth than the
/// corresponding sequential baseline. The scenario deliberately reports that
/// confound instead of restructuring the reviewed contention shape.
#[expect(
    clippy::expect_used,
    reason = "the scenario's configured writer count is what drove the store setup above, so the lookup succeeds, and the session read view resolves after the run commits"
)]
pub(crate) async fn run_once_writer_contention(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let workers = scenario
        .contention_workers()
        .expect("writer contention worker count");
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();
    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let mut runtime = build_runtime(scenario, None).await?;
    let turn_entry = runtime.turn_entry();
    let main_session = runtime.session();
    let mut peer_sessions = Vec::with_capacity(workers);
    for worker in 0..workers {
        peer_sessions.push(
            runtime
                .create_and_open_child_session(SessionId::fixture(format!(
                    "runtime-perf-{}-peer-{worker}",
                    scenario.name()
                )))
                .await?,
        );
    }
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();
    let control = runtime.provider_control()?;
    let run_before_alloc = allocator_stats();
    let run_started = Instant::now();
    let mut metric_samples_ms = BTreeMap::new();

    for operation in WriterContentionOperation::ALL {
        let same_session_targets = vec![main_session.clone(); workers];
        let wave = run_contention_wave(
            &turn_entry,
            scenario,
            operation,
            main_session.clone(),
            &same_session_targets,
            Arc::clone(&control),
            true,
        )
        .await?;
        push_contention_wave_metrics(&mut metric_samples_ms, "same_session", operation, wave);

        let wave = run_contention_wave(
            &turn_entry,
            scenario,
            operation,
            main_session.clone(),
            &peer_sessions,
            Arc::clone(&control),
            false,
        )
        .await?;
        push_contention_wave_metrics(&mut metric_samples_ms, "many_sessions", operation, wave);
    }

    let run_turn_ms = elapsed_ms(run_started);
    let run_turn_alloc = alloc_delta(run_before_alloc, allocator_stats());
    let after_turn_memory = process_memory_sample();
    let phase_profile = contention_phase_profile(&metric_samples_ms);
    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let state = runtime.export_state().await;
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();

    for session in peer_sessions {
        session.close().await?;
    }
    drop(main_session);
    runtime.close().await?;

    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let turn = RuntimePerfTurnResult {
        turn_index: 0,
        stages: turn_stages(
            RuntimePerfStageRunResult::measured(
                run_turn_ms,
                run_turn_alloc.clone(),
                after_turn_memory.rss_kb,
            ),
            None,
            RuntimePerfStageRunResult::measured(
                run_turn_ms,
                run_turn_alloc,
                after_turn_memory.rss_kb,
            ),
        ),
        memory: memory_span(after_build_memory, after_turn_memory),
        phase_profile: phase_profile.clone(),
        turn_usage: LlmUsage::default(),
    };
    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        stages: run_stages(
            [
                (
                    stage::BUILD_RUNTIME,
                    RuntimePerfStageRunResult::measured(
                        build_runtime_ms,
                        build_runtime_alloc,
                        after_build_memory.rss_kb,
                    ),
                ),
                (
                    stage::EXPORT_STATE,
                    RuntimePerfStageRunResult::measured(
                        export_state_ms,
                        export_state_alloc,
                        after_export_memory.rss_kb,
                    ),
                ),
                (
                    stage::TOTAL,
                    RuntimePerfStageRunResult::measured(
                        elapsed_ms(total_started),
                        total_alloc,
                        after_export_memory.rss_kb,
                    ),
                ),
            ],
            std::slice::from_ref(&turn),
        ),
        session_nodes: state.session_graph.nodes.len(),
        active_path_messages: state.read_view().messages().len(),
        extra_counters: BTreeMap::from([
            ("writer_contention.workers".to_string(), workers as u64),
            ("writer_contention.operation_kinds".to_string(), 3),
            ("writer_contention.session_shapes".to_string(), 2),
        ]),
        metric_samples: BTreeMap::new(),
        metric_samples_ms,
        memory: memory_span(before_memory, after_export_memory),
        phase_profile,
        turns: vec![turn],
    })
}

#[expect(
    clippy::expect_used,
    reason = "the settlement scenario configures its child count and metric keys, which the run then writes and reads back per each site's message"
)]
pub(crate) async fn run_once_async_process_settlement(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let children = scenario
        .settlement_children()
        .expect("async settlement child count");
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();
    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let mut runtime = build_runtime(scenario, None).await?;
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();
    let phase_probe = Arc::new(RuntimePerfPhaseProbe::default());
    runtime.set_turn_phase_probe(phase_probe.clone()).await;
    let control = runtime.settlement_control()?;

    let run_before_alloc = allocator_stats();
    let run_started = Instant::now();
    let turn = runtime
        .run_turn(
            TurnInput::text(benchmark_prompt(scenario, 0)),
            CancellationToken::new(),
        )
        .await?;
    validate_runtime_perf_turn(scenario, 0, &turn)?;
    let parent_return_ms = elapsed_ms(run_started);
    control.wait_for_pending(children).await;
    let spawn_ms = elapsed_ms(run_started);
    let open_spans_before_settle = phase_probe.open_span_count();
    if open_spans_before_settle < children {
        anyhow::bail!(
            "async settlement expected at least {children} open child spans, found {open_spans_before_settle}"
        );
    }

    let session = runtime.session();
    let processes = session.admin().processes().list_all().await?;
    if processes.len() != children {
        anyhow::bail!(
            "async settlement expected {children} child processes, found {}",
            processes.len()
        );
    }
    let mut terminals = tokio::task::JoinSet::new();
    for process in processes {
        let session = session.clone();
        terminals.spawn(async move {
            let started = Instant::now();
            session
                .admin()
                .processes()
                .await_output(&process.process_id)
                .await?;
            anyhow::Result::<f64>::Ok(elapsed_ms(started))
        });
    }
    tokio::task::yield_now().await;
    let settle_started = Instant::now();
    control.release(children);
    let mut child_terminal_ms = Vec::with_capacity(children);
    while let Some(result) = terminals.join_next().await {
        child_terminal_ms.push(result.map_err(anyhow::Error::from)??);
    }
    let settle_ms = elapsed_ms(settle_started);
    let drain_started = Instant::now();
    runtime.await_background_work().await?;
    let drain_ms = elapsed_ms(drain_started);
    let run_turn_ms = elapsed_ms(run_started);
    let run_turn_alloc = alloc_delta(run_before_alloc, allocator_stats());
    let after_turn_memory = process_memory_sample();
    let open_spans_after_drain = phase_probe.open_span_count();
    let mut phase_profile = phase_probe.take_completed_after_settlement()?;
    let mut metric_samples_ms = BTreeMap::from([
        (
            "async_settlement.parent_return_ms".to_string(),
            vec![parent_return_ms],
        ),
        ("async_settlement.spawn_ms".to_string(), vec![spawn_ms]),
        ("async_settlement.settle_ms".to_string(), vec![settle_ms]),
        ("async_settlement.drain_ms".to_string(), vec![drain_ms]),
        (
            "async_settlement.child_terminal_ms".to_string(),
            child_terminal_ms,
        ),
    ]);
    metric_samples_ms.insert(
        "async_settlement.child_pending_ms".to_string(),
        control.pending_durations_ms(),
    );
    for key in ["spawn", "settle", "drain"] {
        let samples = metric_samples_ms
            .get(&format!("async_settlement.{key}_ms"))
            .expect("async settlement metric inserted");
        phase_profile.insert(format!("async_settlement.{key}"), metric_phase(samples));
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let state = runtime.export_state().await;
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    drop(session);
    runtime.close().await?;
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let turn_result = RuntimePerfTurnResult {
        turn_index: 0,
        stages: turn_stages(
            RuntimePerfStageRunResult::measured(
                run_turn_ms,
                run_turn_alloc.clone(),
                after_turn_memory.rss_kb,
            ),
            Some(RuntimePerfStageRunResult::measured(
                drain_ms,
                zero_allocation_delta(),
                after_turn_memory.rss_kb,
            )),
            RuntimePerfStageRunResult::measured(
                run_turn_ms,
                run_turn_alloc,
                after_turn_memory.rss_kb,
            ),
        ),
        memory: memory_span(after_build_memory, after_turn_memory),
        phase_profile: phase_profile.clone(),
        turn_usage: turn.usage,
    };
    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        stages: run_stages(
            [
                (
                    stage::BUILD_RUNTIME,
                    RuntimePerfStageRunResult::measured(
                        build_runtime_ms,
                        build_runtime_alloc,
                        after_build_memory.rss_kb,
                    ),
                ),
                (
                    stage::EXPORT_STATE,
                    RuntimePerfStageRunResult::measured(
                        export_state_ms,
                        export_state_alloc,
                        after_export_memory.rss_kb,
                    ),
                ),
                (
                    stage::TOTAL,
                    RuntimePerfStageRunResult::measured(
                        elapsed_ms(total_started),
                        total_alloc,
                        after_export_memory.rss_kb,
                    ),
                ),
            ],
            std::slice::from_ref(&turn_result),
        ),
        session_nodes: state.session_graph.nodes.len(),
        active_path_messages: state.read_view().messages().len(),
        extra_counters: BTreeMap::from([
            ("async_settlement.children".to_string(), children as u64),
            (
                "async_settlement.open_spans_before_settle".to_string(),
                open_spans_before_settle as u64,
            ),
            (
                "async_settlement.open_spans_after_drain".to_string(),
                open_spans_after_drain as u64,
            ),
        ]),
        metric_samples: BTreeMap::new(),
        metric_samples_ms,
        memory: memory_span(before_memory, after_export_memory),
        phase_profile,
        turns: vec![turn_result],
    })
}

#[cfg(test)]
mod contention_tests {
    use super::*;

    #[tokio::test]
    async fn async_settlement_smoke_drains_every_open_child_span() {
        let result = Box::pin(run_once_async_process_settlement(
            RuntimePerfScenario::AsyncProcessSettlement2Children,
            1,
        ))
        .await
        .expect("async settlement smoke");
        eprintln!(
            "PERF_SCENARIO {}",
            serde_json::to_string(&result).expect("serialize measurement")
        );

        assert!(result.extra_counters["async_settlement.open_spans_before_settle"] >= 2);
        assert_eq!(
            result.extra_counters["async_settlement.open_spans_after_drain"],
            0
        );
        for phase in ["spawn_ms", "settle_ms", "drain_ms"] {
            assert!(
                result
                    .metric_samples_ms
                    .contains_key(&format!("async_settlement.{phase}"))
            );
        }
    }

    #[tokio::test]
    async fn gate_bypass_second_completer_hits_receipt_conflict_then_rebuilds_after_backoff() {
        let session_id = "commit-admission-bypass";
        let factory = sqlite_memory_stores()
            .await
            .expect("open a SQLite memory store set")
            .session_store_factory();
        factory
            .admit_session(&runtime_perf_session_create_request(&SessionId::from(
                session_id,
            )))
            .await
            .expect("create synthetic contention store");
        let store: Arc<dyn lash_core::RuntimeStore> = factory;
        let mut first_state = RuntimeSessionState {
            session_id: SessionId::from(session_id),
            ..RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ))
        };
        first_state.policy.model = Some(lash_core::testing::test_llm_profile_config(
            "first-completer",
            lash_core::testing::test_llm_profile_metadata("first-completer"),
        ));
        let mut bypass_state = first_state.clone();
        bypass_state.policy.model = Some(lash_core::testing::test_llm_profile_config(
            "gate-bypass-completer",
            lash_core::testing::test_llm_profile_metadata("gate-bypass-completer"),
        ));
        let shared_operation = lash_core::OperationId::new(
            lash_core::ExecutionScope::runtime_operation("commit-admission-bypass"),
            "commit",
        );
        let first_commit = RuntimeCommit::persisted_state_with_operation_for_testing(
            &first_state,
            shared_operation.clone(),
        );
        // Mutation probe: this second completer deliberately builds its stale
        // intent without entering the process-local admission FIFO.
        let bypass_commit = RuntimeCommit::persisted_state_with_operation_for_testing(
            &bypass_state,
            shared_operation,
        );
        lash_core::facade_support::run_head_advancing_commit_attempt(
            session_id,
            "first",
            CancellationToken::new(),
            lash_core::runtime::CommitAdmissionPolicy::standard(),
            |_, _| async {
                store.commit_runtime_state(first_commit).await?;
                Ok::<(), anyhow::Error>(())
            },
        )
        .await
        .expect("first completer advances the head");

        let conflict = store
            .commit_runtime_state(bypass_commit)
            .await
            .expect_err("gate bypass must still reach the receipt CAS");
        assert!(
            matches!(
                conflict,
                lash_core::StoreError::RuntimeTurnCommitConflict { ref session_id, .. }
                    if session_id == "commit-admission-bypass"
            ),
            "gate bypass must preserve the typed receipt conflict, got {conflict:?}"
        );

        tokio::time::sleep(Duration::from_millis(1)).await;
        lash_core::facade_support::run_head_advancing_commit_attempt(
            session_id,
            "retry",
            CancellationToken::new(),
            lash_core::runtime::CommitAdmissionPolicy::standard(),
            |_, _| async {
                let mut fresh =
                    load_runtime_perf_session_state(&store, &SessionId::from(session_id))
                        .await?
                        .expect("session remains durable");
                fresh.policy.model = Some(lash_core::testing::test_llm_profile_config(
                    "gate-bypass-completer",
                    lash_core::testing::test_llm_profile_metadata("gate-bypass-completer"),
                ));
                let retry_commit = RuntimeCommit::persisted_state_with_operation_for_testing(
                    &fresh,
                    lash_core::OperationId::new(
                        lash_core::ExecutionScope::runtime_operation(
                            "commit-admission-bypass-retry",
                        ),
                        "commit",
                    ),
                );
                store.commit_runtime_state(retry_commit).await?;
                Ok::<(), anyhow::Error>(())
            },
        )
        .await
        .expect("fresh rebuild commits after residual backoff");
    }
}
