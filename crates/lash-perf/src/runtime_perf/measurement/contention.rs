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
            session.configure(lash::SessionConfigPatch::default()).await?;
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
        execution_ms.push(
            measure_writer_operation(session.clone(), scenario, operation, ordinal).await?,
        );
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

        assert!(
            result.extra_counters["async_settlement.open_spans_before_settle"] >= 2
        );
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
        .map(|(key, samples)| (key.trim_end_matches("_ms").to_string(), metric_phase(samples)))
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
                .open_child_session(format!(
                    "runtime-perf-{}-peer-{worker}",
                    scenario.name()
                ))
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
            session.processes().await_output(&process.process_id).await?;
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
        ("async_settlement.parent_return_ms".to_string(), vec![parent_return_ms]),
        ("async_settlement.spawn_ms".to_string(), vec![spawn_ms]),
        ("async_settlement.settle_ms".to_string(), vec![settle_ms]),
        ("async_settlement.drain_ms".to_string(), vec![drain_ms]),
        ("async_settlement.child_terminal_ms".to_string(), child_terminal_ms),
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
