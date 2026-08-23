#[derive(Debug)]
struct HighTrafficOperationResult {
    ordinal: usize,
    kind: &'static str,
    latency_ms: f64,
    pre_phase_dispatch_ms: f64,
    arrival_pacing_lateness_ms: Option<f64>,
    durable_queue_depth: u64,
    phase_profile: BTreeMap<String, RuntimePerfPhaseRunResult>,
    turn_usage: TokenUsage,
}

struct HighTrafficStepResult {
    population: usize,
    elapsed_ms: f64,
    operations: Vec<HighTrafficOperationResult>,
    commits: u64,
    store_transaction: RuntimePerfStoreTiming,
    claim_scan: RuntimePerfStoreTiming,
    queue_enqueue: RuntimePerfStoreTiming,
    queue_depth_samples: Vec<u64>,
    rss_kb: Option<u64>,
    allocations: RuntimePerfAllocationDelta,
}

const KNEE_TURN_INDEX_STEP_FACTOR: usize = 100_000_000;

struct PostgresStepNamespace {
    database: lash_postgres_store::testing::IsolatedDatabase,
}

impl PostgresStepNamespace {
    async fn create(base_database_url: &str, _step_index: usize) -> Self {
        Self {
            database: lash_postgres_store::testing::IsolatedDatabase::create(base_database_url)
                .await,
        }
    }

    fn database_url(&self) -> &str {
        self.database.url()
    }
}

async fn take_population_sessions(
    runtime: &mut super::harness::BenchmarkRuntime,
    scenario: RuntimePerfScenario,
    population: usize,
) -> anyhow::Result<Vec<lash::LashSession>> {
    let core = runtime.core();
    let mut sessions = Vec::with_capacity(population);
    sessions.push(runtime.take_session());
    for index in 1..population {
        sessions.push(
            core.session(format!(
                "runtime-perf-{}-population-{index}-{}",
                scenario.name(),
                uuid::Uuid::new_v4()
            ))
            .open()
            .await?,
        );
    }
    Ok(sessions)
}

