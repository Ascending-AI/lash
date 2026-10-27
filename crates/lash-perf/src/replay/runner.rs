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
    /// Counted SQLite execution with one issuing task and synchronous store calls.
    CountedSqliteFile(std::path::PathBuf),
    /// An isolated database on the PostgreSQL server at this URL.
    Postgres(String),
}

/// One run's settings.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub identity: super::receipt::Identity,
    pub worker_resources: bool,
    pub instructions: bool,
    pub store: StoreChoice,
    /// Run only the first this many turns of each session.
    pub turns: Option<usize>,
    /// Separate Python-worker heap profiles beside the replay receipt.
    pub worker_profile_dir: std::path::PathBuf,
    pub sql_shapes: Option<std::path::PathBuf>,
    pub turn_report: Option<std::path::PathBuf>,
    pub traffic: bool,
    pub effect_phases: Option<std::path::PathBuf>,
    pub effect_phases_off: bool,
    pub heap: super::ReplayHeapArgs,
}

#[derive(Debug)]
struct CountedClock(Instant);

#[async_trait::async_trait]
impl lash_core::Clock for CountedClock {
    fn now(&self) -> Instant {
        self.0
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::UNIX_EPOCH + chrono::TimeDelta::seconds(1_700_000_000)
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        tokio::time::sleep(deadline.saturating_duration_since(self.0)).await;
    }
}

