use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "restate")]
use std::sync::Mutex;

use axum::Router;
use axum::routing::get;
#[cfg(feature = "restate")]
use lash::PluginBinding;
use lash::{
    durability::InlineEffectHost,
    provider::{ProviderHandle, ProviderOptions},
    tracing::{JsonlTraceSink, StderrTraceSink, TeeTraceSink, TraceLevel, TraceSink},
};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

mod board;
mod db;
mod demo_plugin;
#[cfg(feature = "restate")]
mod restate;
mod routes;
mod state;
mod ui;

fn default_openrouter_model_capability() -> lash::provider::ModelCapability {
    lash::provider::ModelCapability {
        reasoning: Some(lash::provider::ReasoningCapability {
            efforts: ["low", "medium", "high"]
                .into_iter()
                .map(String::from)
                .collect(),
            default_effort: Some("medium".to_string()),
            encoding: lash::provider::ReasoningEncoding::Effort,
            ..lash::provider::ReasoningCapability::default()
        }),
        cache_control: Some(lash::provider::CacheControlDialect::Anthropic),
        stream_termination: None,
        sampling: lash::provider::SamplingCapability::Configurable,
    }
}

fn default_openrouter_model_capability_for(model: lash::ModelSpec) -> lash::ModelSpec {
    model.with_capability(default_openrouter_model_capability())
}

use crate::db::AppDb;
#[cfg(feature = "restate")]
use crate::demo_plugin::{DemoPlugin, DemoPluginConfig};
#[cfg(feature = "restate")]
use crate::restate::{AgentServiceTurnWorkflow, AgentServiceTurnWorkflowImpl};
use crate::routes::{
    cancel_turn, chat_board, create_chat, fork_chat, index, list_chat_branch_points, list_chats,
    list_messages, pin_chat_branch_point, send_message, settings, update_chat_model,
};
use crate::state::{AgentServiceDurability, AppStateData, anyhow_like};
#[cfg(feature = "restate")]
use lash::durability::DurableProcessWorker;
#[cfg(feature = "restate")]
use lash_restate::{
    LashDurableWaitIndex, LashDurableWaitIndexImpl, LashDurableWaitWorkflow,
    LashDurableWaitWorkflowImpl, LashProcessWorkflow, RestateProcessDeployment,
    RestateTurnDeployment,
};

const DEFAULT_TOKIO_THREAD_STACK_BYTES: usize = 2 * 1024 * 1024;

