//! Drive a workload end to end through the public facade: `send()` then
//! `outcome()` on live sessions of a core that serves its own node, over a
//! real store, with code-mode cells in real worker processes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use super::meter::{Meter, StoreProbe, difference};
use super::provider::{self, ScriptBook};
use super::receipt::{RoundRecord, WorkloadReceipt, summarize, units};
use super::script::{Protocol, ScriptedTurn, Workload};

/// How long one round may take before the run is declared hung.
const ROUND_WATCHDOG: Duration = Duration::from_secs(600);

/// Where a replay's stores live.
#[derive(Clone, Debug)]
pub enum StoreChoice {
    /// A SQLite file store set under this directory.
    SqliteFile(std::path::PathBuf),
    /// An isolated database on the PostgreSQL server at this URL.
    Postgres(String),
}

/// One run's settings.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub identity: super::receipt::Identity,
    pub store: StoreChoice,
    /// Run only the first this many turns of each session.
    pub turns: Option<usize>,
    /// Separate Python-worker heap profiles beside the replay receipt.
    pub worker_profile_dir: std::path::PathBuf,
    pub sql_shapes: Option<std::path::PathBuf>,
    pub traffic: bool,
    pub effect_phases: Option<std::path::PathBuf>,
    pub effect_phases_off: bool,
    pub heap: super::ReplayHeapArgs,
}

/// What a store set must outlive.
type Keep = Vec<Box<dyn std::any::Any + Send>>;

async fn open_stores(
    choice: &StoreChoice,
    workload: &str,
) -> Result<(
    Arc<dyn lash_execution_contracts::backend::StoreSet>,
    StoreProbe,
    Keep,
)> {
    match choice {
        StoreChoice::SqliteFile(dir) => {
            let dir = dir.join(workload);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            let stores = lash::sqlite::SqliteStoreSet::open(
                dir.join("lash.db"),
                lash::sqlite::SqliteSynchronous::Normal,
            )
            .await
            .map_err(|error| anyhow::anyhow!("open the SQLite store set: {error}"))?;
            let probe = StoreProbe::sqlite(&stores)?;
            Ok((Arc::new(stores), probe, Vec::new()))
        }
        StoreChoice::Postgres(url) => {
            let isolated = lash_postgres_store::testing::IsolatedDatabase::create(url).await;
            let storage = lash_postgres_store::testing::connect(isolated.url())
                .await
                .map_err(|error| anyhow::anyhow!("open the isolated database: {error}"))?;
            let attachments = tempfile_dir()?;
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                lash::sqlite::SqliteStoreSet::open(
                    attachments.join("attachments.db"),
                    lash::sqlite::SqliteSynchronous::Normal,
                )
                .await
                .map_err(|error| anyhow::anyhow!("open the attachment store: {error}"))?
                .attachment_store(),
            );
            let probe = StoreProbe::Postgres {
                pool: storage.pool().clone(),
            };
            Ok((
                Arc::new(stores),
                probe,
                vec![Box::new(isolated), Box::new(storage)],
            ))
        }
    }
}

fn tempfile_dir() -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "lash-perf-replay-attachments-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

