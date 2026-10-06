//! An executing external consumer. Its only Lash dependency is the public
//! facade; the controller owns Restate, process lifetime and service addresses.

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

mod fixture;
mod telemetry;
mod telemetry_scenario;

#[derive(Clone)]
struct Host {
    core: LashCore,
    controls: Arc<fixture::Controls>,
    telemetry: Option<Arc<telemetry::HostTelemetry>>,
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
    let outcome = session
        .attach(input_id.clone())
        .outcome()
        .await
        .map_err(api_error)?;
    Ok(Json(
        serde_json::to_value(outcome.to_remote(&session_id, &input_id)).map_err(api_error)?,
    ))
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

fn required(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("{key} is required"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let http_addr: SocketAddr = required("E2E_CONSUMER_ADDR")?.parse()?;
    let root = PathBuf::from(required("E2E_CONSUMER_DATA_DIR")?);
    std::fs::create_dir_all(&root)?;
    let stores = Arc::new(match required("E2E_CONSUMER_STORE")?.as_str() {
        "memory" => lash::sqlite::SqliteStoreSet::memory().await?,
        "file" => lash::sqlite::SqliteStoreSet::open(root.join("lash.db")).await?,
        store => anyhow::bail!("unsupported consumer store {store}"),
    });
    let backend = lash::durable::DurableBackendBuilder::new(stores.clone())
        .completion_secrets(lash::durable::CompletionKeySecrets::for_testing())
        .build()
        .context("build the durable backend")?;
    let controls = Arc::new(fixture::Controls::default());
    let scenario = match std::env::var("E2E_CONSUMER_SCENARIO").ok().as_deref() {
        None => None,
        Some("S34") => Some(telemetry_scenario::Scenario::default()),
        Some(scenario) => anyhow::bail!("unsupported consumer scenario {scenario}"),
    };
    let provider = scenario
        .as_ref()
        .map_or_else(fixture::provider, telemetry_scenario::Scenario::provider);
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
    let builder = match scenario.as_ref() {
        Some(scenario) => builder.plugin(scenario.plugin()),
        None => builder,
    };
    let builder = match telemetry.as_ref() {
        Some(telemetry) => telemetry.install(builder),
        None => builder,
    };
    let core = builder.build(lash::persistence::LeaseOwnerIdentity::opaque(
        "external-consumer",
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
