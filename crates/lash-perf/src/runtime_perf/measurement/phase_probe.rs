
use lash_sansio::sync::MutexExt;
#[derive(Clone, Copy)]
struct PhaseStart {
    started_at: Instant,
    alloc_before: Stats,
    memory_before: ProcessMemorySample,
}

#[derive(Default)]
struct RuntimePerfPhaseProbeState {
    open: HashMap<String, Vec<PhaseStart>>,
    completed: BTreeMap<String, RuntimePerfPhaseRunResult>,
    first_started_at: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct RuntimePerfPhaseProbe {
    state: Mutex<RuntimePerfPhaseProbeState>,
}

struct ScopedPerfEffectController;

impl lash::runtime::AwaitEventResolver for ScopedPerfEffectController {}

#[async_trait::async_trait]
impl lash::runtime::RuntimeEffectController for ScopedPerfEffectController {
    async fn execute_effect(
        &self,
        envelope: lash::runtime::RuntimeEffectEnvelope,
        local_executor: lash::runtime::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash::runtime::RuntimeEffectOutcome, lash::runtime::RuntimeEffectControllerError>
    {
        local_executor.execute(envelope).await
    }
}

impl RuntimePerfPhaseProbe {
    pub(crate) fn take_completed(&self) -> BTreeMap<String, RuntimePerfPhaseRunResult> {
        let mut state = self.state.lock_recover();
        // Async process phases can still be running at take; leave open spans dropped.
        std::mem::take(&mut state.completed)
    }

    pub(crate) fn open_span_count(&self) -> usize {
        self.state
            .lock_recover()
            .open
            .values()
            .map(Vec::len)
            .sum()
    }

    pub(crate) fn take_completed_after_settlement(
        &self,
    ) -> anyhow::Result<BTreeMap<String, RuntimePerfPhaseRunResult>> {
        let mut state = self.state.lock_recover();
        let open_span_count = state.open.values().map(Vec::len).sum::<usize>();
        if open_span_count != 0 {
            anyhow::bail!(
                "async settlement finished with {open_span_count} open phase spans"
            );
        }
        Ok(std::mem::take(&mut state.completed))
    }

    pub(crate) fn first_phase_delay_ms(&self, operation_started: Instant) -> f64 {
        self.state
            .lock_recover()
            .first_started_at
            .and_then(|started| started.checked_duration_since(operation_started))
            .map_or(0.0, |elapsed| round3(elapsed.as_secs_f64() * 1000.0))
    }
}

impl RuntimeTurnPhaseProbe for RuntimePerfPhaseProbe {
    fn begin(&self, phase: RuntimeTurnPhase) {
        self.begin_named(phase_name(phase));
    }

    fn end(&self, phase: RuntimeTurnPhase) {
        self.end_named(phase_name(phase));
    }

    fn begin_named(&self, phase: &str) {
        let mut state = self.state.lock_recover();
        state.first_started_at.get_or_insert_with(Instant::now);
        state
            .open
            .entry(phase.to_string())
            .or_default()
            .push(PhaseStart {
                started_at: Instant::now(),
                alloc_before: allocator_stats(),
                memory_before: process_memory_sample(),
            });
    }

