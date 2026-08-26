fn measure_runtime_perf_phase<T>(
    name: &'static str,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<(T, (String, RuntimePerfPhaseRunResult))> {
    let before_alloc = allocator_stats();
    let before_memory = process_memory_sample();
    let started = Instant::now();
    let value = f()?;
    let after_alloc = allocator_stats();
    let after_memory = process_memory_sample();
    Ok((
        value,
        (
            name.to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(started),
                allocations: alloc_delta(before_alloc, after_alloc),
                rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_memory.rss_kb),
            },
        ),
    ))
}

async fn measure_runtime_perf_async_phase<T, F>(
    name: &'static str,
    future: F,
) -> anyhow::Result<(T, (String, RuntimePerfPhaseRunResult))>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let before_alloc = allocator_stats();
    let before_memory = process_memory_sample();
    let started = Instant::now();
    let value = future.await?;
    let after_alloc = allocator_stats();
    let after_memory = process_memory_sample();
    Ok((
        value,
        (
            name.to_string(),
            RuntimePerfPhaseRunResult {
                samples: 1,
                duration_ms: elapsed_ms(started),
                allocations: alloc_delta(before_alloc, after_alloc),
                rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_memory.rss_kb),
            },
        ),
    ))
}

async fn run_once_turn_checkpoint(chat_turns: usize) -> anyhow::Result<RuntimePerfRunResult> {
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let configs = CheckpointConfigs::new();
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    let seed_messages = checkpoint_messages();
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    for turn_index in 0..chat_turns {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();

        let llm_phase = measure_checkpoint_phase("standard_llm_checkpoint", || {
            checkpoint_pending_llm(&configs, &seed_messages, turn_index)
        })?;
        phase_profile.insert(llm_phase.0, llm_phase.1);

        let tools_phase = measure_checkpoint_phase("standard_parallel_tools_checkpoint", || {
            checkpoint_pending_parallel_tools(&configs, &seed_messages, turn_index)
        })?;
        phase_profile.insert(tools_phase.0, tools_phase.1);

        let exec_phase = measure_checkpoint_phase("rlm_exec_checkpoint", || {
            checkpoint_pending_exec(&configs, &seed_messages, turn_index)
        })?;
        phase_profile.insert(exec_phase.0, exec_phase.1);

        let run_turn_ms = elapsed_ms(turn_started);
        let run_turn_alloc = alloc_delta(turn_before_alloc, allocator_stats());
        let after_turn_memory = process_memory_sample();

        let await_before_alloc = allocator_stats();
        let background_started = Instant::now();
        tokio::task::yield_now().await;
        let await_background_work_ms = elapsed_ms(background_started);
        let await_background_work_alloc = alloc_delta(await_before_alloc, allocator_stats());
        let after_await_memory = process_memory_sample();
        let turn_total_alloc =
            sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]);

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
            turn_usage: TokenUsage::default(),
            usage_delta: SessionUsageReport::default(),
            cumulative_usage: SessionUsageReport::default(),
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    serde_json::to_vec(&seed_messages)?;
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let last_turn_memory = turns.last().map(|turn| &turn.memory);

    Ok(RuntimePerfRunResult {
        scenario: RuntimePerfScenario::TurnCheckpoint.name().to_string(),
        scenario_harness: RuntimePerfScenario::TurnCheckpoint
            .scenario_harness()
            .name()
            .to_string(),
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
        session_nodes: seed_messages.len(),
        active_path_messages: seed_messages.len(),
        extra_counters: BTreeMap::new(),
        metric_samples: BTreeMap::new(),
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
        cumulative_usage: SessionUsageReport::default(),
    })
}

const CHECKPOINT_STATE_BINDINGS: usize = 300;
const CHECKPOINT_STATE_BODY_BYTES: usize = 3 * 1024 + 512;
const CHECKPOINT_CURVE_SCALE: usize = 4;
const CHECKPOINT_CURVE_COMMIT_BYTES: usize = 32 * 1024 * 1024;
const CHECKPOINT_CURVE_COMMIT_ROWS: usize = 8_192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckpointCurveAxis {
    Components,
    Bytes,
}