async fn run_once_high_traffic(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    config: &HighTrafficConfig,
    postgres_database_url: Option<&str>,
) -> anyhow::Result<RuntimePerfRunResult> {
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let before_alloc = allocator_stats();
    let populations = if scenario.is_high_traffic_knee() {
        config.knee_populations.clone()
    } else {
        vec![config.population]
    };

    let mut steps = Vec::with_capacity(populations.len());
    for (step_index, population) in populations.into_iter().enumerate() {
        let postgres_namespace = if scenario.is_high_traffic_knee() {
            match postgres_database_url {
                Some(database_url) => {
                    Some(PostgresStepNamespace::create(database_url, step_index).await)
                }
                None => None,
            }
        } else {
            None
        };
        let step_database_url = postgres_namespace
            .as_ref()
            .map_or(postgres_database_url, |namespace| {
                Some(namespace.database_url())
            });
        let step_result = Box::pin(run_high_traffic_step(
                scenario,
                chat_turns,
                population,
                config,
                step_database_url,
            ))
            .await;
        drop(postgres_namespace);
        steps.push(step_result?);
    }

    let after_memory = process_memory_sample();
    let total_alloc = alloc_delta(before_alloc, allocator_stats());
    let mut extra_counters = BTreeMap::new();
    let mut turns = Vec::new();
    let mut phase_profiles = Vec::new();
    let mut queue_sample_index = 0usize;
    let mut total_elapsed_ms = 0.0;
    let mut total_commits = 0u64;
    let mut total_store_transaction = RuntimePerfStoreTiming::default();
    let mut total_claim_scan = RuntimePerfStoreTiming::default();
    let mut total_queue_enqueue = RuntimePerfStoreTiming::default();
    let mut pre_phase_dispatch_samples = Vec::new();
    let mut arrival_pacing_lateness_samples = Vec::new();
    let mut knee_baseline = None;
    let mut detected_knee = None;

    for (step_index, step) in steps.iter().enumerate() {
        let latencies = step
            .operations
            .iter()
            .map(|operation| operation.latency_ms)
            .collect::<Vec<_>>();
        let latency = crate::perf_support::metrics::percentile_summary(latencies);
        let turns_per_second = rate_per_second(step.operations.len() as u64, step.elapsed_ms);
        let commits_per_second = rate_per_second(step.commits, step.elapsed_ms);
        let prefix = if scenario.is_high_traffic_knee() {
            format!("knee.step.{step_index}")
        } else {
            "load".to_string()
        };
        extra_counters.insert(format!("{prefix}.population"), step.population as u64);
        extra_counters.insert(
            format!("{prefix}.completed_turns"),
            step.operations.len() as u64,
        );
        extra_counters.insert(
            format!("{prefix}.turns_per_second_milli"),
            scaled_rate(turns_per_second),
        );
        extra_counters.insert(
            format!("{prefix}.commits_per_second_milli"),
            scaled_rate(commits_per_second),
        );
        extra_counters.insert(
            format!("{prefix}.latency_p50_micros"),
            millis_to_micros(latency.p50),
        );
        extra_counters.insert(
            format!("{prefix}.latency_p95_micros"),
            millis_to_micros(latency.p95),
        );
        extra_counters.insert(
            format!("{prefix}.latency_p99_micros"),
            millis_to_micros(latency.p99),
        );
        insert_phase_percentiles(&mut extra_counters, &prefix, &step.operations);
        extra_counters.insert(
            format!("{prefix}.alloc_bytes"),
            step.allocations.bytes_allocated as u64,
        );
        if let Some(rss_kb) = step.rss_kb {
            extra_counters.insert(format!("{prefix}.rss_kb"), rss_kb);
        }

        if scenario.is_high_traffic_knee() {
            let (base_population, base_p95, base_turns_per_second) = *knee_baseline.get_or_insert((
                step.population,
                latency.p95.max(f64::EPSILON),
                turns_per_second.max(f64::EPSILON),
            ));
            let p95_vs_base = latency.p95 / base_p95;
            let linear_throughput =
                base_turns_per_second * step.population as f64 / base_population as f64;
            let throughput_vs_linear = turns_per_second / linear_throughput.max(f64::EPSILON);
            extra_counters.insert(
                format!("{prefix}.p95_vs_base_ratio_milli"),
                (p95_vs_base.max(0.0) * 1_000.0).round() as u64,
            );
            extra_counters.insert(
                format!("{prefix}.throughput_vs_linear_ratio_milli"),
                (throughput_vs_linear.max(0.0) * 1_000.0).round() as u64,
            );
            if step_index > 0
                && p95_vs_base > config.knee_threshold
                && detected_knee.is_none()
            {
                detected_knee = Some(step.population);
            }
        }

        for (name, timing) in [
            ("store_transaction", step.store_transaction),
            ("claim_scan", step.claim_scan),
            ("queue_enqueue", step.queue_enqueue),
        ] {
            extra_counters.insert(
                format!("{prefix}.wait.{name}.micros"),
                average_store_timing(timing),
            );
            extra_counters.insert(format!("{prefix}.wait.{name}.samples"), timing.calls);
        }

        for (step_sample_index, depth) in step.queue_depth_samples.iter().enumerate() {
            let key = if scenario.is_high_traffic_knee() {
                format!(
                    "{prefix}.queue_depth.inflight.sample.{step_sample_index:08}"
                )
            } else {
                format!("queue_depth.inflight.sample.{queue_sample_index:08}")
            };
            extra_counters.insert(key, *depth);
            queue_sample_index += 1;
        }
        for operation in &step.operations {
            extra_counters.insert(
                format!("turn_mix.{}.completed", operation.kind),
                extra_counters
                    .get(&format!("turn_mix.{}.completed", operation.kind))
                    .copied()
                    .unwrap_or_default()
                    + 1,
            );
            pre_phase_dispatch_samples.push(operation.pre_phase_dispatch_ms);
            if let Some(lateness_ms) = operation.arrival_pacing_lateness_ms {
                arrival_pacing_lateness_samples.push(lateness_ms);
            }
            let durable_sample_key = if scenario.is_high_traffic_knee() {
                format!(
                    "{prefix}.queue_depth.durable.sample.{:08}",
                    operation.ordinal
                )
            } else {
                format!("queue_depth.durable.sample.{:08}", operation.ordinal)
            };
            extra_counters.insert(durable_sample_key, operation.durable_queue_depth);
            phase_profiles.push(operation.phase_profile.clone());
            turns.push(RuntimePerfTurnResult {
                turn_index: high_traffic_turn_index(
                    scenario.is_high_traffic_knee(),
                    step_index,
                    operation.ordinal,
                )?,
                run_turn_ms: operation.latency_ms,
                await_background_work_ms: 0.0,
                total_ms: operation.latency_ms,
                memory: RuntimePerfTurnMemoryRunResult {
                    rss_before_kb: None,
                    rss_after_turn_kb: None,
                    rss_after_await_kb: None,
                    peak_hwm_before_kb: None,
                    peak_hwm_after_await_kb: None,
                    rss_growth_kb: None,
                    hwm_growth_kb: None,
                },
                allocations: RuntimePerfTurnAllocationRunResult {
                    run_turn: zero_allocation_delta(),
                    await_background_work: zero_allocation_delta(),
                    total: zero_allocation_delta(),
                },
                phase_profile: operation.phase_profile.clone(),
                turn_usage: operation.turn_usage.clone(),
                usage_delta: SessionUsageReport::default(),
                cumulative_usage: SessionUsageReport::default(),
            });
        }
        total_elapsed_ms += step.elapsed_ms;
        total_commits += step.commits;
        add_store_timing(&mut total_store_transaction, step.store_transaction);
        add_store_timing(&mut total_claim_scan, step.claim_scan);
        add_store_timing(&mut total_queue_enqueue, step.queue_enqueue);
    }
    turns.sort_by_key(|turn| turn.turn_index);

    let completed_turns = turns.len() as u64;
    extra_counters.insert(
        "throughput.turns_per_second_milli".to_string(),
        scaled_rate(rate_per_second(completed_turns, total_elapsed_ms)),
    );
    extra_counters.insert(
        "throughput.commits_per_second_milli".to_string(),
        scaled_rate(rate_per_second(total_commits, total_elapsed_ms)),
    );
    extra_counters.insert("queue_depth.samples".to_string(), queue_sample_index as u64);
    if scenario.is_high_traffic_knee() {
        extra_counters.insert(
            "knee.detected_population".to_string(),
            detected_knee.unwrap_or_default() as u64,
        );
        extra_counters.insert(
            "knee.threshold_ratio_milli".to_string(),
            (config.knee_threshold * 1_000.0).round() as u64,
        );
    }

    let pre_phase_dispatch_us = average_micros(&pre_phase_dispatch_samples);
    let arrival_pacing_lateness_us = average_micros(&arrival_pacing_lateness_samples);
    let store_transaction_us = average_store_timing(total_store_transaction);
    let claim_scan_us = average_store_timing(total_claim_scan);
    let queue_enqueue_us = average_store_timing(total_queue_enqueue);
    let mut observable_wait_values = vec![
        ("store_transaction", store_transaction_us),
        ("pre_phase_dispatch", pre_phase_dispatch_us),
        ("claim_scan", claim_scan_us),
        ("queue_enqueue", queue_enqueue_us),
    ];
    if config.arrival_rate > 0 {
        observable_wait_values.push(("arrival_pacing_lateness", arrival_pacing_lateness_us));
    }
    for (name, value) in &observable_wait_values {
        extra_counters.insert(format!("wait.{name}.micros"), *value);
    }
    extra_counters.insert(
        "wait.arrival_pacing_lateness.micros".to_string(),
        arrival_pacing_lateness_us,
    );
    extra_counters.insert(
        "wait.arrival_pacing_lateness.observable".to_string(),
        u64::from(config.arrival_rate > 0),
    );
    // RuntimePersistence exposes a complete store round trip, not the pool
    // checkout subspan. Keep the key explicit and mark it unavailable instead
    // of manufacturing a pool number from the transaction proxy.
    extra_counters.insert("wait.pool.micros".to_string(), 0);
    extra_counters.insert("wait.pool.observable".to_string(), 0);
    if let Some((name, value)) = observable_wait_values
        .into_iter()
        .max_by_key(|(_, value)| *value)
    {
        extra_counters.insert(format!("wait.top.{name}.micros"), value);
    }

    let mut phase_profile = sum_phase_profiles(phase_profiles.iter());
    insert_wait_phase(
        &mut phase_profile,
        "wait.store_transaction",
        total_store_transaction,
    );
    insert_wait_phase(&mut phase_profile, "wait.claim_scan", total_claim_scan);
    insert_wait_phase(&mut phase_profile, "wait.queue_enqueue", total_queue_enqueue);

    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        build_runtime_ms: 0.0,
        seed_state_ms: 0.0,
        run_turn_ms: round3(turns.iter().map(|turn| turn.run_turn_ms).sum()),
        await_background_work_ms: 0.0,
        export_state_ms: 0.0,
        total_ms: elapsed_ms(total_started),
        session_nodes: 0,
        active_path_messages: 0,
        extra_counters,
        metric_samples_ms: BTreeMap::new(),
        memory: RuntimePerfMemoryRunResult {
            rss_before_kb: before_memory.rss_kb,
            rss_after_build_kb: None,
            rss_after_seed_kb: None,
            rss_after_turn_kb: after_memory.rss_kb,
            rss_after_await_kb: after_memory.rss_kb,
            rss_after_export_kb: after_memory.rss_kb,
            peak_hwm_before_kb: before_memory.hwm_kb,
            peak_hwm_after_export_kb: after_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(before_memory.hwm_kb, after_memory.hwm_kb),
        },
        allocations: RuntimePerfAllocationRunResult {
            build_runtime: zero_allocation_delta(),
            seed_state: zero_allocation_delta(),
            run_turn: total_alloc.clone(),
            await_background_work: zero_allocation_delta(),
            export_state: zero_allocation_delta(),
            total: total_alloc,
        },
        phase_profile,
        turns,
        cumulative_usage: SessionUsageReport::default(),
    })
}

