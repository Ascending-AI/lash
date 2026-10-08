//! An executing external consumer. Its only Lash dependency is the public
//! facade; the controller owns process lifetime and service addresses.
//!
//! It is a real-host E2E node: one lash node over the store the controller
//! names (SQLite memory, one SQLite file, or a PostgreSQL database several
//! nodes share), serving under the node name the controller gives it. The
//! controller reads what the node committed from its commit ledger and the
//! store, and kills or partitions it at the cuts the ledger holds.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use lash::{InputId, LashCore, SessionCreation, SessionId, SessionSpec, TurnId, TurnInput};
use serde::Deserialize;
use serde_json::{Value, json};

#[path = "../../shared/e2e_commit_ledger.rs"]
mod commit_ledger;
mod fixture;
mod receiver;
mod scenario;
mod telemetry;
mod telemetry_scenario;

#[derive(Clone)]
struct Host {
    core: LashCore,
    controls: Arc<fixture::Controls>,
    telemetry: Option<Arc<telemetry::HostTelemetry>>,
    ledger: Option<Arc<commit_ledger::CommitLedger>>,
    case: Option<Arc<scenario::Fixture>>,
}

type ApiResult = Result<Json<Value>, (axum::http::StatusCode, String)>;
fn api_error(error: impl std::fmt::Display) -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        error.to_string(),
    )
}

#[derive(Deserialize)]
struct Submit {
    id: String,
    text: String,
}

async fn submit(
    State(host): State<Host>,
    Path(session): Path<String>,
    Json(input): Json<Submit>,
) -> ApiResult {
    let session_id = SessionId::parse(session).map_err(api_error)?;
    let spec = SessionSpec::new(
        "consumer",
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(8),
    );
    match host
        .core
        .session(session_id.clone())
        .create(SessionCreation::root(spec))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => return Err(api_error(error)),
    }
    let session = host
        .core
        .session(session_id)
        .durable()
        .await
        .map_err(api_error)?;
    let handle = session
        .send(TurnInput::text(input.text))
        .id(TurnId::parse(input.id).map_err(api_error)?)
        .await
        .map_err(api_error)?;
    let receipt = json!({"input_id": handle.input_id(), "receipt": handle.receipt()});
    drop(handle);
    Ok(Json(receipt))
}

/// The settled outcome, or the typed refusal the follow ended with: a cause
/// the engine typed stays typed for the case reading it.
async fn follow(
    State(host): State<Host>,
    Path((session, input)): Path<(String, String)>,
) -> ApiResult {
    let session_id = SessionId::parse(session).map_err(api_error)?;
    let input_id = InputId::parse(input).map_err(api_error)?;
    let session = host
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(api_error)?;
    match session.attach(input_id.clone()).outcome().await {
        Ok(outcome) => Ok(Json(
            serde_json::to_value(outcome.to_remote(&session_id, &input_id)).map_err(api_error)?,
        )),
        Err(lash::EmbedError::Runtime(refusal)) => Ok(Json(json!({
            "type": "refused",
            "session_id": session_id,
            "input_id": input_id,
            "error": refusal,
        }))),
        Err(error) => Err(api_error(error)),
    }
}

async fn cancel(
    State(host): State<Host>,
    Path((session, input)): Path<(String, String)>,
) -> ApiResult {
    let session_id = SessionId::parse(session).map_err(api_error)?;
    let session = host
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(api_error)?;
    let receipt = session
        .attach(InputId::parse(input).map_err(api_error)?)
        .cancel()
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"receipt": format!("{receipt:?}")})))
}

async fn binding(
    State(host): State<Host>,
    Path((session, input)): Path<(String, String)>,
) -> ApiResult {
    let session_id = SessionId::parse(session).map_err(api_error)?;
    let session = host
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(api_error)?;
    let run = session
        .attach(InputId::parse(input).map_err(api_error)?)
        .run()
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"run":run})))
}