impl CheckpointCurveAxis {
    fn name(self) -> &'static str {
        match self {
            Self::Components => "components",
            Self::Bytes => "bytes",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CheckpointCurvePoint {
    pub(crate) axis: CheckpointCurveAxis,
    pub(crate) target: usize,
    pub(crate) transcript_bytes: usize,
    pub(crate) message_count: usize,
    pub(crate) graph_rows: usize,
    pub(crate) component_count: usize,
}

impl CheckpointCurvePoint {
    pub(crate) fn prefix(self) -> String {
        format!("checkpoint_curve.{}.{}", self.axis.name(), self.target)
    }
}

pub(crate) fn checkpoint_curve_points(config: &CheckpointCurveConfig) -> Vec<CheckpointCurvePoint> {
    // Runtime scenarios accept scalar CLI parameters rather than a Cartesian
    // sweep. Derive one small, stable three-point curve on each axis around
    // those configured center values so both axes remain paired in one run.
    let component_targets = [
        config.component_count / CHECKPOINT_CURVE_SCALE,
        config.component_count,
        config
            .component_count
            .saturating_mul(CHECKPOINT_CURVE_SCALE),
    ];
    let byte_targets = [
        config.transcript_bytes / CHECKPOINT_CURVE_SCALE,
        config.transcript_bytes,
        config
            .transcript_bytes
            .saturating_mul(CHECKPOINT_CURVE_SCALE),
    ];
    component_targets
        .into_iter()
        .map(|component_count| CheckpointCurvePoint {
            axis: CheckpointCurveAxis::Components,
            target: component_count,
            transcript_bytes: config.transcript_bytes,
            message_count: config.message_count,
            graph_rows: config.graph_rows,
            component_count,
        })
        .chain(
            byte_targets
                .into_iter()
                .map(|transcript_bytes| CheckpointCurvePoint {
                    axis: CheckpointCurveAxis::Bytes,
                    target: transcript_bytes,
                    transcript_bytes,
                    message_count: config.message_count,
                    graph_rows: config.graph_rows,
                    component_count: config.component_count,
                }),
        )
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CheckpointArtifactShape {
    manifest_count: u64,
    changed_body_count: u64,
    changed_body_bytes: u64,
}

impl CheckpointArtifactShape {
    fn from_snapshot(snapshot: &lash_core::plugin::ExecutionStateSnapshot) -> Self {
        let root = snapshot.root.as_ref();
        let changed_bodies = snapshot
            .components
            .values()
            .filter_map(|component| match component {
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => Some(body),
                lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => None,
            })
            .collect::<Vec<_>>();
        Self {
            manifest_count: snapshot.components.len() as u64 + u64::from(root.is_some()),
            changed_body_count: changed_bodies.len() as u64 + u64::from(root.is_some()),
            changed_body_bytes: changed_bodies
                .iter()
                .map(|body| body.len() as u64)
                .sum::<u64>()
                + root.map_or(0, |body| body.len() as u64),
        }
    }

    fn from_commit(commit: &RuntimeCommit) -> Self {
        let changed = commit
            .checkpoint
            .components
            .values()
            .filter_map(lash_core::HydratedCheckpointComponent::body)
            .collect::<Vec<_>>();
        Self {
            manifest_count: commit.checkpoint.components.len() as u64,
            changed_body_count: changed.len() as u64,
            changed_body_bytes: changed.iter().map(|body| body.len() as u64).sum(),
        }
    }
}

#[cfg(test)]
pub(crate) const CHECKPOINT_HASH_PASSES_PER_CHANGED_BODY_FLOOR: u64 = 5;

struct DurableCheckpointCurveFixture {
    point: CheckpointCurvePoint,
    fixture: lash_protocol_rlm::RlmCheckpointPerfFixture,
    runtime_state: RuntimeSessionState,
    store: Arc<dyn lash_core::RuntimePersistence>,
}

async fn run_once_durable_checkpoint_curve(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    config: &CheckpointCurveConfig,
) -> anyhow::Result<RuntimePerfRunResult> {
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();
    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let sqlite_root = (!scenario.uses_postgres())
        .then(|| make_temp_bench_dir(&format!("lash-runtime-perf-{}", scenario.name())))
        .transpose()?;
    let (store_factory, store_metrics) = if scenario.uses_postgres() {
        let Some(database_url) = configured_postgres_database_url() else {
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
        };
        let postgres = lash_postgres_store::PostgresStorage::connect(&database_url)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        durable_postgres_session_store_factory_without_commit_measurement(&postgres)
    } else {
        let root = sqlite_root.as_ref().expect("SQLite checkpoint curve root");
        durable_sqlite_session_store_factory_without_commit_measurement(
            root.join("sessions"),
            root.join("processes.db").as_path(),
        )
    };
    let points = checkpoint_curve_points(config);
    let run_id = uuid::Uuid::new_v4();
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    let mut fixtures = Vec::with_capacity(points.len());
    for point in points {
        let session_id = format!(
            "runtime-perf-{}-{run_id}-{}-{}",
            scenario.name(),
            point.axis.name(),
            point.target
        );
        let store = store_factory
            .create_store(&runtime_perf_session_create_request(&session_id))
            .await?;
        store
            .admit_and_bind_session(&lash_core::SessionBinding::root(session_id.clone()))
            .await?;
        let mut fixture = lash_protocol_rlm::RlmCheckpointPerfFixture::new(
            point.component_count,
            point.transcript_bytes,
        )?;
        let initial_snapshot = fixture.capture()?;
        let initial_shape = CheckpointArtifactShape::from_snapshot(&initial_snapshot);
        if initial_shape.manifest_count != point.component_count as u64 + 1 {
            anyhow::bail!(
                "{} seeded manifest count {}, expected {}",
                point.prefix(),
                initial_shape.manifest_count,
                point.component_count + 1
            );
        }
        let mut runtime_state = RuntimeSessionState {
            session_id,
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        lash_core::testing::stage_execution_state_components(&mut runtime_state, initial_snapshot)?;
        let seed_commit = RuntimeCommit::persisted_state_for_test_with_budget(
            &runtime_state,
            &[],
            lash_core::CommitBudget::bounded(
                CHECKPOINT_CURVE_COMMIT_BYTES,
                CHECKPOINT_CURVE_COMMIT_ROWS,
            ),
        );
        let seed_result = store.commit_runtime_state(seed_commit).await?;
        runtime_state.apply_persisted_commit_result(seed_result);
        fixture.acknowledge_capture();
        fixtures.push(DurableCheckpointCurveFixture {
            point,
            fixture,
            runtime_state,
            store,
        });
    }
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut extra_counters = BTreeMap::new();
    let mut metric_samples = BTreeMap::<String, Vec<f64>>::new();
    let mut metric_samples_ms = BTreeMap::<String, Vec<f64>>::new();
    for sample in 0..chat_turns {
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();
        for fixture in &mut fixtures {
            append_checkpoint_curve_graph(&mut fixture.runtime_state, fixture.point, sample);
            let prefix = fixture.point.prefix();
            let work_collector = lash_core::perf_witness::Collector::install()?;
            let (snapshot, capture_phase) =
                measure_runtime_perf_async_phase("checkpoint_curve.capture", async {
                    fixture.fixture.assign_one(sample, sample).await?;
                    fixture.fixture.absorb_dirty_assignments();
                    fixture.fixture.capture().map_err(anyhow::Error::from)
                })
                .await?;
            let snapshot_shape = CheckpointArtifactShape::from_snapshot(&snapshot);
            let (serialized, serialize_phase) = measure_runtime_perf_phase(
                "checkpoint_curve.serialize",
                || {
                    lash_core::testing::stage_execution_state_components(
                        &mut fixture.runtime_state,
                        snapshot,
                    )?;
                    let commit = RuntimeCommit::persisted_state_for_test_with_budget(
                        &fixture.runtime_state,
                        &[],
                        lash_core::CommitBudget::bounded(
                            CHECKPOINT_CURVE_COMMIT_BYTES,
                            CHECKPOINT_CURVE_COMMIT_ROWS,
                        ),
                    );
                    let commit_shape = CheckpointArtifactShape::from_commit(&commit);
                    if commit_shape != snapshot_shape {
                        anyhow::bail!(
                            "{prefix} snapshot/commit shape diverged: {snapshot_shape:?} versus {commit_shape:?}"
                        );
                    }
                    commit.turn_commit_hash()?;
                    if commit.graph.nodes.len() != fixture.point.graph_rows {
                        anyhow::bail!(
                            "{prefix} committed {} graph rows, expected {}",
                            commit.graph.nodes.len(),
                            fixture.point.graph_rows
                        );
                    }
                    Ok((commit, commit_shape))
                },
            )?;
            let (commit, shape) = serialized;
            let (_, commit_phase) =
                measure_runtime_perf_async_phase("checkpoint_curve.commit", async {
                    fixture
                        .store
                        .commit_runtime_state(commit)
                        .await
                        .map_err(anyhow::Error::from)
                })
                .await?;
            fixture.fixture.acknowledge_capture();
            let (loaded_state, load_phase) =
                measure_runtime_perf_async_phase("checkpoint_curve.load", async {
                    let loaded_state =
                        lash::persistence::load_persisted_session_state(fixture.store.as_ref())
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("{prefix} commit was not loadable"))?;
                    let execution_state = loaded_state
                        .execution_state_hydration()?
                        .ok_or_else(|| anyhow::anyhow!("{prefix} load omitted execution state"))?;
                    lash_protocol_rlm::RlmCheckpointPerfFixture::restore(&execution_state)
                        .map_err(anyhow::Error::from)?;
                    Ok(loaded_state)
                })
                .await?;
            fixture.runtime_state = loaded_state;
            let work = work_collector.snapshot();
            drop(work_collector);

            for (name, phase) in [
                ("capture", capture_phase.1),
                ("serialize", serialize_phase.1),
                ("commit", commit_phase.1),
                ("load", load_phase.1),
            ] {
                metric_samples_ms
                    .entry(format!("{prefix}.{name}_ms"))
                    .or_default()
                    .push(phase.duration_ms);
                phase_profile.insert(format!("{prefix}.{name}"), phase);
            }
            let counts = [
                ("manifest_count", shape.manifest_count),
                ("changed_body_count", shape.changed_body_count),
                ("changed_body_bytes", shape.changed_body_bytes),
                ("runtime_hash_count", work.hash_passes),
                ("runtime_hash_bytes", work.hashed_bytes),
                ("runtime_body_copy_count", work.body_copy_passes),
                ("runtime_body_copy_bytes", work.copied_bytes),
            ];
            for (name, value) in counts {
                metric_samples
                    .entry(format!("{prefix}.{name}"))
                    .or_default()
                    .push(value as f64);
                extra_counters.insert(format!("{prefix}.{name}"), value);
            }
            for (name, value) in [
                ("transcript_bytes", fixture.point.transcript_bytes),
                ("message_count", fixture.point.message_count),
                ("graph_rows", fixture.point.graph_rows),
                ("component_count", fixture.point.component_count),
                ("samples", chat_turns),
            ] {
                extra_counters.insert(format!("{prefix}.{name}"), value as u64);
            }
        }
        let run_turn_ms = elapsed_ms(turn_started);
        let run_turn_alloc = alloc_delta(turn_before_alloc, allocator_stats());
        let after_turn_memory = process_memory_sample();
        let await_before_alloc = allocator_stats();
        let background_started = Instant::now();
        tokio::task::yield_now().await;
        let await_background_work_ms = elapsed_ms(background_started);
        let await_background_work_alloc = alloc_delta(await_before_alloc, allocator_stats());
        let after_await_memory = process_memory_sample();
        turns.push(RuntimePerfTurnResult {
            turn_index: sample,
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
                run_turn: run_turn_alloc.clone(),
                await_background_work: await_background_work_alloc.clone(),
                total: sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]),
            },
            phase_profile,
            turn_usage: TokenUsage::default(),
            usage_delta: SessionUsageReport::default(),
            cumulative_usage: SessionUsageReport::default(),
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    extra_counters.extend(store_metrics.call_counters());
    extra_counters.insert(
        "checkpoint_curve.point_count".to_string(),
        fixtures.len() as u64,
    );
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    drop(fixtures);
    drop(store_factory);
    if let Some(root) = sqlite_root {
        let _ = std::fs::remove_dir_all(root);
    }
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let last_turn_memory = turns.last().map(|turn| &turn.memory);

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
        session_nodes: config.graph_rows,
        active_path_messages: config.message_count,
        extra_counters,
        metric_samples,
        metric_samples_ms,
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
        cumulative_usage: SessionUsageReport::default(),
    })
}

fn append_checkpoint_curve_graph(
    state: &mut RuntimeSessionState,
    point: CheckpointCurvePoint,
    sample: usize,
) {
    let adds_initial_frame = state.current_frame_node_id.is_none();
    let base_bytes = point.transcript_bytes / point.message_count;
    let remainder = point.transcript_bytes % point.message_count;
    let messages = (0..point.message_count)
        .map(|index| {
            let body_bytes = base_bytes + usize::from(index < remainder);
            checkpoint_message(
                format!("checkpoint-curve-{sample}-message-{index}"),
                if index.is_multiple_of(2) {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                checkpoint_curve_ascii(body_bytes, sample ^ index),
            )
        })
        .collect::<Vec<_>>();
    state.append_active_conversation_messages(&messages);
    let rows_already_appended = point.message_count + usize::from(adds_initial_frame);
    for index in rows_already_appended..point.graph_rows {
        state.session_graph.append_plugin(
            "checkpoint_curve",
            serde_json::json!({"sample": sample, "row": index}),
        );
    }
}

fn checkpoint_curve_ascii(len: usize, seed: usize) -> String {
    // Keep the commit-size benchmark's deterministic printable-payload shape:
    // logical byte targets stay exact without compression-friendly zero fill.
    let bytes = (0..len)
        .map(|index| b'!' + ((index.wrapping_add(seed)) % 94) as u8)
        .collect::<Vec<_>>();
    String::from_utf8(bytes).expect("checkpoint curve generator emits ASCII")
}

async fn run_once_checkpoint_state_hot_paths(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let scenario = RuntimePerfScenario::CheckpointStateHotPaths;
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let mut fixture = lash_protocol_rlm::RlmCheckpointPerfFixture::new(
        CHECKPOINT_STATE_BINDINGS,
        CHECKPOINT_STATE_BODY_BYTES,
    )?;
    let store = lash_core::runtime::InMemorySessionStore::new();
    let mut runtime_state = RuntimeSessionState {
        session_id: "runtime-perf-checkpoint-state".to_string(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(
            runtime_state.session_id.clone(),
        ))
        .await?;
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    let (initial_snapshot, initial_capture_phase) =
        measure_runtime_perf_phase("checkpoint_state.initial_capture", || {
            fixture.capture().map_err(anyhow::Error::from)
        })?;
    if initial_snapshot.root.is_none() {
        anyhow::bail!("checkpoint-state fixture omitted its root");
    }
    let initial_component_count = changed_execution_state_components(&initial_snapshot)?.len();
    if initial_component_count != CHECKPOINT_STATE_BINDINGS {
        anyhow::bail!(
            "checkpoint-state fixture captured {initial_component_count} components, expected {CHECKPOINT_STATE_BINDINGS}"
        );
    }
    lash_core::testing::stage_execution_state_components(
        &mut runtime_state,
        initial_snapshot.clone(),
    )?;
    let initial_commit = RuntimeCommit::persisted_state_for_test_with_budget(
        &runtime_state,
        &[],
        lash_core::CommitBudget::bounded(8 * 1024 * 1024, 2_048),
    );
    let initial_result = store.commit_runtime_state(initial_commit).await?;
    runtime_state.apply_persisted_commit_result(initial_result);
    fixture.acknowledge_capture();
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    let mut last_checkpoint_bytes = 0_u64;
    let mut last_changed_components = 0_u64;
    let mut last_hydrated_bytes = 0_u64;
    for turn_index in 0..chat_turns {
        fixture.assign_one(turn_index, turn_index).await?;
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let mut phase_profile = BTreeMap::new();
        if turn_index == 0 {
            phase_profile.insert(
                initial_capture_phase.0.clone(),
                initial_capture_phase.1.clone(),
            );
        }

        let (_, phase) =
            measure_runtime_perf_phase("checkpoint_state.dirty_binding_update", || {
                fixture.absorb_dirty_assignments();
                Ok(())
            })?;
        phase_profile.insert(phase.0, phase.1);

        let (snapshot, phase) =
            measure_runtime_perf_phase("checkpoint_state.incremental_capture", || {
                fixture.capture().map_err(anyhow::Error::from)
            })?;
        phase_profile.insert(phase.0, phase.1);
        fixture.acknowledge_capture();
        if snapshot.root.is_none() {
            anyhow::bail!("incremental checkpoint capture omitted its root");
        }
        last_changed_components = snapshot
            .components
            .values()
            .filter(|component| {
                matches!(
                    component,
                    lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                )
            })
            .count() as u64;
        if last_changed_components != 1 {
            anyhow::bail!(
                "incremental checkpoint captured {last_changed_components} changed components, expected 1"
            );
        }
        lash_core::testing::stage_execution_state_components(&mut runtime_state, snapshot)?;
        let commit = RuntimeCommit::persisted_state_for_test_with_budget(
            &runtime_state,
            &[],
            lash_core::CommitBudget::bounded(8 * 1024 * 1024, 2_048),
        );

        let (budget_measurement, phase) =
            measure_runtime_perf_phase("checkpoint_state.measure_budget", || {
                lash_core::testing::measure_runtime_commit_budget(&commit)
                    .map_err(anyhow::Error::from)
            })?;
        phase_profile.insert(phase.0, phase.1);
        last_checkpoint_bytes = budget_measurement.checkpoint_bytes as u64;

        let (commit_result, phase) =
            measure_runtime_perf_async_phase("checkpoint_state.component_commit", async {
                store
                    .commit_runtime_state(commit)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        runtime_state.apply_persisted_commit_result(commit_result);

        let (loaded_execution_state, phase) =
            measure_runtime_perf_async_phase("checkpoint_state.component_load", async {
                let persisted = store
                    .load_session()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("checkpoint-state commit was not loadable"))?;
                execution_state_from_checkpoint(
                    persisted
                        .checkpoint
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("loaded session omitted checkpoint"))?,
                )
            })
            .await?;
        phase_profile.insert(phase.0, phase.1);
        last_hydrated_bytes = (loaded_execution_state.root.len()
            + loaded_execution_state
                .components
                .values()
                .map(Vec::len)
                .sum::<usize>()) as u64;

        let (_, phase) = measure_runtime_perf_phase("checkpoint_state.execution_restore", || {
            lash_protocol_rlm::RlmCheckpointPerfFixture::restore(&loaded_execution_state)
                .map_err(anyhow::Error::from)
        })?;
        phase_profile.insert(phase.0, phase.1);

        let run_turn_ms = elapsed_ms(turn_started);
        let run_turn_alloc = alloc_delta(turn_before_alloc, allocator_stats());
        let after_turn_memory = process_memory_sample();
        let await_before_alloc = allocator_stats();
        let background_started = Instant::now();
        tokio::task::yield_now().await;
        let await_background_work_ms = elapsed_ms(background_started);
        let await_background_work_alloc = alloc_delta(await_before_alloc, allocator_stats());
        let after_await_memory = process_memory_sample();

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
                run_turn: run_turn_alloc.clone(),
                await_background_work: await_background_work_alloc.clone(),
                total: sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]),
            },
            phase_profile,
            turn_usage: TokenUsage::default(),
            usage_delta: SessionUsageReport::default(),
            cumulative_usage: SessionUsageReport::default(),
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let last_turn_memory = turns.last().map(|turn| &turn.memory);
    let mut extra_counters = BTreeMap::new();
    extra_counters.insert(
        "execution_state_bindings".to_string(),
        CHECKPOINT_STATE_BINDINGS as u64,
    );
    extra_counters.insert(
        "execution_state_components".to_string(),
        initial_component_count as u64,
    );
    extra_counters.insert(
        "incremental_changed_components".to_string(),
        last_changed_components,
    );
    extra_counters.insert("checkpoint_bytes".to_string(), last_checkpoint_bytes);
    extra_counters.insert(
        "hydrated_execution_state_bytes".to_string(),
        last_hydrated_bytes,
    );

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
        session_nodes: 0,
        active_path_messages: 0,
        extra_counters,
        metric_samples: BTreeMap::new(),
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
        cumulative_usage: SessionUsageReport::default(),
    })
}

