//! `tool-batch-baseline` — the FIG-3398 pre-cutover measurement.
//!
//! One invocation measures one backend. For every (producer, width) cell it
//! drives the FIG-3400 conformance producers' batch through
//! [`lash_conformance::measure_tool_batch`] `reps` times and emits one JSONL
//! row per rep: the turn's wall time, the leaf window (first leaf start to
//! last leaf answer), the journal rows the batch wrote, and the box's load
//! average at the moment the rep ran. A number without its load figure is not
//! evidence; `scripts/tool-batch-baseline.sh` is the one-command entry point
//! that wires the services and enforces the quiet-box precondition.
//!
//! * `sqlite` opens a fresh `SqliteEffectHost` on `--db-path` and counts the
//!   delta in the effect database's tables per rep.
//! * `postgres` connects to `LASH_POSTGRES_DATABASE_URL` (or `--database-url`),
//!   creates an isolated database, and counts the same tables under their
//!   `lash_` names. The build carries lash-postgres-store's `testing` feature,
//!   so statement counts include its testing-only probes; row counts are
//!   unaffected.
//! * `restate` serves an endpoint binding the durable-wait pair plus a probe
//!   workflow whose handler runs the same measured turn through
//!   `RestateRuntimeEffectController`, registers it on the deployment at
//!   `--restate-admin-url`, and counts the invocation's `sys_journal` entries.
//!   Restate is serial today — `supports_concurrent_effects()` is hardcoded
//!   false — so its numbers record a serial baseline, not a defect.

#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged, same as crates/lash-restate"
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use futures_util::FutureExt as _;
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::Serialize;

/// The durable-wait authority the probe's controller and deployment host share.
const RESTATE_AUTHORITY: &str = "lash-perf-tool-batch";
/// The probe workflow's service name as Restate discovery reports it.
const PROBE_SERVICE: &str = "ToolBatchProbe";

#[derive(Parser)]
#[command(about = "FIG-3398 pre-cutover tool-batch baseline measurement")]
struct Args {
    /// Which backend to measure.
    #[arg(long, value_parser = ["sqlite", "postgres", "restate"])]
    backend: String,
    /// Batch widths to measure.
    #[arg(long, default_value = "2,8,50", value_delimiter = ',')]
    widths: Vec<usize>,
    /// Repetitions per (producer, width) cell.
    #[arg(long, default_value_t = 5)]
    reps: usize,
    /// Producers to measure: `standard` is the protocol's parallel model tool
    /// calls, `rlm` is `Promise.all` on the RLM bridge.
    #[arg(
        long,
        default_value = "standard,rlm",
        value_delimiter = ',',
        value_parser = ["standard", "rlm"]
    )]
    producers: Vec<String>,
    /// JSONL output file, appended.
    #[arg(long)]
    out: PathBuf,
    /// SQLite effect database (backend=sqlite).
    #[arg(long)]
    db_path: Option<PathBuf>,
    /// PostgreSQL URL (backend=postgres).
    #[arg(long, env = "LASH_POSTGRES_DATABASE_URL")]
    database_url: Option<String>,
    /// Restate ingress URL (backend=restate).
    #[arg(long, env = "RESTATE_INGRESS_URL")]
    restate_ingress_url: Option<String>,
    /// Restate admin URL (backend=restate).
    #[arg(long, env = "RESTATE_ADMIN_URL")]
    restate_admin_url: Option<String>,
    #[arg(long, env = "EG_RESTATE_ENDPOINT_BIND")]
    restate_endpoint_bind: Option<String>,
    /// The URL Restate reaches the probe endpoint on (backend=restate).
    #[arg(long, env = "EG_RESTATE_ENDPOINT_URL")]
    restate_endpoint_url: Option<String>,
}

/// One rep's evidence, serialized as one JSONL line.
#[derive(Serialize)]
struct MeasurementRow {
    backend: String,
    producer: String,
    width: usize,
    rep: usize,
    session_id: String,
    turn_ms: f64,
    leaf_window_ms: Option<f64>,
    leaves_per_second: f64,
    leaves_started: usize,
    leaves_answered: usize,
    peak_in_flight: usize,
    model_calls: usize,
    journal_rows: BTreeMap<String, i64>,
    load1: f64,
    load5: f64,
    load15: f64,
}