    fn end_named(&self, phase: &str) {
        let mut state = self.state.lock_recover();
        let Some(starts) = state.open.get_mut(phase) else {
            return;
        };
        let Some(start) = starts.pop() else {
            return;
        };
        if starts.is_empty() {
            state.open.remove(phase);
        }
        record_completed_phase(&mut state.completed, phase.to_string(), start);
    }
}

fn record_completed_phase(
    completed: &mut BTreeMap<String, RuntimePerfPhaseRunResult>,
    name: String,
    start: PhaseStart,
) {
    let alloc_after = allocator_stats();
    let memory_after = process_memory_sample();
    let metrics = RuntimePerfPhaseRunResult {
        samples: 1,
        duration_ms: elapsed_ms(start.started_at),
        allocations: alloc_delta(start.alloc_before, alloc_after),
        rss_growth_kb: diff_opt_i64(start.memory_before.rss_kb, memory_after.rss_kb),
    };
    let entry = completed
        .entry(name)
        .or_insert_with(|| RuntimePerfPhaseRunResult {
            samples: 0,
            duration_ms: 0.0,
            allocations: zero_allocation_delta(),
            rss_growth_kb: Some(0),
        });
    entry.samples += metrics.samples;
    entry.duration_ms = round3(entry.duration_ms + metrics.duration_ms);
    entry.allocations = sum_allocation_deltas([&entry.allocations, &metrics.allocations]);
    entry.rss_growth_kb = sum_optional_i64(entry.rss_growth_kb, metrics.rss_growth_kb);
}
pub(crate) async fn run_once(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    high_traffic: &HighTrafficConfig,
) -> anyhow::Result<RuntimePerfRunResult> {
    let before_scheduler = RuntimeSchedulerSample::capture();
    let result = Box::pin(run_once_inner(scenario, chat_turns, high_traffic)).await;
    let after_scheduler = RuntimeSchedulerSample::capture();
    let mut result = result?;
    result
        .metric_samples
        .extend(before_scheduler.window_metric_samples(&after_scheduler));
    Ok(result)
}

async fn run_once_inner(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    high_traffic: &HighTrafficConfig,
) -> anyhow::Result<RuntimePerfRunResult> {
    if scenario.is_high_traffic() {
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
        return Box::pin(run_once_high_traffic(
            scenario,
            chat_turns,
            high_traffic,
            database_url.as_deref(),
        ))
        .await;
    }
    match scenario {
        RuntimePerfScenario::WriterContention2Workers
        | RuntimePerfScenario::WriterContention8Workers => {
            return Box::pin(run_once_writer_contention(scenario, chat_turns)).await;
        }
        RuntimePerfScenario::AsyncProcessSettlement2Children
        | RuntimePerfScenario::AsyncProcessSettlement8Children => {
            return Box::pin(run_once_async_process_settlement(scenario, chat_turns)).await;
        }
        RuntimePerfScenario::TurnCheckpoint => return run_once_turn_checkpoint(chat_turns).await,
        RuntimePerfScenario::CheckpointStateHotPaths => {
            return Box::pin(run_once_checkpoint_state_hot_paths(chat_turns)).await;
        }
        RuntimePerfScenario::LiveReplayPressure => {
            return run_once_live_replay_pressure(chat_turns).await;
        }
        RuntimePerfScenario::TraceJsonlStandard | RuntimePerfScenario::TraceJsonlExtended => {
            return Box::pin(run_once_trace_jsonl(scenario, chat_turns)).await;
        }
        RuntimePerfScenario::OpenAiResponsesSseParse => {
            return run_once_openai_responses_sse_parse(chat_turns).await;
        }
        RuntimePerfScenario::DirectLlmClient => {
            return run_once_direct_llm_client(chat_turns).await;
        }
        RuntimePerfScenario::ProcessListStress => {
            return run_once_process_list_stress(chat_turns).await;
        }
        RuntimePerfScenario::StoreHardeningHotPaths => {
            return run_once_store_hardening_hot_paths(chat_turns).await;
        }
        RuntimePerfScenario::QueuedWorkClaimStress => {
            return Box::pin(run_once_queued_work_claim_stress(chat_turns)).await;
        }
        RuntimePerfScenario::TurnInputIngressInterrupt => {
            return run_once_turn_input_ingress_interrupt(chat_turns).await;
        }
        RuntimePerfScenario::EmbedStandard | RuntimePerfScenario::EmbedRlm => {
            return run_once_embed(scenario, chat_turns).await;
        }
        RuntimePerfScenario::Standard
        | RuntimePerfScenario::Rlm
        | RuntimePerfScenario::StandardToolCalls
        | RuntimePerfScenario::StandardAsyncToolCompletion
        | RuntimePerfScenario::RlmToolCalls
        | RuntimePerfScenario::RlmAsyncToolCompletion
        | RuntimePerfScenario::RlmProcessHandles
        | RuntimePerfScenario::RlmTriggerMailPipeline
        | RuntimePerfScenario::RlmProcessAsyncToolCompletion
        | RuntimePerfScenario::RlmSubagentSpawn
        | RuntimePerfScenario::RlmLlmQuery
        | RuntimePerfScenario::RlmGlobals
        | RuntimePerfScenario::RlmLargePrint
        | RuntimePerfScenario::RlmStreamedPairedLashlang
        | RuntimePerfScenario::RlmLargeToolCatalog
        | RuntimePerfScenario::RlmObliqueStackMix
        | RuntimePerfScenario::OpenAiCompatStream
        | RuntimePerfScenario::StandardShellOutput
        | RuntimePerfScenario::ToolDiscoverySearch
        | RuntimePerfScenario::ScopedEffectController
        | RuntimePerfScenario::StoreReopen
        | RuntimePerfScenario::SqliteStoreReopen
        | RuntimePerfScenario::DeepTurnComposition
        | RuntimePerfScenario::TurnStartGate
        | RuntimePerfScenario::TurnCancelRoundTrip
        | RuntimePerfScenario::IngressClaimProjection
        | RuntimePerfScenario::DurableStandardToolTurnSqlite
        | RuntimePerfScenario::DurableStandardToolTurnPostgres
        | RuntimePerfScenario::DurableRlmCheckpointTurnSqlite
        | RuntimePerfScenario::DurableRlmCheckpointTurnPostgres
        | RuntimePerfScenario::DurableAgentChildTurnSqlite
        | RuntimePerfScenario::DurableAgentChildTurnPostgres
        | RuntimePerfScenario::DurableCheckpointCurveSqlite
        | RuntimePerfScenario::DurableCheckpointCurvePostgres => {}
        RuntimePerfScenario::HighTrafficLoadSqlite
        | RuntimePerfScenario::HighTrafficLoadPostgres
        | RuntimePerfScenario::HighTrafficKneeSqlite
        | RuntimePerfScenario::HighTrafficKneePostgres => {
            unreachable!("high-traffic scenarios return before the generic dispatch")
        }
    }

    let postgres_database_url = if scenario.uses_postgres() {
        configured_postgres_database_url()
    } else {
        None
    };
    if scenario.uses_postgres() && postgres_database_url.is_none() {
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

    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let sqlite_root = if matches!(scenario, RuntimePerfScenario::SqliteStoreReopen)
        || (scenario.is_durable() && !scenario.uses_postgres())
    {
        Some(make_temp_bench_dir(&format!(
            "lash-runtime-perf-{}",
            scenario.name()
        ))?)
    } else {
        None
    };
    let lashlang_trace_root = if matches!(
        scenario,
        RuntimePerfScenario::RlmTriggerMailPipeline | RuntimePerfScenario::RlmObliqueStackMix
    ) {
        Some(make_temp_bench_dir(
            format!("lash-runtime-perf-{}", scenario.name()).as_str(),
        )?)
    } else {
        None
    };
    let trace_config = lashlang_trace_root
        .as_ref()
        .map(|root| RuntimePerfTraceConfig {
            trace_jsonl_path: matches!(scenario, RuntimePerfScenario::RlmObliqueStackMix)
                .then(|| root.join("trace.jsonl")),
            lashlang_execution_jsonl_path: Some(root.join("lashlang-execution.jsonl")),
            trace_level: lash::tracing::TraceLevel::Extended,
        });
    let mut runtime = if let Some(database_url) = postgres_database_url.as_deref() {
        build_runtime_with_postgres_store(scenario, database_url).await?
    } else if let Some(root) = sqlite_root.as_ref() {
        build_runtime_with_sqlite_store(scenario, root.clone()).await?
    } else {
        build_runtime_with_store(scenario, None, trace_config).await?
    };
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    seed_runtime_state(&mut runtime, scenario).await?;
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut extra_counters = BTreeMap::new();
    for turn_index in 0..chat_turns {
        let mut extra_phase_profile = BTreeMap::new();
        if matches!(scenario, RuntimePerfScenario::StoreReopen) && turn_index > 0 {
            let store = runtime.store();
            let store_factory_before_alloc = allocator_stats();
            let store_factory_before_memory = process_memory_sample();
            let store_factory_started = Instant::now();
            let _core = runtime.core();
            extra_phase_profile.insert(
                "store_reopen.store_factory_create".to_string(),
                RuntimePerfPhaseRunResult {
                    samples: 1,
                    duration_ms: elapsed_ms(store_factory_started),
                    allocations: alloc_delta(store_factory_before_alloc, allocator_stats()),
                    rss_growth_kb: diff_opt_i64(
                        store_factory_before_memory.rss_kb,
                        process_memory_sample().rss_kb,
                    ),
                },
            );

            let load_before_alloc = allocator_stats();
            let load_before_memory = process_memory_sample();
            let load_started = Instant::now();
            let state = lash::persistence::load_persisted_session_state(store.as_ref())
                .await?
                .ok_or_else(|| anyhow::anyhow!("store_reopen expected persisted session state"))?;
            extra_phase_profile.insert(
                "store_reopen.persisted_load".to_string(),
                RuntimePerfPhaseRunResult {
                    samples: 1,
                    duration_ms: elapsed_ms(load_started),
                    allocations: alloc_delta(load_before_alloc, allocator_stats()),
                    rss_growth_kb: diff_opt_i64(
                        load_before_memory.rss_kb,
                        process_memory_sample().rss_kb,
                    ),
                },
            );

            let hydrate_before_alloc = allocator_stats();
            let hydrate_before_memory = process_memory_sample();
            let hydrate_started = Instant::now();
            Box::pin(runtime.reopen_with_state(scenario, state)).await?;
            extra_phase_profile.insert(
                "store_reopen.runtime_hydration".to_string(),
                RuntimePerfPhaseRunResult {
                    samples: 1,
                    duration_ms: elapsed_ms(hydrate_started),
                    allocations: alloc_delta(hydrate_before_alloc, allocator_stats()),
                    rss_growth_kb: diff_opt_i64(
                        hydrate_before_memory.rss_kb,
                        process_memory_sample().rss_kb,
                    ),
                },
            );
        }
        if matches!(scenario, RuntimePerfScenario::SqliteStoreReopen) && turn_index > 0 {
            let reopen_before_alloc = allocator_stats();
            let reopen_before_memory = process_memory_sample();
            let reopen_started = Instant::now();
            runtime.reopen_session(scenario).await?;
            extra_phase_profile.insert(
                "sqlite_store_reopen.runtime_reopen".to_string(),
                RuntimePerfPhaseRunResult {
                    samples: 1,
                    duration_ms: elapsed_ms(reopen_started),
                    allocations: alloc_delta(reopen_before_alloc, allocator_stats()),
                    rss_growth_kb: diff_opt_i64(
                        reopen_before_memory.rss_kb,
                        process_memory_sample().rss_kb,
                    ),
                },
            );
        }
        prepare_turn(&mut runtime, scenario, turn_index).await?;

        let deep_turn_id = matches!(scenario, RuntimePerfScenario::DeepTurnComposition)
            .then(|| format!("runtime-perf-deep-turn-{}", lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string()).0));
        if let Some(turn_id) = deep_turn_id.as_deref() {
            runtime
                .enqueue_active_turn_input(
                    turn_id,
                    TurnInput::text("deep composition ingress marker"),
                    &format!("deep-composition-ingress-{}", turn_index + 1),
                )
                .await?;
        }

        let phase_probe = Arc::new(RuntimePerfPhaseProbe::default());
        runtime.set_turn_phase_probe(phase_probe.clone()).await;

        let before_turn_usage = runtime.usage_report();
        let commit_measurement_start = runtime.store_metrics().commit_measurements().len();
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut turn_input = TurnInput {
            items: vec![InputItem::Text {
                text: benchmark_prompt(scenario, turn_index),
            }],
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: lash_core::TurnContext::default(),
        };
        if matches!(scenario, RuntimePerfScenario::RlmGlobals) {
            turn_input =
                turn_input.rlm_project(rlm_perf_projected_bindings(scenario, turn_index)?)?;
        }
        let cancel = CancellationToken::new();
        let turn = if matches!(scenario, RuntimePerfScenario::ScopedEffectController) {
            let effect_controller = ScopedPerfEffectController;
            let turn_id = format!("runtime-perf-scoped-{}", turn_index + 1);
            let scoped_effect_controller = lash::runtime::ScopedEffectController::borrowed(
                &effect_controller,
                runtime.turn_scope(&turn_id),
            )
            .map_err(anyhow::Error::from)?;
            runtime_perf_timed(
                scenario,
                turn_index,
                "run_turn",
                Some(cancel.clone()),
                runtime.run_turn_with_execution_scope(turn_input, cancel, scoped_effect_controller),
            )
            .await
        } else if matches!(scenario, RuntimePerfScenario::TurnCancelRoundTrip) {
            let turn_id = format!(
                "runtime-perf-cancel-round-trip-{}",
                lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string()).0
            );
            let (turn, duration) = runtime_perf_timed(
                scenario,
                turn_index,
                "run_turn",
                Some(cancel.clone()),
                runtime.run_cancel_round_trip(
                    turn_input,
                    &turn_id,
                    cancel,
                    &format!("runtime-perf-cancel-request-{}", turn_index + 1),
                ),
            )
            .await?;
            extra_phase_profile.insert(
                "turn_cancel.request_to_token_to_seal".to_string(),
                RuntimePerfPhaseRunResult {
                    samples: 1,
                    duration_ms: round3(duration.as_secs_f64() * 1000.0),
                    allocations: zero_allocation_delta(),
                    rss_growth_kb: None,
                },
            );
            Ok(turn)
        } else if matches!(scenario, RuntimePerfScenario::IngressClaimProjection) {
            let turn_id = format!(
                "runtime-perf-ingress-projection-{}",
                lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string()).0
            );
            let (turn, duration) = runtime_perf_timed(
                scenario,
                turn_index,
                "run_turn",
                Some(cancel.clone()),
                runtime.run_ingress_claim_projection(
                    turn_input,
                    &turn_id,
                    cancel,
                    &format!("runtime-perf-ingress-projection-{}", turn_index + 1),
                ),
            )
            .await?;
            extra_phase_profile.insert(
                "turn_input_ingress.enqueue_to_claim_to_projection".to_string(),
                RuntimePerfPhaseRunResult {
                    samples: 1,
                    duration_ms: round3(duration.as_secs_f64() * 1000.0),
                    allocations: zero_allocation_delta(),
                    rss_growth_kb: None,
                },
            );
            Ok(turn)
        } else if let Some(turn_id) = deep_turn_id.as_deref() {
            runtime_perf_timed(
                scenario,
                turn_index,
                "run_turn",
                Some(cancel.clone()),
                runtime.run_turn_with_id(turn_input, turn_id, cancel),
            )
            .await
        } else {
            runtime_perf_timed(
                scenario,
                turn_index,
                "run_turn",
                Some(cancel.clone()),
                runtime.run_turn(turn_input, cancel),
            )
            .await
        }
        .with_context(|| {
            format!(
                "run runtime perf scenario {} turn {}",
                scenario.name(),
                turn_index + 1
            )
        })?;
        if matches!(scenario, RuntimePerfScenario::TurnCancelRoundTrip) {
            if !matches!(turn.outcome, TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })) {
                anyhow::bail!("cancel round-trip turn did not finish cancelled: {:?}", turn.outcome);
            }
        } else {
            validate_runtime_perf_turn(scenario, turn_index, &turn)?;
        }
        let run_turn_ms = elapsed_ms(turn_started);
        let run_turn_alloc = alloc_delta(turn_before_alloc, allocator_stats());
        let after_turn_memory = process_memory_sample();

        let await_before_alloc = allocator_stats();
        let background_started = Instant::now();
        runtime_perf_timed(
            scenario,
            turn_index,
            "await_background_work",
            None,
            runtime.await_background_work(),
        )
        .await
        .with_context(|| {
            format!(
                "await background work for {} turn {}",
                scenario.name(),
                turn_index + 1
            )
        })?;
        let await_background_work_ms = elapsed_ms(background_started);
        let await_background_work_alloc = alloc_delta(await_before_alloc, allocator_stats());
        let after_await_memory = process_memory_sample();
        let turn_total_alloc =
            sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]);

        let cumulative_usage = runtime.usage_report();
        let usage_delta_entries =
            lash_core::facade_support::diff_usage_reports(&before_turn_usage, &cumulative_usage)
                .map_err(anyhow::Error::msg)?;
        let mut phase_profile = phase_probe.take_completed();
        phase_profile.extend(extra_phase_profile);
        if let Some(target_bytes) = scenario.checkpoint_curve_bytes(turn_index) {
            phase_profile = phase_profile
                .into_iter()
                .map(|(phase, measurement)| {
                    (
                        format!("checkpoint_curve.{target_bytes}.{phase}"),
                        measurement,
                    )
                })
                .collect();
            let measurements = runtime.store_metrics().commit_measurements();
            let turn_measurements = &measurements[commit_measurement_start..];
            extra_counters.insert(
                format!("checkpoint_curve.{target_bytes}.commit_count"),
                turn_measurements.len() as u64,
            );
            if let Some(commit) = turn_measurements.last() {
                extra_counters.insert(
                    format!("checkpoint_curve.{target_bytes}.logical_bytes"),
                    commit.total_bytes,
                );
                extra_counters.insert(
                    format!("checkpoint_curve.{target_bytes}.checkpoint_bytes"),
                    commit.checkpoint_bytes,
                );
                extra_counters.insert(
                    format!("checkpoint_curve.{target_bytes}.logical_rows"),
                    commit.total_rows,
                );
                extra_counters.insert(
                    format!("checkpoint_curve.{target_bytes}.graph_rows"),
                    commit.graph_rows,
                );
                extra_counters.insert(
                    format!("checkpoint_curve.{target_bytes}.checkpoint_components"),
                    commit.checkpoint_components,
                );
            }
        }
        turns.push(RuntimePerfTurnResult {
            turn_index,
            run_turn_ms,
            await_background_work_ms,
            total_ms: round3(run_turn_ms + await_background_work_ms),
            memory: RuntimePerfTurnMemoryRunResult {
                rss_before_kb: turn_before_memory.rss_kb,
                rss_after_turn_kb: after_turn_memory.rss_kb,
                rss_after_await_kb: after_await_memory.rss_kb,
                peak_hwm_before_kb: turn_before_memory.hwm_kb,
                peak_hwm_after_await_kb: after_await_memory.hwm_kb,
                rss_growth_kb: diff_opt_i64(turn_before_memory.rss_kb, after_await_memory.rss_kb),
                hwm_growth_kb: diff_opt_i64(turn_before_memory.hwm_kb, after_await_memory.hwm_kb),
            },
            allocations: RuntimePerfTurnAllocationRunResult {
                run_turn: run_turn_alloc,
                await_background_work: await_background_work_alloc,
                total: turn_total_alloc,
            },
            phase_profile,
            turn_usage: turn.usage,
            usage_delta: SessionUsageReport::from_entries(&usage_delta_entries),
            cumulative_usage,
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let state = runtime.export_state().await;
    let cumulative_usage = runtime.usage_report();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let last_turn_memory = turns.last().map(|turn| &turn.memory);
    let store_metrics = runtime.store_metrics();
    extra_counters.extend(store_metrics.call_counters());
    let metric_samples = store_metrics.observed_latency_samples();
    if let Some(commit) = store_metrics.commit_measurements().last() {
        extra_counters.insert("durable_commit.logical_bytes".to_string(), commit.total_bytes);
        extra_counters.insert(
            "durable_commit.checkpoint_bytes".to_string(),
            commit.checkpoint_bytes,
        );
        extra_counters.insert("durable_commit.logical_rows".to_string(), commit.total_rows);
        extra_counters.insert("durable_commit.graph_rows".to_string(), commit.graph_rows);
        extra_counters.insert(
            "durable_commit.checkpoint_components".to_string(),
            commit.checkpoint_components,
        );
    }
    runtime.close().await?;
    if let Some(root) = sqlite_root {
        let _ = std::fs::remove_dir_all(root);
    }

    Ok(RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        build_runtime_ms,
        seed_state_ms,
        run_turn_ms: round3(turns.iter().map(|turn| turn.run_turn_ms).sum()),
        await_background_work_ms: round3(
            turns.iter().map(|turn| turn.await_background_work_ms).sum(),
        ),
        export_state_ms,
        total_ms: elapsed_ms(total_started),
        session_nodes: state.session_graph.nodes.len(),
        active_path_messages: state.read_view().messages().len(),
        extra_counters,
        metric_samples,
        metric_samples_ms: BTreeMap::new(),
        memory: RuntimePerfMemoryRunResult {
            rss_before_kb: before_memory.rss_kb,
            rss_after_build_kb: after_build_memory.rss_kb,
            rss_after_seed_kb: after_seed_memory.rss_kb,
            rss_after_turn_kb: last_turn_memory.and_then(|memory| memory.rss_after_turn_kb),
            rss_after_await_kb: last_turn_memory.and_then(|memory| memory.rss_after_await_kb),
            rss_after_export_kb: after_export_memory.rss_kb,
            peak_hwm_before_kb: before_memory.hwm_kb,
            peak_hwm_after_export_kb: after_export_memory.hwm_kb,
            rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_export_memory.rss_kb),
            hwm_growth_kb: diff_opt_i64(before_memory.hwm_kb, after_export_memory.hwm_kb),
        },
        allocations: RuntimePerfAllocationRunResult {
            build_runtime: build_runtime_alloc,
            seed_state: seed_state_alloc,
            run_turn: sum_allocation_deltas(turns.iter().map(|turn| &turn.allocations.run_turn)),
            await_background_work: sum_allocation_deltas(
                turns
                    .iter()
                    .map(|turn| &turn.allocations.await_background_work),
            ),
            export_state: export_state_alloc,
            total: total_alloc,
        },
        phase_profile: sum_phase_profiles(turns.iter().map(|turn| &turn.phase_profile)),
        turns,
        cumulative_usage,
    })
}