fn changed_execution_state_components(
    snapshot: &lash_core::plugin::ExecutionStateSnapshot,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    snapshot
        .components
        .iter()
        .map(|(key, component)| match component {
            lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => {
                Ok((key.clone(), body.clone()))
            }
            lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                anyhow::bail!("initial checkpoint-state component `{key}` was unchanged")
            }
        })
        .collect()
}

fn execution_state_from_checkpoint(
    checkpoint: &lash_core::HydratedSessionCheckpoint,
) -> anyhow::Result<lash_core::plugin::HydratedExecutionState> {
    let root = checkpoint
        .component_body(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        .ok_or_else(|| anyhow::anyhow!("hydrated checkpoint omitted execution-state root"))?
        .to_vec();
    let mut components = BTreeMap::new();
    for key in checkpoint.components.keys() {
        if !key.starts_with("execution_state/") {
            continue;
        }
        let body = checkpoint
            .component_body(key)
            .ok_or_else(|| anyhow::anyhow!("hydrated checkpoint omitted body for `{key}`"))?;
        components.insert(key.clone(), body.to_vec());
    }
    Ok(lash_core::plugin::HydratedExecutionState { root, components })
}

struct CheckpointConfigs {
    llm: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>>,
    tools: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>>,
    exec: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>>,
}

impl CheckpointConfigs {
    fn new() -> Self {
        Self {
            llm: Arc::new(CheckpointDriver::Llm),
            tools: Arc::new(CheckpointDriver::Tools),
            exec: Arc::new(CheckpointDriver::Exec),
        }
    }

    fn llm_config(&self) -> TurnMachineConfig {
        checkpoint_config(Arc::clone(&self.llm))
    }

    fn tools_config(&self) -> TurnMachineConfig {
        checkpoint_config(Arc::clone(&self.tools))
    }

    fn exec_config(&self) -> TurnMachineConfig {
        checkpoint_config(Arc::clone(&self.exec))
    }
}

#[derive(Clone, Copy)]
enum CheckpointDriver {
    Llm,
    Tools,
    Exec,
}

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for CheckpointDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        match self {
            Self::Llm => vec![DriverAction::StartLlm {
                request: ctx.project_llm_request(false),
                driver_state: None,
            }],
            Self::Tools => vec![DriverAction::StartTools {
                calls: checkpoint_tool_calls(ctx.protocol_iteration()),
            }],
            Self::Exec => vec![DriverAction::StartExec {
                language: "code".to_string(),
                code: checkpoint_exec_code(ctx.protocol_iteration()),
                driver_state: lash_core::ProtocolDriverState::new(
                    "runtime_perf_checkpoint",
                    serde_json::json!({
                        "phase": "exec_code",
                        "ip": ctx.protocol_iteration(),
                        "stack": (0..64).map(|index| serde_json::json!({
                            "slot": index,
                            "value": format!("checkpoint-stack-value-{index}")
                        })).collect::<Vec<_>>(),
                    }),
                ),
            }],
        }
    }

    fn handle_llm_success(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingLlmState<lash_core::HostTurnProtocol>,
        _llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        vec![DriverAction::Finish(TurnOutcome::Finished(
            TurnFinish::AssistantMessage {
                text: "runtime perf benchmark ok".to_string(),
            },
        ))]
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        vec![DriverAction::Finish(TurnOutcome::Finished(
            TurnFinish::AssistantMessage {
                text: "runtime perf benchmark ok".to_string(),
            },
        ))]
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingExecState<lash_core::HostTurnProtocol>,
        _result: Result<ExecResponse, String>,
    ) -> Vec<DriverAction> {
        vec![DriverAction::Finish(TurnOutcome::Finished(
            TurnFinish::FinalValue {
                value: serde_json::json!("runtime perf benchmark ok"),
            },
        ))]
    }
}