async fn task(
    State(host): State<Host>,
    Path(session): Path<String>,
    Json(input): Json<Submit>,
) -> ApiResult {
    let session = host
        .core
        .session(SessionId::parse(session).map_err(api_error)?)
        .open()
        .await
        .map_err(api_error)?;
    let task = session
        .plugin_operations()
        .start_task::<fixture::EchoTask>(input.text, input.id)
        .await
        .map_err(api_error)?;
    let run = task.run().clone();
    drop(task);
    Ok(Json(json!({"run": run})))
}

async fn task_result(
    State(host): State<Host>,
    Path((session, run)): Path<(String, String)>,
) -> ApiResult {
    let session = host
        .core
        .session(SessionId::parse(session).map_err(api_error)?)
        .durable()
        .await
        .map_err(api_error)?;
    let result = session
        .run(lash::RunId::parse(run).map_err(api_error)?)
        .result()
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"output": result.output})))
}

async fn task_cancel(
    State(host): State<Host>,
    Path((session, run)): Path<(String, String)>,
) -> ApiResult {
    let session = host
        .core
        .session(SessionId::parse(session).map_err(api_error)?)
        .durable()
        .await
        .map_err(api_error)?;
    let result = session
        .run(lash::RunId::parse(run).map_err(api_error)?)
        .cancel()
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"receipt": format!("{result:?}")})))
}

#[derive(Deserialize)]
struct Completion {
    key: String,
    value: Value,
}

/// A host resolution of a completion key, answered as the store answered it.
async fn complete(State(host): State<Host>, Json(completion): Json<Completion>) -> ApiResult {
    let answer = host
        .core
        .completions()
        .resolve(&completion.key, lash::Resolution::Ok(completion.value))
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"answer": format!("{answer:?}")})))
}

async fn drain(State(host): State<Host>) -> ApiResult {
    match host.core.drain().await {
        Ok(report) => Ok(Json(json!({"drained": format!("{report:?}")}))),
        Err(error) => Ok(Json(json!({"refused": format!("{error:?}")}))),
    }
}

async fn drain_status(State(host): State<Host>) -> ApiResult {
    match host.core.drain_status(true).await {
        Ok(status) => Ok(Json(json!({
            "remaining_invocations": status.remaining_invocations,
            "in_flight_turns": status.in_flight_turns,
        }))),
        Err(error) => Ok(Json(json!({"refused": error.to_string()}))),
    }
}

/// Start the case's intent receiver for `session`.
async fn start_receiver(State(host): State<Host>, Path(session): Path<String>) -> ApiResult {
    let case = host
        .case
        .as_ref()
        .context("no case fixture is installed")
        .map_err(api_error)?;
    let process = receiver::start(
        &host.core,
        &SessionId::parse(session).map_err(api_error)?,
        &case.receiver,
    )
    .await
    .map_err(api_error)?;
    Ok(Json(json!({"process": process})))
}

/// The events the case's receiver holds.
async fn receiver_events(State(host): State<Host>, Path(process): Path<String>) -> ApiResult {
    let events = receiver::events(
        &host.core,
        &lash::ProcessId::parse(&process).map_err(api_error)?,
    )
    .await
    .map_err(api_error)?;
    Ok(Json(serde_json::to_value(events).map_err(api_error)?))
}

async fn release_cut(State(host): State<Host>, Json(label): Json<String>) -> ApiResult {
    let ledger = host
        .ledger
        .as_ref()
        .context("no commit ledger is installed")
        .map_err(api_error)?;
    ledger.release(&label);
    Ok(Json(json!({"released": label})))
}

fn required(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("{key} is required"))
}

/// The connection sizing of every PostgreSQL pool this host opens: a case
/// boots several nodes over one server.
fn postgres_config() -> lash::postgres::PostgresHostConfig {
    let mut config = lash::postgres::PostgresHostConfig::default();
    config.roles.work.max_connections = 4;
    config.roles.max_store_operations = 4;
    config
}