fn configured_postgres_database_url() -> Option<String> {
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .or_else(|| {
            std::env::var("DATABASE_URL")
                .ok()
                .filter(|url| !url.trim().is_empty())
        })
}

fn postgres_is_required() -> bool {
    std::env::var_os("LASH_REQUIRE_POSTGRES").is_some()
}

fn skipped_runtime_perf_result(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> RuntimePerfRunResult {
    let zero_memory = RuntimePerfTurnMemoryRunResult {
        rss_before_kb: None,
        rss_after_turn_kb: None,
        rss_after_await_kb: None,
        peak_hwm_before_kb: None,
        peak_hwm_after_await_kb: None,
        rss_growth_kb: None,
        hwm_growth_kb: None,
    };
    let zero_alloc = zero_allocation_delta();
    let turn = RuntimePerfTurnResult {
        turn_index: 0,
        run_turn_ms: 0.0,
        await_background_work_ms: 0.0,
        total_ms: 0.0,
        memory: zero_memory,
        allocations: RuntimePerfTurnAllocationRunResult {
            run_turn: zero_alloc.clone(),
            await_background_work: zero_alloc.clone(),
            total: zero_alloc.clone(),
        },
        phase_profile: BTreeMap::new(),
        turn_usage: TokenUsage::default(),
        usage_delta: SessionUsageReport::default(),
        cumulative_usage: SessionUsageReport::default(),
    };
    let mut extra_counters = BTreeMap::new();
    extra_counters.insert("skipped.no_database_url".to_string(), 1);
    RuntimePerfRunResult {
        scenario: scenario.name().to_string(),
        scenario_harness: scenario.scenario_harness().name().to_string(),
        chat_turns,
        stack_profile: None,
        build_runtime_ms: 0.0,
        seed_state_ms: 0.0,
        run_turn_ms: 0.0,
        await_background_work_ms: 0.0,
        export_state_ms: 0.0,
        total_ms: 0.0,
        session_nodes: 0,
        active_path_messages: 0,
        extra_counters,
        metric_samples: BTreeMap::new(),
        metric_samples_ms: BTreeMap::new(),
        memory: RuntimePerfMemoryRunResult {
            rss_before_kb: None,
            rss_after_build_kb: None,
            rss_after_seed_kb: None,
            rss_after_turn_kb: None,
            rss_after_await_kb: None,
            rss_after_export_kb: None,
            peak_hwm_before_kb: None,
            peak_hwm_after_export_kb: None,
            rss_growth_kb: None,
            hwm_growth_kb: None,
        },
        allocations: RuntimePerfAllocationRunResult {
            build_runtime: zero_alloc.clone(),
            seed_state: zero_alloc.clone(),
            run_turn: zero_alloc.clone(),
            await_background_work: zero_alloc.clone(),
            export_state: zero_alloc.clone(),
            total: zero_alloc,
        },
        phase_profile: BTreeMap::new(),
        turns: vec![turn],
        cumulative_usage: SessionUsageReport::default(),
    }
}