fn checkpoint_config(
    protocol_driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>>,
) -> TurnMachineConfig {
    TurnMachineConfig {
        protocol_driver,
        projector: Arc::new(ChatContextProjector),
        sync_execution_environment: false,
        model: "mock-model".to_string(),
        max_context_tokens: None,
        turn_budget: lash_core::TurnBudget::bounded(8),
        no_progress_budget: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        tool_specs: Arc::new(Vec::new()),
        system_prompt: Arc::from(
            "Synthetic sans-IO checkpoint profiler prompt. Preserve pending effects across checkpoint restore.",
        ),
        session_id: "runtime-perf-turn-checkpoint".to_string(),
        turn_id: "runtime-perf-turn".to_string(),
        emit_llm_trace: false,
        termination: ProtocolTurnOptions::default(),
        turn_limit_final_message: Arc::new(runtime_perf_turn_limit_final_message),
    }
}

fn runtime_perf_turn_limit_final_message(message_id: String, max_turns: usize) -> Message {
    Message {
        id: message_id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::error(
            format!("{message_id}.p0"),
            format!("Turn limit reached ({max_turns}) before runtime perf completion."),
        )]),
        origin: None,
    }
}

fn checkpoint_messages() -> Vec<Message> {
    (0usize..36)
        .map(|index| {
            let role = if index.is_multiple_of(2) {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            checkpoint_message(
                format!("checkpoint-msg-{index}"),
                role,
                format!(
                    "Historical checkpoint profiler message {index}. This payload is intentionally long enough to make TurnCheckpoint serialization include realistic prompt and transcript bytes. The current topic is standard and RLM turn-effect replay across LLM, tool, checkpoint, sleep, and ExecCode boundaries."
                ),
            )
        })
        .collect()
}

fn checkpoint_message(id: String, role: MessageRole, content: String) -> Message {
    Message {
        id: id.clone(),
        role,
        parts: shared_parts(vec![Part::text(format!("{id}.p0"), content, None)]),
        origin: None,
    }
}

fn measure_checkpoint_phase(
    name: &'static str,
    f: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<(String, RuntimePerfPhaseRunResult)> {
    let before_alloc = allocator_stats();
    let before_memory = process_memory_sample();
    let started = Instant::now();
    f()?;
    let after_alloc = allocator_stats();
    let after_memory = process_memory_sample();
    Ok((
        name.to_string(),
        RuntimePerfPhaseRunResult {
            samples: 1,
            duration_ms: elapsed_ms(started),
            allocations: alloc_delta(before_alloc, after_alloc),
            rss_growth_kb: diff_opt_i64(before_memory.rss_kb, after_memory.rss_kb),
        },
    ))
}

fn checkpoint_pending_llm(
    configs: &CheckpointConfigs,
    seed_messages: &[Message],
    turn_index: usize,
) -> anyhow::Result<()> {
    let config = configs.llm_config();
    let mut machine = checkpoint_machine(config, seed_messages, turn_index);
    let effect = next_checkpoint_effect(&mut machine)
        .ok_or_else(|| anyhow::anyhow!("checkpoint llm scenario produced no effect"))?;
    let Effect::LlmCall { id, .. } = effect else {
        anyhow::bail!("checkpoint llm scenario expected LlmCall effect");
    };
    let checkpoint = machine.checkpoint();
    let bytes = serde_json::to_vec(&checkpoint)?;
    let checkpoint = serde_json::from_slice(&bytes)?;
    let mut restored = TurnMachine::restore_from_checkpoint(configs.llm_config(), checkpoint);
    assert_restored_llm(&mut restored, id)?;
    restored.handle_response(Response::LlmComplete {
        id,
        result: Ok(LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: "runtime perf benchmark ok".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
        text_streamed: false,
    });
    drain_checkpoint_machine(&mut restored);
    Ok(())
}

fn checkpoint_pending_parallel_tools(
    configs: &CheckpointConfigs,
    seed_messages: &[Message],
    turn_index: usize,
) -> anyhow::Result<()> {
    let config = configs.tools_config();
    let mut machine = checkpoint_machine(config, seed_messages, turn_index);
    let effect = next_checkpoint_effect(&mut machine)
        .ok_or_else(|| anyhow::anyhow!("checkpoint tools scenario produced no effect"))?;
    let Effect::ToolCalls { id, calls } = effect else {
        anyhow::bail!("checkpoint tools scenario expected ToolCalls effect");
    };
    let checkpoint = machine.checkpoint();
    let bytes = serde_json::to_vec(&checkpoint)?;
    let checkpoint = serde_json::from_slice(&bytes)?;
    let mut restored = TurnMachine::restore_from_checkpoint(configs.tools_config(), checkpoint);
    assert_restored_tool_batch(&mut restored, id, calls.len())?;
    restored.handle_response(Response::ToolResults {
        id,
        results: calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| completed_checkpoint_tool(index, call))
            .collect(),
    });
    drain_checkpoint_machine(&mut restored);
    Ok(())
}

fn checkpoint_pending_exec(
    configs: &CheckpointConfigs,
    seed_messages: &[Message],
    turn_index: usize,
) -> anyhow::Result<()> {
    let config = configs.exec_config();
    let mut machine = checkpoint_machine(config, seed_messages, turn_index);
    let effect = next_checkpoint_effect(&mut machine)
        .ok_or_else(|| anyhow::anyhow!("checkpoint exec scenario produced no effect"))?;
    let Effect::ExecCode { id, code, .. } = effect else {
        anyhow::bail!("checkpoint exec scenario expected ExecCode effect");
    };
    let checkpoint = machine.checkpoint();
    let bytes = serde_json::to_vec(&checkpoint)?;
    let checkpoint = serde_json::from_slice(&bytes)?;
    let mut restored = TurnMachine::restore_from_checkpoint(configs.exec_config(), checkpoint);
    assert_restored_exec(&mut restored, id, &code)?;
    restored.handle_response(Response::ExecResult {
        id,
        result: Ok(ExecResponse {
            observations: vec![lash_core::Observation {
                text: "checkpoint observation: resumed after ExecCode effect boundary".to_string(),
                projection: Default::default(),
            }],
            tool_calls: Vec::new(),
            executed_calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 1,
            degraded_bindings: Vec::new(),
            terminal_finish: Some(serde_json::json!("runtime perf benchmark ok")),
        }),
    });
    drain_checkpoint_machine(&mut restored);
    Ok(())
}

fn checkpoint_machine(
    config: TurnMachineConfig,
    seed_messages: &[Message],
    turn_index: usize,
) -> TurnMachine {
    let mut messages = seed_messages.to_vec();
    messages.push(checkpoint_message(
        format!("checkpoint-live-turn-{turn_index}"),
        MessageRole::User,
        format!(
            "Durability checkpoint profiler live turn {}",
            turn_index + 1
        ),
    ));
    TurnMachine::new(config, messages, Arc::new(Vec::new()), turn_index)
}

fn checkpoint_tool_calls(protocol_iteration: usize) -> Vec<PendingToolCall> {
    (0..24)
        .map(|index| PendingToolCall {
            call_id: format!("checkpoint-call-{protocol_iteration}-{index}"),
            tool_name: format!("checkpoint_parallel_tool_{}", index % 6),
            args: serde_json::json!({
                "index": index,
                "protocol_iteration": protocol_iteration,
                "payload": format!("synthetic parallel durability payload {index}")
            }),
            replay: None,
        })
        .collect()
}

fn completed_checkpoint_tool(index: usize, call: PendingToolCall) -> CompletedToolCall {
    let output = match index % 4 {
        0 => ToolCallOutput::success(serde_json::json!({
            "ok": true,
            "index": index,
            "payload": call.args,
        })),
        1 => ToolCallOutput::failure(ToolFailure::tool(
            ToolFailureClass::Execution,
            "checkpoint_tool_failed",
            format!("synthetic failure for {}", call.call_id),
        )),
        2 => ToolCallOutput::cancelled(ToolCancellation::runtime(format!(
            "synthetic cancellation for {}",
            call.call_id
        ))),
        _ => ToolCallOutput::success(serde_json::json!({
            "ok": true,
            "index": index,
            "large": "x".repeat(128),
        })),
    };
    CompletedToolCall {
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        args: call.args,
        model_return: ModelToolReturn::from_output(
            call.call_id.clone(),
            call.tool_name.clone(),
            &output,
        ),
        output,
        duration_ms: 1,
        intent_outcomes: Vec::new(),
        replay: call.replay,
    }
}

fn checkpoint_exec_code(protocol_iteration: usize) -> String {
    format!(
        r#"process benchmark_echo_process(tool: Tools, value: str, ordinal: int) {{
  result = await tool.benchmark_echo({{ value: value, ordinal: ordinal }})?
  finish result
}}

print("checkpoint turn {protocol_iteration}")
first = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 1)
second = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 2)
third = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 3)
fanout = await {{
  a: first,
  b: second,
  c: third
}}
finish fanout.a?.value"#
    )
}