/// The live replay store the controller names: the shared PostgreSQL one
/// (FIG-5101) in `E2E_CONSUMER_LIVE_REPLAY_URL`'s database, or the
/// process-local default.
async fn live_replay() -> Result<Option<Arc<dyn lash::observe::LiveReplayStore>>> {
    let Ok(url) = std::env::var("E2E_CONSUMER_LIVE_REPLAY_URL") else {
        return Ok(None);
    };
    let mut policy = lash::postgres::LiveReplayPolicy::default();
    policy.data.schema_mode = lash::postgres::ReplaySchemaMode::Install;
    policy.pool.max_connections = 2;
    policy.pool.min_connections = 0;
    policy.data.publish_concurrency = 2;
    let config = lash::postgres::PostgresHostConfig {
        live_replay: Some(policy),
        ..postgres_config()
    };
    let store = lash::postgres::PostgresLiveReplayStore::connect(
        &lash::postgres::PostgresEndpoints::from_url(&url)?,
        &config,
    )
    .await
    .context("connect the PostgreSQL live replay store")?;
    Ok(Some(Arc::new(store)))
}

/// The store set the controller names, with its root directory.
async fn stores(root: &std::path::Path) -> Result<Arc<dyn lash::StoreSet>> {
    Ok(match required("E2E_CONSUMER_STORE")?.as_str() {
        "memory" => Arc::new(lash::sqlite::SqliteStoreSet::memory().await?),
        "file" => Arc::new(lash::sqlite::SqliteStoreSet::open(root.join("lash.db")).await?),
        "postgres" => {
            let url = required("E2E_CONSUMER_DATABASE_URL")?;
            let endpoints = lash::postgres::PostgresEndpoints::from_url(&url)?;
            let storage = lash::postgres::PostgresStorage::connect(
                &endpoints,
                &postgres_config(),
                Default::default(),
            )
            .await
            .context("open the PostgreSQL store")?;
            Arc::new(lash::postgres::PostgresStoreSet::new(
                &storage,
                Arc::new(lash::persistence::FileAttachmentStore::new(
                    root.join("attachments"),
                )),
            ))
        }
        store => anyhow::bail!("unsupported consumer store {store}"),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let http_addr: SocketAddr = required("E2E_CONSUMER_ADDR")?.parse()?;
    let root = PathBuf::from(required("E2E_CONSUMER_DATA_DIR")?);
    std::fs::create_dir_all(&root)?;
    let node = std::env::var("E2E_CONSUMER_NODE").unwrap_or_else(|_| "external-consumer".into());
    let ledger = commit_ledger::CommitLedger::from_env(
        "E2E_CONSUMER_COMMIT_LEDGER",
        "E2E_CONSUMER_CUTS",
        &node,
    )?;
    let stores = stores(&root).await?;
    let stores = match &ledger {
        Some(ledger) => commit_ledger::ledger_stores(stores, ledger.clone()),
        None => stores,
    };
    let backend = lash::durable::DurableBackendBuilder::new(stores.clone())
        .build()
        .context("build the durable backend")?;
    let controls = Arc::new(fixture::Controls::default());
    let case = scenario::Fixture::from_env("E2E_CONSUMER_FIXTURE")?;
    let scenario = match std::env::var("E2E_CONSUMER_SCENARIO").ok().as_deref() {
        None => None,
        Some("S34") => Some(telemetry_scenario::Scenario::default()),
        Some(scenario) => anyhow::bail!("unsupported consumer scenario {scenario}"),
    };
    let provider = match (&scenario, &case) {
        (Some(scenario), _) => scenario.provider(),
        (None, Some(case)) => case.provider()?,
        (None, None) => fixture::provider(),
    };
    let metadata = lash::LlmProfileMetadata::builder("test/external-consumer")
        .context_window_tokens(200_000)
        .build()?;
    let profiles = lash::LlmProfileRegistry::new().register(
        "consumer",
        lash::RegisteredLlmProfile::new(metadata, provider.clone()),
    )?;
    let telemetry = std::env::var("E2E_CONSUMER_OTLP_ENDPOINT")
        .ok()
        .map(|endpoint| telemetry::HostTelemetry::new(&endpoint).map(Arc::new))
        .transpose()?;
    let trace = std::env::var_os("E2E_CONSUMER_TRACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("trace.jsonl"));
    let builder = LashCore::standard_builder(backend)
        .llm_profiles(Arc::new(profiles))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .plugin(Arc::new(fixture::ConsumerPlugin(controls.clone())))
        .trace_sink(Arc::new(lash::tracing::JsonlTraceSink::new(trace)))
        .trace_level(lash::tracing::TraceLevel::Extended);
    let builder = match live_replay().await? {
        Some(store) => builder.live_replay_store(store),
        None => builder,
    };
    let builder = match &case {
        Some(case) => case
            .plugins()?
            .into_iter()
            .fold(builder, |builder, plugin| builder.plugin(plugin))
            .plugin(Arc::new(receiver::ReceiverEnginePlugin)),
        None => builder,
    };
    let builder = match scenario.as_ref() {
        Some(scenario) => builder.plugin(scenario.plugin()),
        None => builder,
    };
    let builder = match telemetry.as_ref() {
        Some(telemetry) => telemetry.install(builder),
        None => builder,
    };
    let core = builder.build(lash::persistence::LeaseOwnerIdentity::opaque(
        node.clone(),
        std::process::id().to_string(),
    ))?;
    let operation = async {
        let app = Router::new()
            .route(
                "/healthz",
                get(|| async { Json(json!({"service":"external-consumer"})) }),
            )
            .route("/sessions/{session}/inputs", post(submit))
            .route("/sessions/{session}/inputs/{input}", get(follow))
            .route("/sessions/{session}/inputs/{input}/cancel", post(cancel))
            .route("/sessions/{session}/inputs/{input}/binding", get(binding))
            .route("/sessions/{session}/tasks", post(task))
            .route("/sessions/{session}/runs/{run}", get(task_result))
            .route("/sessions/{session}/runs/{run}/cancel", post(task_cancel))
            .route("/completions", post(complete))
            .route("/control/drain", post(drain))
            .route("/control/drain-status", get(drain_status))
            .route("/control/cuts/release", post(release_cut))
            .route("/receiver/{session}", post(start_receiver))
            .route("/receiver/{process}/events", get(receiver_events))
            .route(
                "/control/entered",
                get(|State(host): State<Host>| async move { Json(host.controls.receipts()) }),
            )
            .route(
                "/control/release",
                post(
                    |State(host): State<Host>, Json(key): Json<String>| async move {
                        Json(json!({"released":host.controls.release(&key)}))
                    },
                ),
            )
            .route(
                "/control/telemetry/flush",
                post(|State(host): State<Host>| async move {
                    let telemetry = host
                        .telemetry
                        .context("telemetry was not installed")
                        .map_err(api_error)?;
                    let receipt = tokio::task::spawn_blocking(move || telemetry.flush())
                        .await
                        .map_err(api_error)?;
                    Ok::<_, (axum::http::StatusCode, String)>(Json(receipt))
                }),
            )
            .with_state(Host {
                core: core.clone(),
                controls,
                telemetry: telemetry.clone(),
                ledger,
                case: case.clone().map(Arc::new),
            });
        let app = match scenario {
            Some(scenario) => app.merge(scenario.router(stores.clone())),
            None => app,
        };
        let listener = tokio::net::TcpListener::bind(http_addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown())
            .await?;
        anyhow::Ok(())
    }
    .await;
    core.shutdown().await?;
    provider.close().await?;
    core.flush_trace_sink()?;
    let telemetry_receipt = match telemetry {
        Some(telemetry) => Some(
            tokio::task::spawn_blocking(move || {
                let telemetry = Arc::try_unwrap(telemetry).map_err(|_| {
                    anyhow::anyhow!("live telemetry observer survived HTTP shutdown")
                })?;
                anyhow::Ok(telemetry.shutdown())
            })
            .await??,
        ),
        None => None,
    };
    operation?;
    println!(
        "CONSUMER_SHUTDOWN {}",
        json!({"core":true,"provider":true,"trace":true,"telemetry":telemetry_receipt})
    );
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! { _ = terminate.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
