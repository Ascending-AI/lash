use super::*;
use lash_sansio::SessionId;

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
    // Runtime scenarios accept scalar CLI parameters rather than a Cartesian sweep.
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
pub(crate) const CHECKPOINT_HASH_PASSES_PER_CHANGED_BODY: u64 = 1;

struct DurableCheckpointCurveFixture {
    point: CheckpointCurvePoint,
    fixture: lash_protocol_rlm::RlmCheckpointPerfFixture,
    runtime_state: RuntimeSessionState,
    store: Arc<dyn lash_core::RuntimePersistence>,
}

#[expect(
    clippy::expect_used,
    reason = "the postgres database or the SQLite root is provisioned by the caller for exactly the scenario being run, which each site's message states"
)]
pub(crate) async fn run_once_durable_checkpoint_curve(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    config: &CheckpointCurveConfig,
    postgres_database_url: Option<&str>,
) -> anyhow::Result<RuntimePerfRunResult> {
    let postgres_database = match postgres_database_url {
        Some(database_url) => {
            Some(lash_postgres_store::testing::IsolatedDatabase::create(database_url).await)
        }
        None => None,
    };
    let total_started = Instant::now();
    let before_memory = process_memory_sample();
    let total_before_alloc = allocator_stats();
    let build_before_alloc = allocator_stats();
    let build_started = Instant::now();
    let sqlite_root = (!scenario.uses_postgres())
        .then(|| make_temp_bench_dir(&format!("lash-runtime-perf-{}", scenario.name())))
        .transpose()?;
    let (store_factory, store_metrics) = if scenario.uses_postgres() {
        let database = postgres_database
            .as_ref()
            .expect("PostgreSQL database created for PostgreSQL scenario");
        let postgres = lash_postgres_store::PostgresStorage::connect(database.url())
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
    // The fixtures' cells keep their Lashlang artifacts in one memory backend;
    // the state they capture is what the curve measures.
    let artifacts = lash_sqlite_store::SqliteBackend::memory().await?;
    for point in points {
        let session_id = SessionId::from(format!(
            "runtime-perf-{}-{run_id}-{}-{}",
            scenario.name(),
            point.axis.name(),
            point.target
        ));
        let store = store_factory
            .create_store(&runtime_perf_session_create_request(&session_id))
            .await?;
        store
            .admit_and_bind_session(&lash_core::SessionBinding::root(session_id.clone()))
            .await?;
        let mut fixture = lash_protocol_rlm::RlmCheckpointPerfFixture::new(
            &artifacts.clone().into(),
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
            let (snapshot, capture_phase) = Box::pin(measure_runtime_perf_async_phase(
                "checkpoint_curve.capture",
                async {
                    fixture.fixture.assign_one(sample, sample).await?;
                    fixture.fixture.capture().map_err(anyhow::Error::from)
                },
            ))
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
                    if commit.graph.nodes().len() != fixture.point.graph_rows {
                        anyhow::bail!(
                            "{prefix} committed {} graph rows, expected {}",
                            commit.graph.nodes().len(),
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
            let work: lash_core::perf_witness::Snapshot = work_collector.snapshot();
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
            stages: turn_stages(
                RuntimePerfStageRunResult::measured(
                    run_turn_ms,
                    run_turn_alloc.clone(),
                    after_turn_memory.rss_kb,
                ),
                Some(RuntimePerfStageRunResult::measured(
                    await_background_work_ms,
                    await_background_work_alloc.clone(),
                    after_await_memory.rss_kb,
                )),
                RuntimePerfStageRunResult::measured(
                    round3(run_turn_ms + await_background_work_ms),
                    sum_allocation_deltas([&run_turn_alloc, &await_background_work_alloc]),
                    after_await_memory.rss_kb,
                ),
            ),
            memory: memory_span(turn_before_memory, after_await_memory),
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
                    stage::SEED_STATE,
                    RuntimePerfStageRunResult::measured(
                        seed_state_ms,
                        seed_state_alloc,
                        after_seed_memory.rss_kb,
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
            &turns,
        ),
        session_nodes: config.graph_rows,
        active_path_messages: config.message_count,
        extra_counters,
        metric_samples,
        metric_samples_ms,
        memory: memory_span(before_memory, after_export_memory),
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

#[expect(
    clippy::expect_used,
    reason = "the generator only emits bytes in 33..127, so the UTF-8 conversion cannot fail, per the message"
)]
fn checkpoint_curve_ascii(len: usize, seed: usize) -> String {
    // Keep the commit-size benchmark's deterministic printable-payload shape:
    // logical byte targets stay exact without compression-friendly zero fill.
    let bytes = (0..len)
        .map(|index| b'!' + ((index.wrapping_add(seed)) % 94) as u8)
        .collect::<Vec<_>>();
    String::from_utf8(bytes).expect("checkpoint curve generator emits ASCII")
}