pub(crate) fn build_core(
    workload: &Workload,
    backend: &lash::Backend,
    book: Arc<ScriptBook>,
    workers: Option<&lash::vm::WorkerService>,
    tools: Arc<dyn lash_core::ToolProvider>,
) -> Result<lash::LashCore> {
    let builder = match (workload.protocol, workers) {
        (Protocol::Standard, _) => {
            let builder = lash::LashCore::standard_builder(backend.clone());
            match workload.compact_every {
                Some(_) => builder.plugin(Arc::new(
                    lash_plugin_standard_compaction::StandardCompactionPluginFactory::new(
                        lash_plugin_standard_compaction::StandardCompactionConfig {
                            // Compaction happens on the script's schedule only.
                            pressure_compaction: false,
                            keep_recent_tokens: 64,
                            ..lash_plugin_standard_compaction::StandardCompactionConfig::standard()
                        },
                    ),
                )),
                None => builder,
            }
        }
        (protocol, Some(workers)) => lash::LashCore::rlm_builder(
            backend.clone(),
            lash::rlm::RlmProtocolPluginFactory::new(
                lash::rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash::rlm::RlmChannel::Cell)
                    .instruction_limit(lash::rlm::InstructionBound::instructions(100_000_000))
                    .memory_limit(lash::rlm::MemoryBound::mebibytes(512))
                    .build(),
                if protocol == Protocol::Python {
                    lash::rlm::CellDialect::python()
                } else {
                    lash::rlm::CellDialect::typescript()
                },
            )
            .with_worker_service(workers.clone()),
        ),
        (_, None) => bail!("code mode needs a worker service"),
    };
    builder
        .serve_sessions(true)
        .commit_budget(lash::CommitBudget::bounded(64 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(provider::model(book), provider::metadata()?)
        .tools(lash::tool_provider_key!("tools"), tools)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("lash-perf-replay"),
            lash::persistence::LeaseIncarnationId::new(format!("{}", std::process::id())),
        ))
        .map_err(|error| anyhow::anyhow!("build the replay core: {error}"))
}

/// One session the run drives: its live handle and its scripted inputs.
struct Lane {
    index: usize,
    session: lash::LashSession,
    inputs: Vec<(ScriptedTurn, String)>,
}

/// The node's settings with page retirement left to the rounds' gaps: the
/// background sweep fires on wall-clock time, so whichever round it lands
/// in would carry another round's deletes. Retirement is maintenance, like
/// artifact cleanup, and settles between rounds the same way (FIG-6321).
fn replay_settings() -> lash_durable::DurableSettings {
    let mut settings = lash_durable::DurableSettings::standard();
    settings.retirement.every = Duration::from_secs(24 * 60 * 60);
    settings
}

/// Runs page retirement until a step finds nothing left to do.
async fn retire(pages: Option<&dyn lash_durable::domain::pages::PageStore>) -> Result<()> {
    let Some(pages) = pages else {
        return Ok(());
    };
    let budget = lash_durable::domain::pages::RetirementBudget {
        nodes: lash_durable::RetirementSettings::MAX_BATCH_NODES,
    };
    while pages
        .retire_step(budget)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?
        != lash_durable::domain::pages::RetirementProgress::default()
    {}
    Ok(())
}