fn main() -> anyhow_like::Result<()> {
    let stack_bytes = std::env::var("AGENT_SERVICE_TOKIO_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOKIO_THREAD_STACK_BYTES);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack_bytes)
        .build()
        .map_err(|err| format!("build agent-service Tokio runtime: {err}"))?
        .block_on(async_main())
}

async fn async_main() -> anyhow_like::Result<()> {
    let _ = dotenvy::dotenv();

    let durability = AgentServiceDurability::configured()?;
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| "OPENROUTER_API_KEY is required".to_string())?;
    let model = std::env::var("OPENROUTER_MODEL")
        .unwrap_or_else(|_| "anthropic/claude-sonnet-4.6".to_string());
    let model_variant =
        std::env::var("OPENROUTER_MODEL_VARIANT").unwrap_or_else(|_| "high".to_string());
    let addr: SocketAddr = std::env::var("AGENT_SERVICE_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_string())
        .parse()
        .map_err(|err| format!("invalid AGENT_SERVICE_ADDR: {err}"))?;
    #[cfg(feature = "restate")]
    let restate_endpoint_addr: SocketAddr = std::env::var("AGENT_SERVICE_RESTATE_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:9080".to_string())
        .parse()
        .map_err(|err| format!("invalid AGENT_SERVICE_RESTATE_ADDR: {err}"))?;
    #[cfg(feature = "restate")]
    let restate_ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    #[cfg(not(feature = "restate"))]
    if durability == AgentServiceDurability::Restate {
        return Err(
            "AGENT_SERVICE_DURABILITY=restate requires `cargo run -p agent-service --features restate`"
                .to_string(),
        );
    }
    let data_dir = std::env::var("AGENT_SERVICE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".agent-service"));
    std::fs::create_dir_all(&data_dir).map_err(|err| err.to_string())?;
    let trace_path = std::env::var("AGENT_SERVICE_TRACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("trace.jsonl"));
    eprintln!("agent-service trace: {}", trace_path.display());

    let provider = ProviderHandle::new(
        OpenAiCompatibleProvider::new(api_key, OPENROUTER_BASE_URL)
            .with_compat(OpenAiCompat::openrouter())
            .with_options(ProviderOptions {
                expose_thinking: true,
                ..ProviderOptions::default()
            })
            .into_components(),
    );
    // Retain a clone for the shutdown drain: the core owns the working copy, but
    // the host is what calls `close()` to release transports on the way out.
    let drain_provider = provider.clone();

    // Worker identity for durable session-execution leases. WORKER_ID is stable
    // across restarts (set one per replica in a fleet); the incarnation is
    // bumped every boot. If this process crashes, the lease remains busy until
    // its TTL expires. The identity is stable within a boot, so keep at most one
    // in-flight turn per chat; the fenced head commit is the last-resort
    // single-writer backstop.
    let worker_id = std::env::var("WORKER_ID").unwrap_or_else(|_| "agent-service-1".to_string());
    let worker_incarnation = std::env::var("AGENT_SERVICE_INCARNATION").unwrap_or_else(|_| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis().to_string())
            .unwrap_or_else(|_| "0".to_string())
    });
    let session_owner =
        lash::persistence::LeaseOwnerIdentity::opaque(worker_id, worker_incarnation);
    let process_registry_path = data_dir.join("processes.db");
    let session_store_root = data_dir.join("lash-sessions");
    let store_factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(
            &session_store_root,
            &process_registry_path,
        ),
    );
    // Deployment-level Lashlang artifact store (compiled module cache), shared
    // across the session tree and durable in SQLite.
    let artifact_store = Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
            .await
            .map_err(|err| err.to_string())?,
    ) as Arc<dyn lash::persistence::LashlangArtifactStore>;
    let process_env_store = Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
            .await
            .map_err(|err| err.to_string())?,
    );
    let trigger_store = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
            .await
            .map_err(|err| err.to_string())?,
    );
    let app_db = AppDb::open(&data_dir.join("app.db")).map_err(|err| err.to_string())?;
    #[cfg(feature = "restate")]
    let shared_db = Arc::new(Mutex::new(app_db));
    let model_spec = lash::ModelSpec::from_token_limits(
        model.clone(),
        lash::provider::ReasoningSelection::Effort(model_variant.clone()),
        200_000,
        None,
    )
    .map_err(|err| format!("invalid OPENROUTER_MODEL metadata: {err}"))?
    .with_capability(default_openrouter_model_capability());
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::default(),
        artifact_store,
    );
    let core_builder = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(model_spec)
        .store_factory(store_factory)
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .process_env_store(process_env_store)
        .trace_sink(Arc::new(TeeTraceSink::new([
            Arc::new(StderrTraceSink::default()) as Arc<dyn TraceSink>,
            Arc::new(JsonlTraceSink::new(trace_path)),
        ])))
        .trace_level(TraceLevel::Extended)
        .trigger_store(trigger_store);
    let process_registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(&process_registry_path, session_store_root)
            .await
            .map_err(|err| err.to_string())?,
    ) as Arc<dyn lash::process::ProcessRegistry>;
    #[cfg(feature = "restate")]
    let process_deployment = (durability == AgentServiceDurability::Restate).then(|| {
        RestateProcessDeployment::new(restate_ingress_url.clone(), Arc::clone(&process_registry))
    });
    #[cfg(feature = "restate")]
    let turn_deployment = (durability == AgentServiceDurability::Restate)
        .then(|| RestateTurnDeployment::new(restate_ingress_url.clone()));
    let core = match durability {
        AgentServiceDurability::Local => core_builder
            .effect_host(Arc::new(
                InlineEffectHost::default().allow_process_lifetime_completion_keys(),
            ))
            .process_registry(Arc::clone(&process_registry))
            .build()
            .map_err(|err| err.to_string())?,
        AgentServiceDurability::Restate => {
            #[cfg(feature = "restate")]
            {
                // Deployment host for paths outside a Restate workflow scope;
                // it fails loudly if an effect tries to execute without a
                // handler. Restate-backed turns pass a handler-scoped
                // controller per turn via `.effects(&controller)`. The
                // Restate ingress runner is the sole executor of
                // out-of-turn/background processes.
                core_builder
                    .effect_host(
                        turn_deployment
                            .as_ref()
                            .expect("turn deployment configured for Restate")
                            .effect_host(),
                    )
                    .process_work_driver(
                        process_deployment
                            .as_ref()
                            .expect("process deployment configured for Restate")
                            .process_work_driver(),
                    )
                    .build()
                    .map_err(|err| err.to_string())?
            }
            #[cfg(not(feature = "restate"))]
            unreachable!("restate mode is rejected before core construction");
        }
    };
    #[cfg(feature = "restate")]
    let turn_work_driver = match durability {
        AgentServiceDurability::Local => core.turn_work_driver(),
        AgentServiceDurability::Restate => turn_deployment
            .as_ref()
            .expect("turn deployment configured for Restate")
            .turn_work_driver(),
    };
    #[cfg(not(feature = "restate"))]
    let turn_work_driver = core.turn_work_driver();

    #[cfg(feature = "restate")]
    let process_worker = if durability == AgentServiceDurability::Restate {
        let demo_factory = DemoPlugin::factory(&DemoPluginConfig {
            db: Arc::clone(&shared_db),
        });
        Some(DurableProcessWorker::new(
            core.durable_process_worker_config_with_plugins([demo_factory])
                .map_err(|err| err.to_string())?,
        ))
    } else {
        None
    };
    // Capture a process facade handle before `core` is moved into the app
    // state, so host-scheduled retention runs through the same
    // `Processes::prune` lever every embedder uses.
    let retention_processes = core.processes();
    #[cfg(feature = "restate")]
    let restate_ingress_url =
        (durability == AgentServiceDurability::Restate).then_some(restate_ingress_url);
    #[cfg(feature = "restate")]
    let state = AppStateData::from_shared_db(
        core,
        turn_work_driver,
        Arc::clone(&shared_db),
        session_owner,
        model,
        Some(model_variant),
        durability,
        restate_ingress_url,
    );
    #[cfg(not(feature = "restate"))]
    let state = AppStateData::new(
        core,
        turn_work_driver,
        app_db,
        session_owner,
        model,
        Some(model_variant),
        durability,
    );
    state
        .recover_pending_chat_forks()
        .await
        .map_err(|err| format!("recover pending chat forks: {err}"))?;

    #[cfg(feature = "restate")]
    if durability == AgentServiceDurability::Restate {
        let process_deployment = process_deployment.expect("process deployment configured");
        let endpoint = restate_sdk::endpoint::Endpoint::builder()
            .bind(AgentServiceTurnWorkflowImpl::new(state.clone()).serve())
            .bind(
                process_deployment
                    .workflow(process_worker.expect("process worker configured for Restate"))
                    .serve(),
            )
            .bind(LashDurableWaitWorkflowImpl.serve())
            .bind(LashDurableWaitIndexImpl.serve())
            .build();
        tokio::spawn(async move {
            restate_sdk::http_server::HttpServer::new(endpoint)
                .listen_and_serve(restate_endpoint_addr)
                .await;
        });
        process_deployment
            .process_work_driver()
            .claim_and_run_pending("agent_service_startup")
            .await
            .map_err(|err| err.to_string())?;
        println!("agent-service Restate endpoint listening on http://{restate_endpoint_addr}");
    }

    // Host-scheduled retention for terminal process rows (ADR 0017). This runs
    // in both durability modes: whichever registry backs the deployment is the
    // one that accumulates rows.
    spawn_process_retention(retention_processes);

    // Keep a state clone for the drain; the router consumes the original.
    let drain_state = state.clone();
    let app = Router::new()
        .route("/", get(index))
        .route("/api/settings", get(settings))
        .route("/api/chats", get(list_chats).post(create_chat))
        .route(
            "/api/chats/{chat_id}/model",
            axum::routing::post(update_chat_model),
        )
        .route(
            "/api/chats/{chat_id}/messages",
            get(list_messages).post(send_message),
        )
        .route("/api/chats/{chat_id}/board", get(chat_board))
        .route(
            "/api/chats/{chat_id}/branch-points",
            get(list_chat_branch_points).post(pin_chat_branch_point),
        )
        .route("/api/chats/{chat_id}/forks", axum::routing::post(fork_chat))
        .route(
            "/api/chats/{chat_id}/turns/{turn_id}/cancel",
            axum::routing::post(cancel_turn),
        )
        .with_state(state);

    println!(
        "agent-service listening on http://{addr} (durability: {})",
        durability.as_str()
    );
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| err.to_string())?;
    // Step 1 of the drain (see docs/operations.html): stop admitting. Axum's
    // graceful shutdown stops accepting connections and lets in-flight requests
    // finish once a signal arrives.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| err.to_string())?;
    // Admission has stopped; run the teardown levers this process owns.
    drain(&drain_state, &drain_provider).await;
    Ok(())
}

