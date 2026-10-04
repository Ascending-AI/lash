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

#[derive(Clone)]
struct Host { core: LashCore, controls: Arc<fixture::Controls> }

type ApiResult = Result<Json<Value>, (axum::http::StatusCode, String)>;
fn api_error(error: impl std::fmt::Display) -> (axum::http::StatusCode, String) {
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[derive(Deserialize)]
struct Submit { id: String, text: String }

async fn submit(State(host): State<Host>, Path(session): Path<String>, Json(input): Json<Submit>) -> ApiResult {
    let session_id = SessionId::fixture(session);
    let spec = SessionSpec::new("consumer", lash::TurnBudget::Unbounded, lash::MaxToolCalls::new(8));
    match host.core.session(session_id.clone()).create(SessionCreation::root(spec)).await {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {},
        Err(error) => return Err(api_error(error)),
    }
    let session = host.core.session(session_id).durable().await.map_err(api_error)?;
    let handle = session.send(TurnInput::text(input.text)).id(input.id).await.map_err(api_error)?;
    let receipt = json!({"input_id": handle.input_id(), "receipt": handle.receipt()});
    drop(handle);
    Ok(Json(receipt))
}

async fn follow(State(host): State<Host>, Path((session, input)): Path<(String, String)>) -> ApiResult {
    let session_id = SessionId::fixture(session);
    let input_id = InputId::fixture(input);
    let session = host.core.session(session_id.clone()).durable().await.map_err(api_error)?;
    let outcome = session.attach(input_id.clone()).outcome().await.map_err(api_error)?;
    Ok(Json(serde_json::to_value(outcome.to_remote(&session_id, &input_id)).map_err(api_error)?))
}

async fn cancel(State(host): State<Host>, Path((session, input)): Path<(String, String)>) -> ApiResult {
    let session = host.core.session(session).durable().await.map_err(api_error)?;
    let receipt = session.attach(InputId::fixture(input)).cancel().await.map_err(api_error)?;
    Ok(Json(json!({"receipt": format!("{receipt:?}")})))
}

async fn task(State(host): State<Host>, Path(session): Path<String>, Json(input): Json<Submit>) -> ApiResult {
    let session = host.core.session(session).durable().await.map_err(api_error)?;
    let task = session.plugin_operations().start_task::<fixture::EchoTask>(input.text, input.id).await.map_err(api_error)?;
    let run = task.run().clone();
    drop(task);
    Ok(Json(json!({"run": run})))
}

async fn task_result(State(host): State<Host>, Path((session, run)): Path<(String, String)>) -> ApiResult {
    let session = host.core.session(session).durable().await.map_err(api_error)?;
    let result = session.run(TurnId::fixture(run)).result().await.map_err(api_error)?;
    Ok(Json(json!({"output": result.output})))
}

async fn task_cancel(State(host): State<Host>, Path((session, run)): Path<(String, String)>) -> ApiResult {
    let session = host.core.session(session).durable().await.map_err(api_error)?;
    let result = session.run(TurnId::fixture(run)).cancel().await.map_err(api_error)?;
    Ok(Json(json!({"receipt": format!("{result:?}")})))
}

fn required(key: &str) -> Result<String> { std::env::var(key).with_context(|| format!("{key} is required")) }

#[tokio::main]
async fn main() -> Result<()> {
    let http_addr: SocketAddr = required("E2E_CONSUMER_ADDR")?.parse()?;
    let endpoint_addr: SocketAddr = required("E2E_CONSUMER_RESTATE_ADDR")?.parse()?;
    let root = PathBuf::from(required("E2E_CONSUMER_DATA_DIR")?);
    let config = lash::restate::RestateConfig::new(
        required("RESTATE_INGRESS_URL")?, required("RESTATE_ADMIN_URL")?,
        lash::restate::RestateAuthorityId::new(required("RESTATE_AUTHORITY_ID")?)?,
    ).with_namespace(required("E2E_CONSUMER_NAMESPACE")?.parse()?);
    let stores = Arc::new(lash::sqlite::SqliteStoreSet::open(&root).await?);
    let engine = Arc::new(lash::restate::RestateEngine::new(stores, config));
    let controls = Arc::new(fixture::Controls::default());
    let provider = fixture::provider();
    let metadata = lash::LlmProfileMetadata::builder("test/external-consumer").context_window_tokens(200_000).build()?;
    let profiles = lash::LlmProfileRegistry::new().register("consumer", lash::RegisteredLlmProfile::new(metadata, provider.clone()))?;
    let core = LashCore::standard_builder(lash::Backend::new(engine.clone()))
        .llm_profiles(Arc::new(profiles))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .plugin(Arc::new(fixture::ConsumerPlugin(controls.clone())))
        .trace_sink(Arc::new(lash::tracing::JsonlTraceSink::new(root.join("trace.jsonl"))))
        .trace_level(lash::tracing::TraceLevel::Extended)
        .build(lash::persistence::LeaseOwnerIdentity::opaque("external-consumer", std::process::id().to_string()))?;
    let worker = lash::durability::DurableProcessWorker::new(core.durable_process_worker_config()?)?;
    let endpoint = engine.endpoint_builder(worker)?.build();
    let listener = tokio::net::TcpListener::bind(endpoint_addr).await?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let endpoint_task = tokio::spawn(lash::restate::serve_endpoint(listener, endpoint,
        lash::restate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
        async move { let _ = stopped.await; }));
    let operation = async {
        engine.register_deployment(&format!("http://{endpoint_addr}")).await?;
        let app = Router::new()
            .route("/healthz", get(|| async { Json(json!({"service":"external-consumer"})) }))
            .route("/sessions/{session}/inputs", post(submit))
            .route("/sessions/{session}/inputs/{input}", get(follow))
            .route("/sessions/{session}/inputs/{input}/cancel", post(cancel))
            .route("/sessions/{session}/tasks", post(task))
            .route("/sessions/{session}/runs/{run}", get(task_result))
            .route("/sessions/{session}/runs/{run}/cancel", post(task_cancel))
            .route("/control/entered", get(|State(host): State<Host>| async move { Json(host.controls.receipts()) }))
            .route("/control/release", post(|State(host): State<Host>, Json(key): Json<String>| async move { Json(json!({"released":host.controls.release(&key)})) }))
            .with_state(Host { core: core.clone(), controls });
        let listener = tokio::net::TcpListener::bind(http_addr).await?;
        axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
        anyhow::Ok(())
    }.await;
    let _ = stop.send(());
    endpoint_task.await.context("join consumer endpoint")?;
    core.shutdown().await?;
    provider.close().await?;
    core.flush_trace_sink()?;
    operation?;
    println!("CONSUMER_SHUTDOWN {{\"core\":true,\"provider\":true,\"trace\":true}}");
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => { tokio::select! { _ = terminate.recv() => {}, _ = tokio::signal::ctrl_c() => {} } },
            Err(_) => { let _ = tokio::signal::ctrl_c().await; },
        }
    }
    #[cfg(not(unix))]
    { let _ = tokio::signal::ctrl_c().await; }
}