async fn run_high_traffic_step(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    population: usize,
    config: &HighTrafficConfig,
    postgres_database_url: Option<&str>,
) -> anyhow::Result<HighTrafficStepResult> {
    let sqlite_root = if postgres_database_url.is_none() {
        Some(make_temp_bench_dir(&format!(
            "lash-runtime-perf-{}-{population}",
            scenario.name()
        ))?)
    } else {
        None
    };
    let mut runtime = if let Some(database_url) = postgres_database_url {
        build_runtime_with_postgres_store(scenario, database_url).await?
    } else {
        build_runtime_with_sqlite_store(
            scenario,
            sqlite_root.as_ref().expect("SQLite root").clone(),
        )
        .await?
    };
    let core = runtime.core();
    let metrics = runtime.store_metrics();
    let calls_before = metrics.call_counters();
    let timings_before = metrics.timing_snapshot();
    let sessions = take_population_sessions(&mut runtime, scenario, population).await?;
    let allocation_before = allocator_stats();
    let memory_before = process_memory_sample();
    let queue_depth = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let queue_depth_samples = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    let mut workers = tokio::task::JoinSet::new();

    for (session_index, session) in sessions.into_iter().enumerate() {
        let core = core.clone();
        let config = config.clone();
        let queue_depth = Arc::clone(&queue_depth);
        let queue_depth_samples = Arc::clone(&queue_depth_samples);
        workers.spawn(async move {
            let mut results = Vec::with_capacity(chat_turns);
            for turn_index in 0..chat_turns {
                let ordinal = turn_index * population + session_index;
                let scheduled_at = scheduled_arrival(started, ordinal, config.arrival_rate);
                if let Some(scheduled_at) = scheduled_at {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(scheduled_at)).await;
                }
                let arrival_pacing_lateness_ms = scheduled_at.map(|scheduled| {
                    round3(
                        Instant::now()
                            .saturating_duration_since(scheduled)
                            .as_secs_f64()
                            * 1_000.0,
                    )
                });
                let depth = queue_depth.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                queue_depth_samples.lock_recover().push(depth as u64);
                let operation = run_high_traffic_operation(
                    &core,
                    &session,
                    ordinal,
                    config.operation_kind(ordinal),
                    arrival_pacing_lateness_ms,
                )
                .await;
                let depth = queue_depth.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) - 1;
                queue_depth_samples.lock_recover().push(depth as u64);
                results.push(operation?);
            }
            Ok::<_, anyhow::Error>((session, results))
        });
    }

    let mut operations = Vec::with_capacity(population * chat_turns);
    let mut completed_sessions = Vec::with_capacity(population);
    while let Some(result) = workers.join_next().await {
        let (session, mut session_operations) = result??;
        completed_sessions.push(session);
        operations.append(&mut session_operations);
    }
    let elapsed_ms = elapsed_ms(started);
    operations.sort_by_key(|operation| operation.ordinal);
    let rss_kb = process_memory_sample().rss_kb;
    let allocations = alloc_delta(allocation_before, allocator_stats());
    let calls_after = metrics.call_counters();
    let timings_after = metrics.timing_snapshot();
    let commits = counter_delta(
        &calls_before,
        &calls_after,
        "store_calls.commit_runtime_state",
    );
    let store_transaction = timing_delta(
        &timings_before,
        &timings_after,
        "store_transaction",
    );
    let claim_scan = timing_delta(&timings_before, &timings_after, "claim_scan");
    let queue_enqueue = timing_delta(&timings_before, &timings_after, "queue_enqueue");
    let queue_depth_samples = queue_depth_samples.lock_recover().clone();

    for session in completed_sessions {
        session.close().await?;
    }
    runtime.close().await?;
    drop(runtime);
    if let Some(root) = sqlite_root {
        let _ = std::fs::remove_dir_all(root);
    }

    Ok(HighTrafficStepResult {
        population,
        elapsed_ms,
        operations,
        commits,
        store_transaction,
        claim_scan,
        queue_enqueue,
        queue_depth_samples,
        rss_kb: rss_kb.or(memory_before.rss_kb),
        allocations,
    })
}

