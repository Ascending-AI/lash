#[derive(Clone, Copy)]
enum WriterContentionOperation {
    Configure,
    ProcessRefresh,
    SecondTurn,
}

impl WriterContentionOperation {
    const ALL: [Self; 3] = [Self::Configure, Self::ProcessRefresh, Self::SecondTurn];

    fn name(self) -> &'static str {
        match self {
            Self::Configure => "configure",
            Self::ProcessRefresh => "process_refresh",
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
    session: lash::LashSession,
    scenario: RuntimePerfScenario,
    operation: WriterContentionOperation,
    ordinal: usize,
) -> anyhow::Result<()> {
    match operation {
        WriterContentionOperation::Configure => {
            session
                .configure(lash::SessionConfigPatch::default())
                .await?;
        }
        WriterContentionOperation::ProcessRefresh => {
            session.refresh_background_graph().await?;
        }
        WriterContentionOperation::SecondTurn => {
            let turn = session
                .turn(TurnInput::text(format!(
                    "writer contention operation {ordinal}: reply with exactly: runtime perf benchmark ok"
                )))
                .run()
                .await?;
            validate_runtime_perf_turn(scenario, ordinal, &turn.result)?;
        }
    }
    Ok(())
}

async fn measure_writer_operation(
    session: lash::LashSession,
    scenario: RuntimePerfScenario,
    operation: WriterContentionOperation,
    ordinal: usize,
) -> anyhow::Result<f64> {
    let started = Instant::now();
    run_writer_operation(session, scenario, operation, ordinal).await?;
    Ok(elapsed_ms(started))
}

async fn run_contention_wave(
    scenario: RuntimePerfScenario,
    operation: WriterContentionOperation,
    holder_session: lash::LashSession,
    target_sessions: &[lash::LashSession],
    control: Arc<super::providers::BenchmarkProviderControl>,
) -> anyhow::Result<ContentionWave> {
    let mut execution_ms = Vec::with_capacity(target_sessions.len());
    for (ordinal, session) in target_sessions.iter().enumerate() {
        execution_ms
            .push(measure_writer_operation(session.clone(), scenario, operation, ordinal).await?);
    }

    control.arm();
    let provider_started = control.provider_started.notified();
    let holder = tokio::spawn(async move {
        holder_session
            .turn(TurnInput::text(
                "hold the runtime writer at the provider gate, then reply with exactly: runtime perf benchmark ok",
            ))
            .run()
            .await
            .map_err(anyhow::Error::from)
    });
    provider_started.await;
    let release_latency_started = Instant::now();

    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut waiters = tokio::task::JoinSet::new();
    for (ordinal, session) in target_sessions.iter().cloned().enumerate() {
        let ready_tx = ready_tx.clone();
        waiters.spawn(async move {
            ready_tx
                .send(())
                .map_err(|_| anyhow::anyhow!("writer contention ready receiver dropped"))?;
            measure_writer_operation(session, scenario, operation, ordinal).await
        });
    }
    drop(ready_tx);
    for _ in target_sessions {
        ready_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("writer contention waiter exited before ready"))?;
    }
    tokio::task::yield_now().await;
    let release_latency_ms = elapsed_ms(release_latency_started);
    control.release_provider.notify_one();

    let mut contended_ms = Vec::with_capacity(target_sessions.len());
    while let Some(result) = waiters.join_next().await {
        contended_ms.push(result.map_err(anyhow::Error::from)??);
    }
    holder.await.map_err(anyhow::Error::from)??;
    contended_ms.sort_by(f64::total_cmp);
    execution_ms.sort_by(f64::total_cmp);
    // The facade does not expose its writer acquisition instant. Pairing the
    // ordered contended latencies with ordered uncontended executions leaves
    // the excess residence as the harness-level writer-wait witness. The
    // many-session shape runs the same work without a shared writer and is the
    // control for scheduler/provider overhead in that subtraction.
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

#[cfg(test)]
mod contention_tests {
    use super::*;