/// Host-scheduled retention for terminal process rows (ADR 0017). The process
/// registry keeps a row — plus its events, wake deliveries, handle grants, and leases
/// — for every process this service ever started; once a run is terminal and
/// its outcome has been consumed, those rows have no remaining reader and would
/// grow without bound. `prune_terminal_processes` drops terminal rows older than
/// a cutoff and never touches a non-terminal row. Retention is a host lever, not
/// an automatic sweep inside the registry: only the host knows its window. Run
/// it on the same maintenance cadence as the session-store `vacuum` /
/// `gc_unreachable` reclamation (see docs/persistence.html).
///
/// Retention is driven through the [`Processes::prune`](lash::process::Processes::prune)
/// facade lever rather than the raw registry method, so the example uses the
/// same host surface every embedder does.
fn spawn_process_retention(processes: lash::process::Processes) {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // The window must be comfortably longer than any wait: a cutoff shorter than
    // a live `await_terminal` could prune a process out from under a late await,
    // which then reads back as "unknown process".
    const RETENTION_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PRUNE_INTERVAL);
        // Drop the immediate first tick: nothing is old enough on a fresh boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(since) => since.as_millis() as u64,
                Err(_) => continue,
            };
            let cutoff = now_ms.saturating_sub(RETENTION_WINDOW.as_millis() as u64);
            match processes.prune(cutoff).await {
                Ok(report) if report.pruned_processes > 0 || report.pruned_events > 0 => {
                    println!(
                        "agent-service pruned {} terminal processes and {} events (cutoff {cutoff}ms)",
                        report.pruned_processes, report.pruned_events
                    );
                }
                Ok(_) => {}
                Err(err) => eprintln!("agent-service: process retention prune failed: {err}"),
            }
        }
    });
}