/// Run `workload` once and return its receipt.
pub async fn run(workload: &Workload, options: &RunOptions) -> Result<WorkloadReceipt> {
    ensure!(
        options.identity.sql_shapes == options.sql_shapes.is_some()
            && options.identity.traffic == options.traffic
            && options.identity.heap_window
                == options
                    .heap
                    .profile_turn
                    .map(|first| [first, options.heap.profile_through_turn.unwrap_or(first)]),
        "replay observer identity differs from selected instruments"
    );
    let traffic = options
        .traffic
        .then(lash_sqlite_store::write_observer::Collector::install)
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?;
    let mut phase_capture = options
        .effect_phases
        .as_ref()
        .map(|path| {
            super::effect_phases::Capture::new(
                path,
                options.effect_phases_off,
                workload,
                &options.identity,
            )
        })
        .transpose()?;
    let (stores, probe, keep) = open_stores(&options.store, &workload.name).await?;
    let mut experiment = options.identity.experiment(serde_json::json!({
        "turns": options.turns.unwrap_or(workload.turn_count()).min(workload.turn_count()),
        "sessions": workload.sessions, "compact_every": workload.compact_every,
        "sql_shapes": options.identity.sql_shapes, "traffic": options.identity.traffic,
        "heap_window": options.identity.heap_window, "heap_frames": options.heap.dhat_frames,
        "window": "round first send through last successful outcome; counters at settled cuts",
        "sql_work_window": options.sql_shapes.as_ref().map(|_| "session creation through core shutdown; start/finish phase cuts; includes unowned and cross-window SQL"),
        "terminal": "all scripted replies validated; successful outcomes",
    }));
    experiment.backend = probe.backend().to_owned();
    experiment.workload = format!("replay/{}:{}", workload.name, workload.digest());
    experiment.configured_durability = match &probe {
        StoreProbe::Sqlite { .. } => format!(
            "{:?}",
            lash_sqlite_store::SqliteStoreSetOptions::standard(
                lash_sqlite_store::SqliteSynchronous::Normal
            )
        ),
        StoreProbe::Postgres { pool } => {
            let settings = crate::perf_support::postgres_settings(pool).await?;
            experiment.postgres_settings = Some(settings.clone());
            format!(
                "postgres={settings};attachments={:?}",
                lash_sqlite_store::SqliteStoreSetOptions::standard(
                    lash_sqlite_store::SqliteSynchronous::Normal
                )
            )
        }
    };
    let mut meter = Meter::install(probe, None, options.sql_shapes.is_some(), traffic)?;
    let mut sql_spool = options
        .sql_shapes
        .as_deref()
        .map(super::sql_work::Spool::new)
        .transpose()?;
    let backend_name = meter.backend().to_owned();
    lash_core::perf_witness::sql_shapes::work::set_window(None, "session_start");
    let (start_before, start_shapes, _) = meter.read_with_shapes(0).await;
    let pages = stores.durable_store().pages();
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .config(replay_settings())
        .build()
        .map_err(|error| anyhow::anyhow!("build the durable backend: {error}"))?;
    let mut book = ScriptBook::new(workload.protocol);
    let scripted: Vec<Vec<(ScriptedTurn, String)>> = (0..workload.sessions)
        .map(|session| {
            workload
                .expand(session, options.turns)
                .into_iter()
                .map(|turn| {
                    let input = book.register(session, &turn);
                    (turn, input)
                })
                .collect()
        })
        .collect();
    let book = Arc::new(book);
    let workers = (workload.protocol != Protocol::Standard)
        .then(|| {
            super::workers::service(
                workload.protocol == Protocol::Python,
                &options.worker_profile_dir,
            )
        })
        .transpose()?;
    let workers = workers.map(|service| {
        if options.effect_phases.is_some() && !options.effect_phases_off {
            service.with_effect_phases(32_768)
        } else {
            service
        }
    });
    let mut geometry: serde_json::Value = serde_json::from_str(&experiment.configured_geometry)?;
    geometry["worker"] = serde_json::json!(workers.as_ref().map_or_else(
        || Some("none (standard protocol)".to_owned()),
        |service| crate::perf_support::git::artifact_sha256(&service.config().entry.executable)
    ));
    if options.effect_phases.is_some() {
        geometry["effect_phases"] = serde_json::json!(if options.effect_phases_off {
            "off; identity receipts only"
        } else {
            "bounded host-clock effect phases"
        });
    }
    experiment.configured_geometry = geometry.to_string();
    // The history run label can be "local". Fingerprint the actual captured
    // producer/experiment metadata once for this run's artifact join key.
    use sha2::{Digest, Sha256};
    let experiment_id = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&serde_json::json!({
            "identity": options.identity, "experiment": experiment,
        }))?)
    );
    let core = build_core(
        workload,
        &backend,
        Arc::clone(&book),
        workers.as_ref(),
        provider::fixtures()?,
    )?;
    let cleanup = lash::testing::quiesce_artifact_cleanup(&core).await;
    let pool = workers
        .as_ref()
        .map(lash::vm::WorkerService::pool)
        .transpose()
        .map_err(|error| anyhow::anyhow!("start the worker pool: {error}"))?;
    meter.attach_workers(pool);
    let mut lanes = Vec::with_capacity(workload.sessions);
    for (index, inputs) in scripted.into_iter().enumerate() {
        let id = lash::SessionId::try_from(format!("replay-{}-{index}", workload.name))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        core.session(id.clone())
            .create(lash::SessionCreation::root(
                lash::SessionSpec::new(
                    provider::MODEL,
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1_000_000),
                )
                .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
            ))
            .await
            .map_err(|error| anyhow::anyhow!("create {id}: {error}"))?;
        let session = core
            .session(id.clone())
            .open()
            .await
            .map_err(|error| anyhow::anyhow!("open {id}: {error}"))?;
        lanes.push(Lane {
            index,
            session,
            inputs,
        });
    }
    let turns = lanes.first().map_or(0, |lane| lane.inputs.len());
    if let Some(capture) = &mut phase_capture {
        capture.attach(
            workers
                .as_ref()
                .map(lash::vm::WorkerService::pool)
                .transpose()?
                .as_ref(),
            &backend_name,
            &experiment,
        )?;
    }
    let mut rounds = Vec::with_capacity(turns);
    let mut compactions = Vec::new();
    let mut frame_depth = 0;
    let mut model_calls = 0_u64;
    let mut shape_rounds = Vec::new();
    let (start_after, end_shapes, _) = meter.read_with_shapes(0).await;
    if let (Some(before), Some(after)) = (start_shapes, end_shapes) {
        let mut row = sql_shape_round(0, before, after, &difference(&start_before, &start_after))?;
        row["phase"] = serde_json::json!("session_start");
        shape_rounds.push(row);
    }
    let mut heap_window = None;
    let profile_end = options
        .heap
        .profile_through_turn
        .or(options.heap.profile_turn);
    let mut profiled = false;
    for turn in 0..turns {
        lash_core::perf_witness::sql_shapes::work::set_window(Some(turn + 1), "maintenance");
        if let Some(spool) = &mut sql_spool {
            spool.flush_completed()?;
        }
        cleanup
            .settle()
            .await
            .context("settle between-round artifact cleanup")?;
        retire(pages.as_deref())
            .await
            .context("retire the pages the last round left behind")?;
        if options.heap.dhat_out.is_some() && options.heap.profile_turn == Some(turn + 1) {
            heap_window = Some(crate::perf_support::dhat::ProfileWindow::start(
                "replay-host",
                "selected replay turns: before meter read through outcome and after meter read; excludes compaction",
                &crate::perf_support::dhat::ProfileArgs {
                    dhat_out: options.heap.dhat_out.clone(),
                    dhat_frames: options.heap.dhat_frames,
                    ..Default::default()
                },
            )?);
        }
        lash_core::perf_witness::sql_shapes::work::set_window(Some(turn + 1), "turn");
        let (before, shapes_before, traffic_before) = meter.read_with_shapes(model_calls).await;
        let phase_first = phase_capture
            .as_ref()
            .and_then(super::effect_phases::Capture::cut);
        let started = Instant::now();
        let outcomes = tokio::time::timeout(
            ROUND_WATCHDOG,
            futures_util::future::join_all(
                lanes
                    .iter()
                    .map(|lane| send(lane, turn, phase_capture.is_some())),
            ),
        )
        .await
        .with_context(|| format!("{} turn {} never settled", workload.name, turn + 1))?;
        let wall_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let mut result_digests = Vec::new();
        for (lane, outcome) in lanes.iter().zip(outcomes) {
            let digest = outcome.with_context(|| {
                format!("{} session {} turn {}", workload.name, lane.index, turn + 1)
            })?;
            if let Some(digest) = digest {
                result_digests.push(digest);
            }
            let (scripted, _) = &lane.inputs[turn];
            let calls = book.take_calls(lane.index, scripted.ordinal);
            ensure!(
                calls.scripted == scripted.replies.len() && calls.past_script == 0,
                "{} session {} turn {} left its script: {} scripted and {} unscripted model \
                 calls for {} replies (a cell or tool failed, or the turn ended early); the \
                 last unscripted request ended: {}",
                workload.name,
                lane.index,
                turn + 1,
                calls.scripted,
                calls.past_script,
                scripted.replies.len(),
                calls.last_past_script.as_deref().unwrap_or("(none)")
            );
            model_calls += calls.scripted as u64;
        }
        let (after, shapes_after, traffic_after) = meter.read_with_shapes(model_calls).await;
        if let Some(capture) = &mut phase_capture {
            capture.turn(turn + 1, phase_first, workload.sessions, result_digests);
        }
        if profile_end == Some(turn + 1)
            && let Some(window) = heap_window.take()
        {
            window.finish(true)?;
            let path = options
                .heap
                .dhat_out
                .as_ref()
                .context("active heap window has no output path")?
                .with_extension("receipt.json");
            let mut receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            receipt["workload"] = serde_json::json!(workload.name);
            receipt["script_digest"] = serde_json::json!(workload.digest());
            receipt["first_turn"] = serde_json::json!(options.heap.profile_turn);
            receipt["last_turn"] = serde_json::json!(turn + 1);
            receipt["identity"] = serde_json::to_value(&options.identity)?;
            receipt["experiment"] = serde_json::to_value(&experiment)?;
            std::fs::write(path, serde_json::to_vec_pretty(&receipt)?)?;
            profiled = true;
        }
        let mut metrics = difference(&before, &after);
        if let (Some(before), Some(after)) = (shapes_before, shapes_after) {
            let mut row = sql_shape_round(turn + 1, before, after, &metrics)?;
            row["phase"] = serde_json::json!("turn");
            shape_rounds.push(row);
        }
        metrics.insert("wall_ms".to_owned(), wall_ms);
        rounds.push(RoundRecord {
            turn: turn + 1,
            history_depth: turn,
            frame_depth,
            sessions: lanes.len(),
            setup: lanes[0].inputs[turn].0.setup,
            traffic: traffic_after
                .zip(traffic_before)
                .map(|(after, before)| after.difference(&before, &metrics))
                .transpose()?,
            metrics,
        });
        frame_depth += 1;
        if workload
            .compact_every
            .is_some_and(|every| (turn + 1) % every == 0 && turn + 1 < turns)
        {
            lash_core::perf_witness::sql_shapes::work::set_window(Some(turn + 1), "compaction");
            let (before, shapes_before, traffic_before) = meter.read_with_shapes(model_calls).await;
            let started = Instant::now();
            for lane in &lanes {
                compact(lane, turn + 1).await?;
            }
            let (after, shapes_after, traffic_after) = meter.read_with_shapes(model_calls).await;
            let mut metrics = difference(&before, &after);
            if let (Some(before), Some(after)) = (shapes_before, shapes_after) {
                let mut row = sql_shape_round(turn + 1, before, after, &metrics)?;
                row["phase"] = serde_json::json!("compaction");
                shape_rounds.push(row);
            }
            metrics.insert(
                "wall_ms".to_owned(),
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            compactions.push(RoundRecord {
                turn: turn + 1,
                history_depth: turn + 1,
                frame_depth,
                sessions: lanes.len(),
                setup: false,
                traffic: traffic_after
                    .zip(traffic_before)
                    .map(|(after, before)| after.difference(&before, &metrics))
                    .transpose()?,
                metrics,
            });
            frame_depth = 0;
        }
    }
    if options.heap.dhat_out.is_some() {
        ensure!(
            profiled,
            "heap profile did not reach its selected final turn"
        );
    }
    if let Some(path) = &options.sql_shapes {
        crate::perf_support::report::ensure_parent_dir(path, "SQL shapes")?;
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "kind": "lash.replay-sql-shapes", "identity": options.identity, "experiment": experiment, "experiment_id": experiment_id,
                "workload": workload.name, "script_digest": workload.digest(),
                "backend": backend_name, "scope": "all observed store executions in this host process; same counter cuts as replay rounds",
                "row_limit": lash_core::perf_witness::sql_shapes::ROW_LIMIT,
                "conserved": true, "rounds": shape_rounds,
            }))?,
        )?;
    }
    if let Some(capture) = phase_capture {
        capture.finish()?;
    }
    lash_core::perf_witness::sql_shapes::work::set_window(None, "shutdown");
    if let Some(spool) = &mut sql_spool {
        spool.flush_completed()?;
    }
    for lane in lanes {
        let _ = lane.session.close().await;
    }
    core.shutdown()
        .await
        .map_err(|error| anyhow::anyhow!("shut the replay core down: {error}"))?;
    if let Some(path) = &options.sql_shapes {
        let (work, reference) = meter.sql_work(
            sql_spool
                .take()
                .context("SQL-work spool was not installed")?,
        )?;
        super::sql_work::write(
            path,
            (&options.identity, &experiment, &experiment_id),
            workload,
            &backend_name,
            turns,
            (&work, &reference),
        )?;
    }
    drop(meter);
    drop(keep);
    Ok(WorkloadReceipt {
        experiment,
        workload: workload.name.clone(),
        script_digest: workload.digest(),
        protocol: serde_json::to_value(workload.protocol)?
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        backend: backend_name,
        sessions: workload.sessions,
        turns,
        compact_every: workload.compact_every,
        units: units(&rounds),
        round_statistic: "sum over the round's sessions; summaries divide by sessions".to_owned(),
        summary: summarize(&rounds),
        rounds,
        compactions,
    })
}