    fn median(mut values: Vec<f64>) -> f64 {
        values.sort_by(f64::total_cmp);
        if values.len().is_multiple_of(2) {
            (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
        } else {
            values[values.len() / 2]
        }
    }

    #[tokio::test]
    async fn writer_contention_smoke_reports_wait_release_latency_and_execution() {
        let result = Box::pin(run_once_writer_contention(
            RuntimePerfScenario::WriterContention2Workers,
            1,
        ))
        .await
        .expect("writer contention smoke");
        let same_wait = result
            .metric_samples_ms
            .get("writer_contention.same_session.wait_ms")
            .expect("same-session wait metric");
        let many_wait = result
            .metric_samples_ms
            .get("writer_contention.many_sessions.wait_ms")
            .expect("many-session wait metric");

        assert!(same_wait.iter().any(|sample| *sample > 0.0));
        assert!(median(many_wait.clone()) < median(same_wait.clone()));
        for scope in ["same_session", "many_sessions"] {
            for phase in ["wait_ms", "release_latency_ms", "execution_ms"] {
                assert!(
                    result
                        .metric_samples_ms
                        .contains_key(&format!("writer_contention.{scope}.{phase}"))
                );
            }
        }
    }

    #[tokio::test]
    async fn async_settlement_smoke_drains_every_open_child_span() {
        let result = Box::pin(run_once_async_process_settlement(
            RuntimePerfScenario::AsyncProcessSettlement2Children,
            1,
        ))
        .await
        .expect("async settlement smoke");

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
        let factory = lash_core::facade_support::InMemorySessionStoreFactory::new();
        let store = factory
            .create_store(&runtime_perf_session_create_request(session_id))
            .await
            .expect("create synthetic contention store");
        let mut first_state = RuntimeSessionState {
            session_id: session_id.to_string(),
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        first_state.policy.provider_id = "first-completer".to_string();
        let mut bypass_state = first_state.clone();
        bypass_state.policy.provider_id = "gate-bypass-completer".to_string();
        let shared_operation = lash_core::OperationId::new(
            lash_core::ExecutionScope::runtime_operation("commit-admission-bypass"),
            "commit",
        );
        let first_commit = RuntimeCommit::persisted_state_with_operation_for_testing(
            &first_state,
            &[],
            shared_operation.clone(),
        );
        // Mutation probe: this second completer deliberately builds its stale
        // intent without entering the process-local admission FIFO.
        let bypass_commit = RuntimeCommit::persisted_state_with_operation_for_testing(
            &bypass_state,
            &[],
            shared_operation,
        );
        lash_core::facade_support::run_head_advancing_commit_attempt(
            session_id,
            "first",
            CancellationToken::new(),
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

        let counters = DurableContentionCounters::default();
        counters
            .cas_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut retry_after = Duration::from_millis(1);
        wait_for_durable_contention_retry(&counters, &mut retry_after).await;
        lash_core::facade_support::run_head_advancing_commit_attempt(
            session_id,
            "retry",
            CancellationToken::new(),
            |_, _| async {
                let mut fresh = lash_core::store::load_persisted_session_state(store.as_ref())
                    .await?
                    .expect("session remains durable");
                fresh.policy.provider_id = "gate-bypass-completer".to_string();
                let retry_commit = RuntimeCommit::persisted_state_with_operation_for_testing(
                    &fresh,
                    &[],
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
        assert_eq!(
            counters
                .cas_failures
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .cas_backoff_sleeps
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the receipt conflict must traverse the bounded jittered backoff"
        );
    }
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
    let mut runtime = build_runtime_with_store(scenario, None, None).await?;
    let main_session = runtime.session();
    let mut peer_sessions = Vec::with_capacity(workers);
    for worker in 0..workers {
        peer_sessions.push(
            runtime
                .open_child_session(format!("runtime-perf-{}-peer-{worker}", scenario.name()))
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
            scenario,
            operation,
            main_session.clone(),
            &same_session_targets,
            Arc::clone(&control),
        )
        .await?;
        push_contention_wave_metrics(&mut metric_samples_ms, "same_session", operation, wave);

        let wave = run_contention_wave(
            scenario,
            operation,
            main_session.clone(),
            &peer_sessions,
            Arc::clone(&control),
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
    let cumulative_usage = runtime.usage_report();

    for session in peer_sessions {
        session.close().await?;
    }
    drop(main_session);
    runtime.close().await?;

    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let zero_alloc = zero_allocation_delta();
    let turn = RuntimePerfTurnResult {
        turn_index: 0,
        run_turn_ms,
        await_background_work_ms: 0.0,
        total_ms: run_turn_ms,
        memory: RuntimePerfTurnMemoryRunResult {
            rss_before_kb: after_build_memory.rss_kb,
            rss_after_turn_kb: after_turn_memory.rss_kb,
            rss_after_await_kb: after_turn_memory.rss_kb,
            peak_hwm_before_kb: after_build_memory.hwm_kb,
            peak_hwm_after_await_kb: after_turn_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(after_build_memory.rss_kb, after_turn_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(after_build_memory.hwm_kb, after_turn_memory.hwm_kb),
        },
        allocations: RuntimePerfTurnAllocationRunResult {
            run_turn: run_turn_alloc.clone(),
            await_background_work: zero_alloc.clone(),
            total: run_turn_alloc.clone(),
        },
        phase_profile: phase_profile.clone(),
        turn_usage: TokenUsage::default(),
        usage_delta: SessionUsageReport::default(),
        cumulative_usage: cumulative_usage.clone(),
    };
    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        build_runtime_ms,
        seed_state_ms: 0.0,
        run_turn_ms,
        await_background_work_ms: 0.0,
        export_state_ms,
        total_ms: elapsed_ms(total_started),
        session_nodes: state.session_graph.nodes.len(),
        active_path_messages: state.read_view().messages().len(),
        extra_counters: BTreeMap::from([
            ("writer_contention.workers".to_string(), workers as u64),
            ("writer_contention.operation_kinds".to_string(), 3),
            ("writer_contention.session_shapes".to_string(), 2),
        ]),
        metric_samples: BTreeMap::new(),
        metric_samples_ms,
        memory: RuntimePerfMemoryRunResult {
            rss_before_kb: before_memory.rss_kb,
            rss_after_build_kb: after_build_memory.rss_kb,
            rss_after_seed_kb: after_build_memory.rss_kb,
            rss_after_turn_kb: after_turn_memory.rss_kb,
            rss_after_await_kb: after_turn_memory.rss_kb,
            rss_after_export_kb: after_export_memory.rss_kb,
            peak_hwm_before_kb: before_memory.hwm_kb,
            peak_hwm_after_export_kb: after_export_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_export_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(before_memory.hwm_kb, after_export_memory.hwm_kb),
        },
        allocations: RuntimePerfAllocationRunResult {
            build_runtime: build_runtime_alloc,
            seed_state: zero_alloc.clone(),
            run_turn: run_turn_alloc,
            await_background_work: zero_alloc,
            export_state: export_state_alloc,
            total: total_alloc,
        },
        phase_profile,
        turns: vec![turn],
        cumulative_usage,
    })
}

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
    let mut runtime = build_runtime_with_store(scenario, None, None).await?;
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
    let processes = session.processes().list_all().await?;
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
    let cumulative_usage = runtime.usage_report();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    drop(session);
    runtime.close().await?;
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let zero_alloc = zero_allocation_delta();
    let turn_result = RuntimePerfTurnResult {
        turn_index: 0,
        run_turn_ms,
        await_background_work_ms: drain_ms,
        total_ms: run_turn_ms,
        memory: RuntimePerfTurnMemoryRunResult {
            rss_before_kb: after_build_memory.rss_kb,
            rss_after_turn_kb: after_turn_memory.rss_kb,
            rss_after_await_kb: after_turn_memory.rss_kb,
            peak_hwm_before_kb: after_build_memory.hwm_kb,
            peak_hwm_after_await_kb: after_turn_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(after_build_memory.rss_kb, after_turn_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(after_build_memory.hwm_kb, after_turn_memory.hwm_kb),
        },
        allocations: RuntimePerfTurnAllocationRunResult {
            run_turn: run_turn_alloc.clone(),
            await_background_work: zero_alloc.clone(),
            total: run_turn_alloc.clone(),
        },
        phase_profile: phase_profile.clone(),
        turn_usage: turn.usage,
        usage_delta: cumulative_usage.clone(),
        cumulative_usage: cumulative_usage.clone(),
    };
    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        build_runtime_ms,
        seed_state_ms: 0.0,
        run_turn_ms,
        await_background_work_ms: drain_ms,
        export_state_ms,
        total_ms: elapsed_ms(total_started),
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
        memory: RuntimePerfMemoryRunResult {
            rss_before_kb: before_memory.rss_kb,
            rss_after_build_kb: after_build_memory.rss_kb,
            rss_after_seed_kb: after_build_memory.rss_kb,
            rss_after_turn_kb: after_turn_memory.rss_kb,
            rss_after_await_kb: after_turn_memory.rss_kb,
            rss_after_export_kb: after_export_memory.rss_kb,
            peak_hwm_before_kb: before_memory.hwm_kb,
            peak_hwm_after_export_kb: after_export_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_export_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(before_memory.hwm_kb, after_export_memory.hwm_kb),
        },
        allocations: RuntimePerfAllocationRunResult {
            build_runtime: build_runtime_alloc,
            seed_state: zero_alloc.clone(),
            run_turn: run_turn_alloc,
            await_background_work: zero_alloc,
            export_state: export_state_alloc,
            total: total_alloc,
        },
        phase_profile,
        turns: vec![turn_result],
        cumulative_usage,
    })
}

#[derive(Default)]
struct DurableContentionCounters {
    claim_attempts: std::sync::atomic::AtomicU64,
    claim_refusals: std::sync::atomic::AtomicU64,
    successful_claims: std::sync::atomic::AtomicU64,
    lease_probe_busy: std::sync::atomic::AtomicU64,
    renewals: std::sync::atomic::AtomicU64,
    abandons: std::sync::atomic::AtomicU64,
    reclaims: std::sync::atomic::AtomicU64,
    reclaim_conflicts: std::sync::atomic::AtomicU64,
    store_contention_retries: std::sync::atomic::AtomicU64,
    cas_failures: std::sync::atomic::AtomicU64,
    cas_backoff_sleeps: std::sync::atomic::AtomicU64,
    completions: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct DurableContentionSamples {
    claim_wait_ms: Mutex<Vec<f64>>,
    service_ms: Mutex<Vec<f64>>,
    commit_admission_wait_ms: Mutex<Vec<f64>>,
    commit_admission_queue_depth: Mutex<Vec<f64>>,
}

async fn settle_durable_contention_claim(
    store: &(dyn lash_core::RuntimePersistence + '_),
    completion: QueuedWorkCompletion,
    counters: &DurableContentionCounters,
    samples: &DurableContentionSamples,
) -> anyhow::Result<()> {
    let session_id = completion.session_id.clone();
    let work_identity = completion.claim_id.clone();
    lash_core::facade_support::run_head_advancing_commit_attempt(
        session_id,
        work_identity,
        CancellationToken::new(),
        |admission_wait, admission_queue_depth| async move {
            if admission_queue_depth > 0 {
                samples
                    .commit_admission_wait_ms
                    .lock_recover()
                    .push(round3(admission_wait.as_secs_f64() * 1000.0));
                samples
                    .commit_admission_queue_depth
                    .lock_recover()
                    .push(admission_queue_depth as f64);
            }
            let mut cas_retry_after = Duration::from_millis(1);
            for _ in 0..256 {
                let state = lash_core::store::load_persisted_session_state(store)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("durable contention session state disappeared")
                    })?;
                let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
                commit.completed_queue_claims = vec![completion.clone()];
                match store.commit_runtime_state(commit).await {
                    Ok(_) => return Ok(()),
                    Err(lash_core::StoreError::Contended) => {
                        counters
                            .store_contention_retries
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        wait_for_durable_contention_retry(counters, &mut cas_retry_after).await;
                    }
                    Err(
                        lash_core::StoreError::HeadRevisionConflict { .. }
                        | lash_core::StoreError::RuntimeTurnCommitConflict { .. }
                        | lash_core::StoreError::AppendOperationIdentityConflict { .. },
                    ) => {
                        counters
                            .cas_failures
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        wait_for_durable_contention_retry(counters, &mut cas_retry_after).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            anyhow::bail!("durable contention completion exhausted 256 CAS retries")
        },
    )
    .await
}

async fn wait_for_durable_contention_retry(
    counters: &DurableContentionCounters,
    retry_after: &mut Duration,
) {
    const RETRY_MAX: Duration = Duration::from_millis(25);
    let delay = lash_core::facade_support::bounded_multiplicative_jitter(
        *retry_after,
        Duration::from_millis(1),
        RETRY_MAX,
    );
    counters
        .cas_backoff_sleeps
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(delay).await;
    *retry_after = retry_after.saturating_mul(2).min(RETRY_MAX);
}

async fn run_durable_contention_worker(
    worker: usize,
    target_completions: u64,
    session_id: String,
    store: Arc<dyn lash_core::RuntimePersistence>,
    session_fence: lash_core::SessionExecutionLeaseAuthority,
    counters: Arc<DurableContentionCounters>,
    samples: Arc<DurableContentionSamples>,
) -> anyhow::Result<()> {
    let owner = lash_core::LeaseOwnerIdentity::opaque(
        format!("runtime-perf-contention-worker-{worker}"),
        uuid::Uuid::new_v4().to_string(),
    );
    let competing_lease = store
        .try_claim_session_execution_lease(
            &session_id,
            &owner,
            &format!("runtime-perf-contention-worker-{worker}"),
            QUEUED_WORK_CLAIM_TTL_MS,
        )
        .await?;
    if matches!(
        competing_lease,
        lash_core::SessionExecutionLeaseClaimOutcome::Busy { .. }
    ) {
        counters
            .lease_probe_busy
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    } else {
        anyhow::bail!("contention worker {worker} unexpectedly acquired the controller lease");
    }

    'worker: while counters
        .completions
        .load(std::sync::atomic::Ordering::Acquire)
        < target_completions
    {
        let claim_started = Instant::now();
        let claim_deadline = claim_started + Duration::from_secs(60);
        let mut claim = loop {
            counters
                .claim_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let outcome = store
                .claim_ready_queued_work(
                    &session_id,
                    &session_fence,
                    &owner,
                    QueuedWorkClaimBoundary::Idle,
                    lash_core::testing::queued_work_claim_policy(1),
                )
                .await?;
            if let Some(claim) = outcome.claim() {
                break claim;
            }

            counters
                .claim_refusals
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if counters
                .completions
                .load(std::sync::atomic::Ordering::Acquire)
                >= target_completions
            {
                break 'worker;
            }
            if Instant::now() >= claim_deadline {
                anyhow::bail!("contention worker {worker} waited 60 seconds for a claim");
            }
            tokio::task::yield_now().await;
        };
        let claim_wait_ms = elapsed_ms(claim_started);
        let sequence = counters
            .successful_claims
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let service_started = Instant::now();
        if sequence.is_multiple_of(3) {
            match store
                .renew_session_execution_lease(&session_fence, QUEUED_WORK_CLAIM_TTL_MS)
                .await
            {
                Ok(_) => {
                    counters
                        .renewals
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(error) => {
                    return Err(error.into());
                }
            }
        }

        if sequence.is_multiple_of(2) {
            let batch_ids = claim
                .batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect::<Vec<_>>();
            store.abandon_queued_work_claim(&claim).await?;
            counters
                .abandons
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let reclaimed = store
                .claim_ready_queued_work_by_batch_ids(
                    &session_id,
                    &session_fence,
                    &owner,
                    QueuedWorkClaimBoundary::Idle,
                    &batch_ids,
                    lash_core::testing::queued_work_claim_policy(1),
                )
                .await?;
            let Some(reclaimed) = reclaimed.claim else {
                counters
                    .reclaim_conflicts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                continue;
            };
            claim = reclaimed;
            counters
                .reclaims
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        settle_durable_contention_claim(store.as_ref(), claim.completion(), &counters, &samples)
            .await?;
        counters
            .completions
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        samples.claim_wait_ms.lock_recover().push(claim_wait_ms);
        samples
            .service_ms
            .lock_recover()
            .push(elapsed_ms(service_started));
    }
    Ok(())
}

/// Runs queued-work lifecycle contention against one backend and one session.
///
/// The worker count is supplied by `--runtime-perf-contention-workers`; each
/// worker targets `chat_turns` completions. Wall-clock throughput and latency
/// are quiet-box witnesses only. Tests assert emitted structure and counters,
/// never latency thresholds.
pub(crate) async fn run_once_durable_queued_work_contention(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    workers: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let database_url = scenario
        .uses_postgres()
        .then(configured_postgres_database_url)
        .flatten();
    if scenario.uses_postgres() && database_url.is_none() {
        if postgres_is_required() {
            anyhow::bail!(
                "{} requires LASH_POSTGRES_DATABASE_URL or DATABASE_URL when LASH_REQUIRE_POSTGRES is set",
                scenario.name()
            );
        }
        eprintln!(
            "{}: skipped: no LASH_POSTGRES_DATABASE_URL or DATABASE_URL configured",
            scenario.name()
        );
        return Ok(skipped_runtime_perf_result(scenario, chat_turns));
    }

    let workers = workers.max(1);
    let target_completions = workers
        .checked_mul(chat_turns)
        .ok_or_else(|| anyhow::anyhow!("durable contention work count overflow"))?;
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();
    let sqlite_root = (!scenario.uses_postgres())
        .then(|| make_temp_bench_dir(&format!("lash-runtime-perf-{}", scenario.name())))
        .transpose()?;
    let postgres_namespace = match database_url.as_deref() {
        Some(url) => Some(lash_postgres_store::testing::IsolatedDatabase::create(url).await),
        None => None,
    };

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let mut runtime = match postgres_namespace.as_ref() {
        Some(namespace) => build_runtime_with_postgres_store(scenario, namespace.url()).await?,
        None => {
            build_runtime_with_sqlite_store(
                scenario,
                sqlite_root.as_ref().expect("SQLite root").clone(),
            )
            .await?
        }
    };
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();
    let session_id = runtime.session().session_id().to_string();
    let store = runtime.persistence();
    let store_metrics = runtime.store_metrics();
    // The scenario drives the retained persistence handle directly. Close the
    // facade session before advancing the durable head so its resident cursor
    // cannot attempt a stale close-time commit after the contention window.
    runtime.close().await?;

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    if lash_core::store::load_persisted_session_state(store.as_ref())
        .await?
        .is_none()
    {
        let state = RuntimeSessionState {
            session_id: session_id.clone(),
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await?;
    }
    for index in 0..target_completions {
        let wake = queued_work_stress_wake(
            &session_id,
            &format!("durable contention batch {index}"),
            (index + 1) as u64,
        );
        store
            .enqueue_queued_work(lash_core::runtime::process_wake_batch_draft(wake))
            .await?;
    }
    let controller_owner = lash_core::LeaseOwnerIdentity::opaque(
        "runtime-perf-contention-controller",
        uuid::Uuid::new_v4().to_string(),
    );
    let controller_lease = store
        .try_claim_session_execution_lease(
            &session_id,
            &controller_owner,
            "runtime-perf-contention-controller",
            QUEUED_WORK_CLAIM_TTL_MS,
        )
        .await?
        .acquired()
        .ok_or_else(|| anyhow::anyhow!("durable contention controller lease was busy"))?;
    let session_fence = controller_lease.fence();
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let run_before_alloc = allocator_stats();
    let run_started = Instant::now();
    let pool_wait_collector = if scenario.uses_postgres() {
        Some(lash_core::perf_witness::Collector::install()?)
    } else {
        None
    };
    let counters = Arc::new(DurableContentionCounters::default());
    let samples = Arc::new(DurableContentionSamples::default());
    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..workers {
        tasks.spawn(run_durable_contention_worker(
            worker,
            target_completions as u64,
            session_id.clone(),
            Arc::clone(&store),
            session_fence.clone(),
            Arc::clone(&counters),
            Arc::clone(&samples),
        ));
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(anyhow::Error::from)??;
    }
    let run_turn_ms = elapsed_ms(run_started);
    if let Some(collector) = pool_wait_collector.as_ref() {
        let witness: lash_core::perf_witness::Snapshot = collector.snapshot();
        store_metrics.record_pool_checkout_waits(witness.pool_checkout_wait_nanos);
    }
    drop(pool_wait_collector);
    let run_turn_alloc = alloc_delta(run_before_alloc, allocator_stats());
    let after_turn_memory = process_memory_sample();
    store
        .release_session_execution_lease(&session_fence)
        .await?;

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let remaining = store.list_pending_queued_work(&session_id).await?.len();
    if remaining != 0 {
        anyhow::bail!("durable contention left {remaining} scenario-owned batches pending");
    }
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let claim_wait_ms = samples.claim_wait_ms.lock_recover().clone();
    let service_ms = samples.service_ms.lock_recover().clone();
    let commit_admission_wait_ms = samples.commit_admission_wait_ms.lock_recover().clone();
    let commit_admission_queue_depth = samples.commit_admission_queue_depth.lock_recover().clone();
    let claim_summary = crate::perf_support::metrics::percentile_summary(claim_wait_ms.clone());
    let service_summary = crate::perf_support::metrics::percentile_summary(service_ms.clone());
    let pool_wait_ms = store_metrics.pool_checkout_wait_samples_ms();
    let pool_wait_summary = crate::perf_support::metrics::percentile_summary(pool_wait_ms.clone());
    let completed = counters
        .completions
        .load(std::sync::atomic::Ordering::Relaxed);
    let throughput = rate_per_second(completed, run_turn_ms);
    let metric_samples_ms = BTreeMap::from([
        (
            "durable_contention.claim_wait_ms".to_string(),
            claim_wait_ms,
        ),
        ("durable_contention.service_ms".to_string(), service_ms),
        ("durable_contention.pool_wait_ms".to_string(), pool_wait_ms),
        (
            "durable_contention.commit_admission_wait_ms".to_string(),
            commit_admission_wait_ms,
        ),
    ]);
    let phase_profile = BTreeMap::from([
        (
            "durable_contention.claim_wait".to_string(),
            metric_phase(&metric_samples_ms["durable_contention.claim_wait_ms"]),
        ),
        (
            "durable_contention.service".to_string(),
            metric_phase(&metric_samples_ms["durable_contention.service_ms"]),
        ),
        (
            "durable_contention.pool_wait".to_string(),
            metric_phase(&metric_samples_ms["durable_contention.pool_wait_ms"]),
        ),
        (
            "durable_contention.commit_admission_wait".to_string(),
            metric_phase(&metric_samples_ms["durable_contention.commit_admission_wait_ms"]),
        ),
    ]);
    let metric_samples = BTreeMap::from([(
        "durable_contention.commit_admission_queue_depth".to_string(),
        commit_admission_queue_depth.clone(),
    )]);
    let mut extra_counters = BTreeMap::from([
        ("durable_contention.workers".to_string(), workers as u64),
        (
            "durable_contention.seeded_batches".to_string(),
            target_completions as u64,
        ),
        (
            "durable_contention.completed_batches".to_string(),
            completed,
        ),
        (
            "durable_contention.throughput_per_second_milli".to_string(),
            scaled_rate(throughput),
        ),
        (
            "durable_contention.claim_wait_p50_micros".to_string(),
            millis_to_micros(claim_summary.p50),
        ),
        (
            "durable_contention.claim_wait_p95_micros".to_string(),
            millis_to_micros(claim_summary.p95),
        ),
        (
            "durable_contention.service_p50_micros".to_string(),
            millis_to_micros(service_summary.p50),
        ),
        (
            "durable_contention.service_p95_micros".to_string(),
            millis_to_micros(service_summary.p95),
        ),
        (
            "durable_contention.claim_attempts".to_string(),
            counters
                .claim_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.claim_refusals".to_string(),
            counters
                .claim_refusals
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.successful_claims".to_string(),
            counters
                .successful_claims
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.renewals".to_string(),
            counters.renewals.load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.abandons".to_string(),
            counters.abandons.load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.reclaims".to_string(),
            counters.reclaims.load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.reclaim_conflicts".to_string(),
            counters
                .reclaim_conflicts
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.store_contention_retries".to_string(),
            counters
                .store_contention_retries
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.lease_probe_busy".to_string(),
            counters
                .lease_probe_busy
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.cas_failures".to_string(),
            counters
                .cas_failures
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.cas_backoff_sleeps".to_string(),
            counters
                .cas_backoff_sleeps
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        (
            "durable_contention.commit_admission_waits".to_string(),
            commit_admission_queue_depth.len() as u64,
        ),
        (
            "durable_contention.commit_admission_queue_depth_max".to_string(),
            commit_admission_queue_depth
                .iter()
                .copied()
                .fold(0.0, f64::max) as u64,
        ),
        (
            "durable_contention.pool_wait_observable".to_string(),
            u64::from(scenario.uses_postgres()),
        ),
        ("durable_contention.remaining_batches".to_string(), 0),
    ]);
    if scenario.uses_postgres() {
        extra_counters.insert(
            "durable_contention.pool_wait_p50_micros".to_string(),
            millis_to_micros(pool_wait_summary.p50),
        );
        extra_counters.insert(
            "durable_contention.pool_wait_p95_micros".to_string(),
            millis_to_micros(pool_wait_summary.p95),
        );
    }
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let zero_alloc = zero_allocation_delta();
    let turn = RuntimePerfTurnResult {
        turn_index: 0,
        run_turn_ms,
        await_background_work_ms: 0.0,
        total_ms: run_turn_ms,
        memory: RuntimePerfTurnMemoryRunResult {
            rss_before_kb: after_seed_memory.rss_kb,
            rss_after_turn_kb: after_turn_memory.rss_kb,
            rss_after_await_kb: after_turn_memory.rss_kb,
            peak_hwm_before_kb: after_seed_memory.hwm_kb,
            peak_hwm_after_await_kb: after_turn_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(after_seed_memory.rss_kb, after_turn_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(after_seed_memory.hwm_kb, after_turn_memory.hwm_kb),
        },
        allocations: RuntimePerfTurnAllocationRunResult {
            run_turn: run_turn_alloc.clone(),
            await_background_work: zero_alloc.clone(),
            total: run_turn_alloc.clone(),
        },
        phase_profile: phase_profile.clone(),
        turn_usage: TokenUsage::default(),
        usage_delta: SessionUsageReport::default(),
        cumulative_usage: SessionUsageReport::default(),
    };

    drop(store);
    drop(runtime);
    if let Some(root) = sqlite_root {
        let _ = std::fs::remove_dir_all(root);
    }
    drop(postgres_namespace);

    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        build_runtime_ms,
        seed_state_ms,
        run_turn_ms,
        await_background_work_ms: 0.0,
        export_state_ms,
        total_ms: elapsed_ms(total_started),
        session_nodes: 0,
        active_path_messages: 0,
        extra_counters,
        metric_samples,
        metric_samples_ms,
        memory: RuntimePerfMemoryRunResult {
            rss_before_kb: before_memory.rss_kb,
            rss_after_build_kb: after_build_memory.rss_kb,
            rss_after_seed_kb: after_seed_memory.rss_kb,
            rss_after_turn_kb: after_turn_memory.rss_kb,
            rss_after_await_kb: after_turn_memory.rss_kb,
            rss_after_export_kb: after_export_memory.rss_kb,
            peak_hwm_before_kb: before_memory.hwm_kb,
            peak_hwm_after_export_kb: after_export_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_export_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(before_memory.hwm_kb, after_export_memory.hwm_kb),
        },
        allocations: RuntimePerfAllocationRunResult {
            build_runtime: build_runtime_alloc,
            seed_state: seed_state_alloc,
            run_turn: run_turn_alloc,
            await_background_work: zero_alloc,
            export_state: export_state_alloc,
            total: total_alloc,
        },
        phase_profile,
        turns: vec![turn],
        cumulative_usage: SessionUsageReport::default(),
    })
}
