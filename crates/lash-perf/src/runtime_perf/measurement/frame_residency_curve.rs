use super::*;
use lash_sansio::SessionId;

const PRIOR_HISTORY_ROWS: [usize; 4] = [0, 1_000, 8_000, 32_000];
const CURRENT_FRAME_ROWS: usize = 64;
const HEAP_ALLOWANCE_BYTES: i64 = 64 * 1024;
// FIG-3843 owns the quiet-host rebaseline of this perf-gated latency law.
const COMMIT_LATENCY_RATIO: f64 = 1.2;
const SEED_BUDGET: lash_core::CommitBudget =
    lash_core::CommitBudget::bounded(64 * 1024 * 1024, 40_000);

struct FramePoint {
    prior_rows: usize,
    live_heap_bytes: i64,
    decoded_rows: usize,
    commit_median_ms: f64,
    commit_samples_ms: Vec<f64>,
}

fn check_reopened_frame_residency(points: &[FramePoint]) -> anyhow::Result<()> {
    let baseline = &points[0];
    if baseline.live_heap_bytes <= 0 {
        anyhow::bail!("frame residency heap allocator was not instrumented");
    }
    let heap_limit =
        baseline.live_heap_bytes.unsigned_abs() as f64 * 0.01 + HEAP_ALLOWANCE_BYTES as f64;
    for measured in points {
        if measured.decoded_rows != CURRENT_FRAME_ROWS {
            anyhow::bail!(
                "{} prior rows decoded {} current-frame rows, expected {}",
                measured.prior_rows,
                measured.decoded_rows,
                CURRENT_FRAME_ROWS
            );
        }
        let heap_delta =
            (measured.live_heap_bytes - baseline.live_heap_bytes).unsigned_abs() as f64;
        if heap_delta > heap_limit {
            anyhow::bail!(
                "{} prior rows changed resident heap by {} bytes, limit {}",
                measured.prior_rows,
                heap_delta,
                heap_limit
            );
        }
    }
    Ok(())
}

async fn open_catalog(
    scenario: RuntimePerfScenario,
    sqlite_root: Option<&std::path::Path>,
    postgres_url: Option<&str>,
) -> anyhow::Result<Arc<dyn lash_core::DeploymentStore>> {
    if scenario.uses_postgres() {
        let url = postgres_url.ok_or_else(|| anyhow::anyhow!("PostgreSQL URL is required"))?;
        let storage = lash_postgres_store::PostgresStorage::connect(url).await?;
        Ok(Arc::new(storage.store()))
    } else {
        let root = sqlite_root.ok_or_else(|| anyhow::anyhow!("SQLite root is required"))?;
        Ok(Arc::new(lash_sqlite_store::SqliteStore::open(root).await?))
    }
}

async fn load_frame(
    store: Arc<dyn lash_core::DeploymentStore>,
    session_id: &SessionId,
) -> anyhow::Result<RuntimeSessionState> {
    let runtime: Arc<dyn lash_core::RuntimeStore> = store;
    let view = lash_core::SessionStore::new(runtime, session_id.clone())?;
    let loaded = lash_core::store::load_session_window_state(
        &view,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("seeded session has no current window"))?;
    Ok(loaded.state)
}

