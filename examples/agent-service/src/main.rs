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
    provider::{ProviderHandle, ProviderOptions},
    tracing::{JsonlTraceSink, StderrTraceSink, TeeTraceSink, TraceLevel, TraceSink},
};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

mod board;
mod db;
mod demo_plugin;
#[cfg(feature = "restate")]
mod effect_groups;
#[cfg(test)]
mod fork_compensation_tests;
#[cfg(test)]
mod fork_rewind_contract;
mod lease_triage;
#[path = "../../shared/prior_store_layout.rs"]
mod prior_store_layout;
mod raw_activities;
#[cfg(feature = "restate")]
mod restate;
mod retention;
#[cfg(test)]
mod retention_tests;
mod routes;
#[path = "../../shared/shutdown_marker.rs"]
mod shutdown_marker;
mod state;
mod ui;

fn default_openrouter_model_capability() -> lash::provider::ModelCapability {
    lash::provider::ModelCapability {
        instruction_role: Default::default(),
        native_mid_conversation_system: false,
        attachment_acceptance: service_attachment_acceptance().into(),
        google_dialect: Default::default(),
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
        reasoning_retention: Default::default(),
    }
}

fn default_openrouter_model_capability_for(model: lash::ModelSpec) -> lash::ModelSpec {
    model.with_capability(default_openrouter_model_capability())
}

use crate::db::AppDb;
#[cfg(feature = "restate")]
use crate::demo_plugin::{DemoPlugin, DemoPluginConfig};
#[cfg(feature = "restate")]
use crate::effect_groups::{
    AgentServiceEffectGroupExecutors, AgentServiceEffectGroupWorkflow,
    AgentServiceEffectGroupWorkflowImpl,
};
use crate::raw_activities::stream_raw_activities;
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
use lash_restate::RestateEngine;

const DEFAULT_TOKIO_THREAD_STACK_BYTES: usize = 2 * 1024 * 1024;

/// The backend the service runs on, which also keeps its RLM factory's
/// Lashlang artifacts, with the handles on its stores that the service's own
/// retention pass uses.
struct ServiceBackend {
    backend: lash::Backend,
    store_factory: Arc<lash_sqlite_store::SqliteSessionStoreFactory>,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
}