async fn run_high_traffic_operation(
    core: &lash::LashCore,
    session: &lash::LashSession,
    ordinal: usize,
    kind: &'static str,
    arrival_pacing_lateness_ms: Option<f64>,
) -> anyhow::Result<HighTrafficOperationResult> {
    let probe = Arc::new(RuntimePerfPhaseProbe::default());
    session.set_turn_phase_probe(probe.clone()).await;
    let operation_started = Instant::now();
    let mut durable_queue_depth = 0;
    let turn_usage = if kind == "queued" {
        session
            .enqueue(TurnInput::text(format!(
                "load-kind:queued operation:{ordinal}"
            )))
            .id(format!("runtime-perf-load-{ordinal}"))
            .send()
            .await?;
        durable_queue_depth = session.pending_turn_inputs().await?.len() as u64;
        let controller = lash::runtime::InlineRuntimeEffectController::default()
            .allow_process_lifetime_completion_keys();
        let drain = session
            .queued_turn()
            .effects(&controller)
            .drain_id(format!("runtime-perf-load-drain-{ordinal}"))
            .run()
            .await?;
        if drain.ran().is_none() {
            anyhow::bail!("queued high-traffic operation {ordinal} did not run a turn");
        }
        TokenUsage::default()
    } else {
        run_high_traffic_direct_turn(session, ordinal, kind).await?
    };
    if kind == "trigger" {
        let source_key = lash_core::facade_support::empty_trigger_source_key(
            super::providers::BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE,
        )?;
        let request = lash_core::TriggerOccurrenceRequest::new(
            super::providers::BENCHMARK_MAIL_RECEIVED_SOURCE_TYPE,
            source_key,
            serde_json::json!({
                "account": "test",
                "title": "load",
                "text": "runtime perf benchmark ok",
            }),
            format!("runtime-perf-load-trigger:{}:{ordinal}", session.session_id()),
        )
        .with_source(serde_json::json!({}))
        .for_session(session.session_id());
        let trigger_controller =
            lash::runtime::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys();
        let controller = lash_core::ScopedEffectController::borrowed(
            &trigger_controller,
            session.turn_scope(format!(
                "runtime-perf-load-trigger-emission-{ordinal}"
            )),
        )?;
        let delivery_report = core.triggers().emit(request, controller).await?;
        let delivery_process_ids = delivery_report.started_process_ids();
        if delivery_process_ids.is_empty() {
            let registrations = session.triggers().list_all().await?;
            anyhow::bail!(
                "trigger high-traffic operation {ordinal} did not expose a delivery process: report={delivery_report:?}, registrations={registrations:?}"
            );
        }
        for process_id in delivery_process_ids {
            let outcome = core.processes().await_output(&process_id).await?;
            if !matches!(outcome, lash_core::ProcessAwaitOutput::Success { .. }) {
                anyhow::bail!(
                    "trigger high-traffic operation {ordinal} delivery process {process_id} did not succeed: {outcome:?}"
                );
            }
        }
    }
    let latency_ms = elapsed_ms(operation_started);
    let pre_phase_dispatch_ms = probe.first_phase_delay_ms(operation_started);
    let mut phase_profile = probe.take_completed();
    phase_profile.insert(
        "wait.pre_phase_dispatch".to_string(),
        synthetic_phase(pre_phase_dispatch_ms, 1),
    );
    if let Some(lateness_ms) = arrival_pacing_lateness_ms {
        phase_profile.insert(
            "wait.arrival_pacing_lateness".to_string(),
            synthetic_phase(lateness_ms, 1),
        );
    }

    Ok(HighTrafficOperationResult {
        ordinal,
        kind,
        latency_ms,
        pre_phase_dispatch_ms,
        arrival_pacing_lateness_ms,
        durable_queue_depth,
        phase_profile,
        turn_usage,
    })
}