impl MeasurementRow {
    fn new(
        backend: &str,
        producer_label: &str,
        rep: usize,
        session_id: &str,
        measurement: &lash_conformance::ToolBatchMeasurement,
        journal_rows: BTreeMap<String, i64>,
        load: LoadAverage,
    ) -> Self {
        let leaf_window_ms = measurement
            .leaf_window
            .map(|window| window.as_secs_f64() * 1000.0);
        Self {
            backend: backend.to_string(),
            producer: producer_label.to_string(),
            width: measurement.width,
            rep,
            session_id: session_id.to_string(),
            turn_ms: measurement.turn.as_secs_f64() * 1000.0,
            leaf_window_ms,
            leaves_per_second: leaf_window_ms
                .filter(|window| *window > 0.0)
                .map(|window| measurement.width as f64 / (window / 1000.0))
                .unwrap_or(0.0),
            leaves_started: measurement.leaves_started,
            leaves_answered: measurement.leaves_answered,
            peak_in_flight: measurement.peak_in_flight,
            model_calls: measurement.model_calls,
            journal_rows,
            load1: load.one,
            load5: load.five,
            load15: load.fifteen,
        }
    }
}

/// `/proc/loadavg` sampled beside the rep that produced a row.
#[derive(Clone, Copy)]
struct LoadAverage {
    one: f64,
    five: f64,
    fifteen: f64,
}

fn load_average() -> LoadAverage {
    let contents = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut fields = contents
        .split_whitespace()
        .map(|field| field.parse::<f64>().unwrap_or(f64::NAN));
    LoadAverage {
        one: fields.next().unwrap_or(f64::NAN),
        five: fields.next().unwrap_or(f64::NAN),
        fifteen: fields.next().unwrap_or(f64::NAN),
    }
}

/// The RLM bridge's plugin factory, spelled the way the conformance
/// registrations spell it: `Promise.all` is the cell-bridge surface, so the
/// factory does not advertise the process lifecycle.
fn rlm_factory() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new()),
        )
        .with_process_lifecycle(false),
    )
}

fn producers(names: &[String]) -> Vec<lash_conformance::ToolBatchProducer> {
    names
        .iter()
        .map(|name| match name.as_str() {
            "standard" => lash_conformance::parallel_model_tool_calls_producer(),
            "rlm" => lash_conformance::rlm_promise_all_producer(vec![rlm_factory()]),
            other => panic!("unknown producer `{other}`"),
        })
        .collect()
}

fn session_id(prefix: &str, producer_label: &str, width: usize, rep: usize) -> String {
    format!("{prefix}-{producer_label}-w{width}-r{rep}")
}

fn emit(out: &std::path::Path, row: &MeasurementRow) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)?;
    writeln!(file, "{}", serde_json::to_string(row)?)?;
    println!(
        "[{}/{} w={} r{}] turn={:.1}ms leaf_window={} rows={:?} load1={:.2}",
        row.backend,
        row.producer,
        row.width,
        row.rep,
        row.turn_ms,
        row.leaf_window_ms
            .map(|window| format!("{window:.1}ms"))
            .unwrap_or_else(|| "none".to_string()),
        row.journal_rows,
        row.load1,
    );
    Ok(())
}

#[async_trait::async_trait]
trait JournalCounter {
    async fn count(&self) -> anyhow::Result<BTreeMap<String, i64>>;
}

struct SqliteJournalCounter {
    path: PathBuf,
}

#[async_trait::async_trait]
impl JournalCounter for SqliteJournalCounter {
    async fn count(&self) -> anyhow::Result<BTreeMap<String, i64>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = rusqlite::Connection::open(&path)?;
            let mut tables = connection.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND \
                 (name LIKE 'runtime_effect%' OR name LIKE 'tool_intent%')",
            )?;
            let names = tables
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut counts = BTreeMap::new();
            for name in names {
                let count: i64 = connection.query_row(
                    &format!("SELECT count(*) FROM \"{name}\""),
                    [],
                    |row| row.get(0),
                )?;
                counts.insert(name, count);
            }
            Ok(counts)
        })
        .await?
    }
}

struct PostgresJournalCounter {
    pool: sqlx::PgPool,
}