/// Send `lane`'s turn `turn` and follow it to its outcome; refuse anything
/// but an answer.
async fn send(lane: &Lane, turn: usize, observe_result: bool) -> Result<Option<String>> {
    let (_, input) = &lane.inputs[turn];
    let handle = lane
        .session
        .send(lash::TurnInput::text(input.clone()))
        .into_future()
        .await
        .map_err(|error| anyhow::anyhow!("send: {error}"))?;
    let outcome = handle
        .outcome()
        .await
        .map_err(|error| anyhow::anyhow!("outcome: {error}"))?;
    match outcome.output() {
        Some(output) if output.is_success() => {
            if observe_result {
                use sha2::{Digest as _, Sha256};
                Ok(Some(format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&output.outcome)?)
                )))
            } else {
                Ok(None)
            }
        }
        Some(output) => bail!(
            "the turn did not answer: {:?}; issues: {:?}",
            output.status(),
            output.outcome.error_cause()
        ),
        None => bail!("the turn has no output: {:?}", outcome.status()),
    }
}

/// Compact `lane`'s context after turn `after`, keyed so a retry is one
/// compaction.
async fn compact(lane: &Lane, after: usize) -> Result<()> {
    let admin = lane.session.admin();
    let opened = admin
        .state()
        .compact_context(None, format!("replay-compact-{}-{after}", lane.index))
        .await
        .map_err(|error| anyhow::anyhow!("compact after turn {after}: {error}"))?
        .settle_with(&admin.commands(), lash::testing::admin_fixture_outcome)
        .await
        .map_err(|error| anyhow::anyhow!("settle the compaction after turn {after}: {error}"))?;
    ensure!(opened, "the compaction after turn {after} opened no frame");
    Ok(())
}