async fn run_high_traffic_direct_turn(
    session: &lash::LashSession,
    ordinal: usize,
    kind: &str,
) -> anyhow::Result<TokenUsage> {
    let controller = lash::runtime::InlineRuntimeEffectController::default()
        .allow_process_lifetime_completion_keys();
    let report = session
        .turn(TurnInput::text(format!(
            "load-kind:{kind} operation:{ordinal} session:{}",
            session.session_id()
        )))
        .turn_id(format!("runtime-perf-load-turn-{ordinal}"))
        .effects(&controller)
        .run()
        .await?
        .result;
    if !matches!(report.outcome, lash::TurnOutcome::Finished(_)) {
        anyhow::bail!(
            "high-traffic {kind} operation {ordinal} did not finish: {:?}; errors={:?}; output={:?}",
            report.outcome,
            report.errors,
            report.assistant_output
        );
    }
    Ok(report.usage)
}

fn high_traffic_turn_index(
    knee_mode: bool,
    step_index: usize,
    ordinal: usize,
) -> anyhow::Result<usize> {
    if !knee_mode {
        return Ok(ordinal);
    }
    if ordinal >= KNEE_TURN_INDEX_STEP_FACTOR {
        anyhow::bail!(
            "knee step operation ordinal {ordinal} exceeds the {KNEE_TURN_INDEX_STEP_FACTOR}-wide turn-index namespace"
        );
    }
    step_index
        .checked_mul(KNEE_TURN_INDEX_STEP_FACTOR)
        .and_then(|prefix| prefix.checked_add(ordinal))
        .ok_or_else(|| anyhow::anyhow!("knee turn index overflow at step {step_index}"))
}