/// Resolve when the process receives Ctrl-C or SIGTERM — the host-owned signal
/// that begins the drain. lash has no opinion on which signal means "drain".
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    println!("agent-service draining");
}

/// Host-composed teardown. lash ships no drain orchestrator (ADR-0014): each
/// step is an explicit lever the host calls in its own order.
///
/// This service opens a fresh session per request and detaches the turn task,
/// so it holds no long-lived sessions to `park()`/`close()` here and no external
/// queued-work claims to hand back. A host that caches live sessions would, at
/// this point, `cancel_running_turns()`, then `park()` (or `close()`) each one,
/// and `abandon_queued_work_claim` / `revoke_durable_waits` for any driver it
/// stopped mid-claim. See docs/operations.html for the full lever list.
async fn drain(state: &AppStateData, provider: &ProviderHandle) {
    // Release provider transports (the Codex provider sends WebSocket Close
    // frames; the default provider close is a no-op).
    if let Err(err) = provider.close().await {
        eprintln!("agent-service: provider close failed: {err}");
    }
    // Flush the trace sink (fsync the JSONL). An OTel host would also flush its
    // own TracerProvider here, which lash cannot do for it.
    if let Err(err) = state.core().flush_trace_sink() {
        eprintln!("agent-service: trace flush failed: {err}");
    }
}