/// Subtract cumulative shape counters, then verify against the existing turn
/// metrics. ROW and PROFILE events may straddle a cut; each keeps its own unit.
fn sql_shape_round(
    turn: usize,
    before: lash_core::perf_witness::sql_shapes::Receipt,
    after: lash_core::perf_witness::sql_shapes::Receipt,
    metrics: &std::collections::BTreeMap<String, f64>,
) -> Result<serde_json::Value> {
    use lash_core_ids::perf_witness::sql_shapes::Counts;
    use lash_core_ids::perf_witness::sql_shapes::Row;
    ensure!(
        after.overflow == Counts::default(),
        "SQL shape attribution exceeded its row limit; refusing incomplete export"
    );
    let old: std::collections::BTreeMap<_, _> = before
        .rows
        .into_iter()
        .map(|row| ((row.shape, row.store_origin), row.counts))
        .collect();
    let rows: Vec<_> = after
        .rows
        .into_iter()
        .filter_map(|row| {
            let zero = Counts::default();
            let previous = old
                .get(&(row.shape.clone(), row.store_origin.clone()))
                .unwrap_or(&zero);
            let counts = Counts {
                statements: row.counts.statements - previous.statements,
                selects: row.counts.selects - previous.selects,
                rows_returned: row.counts.rows_returned - previous.rows_returned,
            };
            (counts != zero).then_some(Row { counts, ..row })
        })
        .collect();
    let statements: u64 = rows.iter().map(|r| r.counts.statements).sum();
    let selects: u64 = rows.iter().map(|r| r.counts.selects).sum();
    let returned: u64 = rows.iter().map(|r| r.counts.rows_returned).sum();
    ensure!(
        statements as f64 == metrics["sql_statements"]
            && selects as f64 == metrics["sql_selects"]
            && returned as f64 == metrics["sql_rows_read"],
        "turn {turn}: SQL shape sums differ from replay totals"
    );
    Ok(serde_json::json!({"turn": turn, "conserved": true,
        "totals": {"statements": statements, "selects": selects, "rows_returned": returned}, "rows": rows}))
}
