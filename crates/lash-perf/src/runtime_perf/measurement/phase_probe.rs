use super::*;
use lash_sansio::TurnId;

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
    deferred_closes: HashSet<String>,
}

#[derive(Default)]
pub(crate) struct RuntimePerfPhaseProbe {
    state: Mutex<RuntimePerfPhaseProbeState>,
}

impl RuntimePerfPhaseProbe {
    fn catalog_observation_stage(&self, warm: bool) -> u8 {
        let state = self.state.lock_recover();
        if state.completed.contains_key("post_commit_delivery") {
            return 2;
        }
        if warm
            && !state.open.contains_key("prepared_turn")
            && !state.completed.contains_key("prepared_turn")
        {
            return 0;
        }
        1
    }

    pub(crate) fn take_completed(&self) -> BTreeMap<String, RuntimePerfPhaseRunResult> {
        let mut state = self.state.lock_recover();
        // Async process phases can still be running at take; leave open spans dropped.
        std::mem::take(&mut state.completed)
    }

    pub(crate) fn open_span_count(&self) -> usize {
        self.state.lock_recover().open.values().map(Vec::len).sum()
    }

    pub(crate) fn take_completed_after_settlement(
        &self,
    ) -> anyhow::Result<BTreeMap<String, RuntimePerfPhaseRunResult>> {
        let mut state = self.state.lock_recover();
        let open_span_count = state.open.values().map(Vec::len).sum::<usize>();
        if open_span_count != 0 {
            anyhow::bail!("async settlement finished with {open_span_count} open phase spans");
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

    fn defer_named_close(&self, phase: &str) {
        let inserted = self
            .state
            .lock_recover()
            .deferred_closes
            .insert(phase.to_string());
        assert!(inserted, "named phase close already deferred: {phase}");
    }

    fn close_deferred_named(&self, phase: &str) {
        let mut state = self.state.lock_recover();
        assert!(
            state.deferred_closes.remove(phase),
            "named phase close was not deferred: {phase}"
        );
        let starts = state
            .open
            .get_mut(phase)
            .unwrap_or_else(|| panic!("deferred named phase never opened: {phase}"));
        let start = starts
            .pop()
            .unwrap_or_else(|| panic!("deferred named phase had no start: {phase}"));
        if starts.is_empty() {
            state.open.remove(phase);
        }
        record_completed_phase(&mut state.completed, phase.to_string(), start);
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
        if state.deferred_closes.contains(phase)
            && state
                .open
                .get(phase)
                .is_some_and(|starts| !starts.is_empty())
        {
            return;
        }
        let start = PhaseStart {
            started_at: Instant::now(),
            alloc_before: allocator_stats(),
            memory_before: process_memory_sample(),
        };
        state.open.entry(phase.to_string()).or_default().push(start);
    }

    fn end_named(&self, phase: &str) {
        let mut state = self.state.lock_recover();
        if state.deferred_closes.contains(phase) {
            return;
        }
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
    contention_workers: usize,
    checkpoint_curve: &CheckpointCurveConfig,
    high_traffic: &HighTrafficConfig,
) -> anyhow::Result<RuntimePerfRunResult> {
    let before_scheduler = RuntimeSchedulerSample::capture();
    let result = Box::pin(run_once_inner(
        scenario,
        chat_turns,
        contention_workers,
        checkpoint_curve,
        high_traffic,
    ))
    .await;
    let after_scheduler = RuntimeSchedulerSample::capture();
    let mut result = result?;
    result
        .metric_samples
        .extend(before_scheduler.window_metric_samples(&after_scheduler));
    Ok(result)
}

/// How a scenario's PostgreSQL requirement resolved.
enum PostgresTarget {
    /// The scenario does not use PostgreSQL.
    NotNeeded,
    /// The scenario uses PostgreSQL and a URL is configured.
    Configured(String),
    /// The scenario uses PostgreSQL, none is configured, and it is not
    /// required: the run reports itself skipped rather than failing.
    Skipped,
}

impl PostgresTarget {
    fn url(&self) -> Option<&str> {
        match self {
            Self::Configured(url) => Some(url.as_str()),
            Self::NotNeeded | Self::Skipped => None,
        }
    }
}

/// The "does this scenario have the database it needs" rule, stated once.
///
/// It used to be written out four times -- the checkpoint-curve branch, the
/// high-traffic branch, the generic dispatch below, and a fourth copy inside
/// `run_once_durable_queued_work_contention` -- with the same bail message and
/// the same skip message in each.
fn resolve_postgres_target(scenario: RuntimePerfScenario) -> anyhow::Result<PostgresTarget> {
    if !scenario.uses_postgres() {
        return Ok(PostgresTarget::NotNeeded);
    }
    if let Some(url) = configured_postgres_database_url() {
        return Ok(PostgresTarget::Configured(url));
    }
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
    Ok(PostgresTarget::Skipped)
}

#[expect(
    clippy::expect_used,
    reason = "the run commits into one active runtime frame scope, so its read view resolves after the run, per the message"
)]
async fn run_once_inner(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    contention_workers: usize,
    checkpoint_curve: &CheckpointCurveConfig,
    high_traffic: &HighTrafficConfig,
) -> anyhow::Result<RuntimePerfRunResult> {
    let postgres = resolve_postgres_target(scenario)?;
    if matches!(postgres, PostgresTarget::Skipped) {
        return Ok(skipped_runtime_perf_result(scenario, chat_turns));
    }

    // One dispatch. The three groups below used to be selected by predicate
    // early-returns above this match, so their membership was stated twice --
    // once in `scenarios.rs` and once as the `unreachable!()` arm this match
    // ended with. Narrowing a predicate compiled green and panicked at run
    // time; now the compiler owns the partition.
    match scenario {
        RuntimePerfScenario::DurableCheckpointCurveSqlite
        | RuntimePerfScenario::DurableCheckpointCurvePostgres => {
            return Box::pin(run_once_durable_checkpoint_curve(
                scenario,
                chat_turns,
                checkpoint_curve,
                postgres.url(),
            ))
            .await;
        }
        RuntimePerfScenario::DurableQueuedWorkContentionSqlite
        | RuntimePerfScenario::DurableQueuedWorkContentionPostgres => {
            return Box::pin(run_once_durable_queued_work_contention(
                scenario,
                chat_turns,
                contention_workers,
                postgres.url(),
            ))
            .await;
        }
        RuntimePerfScenario::HighTrafficLoadSqlite
        | RuntimePerfScenario::HighTrafficLoadPostgres
        | RuntimePerfScenario::HighTrafficKneeSqlite
        | RuntimePerfScenario::HighTrafficKneePostgres => {
            return Box::pin(run_once_high_traffic(
                scenario,
                chat_turns,
                high_traffic,
                postgres.url(),
            ))
            .await;
        }
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
        RuntimePerfScenario::ResidentGraphAppendCurve => {
            return run_once_resident_graph_append_curve(chat_turns).await;
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
            let postgres_database_url = configured_postgres_database_url().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} requires LASH_POSTGRES_DATABASE_URL or DATABASE_URL (the full perf workflows provide it)",
                    scenario.name()
                )
            })?;
            return run_once_store_hardening_hot_paths(chat_turns, &postgres_database_url).await;
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
        | RuntimePerfScenario::RlmToolCatalogCold
        | RuntimePerfScenario::RlmToolCatalogWarm
        | RuntimePerfScenario::RlmObliqueStackMix
        | RuntimePerfScenario::OpenAiCompatStream
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
        | RuntimePerfScenario::DurableAgentChildTurnPostgres => {
            // The generic turn harness below. Every other scenario returned
            // from its own arm, so this list is what "generic" means.
        }
    }

    let postgres_database_url = postgres.url();

    // The runtime-work witness is process-global and exclusive. Durable
    // scenarios are the ones whose commit boundary is worth counting, and the
    // two scenarios with their own collector (the checkpoint curve and the
    // queued-work contention sweep) return before this point, so installing
    // here never contends.
    let work_collector = scenario
        .is_durable()
        .then(lash_core::perf_witness::Collector::install)
        .transpose()?;

    let mut run = RunRecorder::start(scenario, chat_turns);

    let (sqlite_root, mut runtime) = run
        .build(async {
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
                RuntimePerfScenario::RlmTriggerMailPipeline
                    | RuntimePerfScenario::RlmObliqueStackMix
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
            let runtime = if let Some(database_url) = postgres_database_url {
                build_runtime_with_postgres_store(scenario, database_url).await?
            } else if let Some(root) = sqlite_root.as_ref() {
                build_runtime_with_sqlite_store(scenario, root.clone()).await?
            } else {
                build_runtime(scenario, trace_config).await?
            };
            Ok((sqlite_root, runtime))
        })
        .await?;

    run.seed(async { seed_runtime_state(&mut runtime, scenario).await })
        .await?;

    if matches!(scenario, RuntimePerfScenario::RlmToolCatalogWarm) {
        runtime
            .refresh_tool_catalog("runtime-perf-catalog-warm")
            .await?;
        runtime.await_background_work().await?;
    }

    // Both the run and the await closures insert counters while their span
    // is open, so the map is shared through a Mutex rather than borrowed.
    let extra_counters = std::sync::Mutex::new(BTreeMap::new());
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
        let catalog_variant = match scenario {
            RuntimePerfScenario::RlmToolCatalogCold => Some("cold"),
            RuntimePerfScenario::RlmToolCatalogWarm => Some("warm"),
            _ => None,
        };

        let turn_input = TurnInput::text(benchmark_prompt(scenario, turn_index));

        let phase_probe = Arc::new(RuntimePerfPhaseProbe::default());
        runtime.set_turn_phase_probe(phase_probe.clone()).await;

        if matches!(scenario, RuntimePerfScenario::RlmToolCatalogCold) {
            let refresh_key = format!("runtime-perf-catalog-cold-{turn_index}");
            runtime.suppress_tool_catalog_composition_counting();
            runtime.refresh_tool_catalog(&refresh_key).await?;
            runtime.resume_tool_catalog_composition_counting();
        }

        let deep_turn_id =
            matches!(scenario, RuntimePerfScenario::DeepTurnComposition).then(|| {
                format!(
                    "runtime-perf-deep-turn-{}",
                    lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string()).0
                )
            });
        if let Some(turn_id) = deep_turn_id.as_deref() {
            runtime
                .enqueue_active_turn_input(
                    &TurnId::from(turn_id),
                    TurnInput::text("deep composition ingress marker"),
                    &format!("deep-composition-ingress-{}", turn_index + 1),
                )
                .await?;
        }

        let trigger_end_to_end = matches!(scenario, RuntimePerfScenario::RlmTriggerMailPipeline);
        if trigger_end_to_end {
            phase_probe.defer_named_close("trigger.occurrence_to_delivery");
        }

        let before_turn_usage = runtime.usage_report();
        if let Some(variant) = catalog_variant {
            let (manifest_count, rendered_bytes) = runtime.tool_catalog_metrics()?;
            extra_counters.lock_recover().insert(
                format!("tool_catalog.{variant}.registry_manifest_count"),
                manifest_count as u64,
            );
            extra_counters.lock_recover().insert(
                format!("tool_catalog.{variant}.registry_rendered_bytes"),
                rendered_bytes as u64,
            );
            let composition_probe = Arc::clone(&phase_probe);
            let warm = variant == "warm";
            runtime.arm_tool_catalog_observation(
                variant,
                Arc::new(move || composition_probe.catalog_observation_stage(warm)),
            );
        }

        // The run closure moves the turn input in, so pre-bind shared
        // references for everything else it touches; the delivery
        // observation crosses into the await span through the Mutex.
        let trigger_delivery_observation = std::sync::Mutex::new(None);
        let runtime_ref = &runtime;
        let counters_ref = &extra_counters;
        let observation_ref = &trigger_delivery_observation;
        let probe_ref = &phase_probe;
        run.turn_then(
            turn_index,
            async move {
                let runtime = runtime_ref;
                let extra_counters = counters_ref;
                let trigger_delivery_observation = observation_ref;
                let phase_probe = probe_ref;
                let cancel = CancellationToken::new();
                let turn = if matches!(scenario, RuntimePerfScenario::ScopedEffectController) {
                    let turn_id = TurnId::from(format!("runtime-perf-scoped-{}", turn_index + 1));
                    runtime_perf_timed(
                        scenario,
                        turn_index,
                        "run_turn",
                        Some(cancel.clone()),
                        runtime.run_turn_with_execution_scope(turn_input, &turn_id, cancel),
                    )
                    .await
                } else if matches!(scenario, RuntimePerfScenario::TurnCancelRoundTrip) {
                    let turn_id = TurnId::from(format!(
                        "runtime-perf-cancel-round-trip-{}",
                        lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string()).0
                    ));
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
                    let turn_id = TurnId::from(format!(
                        "runtime-perf-ingress-projection-{}",
                        lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string()).0
                    ));
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
                        runtime.run_turn_with_id(turn_input, &TurnId::from(turn_id), cancel),
                    )
                    .await
                } else if trigger_end_to_end {
                    let (turn, observation) = tokio::join!(
                        runtime_perf_timed(
                            scenario,
                            turn_index,
                            "run_turn",
                            Some(cancel.clone()),
                            runtime.run_turn(turn_input, cancel),
                        ),
                        runtime.observe_trigger_delivery_terminals(),
                    );
                    phase_probe.close_deferred_named("trigger.occurrence_to_delivery");
                    *trigger_delivery_observation.lock_recover() = Some(observation?);
                    turn
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
                    if !matches!(
                        turn.outcome,
                        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
                    ) {
                        anyhow::bail!(
                            "cancel round-trip turn did not finish cancelled: {:?}",
                            turn.outcome
                        );
                    }
                } else {
                    validate_runtime_perf_turn(scenario, turn_index, &turn)?;
                }
                if let Some(variant) = catalog_variant {
                    let observation = runtime.finish_tool_catalog_observation();
                    extra_counters.lock_recover().insert(
                        format!("tool_catalog.{variant}.cache_state"),
                        observation.cache_state,
                    );
                    extra_counters.lock_recover().insert(
                        format!("tool_catalog.{variant}.setup_recomposition_count"),
                        observation.setup_recomposition_count,
                    );
                    extra_counters.lock_recover().insert(
                        format!("tool_catalog.{variant}.recomposition_count"),
                        observation.recomposition_count,
                    );
                }
                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        phase_profile: std::mem::take(&mut extra_phase_profile),
                        turn_usage: turn.usage,
                        ..TurnTail::default()
                    },
                })
            },
            async {
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
                if trigger_end_to_end {
                    let observation = trigger_delivery_observation
                        .lock_recover()
                        .take()
                        .context("trigger delivery observation was not collected")?;
                    extra_counters.lock_recover().insert(
                        "trigger.delivery_process_count".to_string(),
                        observation.process_count,
                    );
                    extra_counters.lock_recover().insert(
                        "trigger.delivery_durable_claim_count".to_string(),
                        observation.durable_claim_count,
                    );
                    extra_counters.lock_recover().insert(
                        "trigger.delivery_terminal_count".to_string(),
                        observation.terminal_count,
                    );
                }
                Ok(())
            },
            |_, _, tail| {
                let cumulative_usage = runtime.usage_report();
                let usage_delta_entries = lash_core::facade_support::diff_usage_reports(
                    &before_turn_usage,
                    &cumulative_usage,
                )
                .map_err(anyhow::Error::msg)?;
                let mut phase_profile = phase_probe.take_completed();
                phase_profile.extend(std::mem::take(&mut tail.phase_profile));
                tail.phase_profile = phase_profile;
                tail.usage_delta = SessionUsageReport::from_entries(&usage_delta_entries);
                tail.cumulative_usage = cumulative_usage;
                Ok(())
            },
        )
        .await?;
    }

    let (state, cumulative_usage) = run
        .export(async {
            let state = runtime.export_state().await;
            let cumulative_usage = runtime.usage_report();
            Ok((state, cumulative_usage))
        })
        .await?;
    let store_metrics = runtime.store_metrics();
    if let Some(collector) = work_collector {
        let work = collector.snapshot();
        drop(collector);
        store_metrics.record_pool_checkout_waits(work.pool_checkout_wait_nanos);
        for (name, value) in [
            ("runtime_work.hash_passes", work.hash_passes),
            ("runtime_work.hashed_bytes", work.hashed_bytes),
            ("runtime_work.body_copy_passes", work.body_copy_passes),
            ("runtime_work.copied_bytes", work.copied_bytes),
        ] {
            extra_counters
                .lock_recover()
                .insert(name.to_string(), value);
        }
        // Only SQLite carries the statement witness today; emitting a zero for
        // PostgreSQL would read as "no statements" rather than "not observed".
        if !scenario.uses_postgres() {
            extra_counters.lock_recover().insert(
                "runtime_work.sql_statements".to_string(),
                work.sql_statements,
            );
            for (verb, count) in work.sql_statements_by_verb {
                extra_counters
                    .lock_recover()
                    .insert(format!("runtime_work.sql_statements.{verb}"), count);
            }
        }
    }
    extra_counters
        .lock_recover()
        .extend(store_metrics.call_counters());
    let metric_samples = store_metrics.observed_latency_samples();
    let mut metric_samples_ms = BTreeMap::new();
    let pool_checkout_wait_ms = store_metrics.pool_checkout_wait_samples_ms();
    if !pool_checkout_wait_ms.is_empty() {
        metric_samples_ms.insert(
            "store.pool_checkout_wait_ms".to_string(),
            pool_checkout_wait_ms,
        );
    }
    if let Some(commit) = store_metrics.commit_measurements().last() {
        extra_counters.lock_recover().insert(
            "durable_commit.logical_bytes".to_string(),
            commit.total_bytes,
        );
        extra_counters.lock_recover().insert(
            "durable_commit.checkpoint_bytes".to_string(),
            commit.checkpoint_bytes,
        );
        extra_counters
            .lock_recover()
            .insert("durable_commit.logical_rows".to_string(), commit.total_rows);
        extra_counters
            .lock_recover()
            .insert("durable_commit.graph_rows".to_string(), commit.graph_rows);
        extra_counters.lock_recover().insert(
            "durable_commit.checkpoint_components".to_string(),
            commit.checkpoint_components,
        );
    }
    runtime.close().await?;
    if let Some(root) = sqlite_root {
        let _ = std::fs::remove_dir_all(root);
    }

    Ok(run.finish(RunTail {
        session_nodes: state.session_graph.nodes.len(),
        active_path_messages: state
            .read_view()
            .expect("runtime frame scope resolves")
            .messages()
            .len(),
        extra_counters: std::mem::take(&mut extra_counters.lock_recover()),
        metric_samples,
        metric_samples_ms,
        cumulative_usage,
        ..RunTail::default()
    }))
}