fn sqlite_options(counted: bool) -> lash_sqlite_store::SqliteStoreSetOptions {
    let mut options = lash_sqlite_store::SqliteStoreSetOptions::standard(
        lash_sqlite_store::SqliteSynchronous::Normal,
    );
    if counted {
        options.inline_calls = true;
        options.store.connection_policy.wal_autocheckpoint_pages = 0;
        options.store.connection_policy.operational.wake_poll = Duration::from_secs(24 * 60 * 60);
    }
    options
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
        StoreChoice::SqliteFile(dir) | StoreChoice::CountedSqliteFile(dir) => {
            let dir = dir.join(workload);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            let counted = matches!(choice, StoreChoice::CountedSqliteFile(_));
            let clock: Arc<dyn lash_core::Clock> = if counted {
                Arc::new(CountedClock(Instant::now()))
            } else {
                Arc::new(lash_core_ids::clock::SystemClock)
            };
            let stores = lash::sqlite::SqliteStoreSet::open_with_options_and_clock(
                dir.join("lash.db"),
                sqlite_options(counted),
                clock,
            )
            .await
            .map_err(|error| anyhow::anyhow!("open the SQLite store set: {error}"))?;
            let probe = StoreProbe::sqlite(&stores)?;
            let stores: Arc<dyn lash_execution_contracts::backend::StoreSet> = Arc::new(stores);
            let stores = if counted {
                lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                    .without_node_wakes()
                    .with_binding_identity(lash_execution_contracts::backend::StoreBindingId::new(
                        Arc::<str>::from(format!("sqlite:/tmp/lash-counted-00000000-0000-4000-8000-000000000000/{workload}/lash.db")),
                    ))
                    .into_store_set()
            } else {
                stores
            };
            Ok((stores, probe, Vec::new()))
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
    build_core_with_sink(workload, backend, book, workers, tools, None, false)
}

fn build_core_with_sink(
    workload: &Workload,
    backend: &lash::Backend,
    book: Arc<ScriptBook>,
    workers: Option<&lash::vm::WorkerService>,
    tools: Arc<dyn lash_core::ToolProvider>,
    sink: Option<Arc<lash::testing::RecordingTraceSink>>,
    counted: bool,
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
                    .instruction_limit(workload.instruction_limit.map_or(
                        crate::cell_bounds::INSTRUCTION_LIMIT,
                        lash::rlm::InstructionBound::instructions,
                    ))
                    .memory_limit(workload.memory_limit_mib.map_or(
                        crate::cell_bounds::MEMORY_LIMIT,
                        lash::rlm::MemoryBound::mebibytes,
                    ))
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
    let builder = if let Some(sink) = sink {
        builder.trace_sink(sink)
    } else {
        builder
    };
    let builder = if counted {
        let day = Duration::from_secs(24 * 60 * 60);
        let poll = lash::PollPacing::new(day, day)?;
        let live = lash::ToolLivePolicy::default();
        let live = lash::ToolLivePolicy::new(
            live.events(),
            live.logs(),
            live.bytes(),
            live.fact_bytes(),
            live.progress_bytes(),
            live.calls(),
            Duration::from_secs(60),
            live.write_timeout(),
            Duration::from_secs(3600),
        )?;
        builder
            .tool_live_policy(live)
            .observer_pacing(lash::ObserverPacing {
                follow: poll,
                terminal: poll,
                ..lash::ObserverPacing::standard()
            })
    } else {
        builder
    };
    let builder = builder
        .serve_sessions(true)
        .commit_budget(lash::CommitBudget::bounded(64 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(provider::model(book), provider::metadata()?);
    let builder = if workload.host.is_some() {
        let host = super::capabilities::HostTools::new(workload.host.as_ref())?;
        builder.plugin(host.factory(tools)?)
    } else {
        builder.plugin(Arc::new(crate::perf_support::tool_plugin(
            "tools",
            lash::tool_provider_key!("tools"),
            tools,
        )?))
    };
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("lash-perf-replay"),
            lash::persistence::LeaseIncarnationId::new(if counted {
                // PID digit count changes repeated string and encoded-buffer
                // allocations, even on otherwise identical private captures.
                "1000000".to_owned()
            } else {
                std::process::id().to_string()
            }),
        ))
        .map_err(|error| anyhow::anyhow!("build the replay core: {error}"))
}

/// One session the run drives: its live handle and its scripted inputs.
struct Lane {
    index: usize,
    session: lash::LashSession,
    inputs: Vec<(ScriptedTurn, String)>,
    counted: bool,
}

/// The node's settings with page retirement left to the rounds' gaps: the
/// background sweep fires on wall-clock time, so whichever round it lands
/// in would carry another round's deletes. Retirement is maintenance, like
/// artifact cleanup, and settles between rounds the same way (FIG-6321).
fn replay_settings() -> lash_durable::DurableSettings {
    let mut settings = lash_durable::DurableSettings::standard();
    let day = Duration::from_secs(24 * 60 * 60);
    settings.retirement.every = day;
    // This private single-node fixture is driven by after-commit wake hints.
    // Periodic lease/reap/liveness polls otherwise become work proportional
    // to executor descheduling rather than the scripted foreground turns.
    settings.lease.heartbeat_every = day;
    settings.lease.self_stop_after = day * 2;
    settings.lease.ttl = day * 3;
    settings.lease.reap_every = day;
    settings.lease.claim_poll = day;
    settings.lease.claim_backoff = day;
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
            && options.identity.turn_profile == options.turn_report.is_some()
            && options.identity.traffic == options.traffic
            && options.identity.worker_resources == options.worker_resources
            && options.identity.instructions == options.instructions
            && options.identity.heap_window
                == options
                    .heap
                    .profile_turn
                    .map(|first| [first, options.heap.profile_through_turn.unwrap_or(first)]),
        "replay observer identity differs from selected instruments"
    );
    ensure!(
        !workload.turns.iter().any(|group| group.oracle.is_some())
            || options
                .turns
                .is_none_or(|limit| limit >= workload.turn_count()),
        "a checked workload must run through every oracle turn"
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
        "runtime_workers": tokio::runtime::Handle::current().metrics().num_workers(),
        "input_policy": if matches!(options.store, StoreChoice::CountedSqliteFile(_)) {
            "fixed injected clock, send IDs, lease incarnation, sequential environment identities and same-length logical publication domain; private physical files; local wakes only; facade recovery polls deferred beyond run; tool-live flush 60s and slot linger 3600s"
        } else {
            "system clock; ordinary identity sources"
        },
        "maintenance_policy": "private single node; after-commit wakes; periodic housekeeping deferred beyond the run",
        "worker_resources": options.worker_resources, "instructions": options.instructions,
        "sql_shapes": options.identity.sql_shapes, "traffic": options.identity.traffic,
        "turn_profile": options.identity.turn_profile,
        "heap_window": options.identity.heap_window, "heap_frames": options.heap.dhat_frames,
        "window": "round first send through last successful outcome; counters at settled cuts",
        "sql_work_window": options.sql_shapes.as_ref().map(|_| "node startup through core shutdown; start/finish phase cuts; includes unowned and cross-window SQL"),
        "terminal": "all scripted replies validated; successful outcomes",
    }));
    experiment.backend = probe.backend().to_owned();
    experiment.workload = format!("replay/{}:{}", workload.name, workload.digest());
    experiment.configured_durability = match &probe {
        StoreProbe::Sqlite { .. } => format!(
            "{:?}",
            sqlite_options(matches!(options.store, StoreChoice::CountedSqliteFile(_)))
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
    lash_core::perf_witness::sql_shapes::work::set_window(None, "node_start");
    let (start_before, start_shapes, _) = meter.read_with_shapes(0).await;
    let pages = stores.durable_store().pages();
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .config(replay_settings())
        .build()
        .map_err(|error| anyhow::anyhow!("build the durable backend: {error}"))?;
    let mut book = ScriptBook::new();
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
                workload.instruction_limit,
                options.worker_resources,
                options.instructions,
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
    geometry["host"] = serde_json::to_value(&workload.host)?;
    if let Some(limit) = workload.instruction_limit {
        geometry["cell_instruction_limit"] = serde_json::json!(limit);
    }
    geometry["cell_memory_mib"] = serde_json::json!(workload.memory_limit_mib.unwrap_or(512));
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
    let capability_counts = lash_core::testing::CallbackCounts::default();
    let host_tools = workload
        .turns
        .iter()
        .any(|turn| turn.capability_turn.is_some())
        .then(|| super::capabilities::HostTools::new(workload.host.as_ref()))
        .transpose()?;
    let tools = match &host_tools {
        Some(host) => host.counted(&capability_counts),
        None => provider::fixtures()?,
    };
    let turn_sink = options
        .turn_report
        .as_ref()
        .map(|_| Arc::new(lash::testing::RecordingTraceSink::new()));
    let core = build_core_with_sink(
        workload,
        &backend,
        Arc::clone(&book),
        workers.as_ref(),
        tools,
        turn_sink.clone(),
        matches!(options.store, StoreChoice::CountedSqliteFile(_)),
    )?;
    // Registration follows package holding. Finish both before session and
    // turn cuts, so install SQL belongs to startup even on a slow store.
    tokio::time::timeout(ROUND_WATCHDOG, async {
        while backend.durable().live_nodes().await?.is_empty() {
            tokio::task::yield_now().await;
        }
        Ok::<_, lash_durable::DurableError>(())
    })
    .await
    .context("the replay node never registered")?
    .map_err(|error| anyhow::anyhow!("the replay node's registration: {error}"))?;
    let (node_after, node_shapes, _) = meter.read_with_shapes(0).await;
    let mut shape_rounds = Vec::new();
    let mut turn_profiles = Vec::new();
    if let (Some(before), Some(after)) = (start_shapes, node_shapes) {
        let mut row = sql_shape_round(0, before, after, &difference(&start_before, &node_after))?;
        row["phase"] = serde_json::json!("node_start");
        shape_rounds.push(row);
    }
    lash_core::perf_witness::sql_shapes::work::set_window(None, "session_start");
    let (start_before, start_shapes, _) = meter.read_with_shapes(0).await;
    let cleanup = lash::testing::quiesce_artifact_cleanup(&core).await;
    let pool = workers
        .as_ref()
        .map(lash::vm::WorkerService::pool)
        .transpose()
        .map_err(|error| anyhow::anyhow!("start the worker pool: {error}"))?;
    meter.attach_workers(pool);
    let live_before = crate::GLOBAL_ALLOCATOR.stats();
    let mut lanes = Vec::with_capacity(workload.sessions);
    for (index, inputs) in scripted.into_iter().enumerate() {
        let id = lash_kernel_value::identity::SessionId::try_from(format!(
            "replay-{}-{index}",
            workload.name
        ))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        let spec = lash::SessionSpec::new(
            provider::MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1_000_000),
            lash_core::NoProgressBudget::bounded(12),
        );
        let spec = if host_tools.is_some() {
            spec.attachment_acceptance(super::capabilities::acceptance())
        } else {
            spec
        };
        let mut creation = lash::SessionCreation::root(spec);
        if workload.host.is_some() {
            creation = creation.with_prompt_plan(lash::prompt::PromptPlan {
                limits: lash::prompt::PromptLimits {
                    max_section_bytes: std::num::NonZeroU32::new(128 * 1024)
                        .context("host section bytes")?,
                    max_total_bytes: std::num::NonZeroU32::new(512 * 1024)
                        .context("host total prompt bytes")?,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        core.session(id.clone())
            .create(creation)
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
            counted: matches!(options.store, StoreChoice::CountedSqliteFile(_)),
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
    let off_cpu_marks = std::env::var_os("LASH_OFF_CPU_MARKS").is_some();
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
        let callbacks_before = host_tools
            .as_ref()
            .map(|_| capability_counts.provider_snapshot());
        let authorized_before = host_tools
            .as_ref()
            .map_or(0, |host| host.authorized_calls());
        if off_cpu_marks {
            crate::perf_support::time::instruction_control("enable")?;
        }
        let pmu_before = options
            .instructions
            .then(crate::perf_support::instructions::host_snapshot)
            .transpose()?;
        let started = Instant::now();
        let outcomes = tokio::time::timeout(
            ROUND_WATCHDOG,
            futures_util::future::join_all(lanes.iter().map(|lane| async {
                let started = Instant::now();
                let client = options
                    .turn_report
                    .as_ref()
                    .map(|_| lash_core_ids::operational_metrics::turn::Context::default());
                let future = send(
                    lane,
                    turn,
                    phase_capture.is_some(),
                    host_tools.as_deref(),
                    workload.host.as_ref(),
                );
                let outcome = match &client {
                    Some(context) => {
                        context
                            .within(async {
                                let window = lash_core_ids::operational_metrics::sql::Window::new(
                                    "facade", "mixed",
                                );
                                window.within(future).await
                            })
                            .await
                    }
                    None => future.await,
                };
                (
                    outcome,
                    started.elapsed().as_secs_f64() * 1000.0,
                    client.map(|c| c.snapshot()),
                )
            })),
        )
        .await
        .with_context(|| format!("{} turn {} never settled", workload.name, turn + 1))?;
        let wall_ms = started.elapsed().as_secs_f64() * 1_000.0;
        if off_cpu_marks {
            crate::perf_support::time::instruction_control("disable")?;
        }
        let mut result_digests = Vec::new();
        let mut oracle_values = Vec::new();
        let mut session_wall_ms = Vec::new();
        for (lane, (outcome, session_ms, client)) in lanes.iter().zip(outcomes) {
            if let Some(client) = client {
                turn_profiles.push(client);
            }
            session_wall_ms.push(session_ms);
            let (digest, oracle_value, run) = outcome.with_context(|| {
                format!("{} session {} turn {}", workload.name, lane.index, turn + 1)
            })?;
            if let Some(digest) = digest {
                result_digests.push(digest);
            }
            if let (Some(sink), Some(client)) = (&turn_sink, turn_profiles.last_mut()) {
                core.flush_trace_sink()
                    .map_err(|error| anyhow::anyhow!("flush turn report: {error}"))?;
                for record in sink.records() {
                    if record
                        .context
                        .turn_id
                        .as_ref()
                        .is_some_and(|id| id.as_str() == run)
                        && let lash::tracing::TraceEvent::TurnCompleted {
                            profile: Some(owner),
                            ..
                        } = record.event
                    {
                        super::turn_profile::merge(client, owner);
                    }
                }
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
            if let Some(oracle) = &scripted.oracle {
                oracle_values.push((
                    oracle,
                    oracle_value.context("oracle turn has no finish value")?,
                ));
            }
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
        if let Some(bodies) = metrics.get("tool_bodies").copied().filter(|n| *n > 0.0) {
            for name in [
                "provider_snapshots",
                "provider_manifests",
                "provider_link_payloads",
            ] {
                if let Some(entries) = metrics.get(name).copied() {
                    metrics.insert(format!("{name}_per_tool_body"), entries / bodies);
                }
            }
        }
        for (oracle, value) in oracle_values {
            let keys = oracle
                .check(&value)
                .with_context(|| format!("turn {} result oracle", turn + 1))?;
            let super::oracle::Oracle::RecordKeys {
                records,
                offset,
                sorted,
                ..
            } = *oracle;
            metrics.insert("oracle_record_count".to_owned(), records as f64);
            metrics.insert("oracle_offset".to_owned(), offset as f64);
            metrics.insert("oracle_keys_checked".to_owned(), keys.len() as f64);
            for (name, position) in [
                ("first", 0),
                ("middle", keys.len() / 2),
                ("last", keys.len() - 1),
            ] {
                metrics.insert(format!("oracle_{name}_key"), keys[position] as f64);
            }
            metrics.insert("oracle_sorted".to_owned(), f64::from(u8::from(sorted)));
        }
        if let Some(mode) = lanes[0].inputs[turn].0.capability_turn {
            metrics.extend(super::capabilities::metrics(
                callbacks_before
                    .as_ref()
                    .context("capability callbacks were not observed")?,
                &capability_counts,
            ));
            let authorized = host_tools
                .as_ref()
                .map_or(0, |host| host.authorized_calls())
                - authorized_before;
            super::capabilities::check(mode, &metrics, authorized)?;
        }
        if let Some(before) = pmu_before {
            let delta = crate::perf_support::instructions::host_snapshot()?
                .since(before)?
                .qualify()?;
            metrics.insert("host_instructions".into(), delta.instructions as f64);
            metrics.insert(
                "host_instruction_enabled_ns".into(),
                delta.time_enabled as f64,
            );
            metrics.insert(
                "host_instruction_running_ns".into(),
                delta.time_running as f64,
            );
        }
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
            session_wall_ms,
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
            if let Some(bodies) = metrics.get("tool_bodies").copied().filter(|n| *n > 0.0) {
                for name in [
                    "provider_snapshots",
                    "provider_manifests",
                    "provider_link_payloads",
                ] {
                    if let Some(entries) = metrics.get(name).copied() {
                        metrics.insert(format!("{name}_per_tool_body"), entries / bodies);
                    }
                }
            }
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
                session_wall_ms: Vec::new(),
                traffic: traffic_after
                    .zip(traffic_before)
                    .map(|(after, before)| after.difference(&before, &metrics))
                    .transpose()?,
                metrics,
            });
            frame_depth = 0;
        }
    }
    // Keep actual facade handles alive through the final retained-byte cut.
    std::hint::black_box(&lanes);
    let live_after = crate::GLOBAL_ALLOCATOR.stats();
    let retained = (live_after.bytes_allocated as i128 - live_after.bytes_deallocated as i128)
        - (live_before.bytes_allocated as i128 - live_before.bytes_deallocated as i128);
    if let Some(last) = rounds.last_mut() {
        last.metrics
            .insert("facade_live_bytes".to_owned(), retained.max(0) as f64);
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
    if let (Some(path), Some(sink)) = (&options.turn_report, turn_sink) {
        core.flush_trace_sink()
            .map_err(|error| anyhow::anyhow!("flush turn report: {error}"))?;
        drop(sink);
        std::fs::write(path, serde_json::to_vec_pretty(&turn_profiles)?)?;
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
async fn send(
    lane: &Lane,
    turn: usize,
    observe_result: bool,
    host_tools: Option<&super::capabilities::HostTools>,
    host_geometry: Option<&super::script::HostGeometry>,
) -> Result<(Option<String>, Option<serde_json::Value>, String)> {
    let (scripted, input) = &lane.inputs[turn];
    let mut input = lash::TurnInput::text(input.clone());
    if scripted.capability_turn == Some(super::script::CapabilityTurn::FreshDocument) {
        let reference = lane
            .session
            .put_attachment(
                format!(
                    "Fresh document for session {} turn {}: checked host publication.",
                    lane.index,
                    turn + 1
                )
                .into_bytes(),
                lash_core::AttachmentCreateMeta::new(
                    "text/plain".parse()?,
                    None,
                    Some(format!("document-{}.txt", turn + 1)),
                ),
            )
            .await?;
        input = input.with_attachment(reference.reference);
        host_tools
            .context("fresh document requires host catalogue")?
            .attach_document();
    }
    let mut send = lane.session.send(input);
    if lane.counted {
        use sha2::{Digest as _, Sha256};
        let mut hash = Sha256::new();
        hash.update(lane.session.session_id().as_str());
        hash.update(turn.to_le_bytes());
        let digest = hash.finalize();
        let mut id = [0; 16];
        id.copy_from_slice(&digest[..16]);
        send = send.id(lash_vocabulary::identity::TurnId::from_uuid(
            uuid::Uuid::from_bytes(id).as_u128(),
        ));
    }
    let send = if scripted.capability_turn.is_some() || host_geometry.is_some() {
        send.run(
            host_tools
                .context("host turn requires its catalogue")?
                .run_spec(host_geometry, turn + 1)?,
        )
    } else {
        send
    };
    let handle =
        lash_core_ids::operational_metrics::turn::phase("admission", None, send.into_future())
            .await
            .map_err(|error| anyhow::anyhow!("send: {error}"))?;
    let outcome =
        lash_core_ids::operational_metrics::turn::phase("readback", None, handle.outcome())
            .await
            .map_err(|error| anyhow::anyhow!("outcome: {error}"))?;
    let run = outcome
        .run()
        .context("successful turn has no durable run id")?
        .turn_id()
        .to_string();
    match outcome.output() {
        Some(output) if output.is_success() => {
            let oracle_value = lane.inputs[turn]
                .0
                .oracle
                .as_ref()
                .and_then(|_| output.finished().map(|(_, value)| value.clone()));
            let digest = if observe_result {
                use sha2::{Digest as _, Sha256};
                Some(format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&output.outcome)?)
                ))
            } else {
                None
            };
            Ok((digest, oracle_value, run))
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
