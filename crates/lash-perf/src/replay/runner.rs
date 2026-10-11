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
    pub store: StoreChoice,
    /// Run only the first this many turns of each session.
    pub turns: Option<usize>,
}

/// What a store set must outlive.
type Keep = Vec<Box<dyn std::any::Any + Send>>;

async fn open_stores(
    choice: &StoreChoice,
    workload: &str,
) -> Result<(Arc<dyn lash_core_execution::StoreSet>, StoreProbe, Keep)> {
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

fn build_core(
    workload: &Workload,
    backend: &lash::Backend,
    book: Arc<ScriptBook>,
    workers: Option<&lash::vm::WorkerService>,
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
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(provider::model(book), provider::metadata()?)
        .tools(provider::fixtures()?)
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

/// Run `workload` once and return its receipt.
pub async fn run(workload: &Workload, options: &RunOptions) -> Result<WorkloadReceipt> {
    let (stores, probe, keep) = open_stores(&options.store, &workload.name).await?;
    let backend = lash::durable::DurableBackendBuilder::new(stores)
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
        .then(|| super::workers::service(workload.protocol == Protocol::Python))
        .transpose()?;
    let core = build_core(workload, &backend, Arc::clone(&book), workers.as_ref())?;
    let cleanup = lash::testing::quiesce_artifact_cleanup(&core).await;
    let pool = workers
        .as_ref()
        .map(lash::vm::WorkerService::pool)
        .transpose()
        .map_err(|error| anyhow::anyhow!("start the worker pool: {error}"))?;
    let mut lanes = Vec::with_capacity(workload.sessions);
    for (index, inputs) in scripted.into_iter().enumerate() {
        let id = lash::SessionId::try_from(format!("replay-{}-{index}", workload.name))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        core.session(id.clone())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
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
    let meter = Meter::install(probe, pool)?;
    let backend_name = meter.backend().to_owned();
    let mut rounds = Vec::with_capacity(turns);
    let mut compactions = Vec::new();
    let mut frame_depth = 0;
    let mut model_calls = 0_u64;
    for turn in 0..turns {
        cleanup
            .settle()
            .await
            .context("settle between-round artifact cleanup")?;
        let before = meter.read(model_calls).await;
        let started = Instant::now();
        let outcomes = tokio::time::timeout(
            ROUND_WATCHDOG,
            futures_util::future::join_all(lanes.iter().map(|lane| send(lane, turn))),
        )
        .await
        .with_context(|| format!("{} turn {} never settled", workload.name, turn + 1))?;
        let wall_ms = started.elapsed().as_secs_f64() * 1_000.0;
        for (lane, outcome) in lanes.iter().zip(outcomes) {
            outcome.with_context(|| {
                format!("{} session {} turn {}", workload.name, lane.index, turn + 1)
            })?;
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
        let mut metrics = difference(&before, &meter.read(model_calls).await);
        metrics.insert("wall_ms".to_owned(), wall_ms);
        rounds.push(RoundRecord {
            turn: turn + 1,
            history_depth: turn,
            frame_depth,
            sessions: lanes.len(),
            setup: lanes[0].inputs[turn].0.setup,
            metrics,
        });
        frame_depth += 1;
        if workload
            .compact_every
            .is_some_and(|every| (turn + 1) % every == 0 && turn + 1 < turns)
        {
            let before = meter.read(model_calls).await;
            let started = Instant::now();
            for lane in &lanes {
                compact(lane, turn + 1).await?;
            }
            let mut metrics = difference(&before, &meter.read(model_calls).await);
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
                metrics,
            });
            frame_depth = 0;
        }
    }
    drop(meter);
    for lane in lanes {
        let _ = lane.session.close().await;
    }
    core.shutdown()
        .await
        .map_err(|error| anyhow::anyhow!("shut the replay core down: {error}"))?;
    drop(keep);
    Ok(WorkloadReceipt {
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
async fn send(lane: &Lane, turn: usize) -> Result<()> {
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
        Some(output) if output.is_success() => Ok(()),
        Some(output) => bail!(
            "the turn did not answer: {:?}; issues: {:?}",
            output.status(),
            output.result.errors
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