#[async_trait::async_trait]
impl JournalCounter for PostgresJournalCounter {
    async fn count(&self) -> anyhow::Result<BTreeMap<String, i64>> {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public' AND \
             (tablename LIKE 'lash\\_runtime\\_effect%' OR tablename LIKE 'lash\\_tool\\_intent%')",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut counts = BTreeMap::new();
        for name in names {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM \"{name}\""))
                .fetch_one(&self.pool)
                .await?;
            counts.insert(name, count);
        }
        Ok(counts)
    }
}

fn count_delta(
    before: &BTreeMap<String, i64>,
    after: &BTreeMap<String, i64>,
) -> BTreeMap<String, i64> {
    after
        .iter()
        .map(|(table, count)| {
            (
                table.clone(),
                count - before.get(table).copied().unwrap_or(0),
            )
        })
        .collect()
}

async fn run_sqlite(
    args: &Args,
    producers: &[lash_conformance::ToolBatchProducer],
) -> anyhow::Result<()> {
    let db_path = args
        .db_path
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("tool-batch-baseline.db"));
    if db_path.exists() {
        anyhow::bail!("--db-path {} exists; pass a fresh path", db_path.display());
    }
    let host = Arc::new(lash_sqlite_store::SqliteEffectHost::open(&db_path).await?)
        as Arc<dyn lash_core::EffectHost>;
    let counter = SqliteJournalCounter { path: db_path };
    for producer in producers {
        for &width in &args.widths {
            for rep in 0..args.reps {
                let session = session_id("sqlite", &producer.label, width, rep);
                let before = counter.count().await?;
                let load = load_average();
                let measurement = lash_conformance::measure_tool_batch(
                    lash_sansio::SessionId::from(session.clone()),
                    Arc::clone(&host),
                    None,
                    producer,
                    width,
                )
                .await;
                let after = counter.count().await?;
                emit(
                    &args.out,
                    &MeasurementRow::new(
                        "sqlite",
                        &producer.label,
                        rep,
                        &session,
                        &measurement,
                        count_delta(&before, &after),
                        load,
                    ),
                )?;
            }
        }
    }
    Ok(())
}