fn assert_restored_llm(
    machine: &mut TurnMachine,
    expected_id: lash_core::facade_support::EffectId,
) -> anyhow::Result<()> {
    match next_checkpoint_effect(machine) {
        Some(Effect::LlmCall { id, .. }) if id == expected_id => Ok(()),
        Some(_) => anyhow::bail!("restored checkpoint did not replay LlmCall"),
        None => anyhow::bail!("restored checkpoint had no LlmCall"),
    }
}

fn assert_restored_tool_batch(
    machine: &mut TurnMachine,
    expected_id: lash_core::facade_support::EffectId,
    expected_calls: usize,
) -> anyhow::Result<()> {
    match next_checkpoint_effect(machine) {
        Some(Effect::ToolCalls { id, calls })
            if id == expected_id && calls.len() == expected_calls =>
        {
            Ok(())
        }
        Some(_) => anyhow::bail!("restored checkpoint did not replay matching ToolCalls"),
        None => anyhow::bail!("restored checkpoint had no ToolCalls"),
    }
}

fn assert_restored_exec(
    machine: &mut TurnMachine,
    expected_id: lash_core::facade_support::EffectId,
    expected_code: &str,
) -> anyhow::Result<()> {
    match next_checkpoint_effect(machine) {
        Some(Effect::ExecCode { id, code, .. }) if id == expected_id && code == expected_code => {
            Ok(())
        }
        Some(_) => anyhow::bail!("restored checkpoint did not replay matching ExecCode"),
        None => anyhow::bail!("restored checkpoint had no ExecCode"),
    }
}