fn scheduled_arrival(started: Instant, ordinal: usize, arrival_rate: u64) -> Option<Instant> {
    (arrival_rate > 0).then(|| {
        started
            + Duration::from_secs_f64(ordinal as f64 / arrival_rate.max(1) as f64)
    })
}

fn timing_delta(
    before: &BTreeMap<String, RuntimePerfStoreTiming>,
    after: &BTreeMap<String, RuntimePerfStoreTiming>,
    key: &str,
) -> RuntimePerfStoreTiming {
    let before = before.get(key).copied().unwrap_or_default();
    let after = after.get(key).copied().unwrap_or_default();
    RuntimePerfStoreTiming {
        calls: after.calls.saturating_sub(before.calls),
        total_micros: after.total_micros.saturating_sub(before.total_micros),
    }
}

fn counter_delta(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
    key: &str,
) -> u64 {
    after
        .get(key)
        .copied()
        .unwrap_or_default()
        .saturating_sub(before.get(key).copied().unwrap_or_default())
}

fn add_store_timing(total: &mut RuntimePerfStoreTiming, value: RuntimePerfStoreTiming) {
    total.calls = total.calls.saturating_add(value.calls);
    total.total_micros = total.total_micros.saturating_add(value.total_micros);
}

fn average_store_timing(timing: RuntimePerfStoreTiming) -> u64 {
    timing.total_micros / timing.calls.max(1)
}