/// Ask the store whether its durable data opens under this build, before a
/// single thing is wired.
///
/// Every durable format lash writes fails closed at a version boundary: there
/// is no migration decoder, so state parked by another build is refused rather
/// than read. Discovering that by booting is a crash loop — the supervisor sees
/// a process that died, restarts it, and it dies the same way forever — because
/// the refusal is permanent and a restart is the one remedy that cannot fix it.
///
/// So the host asks first, on a read-only handle built from the paths its
/// backend is about to open rather than from a store it has already
/// constructed. Constructing the store is itself the side-effectful act this
/// precedes: it takes the write lock and applies the schema batch.
///
/// The answer is one line and one exit. A supervisor reading a single sentence
/// naming the boundary and the remedy can be configured not to restart; a
/// supervisor reading a stack trace from somewhere inside turn execution
/// cannot.
///
/// Summary mode is the right mode for a boot: it reads the schema stamps and
/// the process registry, both bounded by the number of parked processes, and
/// skips the per-session blob walk that an operator runs deliberately before a
/// version bump. The report names what it skipped, so the exit code is never
/// justified by a silence.
async fn preflight_or_exit(session_store_root: &std::path::Path) -> anyhow_like::Result<()> {
    let handle =
        lash_sqlite_store::SqliteStorePreflight::for_session_store_root(session_store_root)
            .with_process_registry(
                session_store_root
                    .join(lash_sqlite_store::SqliteDatabase::ProcessRegistry.file_name()),
            )
            .with_trigger_store(
                session_store_root.join(lash_sqlite_store::SqliteDatabase::Triggers.file_name()),
            );
    let report =
        lash::preflight::probe_store(&handle, lash::preflight::PreflightOptions::summary())
            .await
            .map_err(|err| format!("agent-service could not read its store: {err}"))?;
    // Only `Refused` exits, which means an `Undecided` report boots. That is
    // deliberate: undecided says the probe could not read far enough to decide,
    // usually a surface it could not reach, and refusing to start on a report
    // that never found a boundary would turn a preflight into an outage of its
    // own. The store's own open path is still fail-closed, so the boundaries
    // this probe could not see are still refused where they matter.
    let Some(refusal) = report.refusal_message() else {
        eprintln!(
            "agent-service store preflight: {} ({} mode)",
            report.outcome.name(),
            report.mode.name()
        );
        return Ok(());
    };
    // The drain list is what turns "refused" into work somebody can do. It is
    // printed beside the refusal rather than left for a second command, because
    // the process is about to exit and there is no second command.
    for blocker in &report.drain {
        eprintln!("agent-service drain first: {}", blocker.detail);
    }
    Err(refusal)
}

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
    #[cfg(feature = "restate")]
    let restate_authority_id = (durability == AgentServiceDurability::Restate)
        .then(|| {
            std::env::var("RESTATE_AUTHORITY_ID")
                .map_err(|_| "RESTATE_AUTHORITY_ID is required for Restate durability".to_string())
                .and_then(|value| {
                    lash_restate::RestateAuthorityId::new(value).map_err(|error| error.to_string())
                })
        })
        .transpose()?;
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
    let session_store_root = data_dir.join("lash-sessions");
    prior_store_layout::refuse_prior_store_layout(
        &data_dir,
        &[
            "processes.db",
            "triggers.db",
            "artifacts.db",
            "process-env.db",
            "attachments",
        ],
    )
    .map_err(|refusal| refusal.to_string())?;
    preflight_or_exit(&session_store_root).await?;
    std::fs::create_dir_all(&session_store_root)
        .map_err(|err| format!("create session store root: {err}"))?;
    // One SQLite store set under the sessions root keeps the sessions, the
    // process registry, the triggers, the process environments and the
    // attachments. Local durability is the file `SqliteBackend` there, its
    // effect journal beside the stores; Restate durability puts the Restate
    // engine host over the same store set.
    #[cfg(feature = "restate")]
    let mut restate_backend: Option<Arc<RestateEngine>> = None;
    let ServiceBackend {
        backend,
        store_factory,
        attachment_store,
    } = match durability {
        AgentServiceDurability::Local => {
            let backend = Arc::new(
                lash_sqlite_store::SqliteBackend::open(&session_store_root)
                    .await
                    .map_err(|err| err.to_string())?,
            );
            ServiceBackend {
                store_factory: backend.session_store_factory(),
                attachment_store: backend.attachment_store(),
                backend: backend.into(),
            }
        }
        AgentServiceDurability::Restate => {
            #[cfg(feature = "restate")]
            {
                let stores = lash_sqlite_store::SqliteStoreSet::open(&session_store_root)
                    .await
                    .map_err(|err| err.to_string())?;
                let store_factory = stores.session_store_factory();
                let attachment_store =
                    stores.attachment_store() as Arc<dyn lash::persistence::AttachmentStore>;
                let backend = Arc::new(RestateEngine::new(
                    Arc::new(stores),
                    lash::restate::config(
                        restate_ingress_url.clone(),
                        restate_authority_id
                            .clone()
                            .expect("Restate authority configured"),
                        // The service runs every turn in the foreground through a
                        // handler-scoped controller and enqueues no work; an
                        // in-process queue pump would race the Restate handlers.
                        lash_restate::RestateQueuedWork::Disabled,
                    ),
                ));
                // Restate-backed turns pass a handler-scoped controller per
                // turn via `.stream_to_with_effects(..., &controller)`; the
                // backend host serves paths outside a workflow scope and
                // fails loudly if an effect tries to execute without a
                // handler. The worked example keeps its Sleep-only resolver as
                // the host's one answer, so no tool-child host is installed —
                // the same shape the conformance suites use.
                backend
                    .effect_host()
                    .register_group_executors(Arc::new(AgentServiceEffectGroupExecutors))
                    .map_err(|err| err.to_string())?;
                restate_backend = Some(Arc::clone(&backend));
                ServiceBackend {
                    backend: backend.into(),
                    store_factory,
                    attachment_store,
                }
            }
            #[cfg(not(feature = "restate"))]
            unreachable!("restate mode is rejected before core construction");
        }
    };
    // An unbound handle is the factory-wide reachability-audit target. Vacuum
    // deliberately uses separately opened, session-bound handles in the
    // retention pass below.
    let maintenance_store = Arc::new(
        lash_sqlite_store::Store::open(
            &session_store_root.join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name()),
        )
        .await
        .map_err(|err| err.to_string())?,
    );
    let app_db = AppDb::open(&data_dir.join("app.db")).map_err(|err| err.to_string())?;
    #[cfg(feature = "restate")]
    let shared_db = Arc::new(Mutex::new(app_db));
    let model_spec = lash::ModelSpec::builder(model.clone())
        .variant(lash::provider::ReasoningSelection::Effort(
            model_variant.clone(),
        ))
        .context_window_tokens(200_000)
        .build()
        .map_err(|err| format!("invalid OPENROUTER_MODEL metadata: {err}"))?
        .with_capability(default_openrouter_model_capability());
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    let mut core_builder = lash::LashCore::rlm_builder(
        backend,
        lash::TurnBudget::Unbounded,
        factory,
    )
    .provider(provider)
    .model(model_spec)
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
    .trace_sink(Arc::new(TeeTraceSink::new([
        Arc::new(StderrTraceSink::default()) as Arc<dyn TraceSink>,
        Arc::new(JsonlTraceSink::new(trace_path)),
    ])))
    .trace_level(TraceLevel::Extended)
    // The `processes` module is catalogue presence, not an ability bit
    // (ADR 0095): the served cells author `processes.start`, so the surface
    // exists only where this factory is installed.
    .plugin(Arc::new(
        lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
    ));
    if let Some(marker) = shutdown_marker::factory_from_env("agent-service")? {
        core_builder = core_builder.plugin(marker);
    }
    let core = core_builder
        .build(session_owner.clone())
        .map_err(|err| err.to_string())?;
    let shutdown_core = core.clone();
    let operation = async {
        #[cfg(feature = "restate")]
        let turn_work_driver = match &restate_backend {
            None => core.turn_work_driver(),
            Some(backend) => backend.turn_work_driver(),
        };
        #[cfg(not(feature = "restate"))]
        let turn_work_driver = core.turn_work_driver();

        #[cfg(feature = "restate")]
        let process_worker = if durability == AgentServiceDurability::Restate {
            let demo_factory = DemoPlugin::factory(&DemoPluginConfig {
                db: Arc::clone(&shared_db),
            });
            Some(
                DurableProcessWorker::new(
                    core.durable_process_worker_config_with_plugins([demo_factory])
                        .map_err(|err| err.to_string())?,
                )
                .map_err(|err| err.to_string())?,
            )
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
        let restate_authority_id = (durability == AgentServiceDurability::Restate)
            .then_some(restate_authority_id.expect("Restate authority configured"));
        #[cfg(feature = "restate")]
        let state = AppStateData::from_shared_db(
            core,
            turn_work_driver,
            Arc::clone(&shared_db),
            model,
            Some(model_variant),
            durability,
            restate_ingress_url,
            restate_authority_id,
        );
        #[cfg(not(feature = "restate"))]
        let state = AppStateData::new(
            core,
            turn_work_driver,
            app_db,
            model,
            Some(model_variant),
            durability,
        );
        state
            .recover_pending_chat_forks()
            .await
            .map_err(|err| format!("recover pending chat forks: {err}"))?;

        #[cfg(feature = "restate")]
        let restate_endpoint = if let Some(restate_backend) = restate_backend {
            // Lash's own services come from the backend; the service binds
            // only its turn and effect-group demo workflows beside them.
            let endpoint = restate_backend
                .endpoint_builder(process_worker.expect("process worker configured for Restate"))
                .bind(lash_restate::turn_service(
                    AgentServiceTurnWorkflowImpl::new(state.clone()).serve(),
                    "run",
                ))
                .bind(AgentServiceEffectGroupWorkflowImpl.serve())
                .build();
            let _ = restate_backend
                .process_deployment()
                .process_work()
                .admit_pending_processes("agent_service_startup")
                .await
                .map_err(|err| err.to_string())?;
            let listener = tokio::net::TcpListener::bind(restate_endpoint_addr)
                .await
                .map_err(|err| format!("bind agent-service Restate endpoint: {err}"))?;
            println!("agent-service Restate endpoint listening on http://{restate_endpoint_addr}");
            Some((endpoint, listener))
        } else {
            None
        };

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
        .route(
            "/api/chats/{chat_id}/activities",
            axum::routing::post(stream_raw_activities),
        )
        .route("/api/chats/{chat_id}/board", get(chat_board))
        .route(
            "/api/chats/{chat_id}/branch-points",
            get(list_chat_branch_points).post(pin_chat_branch_point),
        )
        .route("/api/chats/{chat_id}/forks", axum::routing::post(fork_chat))
        // Operator triage read for a chat whose turn looks stuck. This is a
        // read-only diagnostic that never authorizes fencing or cancellation.
        // It names the replica and boot running the session, so any deployment
        // beyond this localhost demo must authenticate and authorize the caller
        // before this route is reachable.
        .route(
            "/api/chats/{chat_id}/lease",
            get(crate::lease_triage::chat_lease_triage),
        )
        .route(
            "/api/chats/{chat_id}/turns/{turn_id}/cancel",
            axum::routing::post(cancel_turn),
        );
        #[cfg(feature = "restate")]
        let app = app
            .route(
                "/api/effect-groups",
                axum::routing::post(crate::effect_groups::run_effect_group),
            )
            .route(
                "/api/effect-groups/{run_id}",
                get(crate::effect_groups::get_effect_group),
            );
        let app = app.with_state(state);

        println!(
            "agent-service listening on http://{addr} (durability: {})",
            durability.as_str()
        );
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|err| err.to_string())?;
        let (host_shutdown, _) = tokio::sync::watch::channel(false);
        // Host-scheduled store and process retention runs in both durability modes:
        // whichever durable stores back the deployment are the ones that grow.
        let retention_task = crate::retention::spawn_retention(
            drain_state.clone(),
            crate::retention::StoreRetentionTargets {
                factory: store_factory,
                gc_store: maintenance_store as Arc<dyn lash::persistence::StoreMaintenance>,
                attachment_store,
            },
            retention_processes,
            host_shutdown.subscribe(),
        );
        #[cfg(feature = "restate")]
        let restate_task = restate_endpoint.map(|(endpoint, listener)| {
            let mut shutdown = host_shutdown.subscribe();
            tokio::spawn(async move {
                restate_sdk::http_server::HttpServer::new(endpoint)
                    .serve_with_cancel(listener, async move {
                        while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
                    })
                    .await;
            })
        });
        // This example's first drain step is to stop admitting. Axum's graceful
        // shutdown stops accepting connections and lets in-flight requests finish
        // once a signal arrives.
        let signal_shutdown = host_shutdown.clone();
        let serve_result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                let _ = signal_shutdown.send(true);
            })
            .await
            .map_err(|err| err.to_string());
        let _ = host_shutdown.send(true);
        if let Err(error) = retention_task.await {
            eprintln!("agent-service: retention task join failed: {error}");
        }
        #[cfg(feature = "restate")]
        if let Some(task) = restate_task
            && let Err(error) = task.await
        {
            eprintln!("agent-service: Restate endpoint task join failed: {error}");
        }
        serve_result
    }
    .await;
    // Admission and owned maintenance/endpoints have stopped. Release the core
    // factories before provider and trace finalization.
    let cleanup = drain(&shutdown_core, &drain_provider).await;
    match (operation, cleanup) {
        (Err(primary), Err(cleanup_error)) => {
            eprintln!(
                "agent-service: cleanup failed after primary error `{primary}`: {cleanup_error}"
            );
            Err(primary)
        }
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Ok(()), Ok(())) => {
            println!("agent-service shutdown complete");
            Ok(())
        }
    }
}

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
/// stopped mid-claim. The host also closes provider transports and flushes its
/// trace sink, as this example does below.
async fn drain(core: &lash::LashCore, provider: &ProviderHandle) -> anyhow_like::Result<()> {
    let mut first_error = None;
    if let Err(err) = core.shutdown().await {
        first_error = Some(format!("core shutdown failed: {err}"));
    }
    // Release provider transports (the Codex provider sends WebSocket Close
    // frames; the default provider close is a no-op).
    if let Err(err) = provider.close().await {
        eprintln!("agent-service: provider close failed: {err}");
        first_error.get_or_insert_with(|| format!("provider close failed: {err}"));
    }
    // Flush the trace sink (fsync the JSONL). An OTel host would also flush its
    // own TracerProvider here, which lash cannot do for it.
    if let Err(err) = core.flush_trace_sink() {
        eprintln!("agent-service: trace flush failed: {err}");
        first_error.get_or_insert_with(|| format!("trace flush failed: {err}"));
    }
    first_error.map_or(Ok(()), Err)
}

fn service_attachment_acceptance() -> lash::provider::AttachmentCapabilitySnapshot {
    use lash::provider::{
        AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
        AttachmentMimeSource,
    };
    // This example host owns its model catalogue and revision. Existing sessions
    // retain the opening snapshot when this catalogue changes.
    AttachmentCapabilitySnapshot {
        revision: "service-attachments-1".into(),
        acceptors: ["OpenAI Chat Completions"]
            .into_iter()
            .map(|provider| AttachmentAcceptor {
                provider: provider.into(),
                rules: [
                    AttachmentMimeSource::Inline,
                    AttachmentMimeSource::Stored,
                    AttachmentMimeSource::ExternalUrl,
                ]
                .into_iter()
                .map(|source| AttachmentAcceptanceRule::Mime {
                    source,
                    media_types: ["image/jpeg", "image/png", "image/gif", "image/webp"]
                        .into_iter()
                        .map(String::from)
                        .collect(),
                    media_families: Vec::new(),
                })
                .collect(),
            })
            .collect(),
    }
}