fn drain_checkpoint_machine(machine: &mut TurnMachine) {
    while machine.poll_effect().is_some() {}
}

fn next_checkpoint_effect(machine: &mut TurnMachine) -> Option<Effect> {
    loop {
        match machine.poll_effect()? {
            Effect::Emit(_)
            | Effect::Log { .. }
            | Effect::Progress { .. }
            | Effect::Done { .. } => continue,
            effect => return Some(effect),
        }
    }
}

pub(crate) async fn run_once_embed(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();

    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let store = Arc::new(RuntimePerfStore::default());
    let core = build_embed_core(scenario, Arc::clone(&store))?;
    let session = core
        .open_session(format!("runtime-perf-{}", scenario.name()))
        .await
        .with_context(|| format!("open embed session for {}", scenario.name()))?;
    let build_runtime_ms = elapsed_ms(build_started);
    let build_runtime_alloc = alloc_delta(build_before_alloc, allocator_stats());
    let after_build_memory = process_memory_sample();

    let seed_before_alloc = allocator_stats();
    let seed_started = Instant::now();
    let seed_state_ms = elapsed_ms(seed_started);
    let seed_state_alloc = alloc_delta(seed_before_alloc, allocator_stats());
    let after_seed_memory = process_memory_sample();

    let mut turns = Vec::with_capacity(chat_turns);
    for turn_index in 0..chat_turns {
        let before_turn_usage = SessionUsageReport::default();
        let turn_before_alloc = allocator_stats();
        let turn_before_memory = process_memory_sample();
        let turn_started = Instant::now();
        let cancel = CancellationToken::new();
        let turn = runtime_perf_timed(
            scenario,
            turn_index,
            "run_turn",
            Some(cancel.clone()),
            async {
                let effect_host = session.effect_host();
                let scoped_effect_controller = effect_host
                    .scoped(session.turn_scope(format!("runtime-perf-embed-{}", turn_index + 1)))
                    .map_err(anyhow::Error::from)?;
                session
                    .turn(lash_core::TurnInput::text(benchmark_prompt(
                        scenario, turn_index,
                    )))
                    .cancel(cancel)
                    .advanced()
                    .collect_session_events_with_scope(
                        &lash::runtime::NoopEventSink,
                        scoped_effect_controller,
                    )
                    .await
                    .map_err(anyhow::Error::from)
            },
        )
        .await
        .with_context(|| {
            format!(
                "run embed runtime perf scenario {} turn {}",
                scenario.name(),
                turn_index + 1
            )
        })?;
        validate_runtime_perf_turn(scenario, turn_index, &turn)?;
        let run_turn_ms = elapsed_ms(turn_started);
        let run_turn_alloc = alloc_delta(turn_before_alloc, allocator_stats());
        let after_turn_memory = process_memory_sample();

        let await_before_alloc = allocator_stats();
        let background_started = Instant::now();
        let await_background_work_ms = elapsed_ms(background_started);
        let await_background_work_alloc = alloc_delta(await_before_alloc, allocator_stats());
        let after_await_memory = process_memory_sample();
        let turn_total_alloc =
            sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]);

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
            phase_profile: BTreeMap::new(),
            turn_usage: turn.usage,
            usage_delta: before_turn_usage,
            cumulative_usage: SessionUsageReport::default(),
        });
    }

    let export_before_alloc = allocator_stats();
    let export_started = Instant::now();
    let read_view = session.read_view();
    let export_state_ms = elapsed_ms(export_started);
    let export_state_alloc = alloc_delta(export_before_alloc, allocator_stats());
    let after_export_memory = process_memory_sample();
    let total_alloc = alloc_delta(total_before_alloc, allocator_stats());
    let last_turn_memory = turns.last().map(|turn| &turn.memory);

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
        session_nodes: store.graph_node_count(),
        active_path_messages: read_view.messages().len(),
        extra_counters: BTreeMap::new(),
        metric_samples: BTreeMap::new(),
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
        phase_profile: BTreeMap::new(),
        turns,
        cumulative_usage: SessionUsageReport::default(),
    })
}
pub(crate) fn sum_phase_profiles<'a>(
    profiles: impl IntoIterator<Item = &'a BTreeMap<String, RuntimePerfPhaseRunResult>>,
) -> BTreeMap<String, RuntimePerfPhaseRunResult> {
    let mut totals: BTreeMap<String, RuntimePerfPhaseRunResult> = BTreeMap::new();
    for profile in profiles {
        for (phase, metrics) in profile {
            let entry = totals
                .entry(phase.clone())
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
    }
    totals
}

pub(crate) fn mean_phase_profiles<'a>(
    profiles: impl IntoIterator<Item = &'a BTreeMap<String, RuntimePerfPhaseRunResult>>,
) -> BTreeMap<String, RuntimePerfPhaseRunResult> {
    let profiles = profiles.into_iter().collect::<Vec<_>>();
    if profiles.is_empty() {
        return BTreeMap::new();
    }
    let count = profiles.len() as f64;
    let sums = sum_phase_profiles(profiles);
    sums.into_iter()
        .map(|(phase, metrics)| {
            (
                phase,
                RuntimePerfPhaseRunResult {
                    samples: ((metrics.samples as f64) / count).round() as usize,
                    duration_ms: round3(metrics.duration_ms / count),
                    allocations: scale_allocation_delta(&metrics.allocations, count),
                    rss_growth_kb: metrics
                        .rss_growth_kb
                        .map(|value| ((value as f64) / count).round() as i64),
                },
            )
        })
        .collect()
}