fn append_messages(state: &mut RuntimeSessionState, prefix: &str, count: usize) {
    let messages = (0..count)
        .map(|index| {
            checkpoint_message(
                format!("{prefix}-{index}"),
                if index.is_multiple_of(2) {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                format!("Frame residency row {index}"),
            )
        })
        .collect::<Vec<_>>();
    state.append_active_conversation_messages(&messages);
}

async fn commit_state(
    store: &dyn lash_core::DeploymentStore,
    state: &RuntimeSessionState,
) -> anyhow::Result<f64> {
    let commit = RuntimeCommit::persisted_state_for_test_with_budget(state, &[], SEED_BUDGET);
    let started = Instant::now();
    store.commit_runtime_state(commit).await?;
    Ok(elapsed_ms(started))
}

fn open_next_frame(
    state: &mut RuntimeSessionState,
    prior_rows: usize,
    sample: usize,
) -> anyhow::Result<()> {
    let key =
        lash_core::FrameKey::from_caller_material(&format!("residency-{prior_rows}-{sample}"))?;
    let frame_node_id = lash_core::session_graph::frame_node_id(&state.session_id, key.as_str());
    let opened = state.session_graph.append_frame_open_with_id_at(
        frame_node_id.clone(),
        key,
        lash_core::AgentFrameReason::compaction(),
        lash_core::AgentFrameAssignment::from_policy(state.policy.clone()),
        state.protocol_turn_options.clone(),
        chrono::Utc::now().to_rfc3339(),
    );
    if !opened {
        anyhow::bail!("frame residency key was reused");
    }
    state.current_frame_node_id = Some(frame_node_id);
    append_messages(
        state,
        &format!("current-{prior_rows}-{sample}"),
        CURRENT_FRAME_ROWS - 1,
    );
    Ok(())
}

async fn point(
    scenario: RuntimePerfScenario,
    sqlite_root: Option<&std::path::Path>,
    postgres_url: Option<&str>,
    prior_rows: usize,
    commit_samples: usize,
) -> anyhow::Result<FramePoint> {
    let session_id = SessionId::from(format!("frame-residency-{prior_rows}"));
    let catalog = open_catalog(scenario, sqlite_root, postgres_url).await?;
    catalog
        .admit_session(&runtime_perf_session_create_request(&session_id))
        .await?;
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    if prior_rows == 0 {
        append_messages(&mut state, "current-0-0", CURRENT_FRAME_ROWS - 1);
        commit_state(catalog.as_ref(), &state).await?;
    } else {
        // The initial FrameOpen is one of the earlier rows.
        append_messages(&mut state, &format!("prior-{prior_rows}"), prior_rows - 1);
        if state.session_graph.nodes.len() != prior_rows {
            anyhow::bail!("earlier history seed has the wrong row count");
        }
        commit_state(catalog.as_ref(), &state).await?;
        drop(state);
        state = load_frame(catalog.clone(), &session_id).await?;
        open_next_frame(&mut state, prior_rows, 0)?;
        commit_state(catalog.as_ref(), &state).await?;
    }
    drop(state);
    drop(catalog);

    let catalog = open_catalog(scenario, sqlite_root, postgres_url).await?;
    // Prime one-time decoder allocations before comparing resident windows.
    drop(load_frame(catalog.clone(), &session_id).await?);
    let before = allocator_stats();
    let mut state = load_frame(catalog.clone(), &session_id).await?;
    let after = allocator_stats();
    let live_heap_bytes = alloc_delta(before, after).net_live_bytes;
    let decoded_rows = state.session_graph.nodes.len();
    if decoded_rows != CURRENT_FRAME_ROWS {
        anyhow::bail!(
            "{} prior rows decoded {} current-frame rows, expected {}",
            prior_rows,
            decoded_rows,
            CURRENT_FRAME_ROWS
        );
    }
    let mut commit_samples_ms = Vec::with_capacity(commit_samples);
    for sample in 1..=commit_samples {
        open_next_frame(&mut state, prior_rows, sample)?;
        commit_samples_ms.push(commit_state(catalog.as_ref(), &state).await?);
        state = load_frame(catalog.clone(), &session_id).await?;
        if state.session_graph.nodes.len() != CURRENT_FRAME_ROWS {
            anyhow::bail!("commit sample {sample} changed the current-frame row count");
        }
    }
    commit_samples_ms.sort_by(f64::total_cmp);
    let commit_median_ms = commit_samples_ms[commit_samples_ms.len() / 2];
    Ok(FramePoint {
        prior_rows,
        live_heap_bytes,
        decoded_rows,
        commit_median_ms,
        commit_samples_ms,
    })
}

pub(super) async fn run_once_frame_residency_curve(
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    postgres_url: Option<&str>,
) -> anyhow::Result<RuntimePerfRunResult> {
    let sqlite_root = (!scenario.uses_postgres())
        .then(|| make_temp_bench_dir("lash-frame-residency"))
        .transpose()?;
    let postgres_database = match postgres_url {
        Some(url) => Some(lash_postgres_store::testing::IsolatedDatabase::create(url).await),
        None => None,
    };
    let database_url = postgres_database.as_ref().map(|database| database.url());
    eprintln!(
        "frame residency argv: {:?}",
        std::env::args().collect::<Vec<_>>()
    );
    let mut run = RunRecorder::start(scenario, PRIOR_HISTORY_ROWS.len());
    let mut points = Vec::with_capacity(PRIOR_HISTORY_ROWS.len());
    for (index, prior_rows) in PRIOR_HISTORY_ROWS.into_iter().enumerate() {
        let measured = run
            .turn(
                index,
                async {
                    let result = point(
                        scenario,
                        sqlite_root.as_deref(),
                        database_url,
                        prior_rows,
                        chat_turns.max(3),
                    )
                    .await?;
                    Ok(TurnRun {
                        value: result,
                        tail: TurnTail::default(),
                    })
                },
                async { Ok(()) },
            )
            .await?;
        eprintln!(
            "frame residency backend={} prior_rows={} decoded_rows={} live_heap_bytes={} median_commit_ms={:.3}",
            scenario.name(),
            measured.prior_rows,
            measured.decoded_rows,
            measured.live_heap_bytes,
            measured.commit_median_ms
        );
        points.push(measured);
    }
    check_reopened_frame_residency(&points)?;
    let baseline = &points[0];
    let commit_ratio = points[3].commit_median_ms / baseline.commit_median_ms.max(f64::EPSILON);
    if commit_ratio > COMMIT_LATENCY_RATIO {
        anyhow::bail!("32,000-row commit median ratio {commit_ratio:.3} exceeds 1.2");
    }
    let mut counters = BTreeMap::new();
    counters.insert(
        "frame_residency.current_frame_rows".into(),
        CURRENT_FRAME_ROWS as u64,
    );
    counters.insert(
        "frame_residency.commit_samples_per_point".into(),
        chat_turns.max(3) as u64,
    );
    let mut metric_samples_ms = BTreeMap::new();
    for measured in &points {
        counters.insert(
            format!("frame_residency.prior_{}.decoded_rows", measured.prior_rows),
            measured.decoded_rows as u64,
        );
        counters.insert(
            format!(
                "frame_residency.prior_{}.live_heap_bytes",
                measured.prior_rows
            ),
            measured.live_heap_bytes.max(0) as u64,
        );
        metric_samples_ms.insert(
            format!("frame_residency.prior_{}.commit_ms", measured.prior_rows),
            measured.commit_samples_ms.clone(),
        );
    }
    Ok(run.finish(RunTail {
        session_nodes: CURRENT_FRAME_ROWS,
        active_path_messages: CURRENT_FRAME_ROWS - 1,
        extra_counters: counters,
        metric_samples_ms,
        ..RunTail::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reopened_frame_heap_and_decoded_rows_ignore_prior_history() {
        let root = make_temp_bench_dir("lash-frame-residency-unit")
            .expect("create SQLite frame residency catalog");
        let mut points = Vec::with_capacity(PRIOR_HISTORY_ROWS.len());
        for prior_rows in PRIOR_HISTORY_ROWS {
            points.push(
                point(
                    RuntimePerfScenario::FrameResidencyCurveSqlite,
                    Some(&root),
                    None,
                    prior_rows,
                    1,
                )
                .await
                .expect("seed and reopen a fixed current frame"),
            );
        }
        check_reopened_frame_residency(&points)
            .expect("resident heap and decoded rows stay fixed across earlier history");
    }
}