fn average_micros(values: &[f64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    millis_to_micros(values.iter().sum::<f64>() / values.len() as f64)
}

fn rate_per_second(count: u64, elapsed_ms: f64) -> f64 {
    count as f64 * 1_000.0 / elapsed_ms.max(f64::EPSILON)
}

fn scaled_rate(rate: f64) -> u64 {
    (rate.max(0.0) * 1_000.0).round() as u64
}

fn millis_to_micros(value: f64) -> u64 {
    (value.max(0.0) * 1_000.0).round() as u64
}

fn synthetic_phase(duration_ms: f64, samples: usize) -> RuntimePerfPhaseRunResult {
    RuntimePerfPhaseRunResult {
        samples,
        duration_ms: round3(duration_ms),
        allocations: zero_allocation_delta(),
        rss_growth_kb: None,
    }
}

fn insert_wait_phase(
    profile: &mut BTreeMap<String, RuntimePerfPhaseRunResult>,
    name: &str,
    timing: RuntimePerfStoreTiming,
) {
    profile.insert(
        name.to_string(),
        synthetic_phase(timing.total_micros as f64 / 1_000.0, timing.calls as usize),
    );
}

fn insert_phase_percentiles(
    counters: &mut BTreeMap<String, u64>,
    prefix: &str,
    operations: &[HighTrafficOperationResult],
) {
    let mut phases: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for operation in operations {
        for (name, measurement) in &operation.phase_profile {
            phases
                .entry(name.as_str())
                .or_default()
                .push(measurement.duration_ms);
        }
    }
    for (name, durations) in phases {
        let summary = crate::perf_support::metrics::percentile_summary(durations);
        counters.insert(
            format!("{prefix}.phase.{name}.p50_micros"),
            millis_to_micros(summary.p50),
        );
        counters.insert(
            format!("{prefix}.phase.{name}.p95_micros"),
            millis_to_micros(summary.p95),
        );
        counters.insert(
            format!("{prefix}.phase.{name}.p99_micros"),
            millis_to_micros(summary.p99),
        );
    }
}

#[cfg(test)]
mod high_traffic_tests {
    use super::*;

    #[test]
    fn mix_parser_selects_each_weighted_kind_deterministically() {
        let config = HighTrafficConfig::parse(
            4,
            0,
            "plain=1,tool=1,queued=1,child=1,wake=1,trigger=1",
            "4,8",
            1.25,
        )
        .expect("valid mix");
        assert_eq!(
            (0..6)
                .map(|ordinal| config.operation_kind(ordinal))
                .collect::<Vec<_>>(),
            vec!["plain", "tool", "queued", "child", "wake", "trigger"]
        );
    }

    #[test]
    fn knee_turn_indices_prefix_the_step_without_collisions() {
        assert_eq!(high_traffic_turn_index(true, 0, 12).unwrap(), 12);
        assert_eq!(
            high_traffic_turn_index(true, 1, 12).unwrap(),
            KNEE_TURN_INDEX_STEP_FACTOR + 12
        );
        assert_eq!(high_traffic_turn_index(false, 7, 12).unwrap(), 12);
    }

}