pub(crate) fn sum_allocation_deltas<'a>(
    deltas: impl IntoIterator<Item = &'a RuntimePerfAllocationDelta>,
) -> RuntimePerfAllocationDelta {
    let mut total = zero_allocation_delta();
    for delta in deltas {
        total.allocations += delta.allocations;
        total.deallocations += delta.deallocations;
        total.reallocations += delta.reallocations;
        total.bytes_allocated += delta.bytes_allocated;
        total.bytes_deallocated += delta.bytes_deallocated;
        total.bytes_reallocated += delta.bytes_reallocated;
        total.net_live_bytes += delta.net_live_bytes;
    }
    total
}

pub(crate) fn mean_allocation_delta<'a>(
    deltas: impl IntoIterator<Item = &'a RuntimePerfAllocationDelta>,
) -> RuntimePerfAllocationDelta {
    let deltas = deltas.into_iter().collect::<Vec<_>>();
    if deltas.is_empty() {
        return zero_allocation_delta();
    }
    let count = deltas.len() as f64;
    scale_allocation_delta(&sum_allocation_deltas(deltas), count)
}

pub(crate) fn scale_allocation_delta(
    delta: &RuntimePerfAllocationDelta,
    divisor: f64,
) -> RuntimePerfAllocationDelta {
    RuntimePerfAllocationDelta {
        allocations: ((delta.allocations as f64) / divisor).round() as usize,
        deallocations: ((delta.deallocations as f64) / divisor).round() as usize,
        reallocations: ((delta.reallocations as f64) / divisor).round() as usize,
        bytes_allocated: ((delta.bytes_allocated as f64) / divisor).round() as usize,
        bytes_deallocated: ((delta.bytes_deallocated as f64) / divisor).round() as usize,
        bytes_reallocated: ((delta.bytes_reallocated as f64) / divisor).round() as isize,
        net_live_bytes: ((delta.net_live_bytes as f64) / divisor).round() as i64,
    }
}