async fn run_postgres(
    args: &Args,
    producers: &[lash_conformance::ToolBatchProducer],
) -> anyhow::Result<()> {
    let url = args.database_url.clone().ok_or_else(|| {
        anyhow::anyhow!("postgres needs --database-url or LASH_POSTGRES_DATABASE_URL")
    })?;
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url()).await?;
    let host = Arc::new(storage.effect_host()) as Arc<dyn lash_core::EffectHost>;
    let counter = PostgresJournalCounter {
        pool: storage.pool().clone(),
    };
    for producer in producers {
        for &width in &args.widths {
            for rep in 0..args.reps {
                let session = session_id("postgres", &producer.label, width, rep);
                let before = counter.count().await?;
                let load = load_average();
                let measurement = lash_conformance::measure_tool_batch(
                    lash_sansio::SessionId::from(session.clone()),
                    Arc::clone(&host),
                    None,
                    producer,
                    width,
                )
                .await;
                let after = counter.count().await?;
                emit(
                    &args.out,
                    &MeasurementRow::new(
                        "postgres",
                        &producer.label,
                        rep,
                        &session,
                        &measurement,
                        count_delta(&before, &after),
                        load,
                    ),
                )?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Restate
// ---------------------------------------------------------------------------

/// What the probe workflow takes: which producer, how wide, under which
/// session.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ProbeRequest {
    session_id: String,
    producer: String,
    width: usize,
}

/// What it measured. The journal rows are read out of `sys_journal` by the
/// driver, not the handler — a handler cannot see its own journal.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ProbeResponse {
    turn_ms: f64,
    leaf_window_ms: Option<f64>,
    leaves_started: usize,
    leaves_answered: usize,
    peak_in_flight: usize,
    model_calls: usize,
}

impl From<lash_conformance::ToolBatchMeasurement> for ProbeResponse {
    fn from(measurement: lash_conformance::ToolBatchMeasurement) -> Self {
        Self {
            turn_ms: measurement.turn.as_secs_f64() * 1000.0,
            leaf_window_ms: measurement
                .leaf_window
                .map(|window| window.as_secs_f64() * 1000.0),
            leaves_started: measurement.leaves_started,
            leaves_answered: measurement.leaves_answered,
            peak_in_flight: measurement.peak_in_flight,
            model_calls: measurement.model_calls,
        }
    }
}

/// The workflow whose handler runs the measured turn where Restate runs
/// turns: inside the handler, on a `RestateRuntimeEffectController`.
#[restate_sdk::workflow]
trait ToolBatchProbe {
    async fn run(request: Json<ProbeRequest>) -> HandlerResult<Json<ProbeResponse>>;
}

struct ToolBatchProbeImpl {
    ingress_url: String,
    authority: lash_restate::RestateAuthorityId,
    producers: Vec<lash_conformance::ToolBatchProducer>,
}

impl ToolBatchProbe for ToolBatchProbeImpl {
    async fn run(
        &self,
        ctx: restate_sdk::context::WorkflowContext<'_>,
        Json(request): Json<ProbeRequest>,
    ) -> HandlerResult<Json<ProbeResponse>> {
        let session_id = lash_sansio::SessionId::from(request.session_id.clone());
        let controller =
            lash_restate::RestateRuntimeEffectController::new(ctx, self.authority.clone());
        let scoped = controller
            .scoped_effect_controller(lash_core::AdmittedScope::turn(
                &session_id,
                lash_conformance::tool_batch_turn_id(&session_id),
            ))
            .map_err(TerminalError::from_error)?;
        let host = Arc::new(lash_restate::RestateEffectHost::new(
            self.ingress_url.clone(),
            self.authority.clone(),
        )) as Arc<dyn lash_core::EffectHost>;
        let Some(producer) = self
            .producers
            .iter()
            .find(|producer| producer.label == request.producer)
        else {
            return Err(
                TerminalError::new(format!("unknown producer `{}`", request.producer)).into(),
            );
        };
        // A panic in the measurement must fail the invocation terminally: an
        // unwinding panic would read as retryable and the workflow would
        // redrive the same broken rep forever.
        let measurement = AssertUnwindSafe(lash_conformance::measure_tool_batch(
            session_id,
            host,
            Some(scoped),
            producer,
            request.width,
        ))
        .catch_unwind()
        .await
        .map_err(|_| TerminalError::new("the tool-batch measurement panicked"))?;
        Ok(Json(ProbeResponse::from(measurement)))
    }
}

/// The invocation id a workflow key produced, for the journal count.
async fn restate_invocation_id(
    admin: &lash_restate::RestateAdminClient,
    workflow_key: &str,
) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct InvocationRow {
        id: String,
    }
    let mut rows = admin
        .query_json::<InvocationRow>(&format!(
            "SELECT id FROM sys_invocation WHERE target_service_name = '{PROBE_SERVICE}' \
             AND target_service_key = '{workflow_key}' AND target_handler_name = 'run' \
             ORDER BY modified_at DESC LIMIT 1"
        ))
        .await?;
    rows.pop()
        .map(|row| row.id)
        .ok_or_else(|| anyhow::anyhow!("no sys_invocation row for probe key `{workflow_key}`"))
}

/// One invocation's journal rows, grouped by entry type.
async fn restate_journal_rows(
    admin: &lash_restate::RestateAdminClient,
    invocation_id: &str,
) -> anyhow::Result<BTreeMap<String, i64>> {
    #[derive(serde::Deserialize)]
    struct EntryCount {
        entry_type: String,
        n: i64,
    }
    let rows = admin
        .query_json::<EntryCount>(&format!(
            "SELECT entry_type, count(*) AS n FROM sys_journal WHERE id = '{invocation_id}' \
             GROUP BY entry_type"
        ))
        .await?;
    let mut counts = BTreeMap::new();
    let mut total = 0;
    for row in rows {
        total += row.n;
        counts.insert(format!("sys_journal:{}", row.entry_type), row.n);
    }
    counts.insert("sys_journal:total".to_string(), total);
    Ok(counts)
}

async fn wait_for_endpoint(addr: SocketAddr) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "Restate probe endpoint did not open at {addr}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn register_restate_deployment(admin_url: &str, endpoint_url: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder().http2_prior_knowledge().build()?;
    let response = client
        .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
        .json(&serde_json::json!({
            "uri": endpoint_url,
            "force": true,
            "breaking": true,
        }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Restate deployment registration failed: {status} {body}");
    }
    Ok(())
}