pub(super) fn configured_postgres_database_url() -> Option<String> {
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .or_else(|| {
            std::env::var("DATABASE_URL")
                .ok()
                .filter(|url| !url.trim().is_empty())
        })
}

pub(super) fn postgres_is_required() -> bool {
    std::env::var_os("LASH_REQUIRE_POSTGRES").is_some()
}

pub(crate) fn skipped_runtime_perf_result(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> RuntimePerfRunResult {
    let empty_memory = RuntimePerfMemoryRunResult {
        rss_before_kb: None,
        peak_hwm_before_kb: None,
        peak_hwm_after_kb: None,
        rss_growth_kb: None,
        hwm_growth_kb: None,
    };
    // A skipped run measured nothing: every stage map stays empty so no
    // stage — `total` included — emits a fake zero into downstream
    // aggregations or the duration-trend history.
    let turn = RuntimePerfTurnResult {
        turn_index: 0,
        stages: BTreeMap::new(),
        memory: empty_memory.clone(),
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
        stages: BTreeMap::new(),
        session_nodes: 0,
        active_path_messages: 0,
        extra_counters,
        metric_samples: BTreeMap::new(),
        metric_samples_ms: BTreeMap::new(),
        memory: empty_memory,
        phase_profile: BTreeMap::new(),
        turns: vec![turn],
        cumulative_usage: SessionUsageReport::default(),
    }
}