pub(crate) fn zero_allocation_delta() -> RuntimePerfAllocationDelta {
    RuntimePerfAllocationDelta {
        allocations: 0,
        deallocations: 0,
        reallocations: 0,
        bytes_allocated: 0,
        bytes_deallocated: 0,
        bytes_reallocated: 0,
        net_live_bytes: 0,
    }
}

pub(crate) fn mean_token_usage<'a>(usages: impl IntoIterator<Item = &'a TokenUsage>) -> TokenUsage {
    let usages = usages.into_iter().collect::<Vec<_>>();
    if usages.is_empty() {
        return TokenUsage::default();
    }
    let count = usages.len() as i64;
    TokenUsage {
        input_tokens: usages.iter().map(|usage| usage.input_tokens).sum::<i64>() / count,
        output_tokens: usages.iter().map(|usage| usage.output_tokens).sum::<i64>() / count,
        cache_read_input_tokens: usages
            .iter()
            .map(|usage| usage.cache_read_input_tokens)
            .sum::<i64>()
            / count,
        cache_write_input_tokens: usages
            .iter()
            .map(|usage| usage.cache_write_input_tokens)
            .sum::<i64>()
            / count,
        reasoning_output_tokens: usages
            .iter()
            .map(|usage| usage.reasoning_output_tokens)
            .sum::<i64>()
            / count,
    }
}

fn token_usage_from_llm_usage(usage: &LlmUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
    }
}

pub(crate) fn mean_option_i64(values: impl IntoIterator<Item = Option<i64>>) -> Option<i64> {
    let values = values.into_iter().flatten().collect::<Vec<_>>();
    if values.is_empty() {
        None
    } else {
        Some((values.iter().sum::<i64>() as f64 / values.len() as f64).round() as i64)
    }
}

pub(crate) fn sum_optional_i64(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}
pub(crate) fn phase_name(phase: RuntimeTurnPhase) -> &'static str {
    match phase {
        RuntimeTurnPhase::ContextTransform => "context_transform",
        RuntimeTurnPhase::BeforeTurnHooks => "before_turn_hooks",
        RuntimeTurnPhase::PromptBuild => "prompt_build",
        RuntimeTurnPhase::EffectLoop => "effect_loop",
        RuntimeTurnPhase::PreparedTurn => "prepared_turn",
        RuntimeTurnPhase::CommittedTurn => "committed_turn",
        RuntimeTurnPhase::PostCommitDelivery => "post_commit_delivery",
    }
}

pub(crate) fn allocator_stats() -> Stats {
    crate::GLOBAL_ALLOCATOR.stats()
}

pub(crate) fn alloc_delta(before: Stats, after: Stats) -> RuntimePerfAllocationDelta {
    let diff = after - before;
    RuntimePerfAllocationDelta {
        allocations: diff.allocations,
        deallocations: diff.deallocations,
        reallocations: diff.reallocations,
        bytes_allocated: diff.bytes_allocated,
        bytes_deallocated: diff.bytes_deallocated,
        bytes_reallocated: diff.bytes_reallocated,
        net_live_bytes: diff.bytes_allocated as i64 - diff.bytes_deallocated as i64,
    }
}