async fn run_restate(
    args: &Args,
    producers: &[lash_conformance::ToolBatchProducer],
) -> anyhow::Result<()> {
    let ingress_url = args
        .restate_ingress_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("restate needs RESTATE_INGRESS_URL"))?;
    let admin_url = args
        .restate_admin_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("restate needs RESTATE_ADMIN_URL"))?;
    let bind = args
        .restate_endpoint_bind
        .clone()
        .ok_or_else(|| anyhow::anyhow!("restate needs EG_RESTATE_ENDPOINT_BIND"))?
        .parse::<SocketAddr>()?;
    let endpoint_url = args
        .restate_endpoint_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("restate needs EG_RESTATE_ENDPOINT_URL"))?;
    let authority = lash_restate::RestateAuthorityId::new(RESTATE_AUTHORITY)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    use lash_restate::{LashDurableWaitIndex, LashDurableWaitWorkflow};
    use restate_sdk::endpoint::Endpoint;
    use restate_sdk::http_server::HttpServer;
    let endpoint = Endpoint::builder()
        .bind(lash_restate::LashDurableWaitWorkflowImpl.serve())
        .bind(lash_restate::LashDurableWaitIndexImpl.serve())
        .bind(
            ToolBatchProbeImpl {
                ingress_url: ingress_url.clone(),
                authority: authority.clone(),
                producers: producers.to_vec(),
            }
            .serve(),
        )
        .build();
    // The probe turns only on the durable-wait pair; a dropped bind would
    // surface mid-rep as a 404, so assert the whole surface up front.
    lash_restate::assert_services_bound(
        &endpoint,
        &[
            "LashDurableWaitWorkflow",
            "LashDurableWaitIndex",
            PROBE_SERVICE,
        ],
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let server = tokio::spawn(HttpServer::new(endpoint).serve(listener));
    wait_for_endpoint(bind).await?;
    register_restate_deployment(&admin_url, &endpoint_url).await?;

    let ingress = lash_restate::RestateIngressClient::new(ingress_url.clone());
    let admin = lash_restate::RestateAdminClient::new(admin_url.clone());
    for producer in producers {
        for &width in &args.widths {
            for rep in 0..args.reps {
                let session = session_id("restate", &producer.label, width, rep);
                let load = load_average();
                let request = ProbeRequest {
                    session_id: session.clone(),
                    producer: producer.label.clone(),
                    width,
                };
                // The ingress call carries no wall-clock bound of its own; a
                // serial tier needs the width times a journaled step each,
                // plus margin — bound only a wedge.
                let response = tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    ingress.call_workflow_json::<_, ProbeResponse>(
                        PROBE_SERVICE,
                        &session,
                        "run",
                        &request,
                    ),
                )
                .await
                .map_err(|_| anyhow::anyhow!("probe call for `{session}` timed out"))?
                .map_err(|error| anyhow::anyhow!("probe call for `{session}` failed: {error}"))?;
                let invocation_id = restate_invocation_id(&admin, &session).await?;
                let journal_rows = restate_journal_rows(&admin, &invocation_id).await?;
                let measurement = lash_conformance::ToolBatchMeasurement {
                    width,
                    turn: std::time::Duration::from_secs_f64(response.turn_ms / 1000.0),
                    leaf_window: response
                        .leaf_window_ms
                        .map(|ms| std::time::Duration::from_secs_f64(ms / 1000.0)),
                    leaves_started: response.leaves_started,
                    leaves_answered: response.leaves_answered,
                    peak_in_flight: response.peak_in_flight,
                    model_calls: response.model_calls,
                };
                emit(
                    &args.out,
                    &MeasurementRow::new(
                        "restate",
                        &producer.label,
                        rep,
                        &session,
                        &measurement,
                        journal_rows,
                        load,
                    ),
                )?;
            }
        }
    }
    server.abort();
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let producers = producers(&args.producers);
    match args.backend.as_str() {
        "sqlite" => run_sqlite(&args, &producers).await?,
        "postgres" => run_postgres(&args, &producers).await?,
        "restate" => run_restate(&args, &producers).await?,
        other => anyhow::bail!("unknown backend `{other}`"),
    }
    Ok(())
}
