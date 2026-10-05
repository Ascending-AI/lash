use super::*;

fn workbench_restate_namespace() -> AnyhowResult<lash::restate::RestateNamespace> {
    Ok(std::env::var("AGENT_WORKBENCH_RESTATE_NAMESPACE")
        .ok()
        .map(|namespace| namespace.parse())
        .transpose()?
        .unwrap_or_default())
}

/// Outer bound on one workbench turn: how many model calls a single send may
/// spend. Generous, because a real workbench task legitimately takes many
/// steps; finite, because no send should be able to run forever.
pub(crate) const WORKBENCH_MAX_TURNS: usize = 128;

/// Inner bound on one workbench turn: how many consecutive model calls may
/// commit no successful execution before the turn stops. Judged workbench
/// traffic repairs a bad cell within a handful of attempts, so twelve is far
/// above ordinary repair and far below a loop worth paying for.
pub(crate) const WORKBENCH_MAX_NO_PROGRESS_ATTEMPTS: usize = 12;

/// The free Parallel Search MCP server backing the workbench's web tools. It
/// needs no API key and no auth headers.
pub(crate) const WORKBENCH_SEARCH_MCP_SERVER: &str = "parallel";
const WORKBENCH_SEARCH_MCP_URL: &str = "https://search.parallel.ai/mcp";

pub(crate) fn configure_workbench_plugins(
    plugins: &mut lash::PluginStack,
    mail_world: mail::MailWorld,
    subagent_registry: Arc<lash::subagents::CapabilityRegistry>,
    deferred_tools: deferred_tools::WorkbenchDeferredTools,
    approvals: approvals::WorkbenchApprovals,
    mcp: Arc<dyn PluginFactory>,
) {
    plugins.push(Arc::new(
        WorkbenchPluginFactory::new()
            .with_mail_world(mail_world)
            .with_deferred_tools(deferred_tools)
            .with_approvals(approvals),
    ));
    plugins.push(Arc::new(
        lash::process_controls::SessionProcessAdminPluginFactory::new(
            lash::process::lifetime::session_or_starter,
        ),
    ));
    plugins.push(Arc::new(
        lash::subagents::SubagentsPluginFactory::new(
            subagent_registry,
            lash::process::lifetime::starter,
        )
        .with_session_spec(SessionSpec::inherit()),
    ));
    plugins.push(mcp);
}

/// The `LASH_RLM_CHANNEL` value the workbench's RLM protocol factory is built
/// with, read the same way for the serving engine and for the registration
/// engine so both compose the same factories — and bind the same generation.
fn workbench_rlm_channel() -> AnyhowResult<lash::rlm::RlmChannel> {
    match std::env::var("LASH_RLM_CHANNEL") {
        Ok(value) => value.parse().map_err(anyhow::Error::msg),
        Err(std::env::VarError::NotPresent) => Ok(lash::rlm::RlmChannel::Cell),
        Err(error) => Err(error.into()),
    }
}

/// Refuse a broken deployment before any session can admit a turn. Starting
/// the real pool also checks the worker handshake, not just file existence.
fn prewarm_workbench_worker(
    workers: lash::rlm::WorkerService,
) -> AnyhowResult<lash::rlm::WorkerService> {
    workers.pool().context(
        "agent-workbench VM worker deployment is unavailable; ship lash-vm-worker beside the host or set LASH_VM_WORKER",
    )?;
    Ok(workers)
}

fn workbench_rlm_workers() -> AnyhowResult<Option<lash::rlm::WorkerService>> {
    if matches!(
        crate::session_protocol::selected()?,
        crate::session_protocol::SessionProtocol::Standard
    ) {
        return Ok(None);
    }
    let workers = std::env::var_os("LASH_VM_WORKER")
        .map(lash::rlm::WorkerService::subprocess)
        .unwrap_or_default();
    prewarm_workbench_worker(workers).map(Some)
}

/// Everything the workbench plugin stack is configured with, shared by the
/// serving engine and the `register-deployment` engine so both compute the
/// same plugin composition — and the same bound build generation.
struct WorkbenchCorePlugins {
    rlm_workers: Option<lash::rlm::WorkerService>,
    tool_provider: Option<Arc<dyn lash::tools::ToolProvider>>,
    mail_world: mail::MailWorld,
    subagent_registry: Arc<lash::subagents::CapabilityRegistry>,
    deferred_tools: deferred_tools::WorkbenchDeferredTools,
    approvals: approvals::WorkbenchApprovals,
    mcp: Arc<dyn PluginFactory>,
    #[cfg(feature = "e2e-tools")]
    operation: Arc<crate::e2e_operation::Controls>,
    /// The tool fixture's isolated worker engine, when its scenario binds one.
    #[cfg(feature = "e2e-tools")]
    worker_engine: Option<Arc<dyn PluginFactory>>,
}

/// The builder behind every workbench Restate core: the selected protocol factory
/// over `host_backend`, the required budgets, the optional dev-scenario tool
/// surface and the workbench plugin stack, shutdown marker included. The
/// caller applies serving-only extras (tracing, model profiles) and builds;
/// `build` binds the backend's generation to this composition.
async fn workbench_core_builder(
    host_backend: lash::Backend,
    rlm_channel: lash::rlm::RlmChannel,
    context_window_tokens: usize,
    plugins: WorkbenchCorePlugins,
) -> AnyhowResult<lash::LashCoreBuilder> {
    let WorkbenchCorePlugins {
        rlm_workers,
        tool_provider,
        mail_world,
        subagent_registry,
        deferred_tools,
        approvals,
        mcp,
        #[cfg(feature = "e2e-tools")]
        operation,
        #[cfg(feature = "e2e-tools")]
        worker_engine,
    } = plugins;
    let mut builder = match crate::session_protocol::selected()? {
        crate::session_protocol::SessionProtocol::Standard => {
            LashCore::standard_builder(host_backend)
        }
        crate::session_protocol::SessionProtocol::Rlm => {
            let mut rlm_config = lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(rlm_channel)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build()
                .with_lashlang_abilities(workbench_lashlang_abilities());
            if let Some(warn_tokens) =
                continue_as_warn_tokens_from_environment(context_window_tokens)?
            {
                rlm_config.continue_as_soft_warn_tokens = Some(warn_tokens);
            }
            let factory = lash::rlm::RlmProtocolPluginFactory::new(
                rlm_config,
                std::sync::Arc::new(lash::rlm::TypescriptDialect),
                &host_backend,
            )
            .with_worker_service(rlm_workers.context("RLM worker was not prewarmed")?)
            .with_deferred_tool_resolver(deferred_tools.resolver());
            LashCore::rlm_builder(host_backend, factory)
        }
    }
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024));
    if let Some(tool_provider) = tool_provider {
        builder = builder.tools(tool_provider);
    }
    let shutdown_marker =
        shutdown_marker::factory_from_env("agent-workbench").map_err(anyhow::Error::msg)?;
    #[cfg(feature = "e2e-tools")]
    let operation_namespace = workbench_restate_namespace()?.as_str().to_owned();
    Ok(builder.configure_plugins(move |plugins| {
        configure_workbench_plugins(
            plugins,
            mail_world,
            subagent_registry,
            deferred_tools,
            approvals,
            mcp,
        );
        #[cfg(feature = "e2e-tools")]
        plugins.push(Arc::new(crate::e2e_operation::OperationPlugin::new(
            operation,
            operation_namespace,
        )));
        #[cfg(feature = "e2e-tools")]
        if let Some(engine) = worker_engine {
            plugins.push(engine);
        }
        if let Some(marker) = shutdown_marker {
            plugins.push(marker);
        }
    }))
}

/// A workbench `RestateEngine` whose generation a core has bound, over a
/// scratch in-memory store set: registration reads only the engine's
/// authority, namespace, admin connection and bound generation, never the
/// stores. The core is built through [`workbench_core_builder`], the serving
/// engine's own construction, so the registered deployment advertises the
/// build the host will actually serve (FIG-4969).
pub(crate) async fn bound_workbench_engine(
    config: lash::restate::RestateConfig,
) -> AnyhowResult<Arc<WorkbenchRestateBackend>> {
    let rlm_workers = workbench_rlm_workers()?;
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .context("open the registration engine's scratch store set")?;
    let engine = Arc::new(lash::restate::RestateEngine::new(Arc::new(stores), config));
    let host_backend = lash::Backend::new(engine.clone());
    let tool_provider = failure_provider::DevProviderScenario::from_environment()?
        .and_then(failure_provider::DevProviderScenario::tool_provider);
    #[cfg(feature = "e2e-tools")]
    let tool_fixture = crate::e2e_tools::Fixture::from_env("AGENT_WORKBENCH_TOOL_FIXTURE")?;
    #[cfg(feature = "e2e-tools")]
    let tool_provider = match &tool_fixture {
        Some(fixture) => Some(fixture.tools()?),
        None => tool_provider,
    };
    let plugins = WorkbenchCorePlugins {
        rlm_workers,
        tool_provider,
        mail_world: mail::MailWorld::new(),
        subagent_registry: Arc::new(lash::subagents::default_registry(&BTreeMap::new())),
        deferred_tools: deferred_tools::WorkbenchDeferredTools::in_memory()
            .context("open the registration core's scratch deferred-tool grants")?,
        approvals: approvals::WorkbenchApprovals::in_memory()
            .context("open the registration core's scratch approval ledger")?,
        // Bind the same MCP declaration without starting another live peer.
        mcp: Arc::new(lash::mcp::McpPluginFactory::empty()),
        #[cfg(feature = "e2e-tools")]
        operation: Arc::new(crate::e2e_operation::Controls::default()),
        #[cfg(feature = "e2e-tools")]
        worker_engine: tool_fixture
            .as_ref()
            .and_then(crate::e2e_tools::Fixture::worker_engine),
    };
    let _core = workbench_core_builder(
        host_backend,
        workbench_rlm_channel()?,
        context_window_tokens_from_environment()?,
        plugins,
    )
    .await?
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        "agent-workbench",
        process_incarnation_id(),
    ))
    .context("build Lash core")?;
    Ok(engine)
}

/// The `register-deployment <endpoint-url>` subcommand: the dev launcher's
/// registration step, run as an invocation of this binary so
/// `scripts/agent-workbench-dev.sh` keeps owning when registration happens —
/// a fresh `up` registers, a restart does not — while the registration
/// itself goes through [`RestateEngine::register_deployment`] and its
/// namespace collision guard (FIG-3898) exactly as the serving engine would
/// do it.
///
/// `register_deployment` refuses an engine whose generation was never bound
/// (FIG-4969), and only a core built over the engine's backend binds it, so
/// the subcommand first builds the workbench's core — the serving engine's
/// own construction, over a scratch in-memory store set the registration
/// never touches — then registers through that bound engine.
///
/// A `NameTaken` refusal is permanent, so it exits 2 for the launcher to
/// stop retrying; every other failure is an ordinary nonzero exit.
pub(crate) async fn register_deployment_command(endpoint_url: &str) -> AnyhowResult<()> {
    let ingress_url =
        std::env::var("RESTATE_INGRESS_URL").context("RESTATE_INGRESS_URL is required")?;
    let admin_url = std::env::var("RESTATE_ADMIN_URL").context("RESTATE_ADMIN_URL is required")?;
    let authority = lash::restate::RestateAuthorityId::new(
        std::env::var("RESTATE_AUTHORITY_ID").context("RESTATE_AUTHORITY_ID is required")?,
    )
    .map_err(|error| anyhow!("RESTATE_AUTHORITY_ID: {error}"))?;
    let engine = bound_workbench_engine(
        lash::restate::RestateConfig::new(ingress_url, admin_url, authority)
            .with_namespace(workbench_restate_namespace()?),
    )
    .await?;
    match engine.register_deployment(endpoint_url).await {
        Ok(()) => Ok(()),
        Err(error @ lash::restate::RestateRegistrationError::NameTaken { .. }) => {
            eprintln!("{error}");
            std::process::exit(2)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn async_main() -> AnyhowResult<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt::init();
    let rlm_workers = workbench_rlm_workers()?;
    let context_window_tokens = context_window_tokens_from_environment()?;
    WORKBENCH_CONTEXT_WINDOW_TOKENS
        .set(context_window_tokens)
        .map_err(|_| anyhow!("agent-workbench context window was initialized more than once"))?;

    #[cfg(feature = "e2e-tools")]
    let protocol = crate::session_protocol::selected()?;
    #[cfg(feature = "e2e-tools")]
    let tool_fixture = crate::e2e_tools::Fixture::from_env("AGENT_WORKBENCH_TOOL_FIXTURE")?;
    let dev_provider_scenario = failure_provider::DevProviderScenario::from_environment()?;
    let api_key = std::env::var(OPENROUTER_API_KEY_ENV).unwrap_or_default();
    let rlm_channel = workbench_rlm_channel()?;
    validate_provider_credentials(dev_provider_scenario, &api_key)?;

    let addr: SocketAddr = std::env::var("AGENT_WORKBENCH_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3030".to_string())
        .parse()
        .context("invalid AGENT_WORKBENCH_ADDR")?;
    let restate_endpoint_addr: SocketAddr = std::env::var("AGENT_WORKBENCH_RESTATE_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:9081".to_string())
        .parse()
        .context("invalid AGENT_WORKBENCH_RESTATE_ADDR")?;
    let restate_ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let restate_authority_id =
        lash::restate::RestateAuthorityId::new(std::env::var("RESTATE_AUTHORITY_ID").context(
            "RESTATE_AUTHORITY_ID is required and must remain stable for one Restate state",
        )?)?;
    let restate_admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19070".to_string());
    let data_dir = std::env::var("AGENT_WORKBENCH_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".agent-workbench"));
    std::fs::create_dir_all(&data_dir).with_context(|| format!("create {}", data_dir.display()))?;
    let trace_path = std::env::var("AGENT_WORKBENCH_TRACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("trace.jsonl"));
    eprintln!("agent-workbench trace: {}", trace_path.display());
    let trace_path_display = trace_path.display().to_string();
    let trace_sink = Arc::new(TeeTraceSink::new([
        Arc::new(StderrTraceSink::default()) as Arc<dyn TraceSink>,
        Arc::new(JsonlTraceSink::new(trace_path)),
    ])) as Arc<dyn TraceSink>;
    let lashlang_execution_path = std::env::var("AGENT_WORKBENCH_LASHLANG_EXECUTION_TRACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("lashlang-execution.jsonl"));
    eprintln!(
        "agent-workbench Lashlang execution trace: {}",
        lashlang_execution_path.display()
    );
    let lashlang_execution = Arc::new(TraceLashlangGraphStore::default());
    let lashlang_execution_sink = Arc::new(TeeTraceSink::new([
        Arc::clone(&lashlang_execution) as Arc<dyn TraceSink>,
        Arc::new(JsonlTraceSink::new(lashlang_execution_path.clone())) as Arc<dyn TraceSink>,
    ])) as Arc<dyn TraceSink>;

    let model = dev_provider_scenario
        .map(|scenario| scenario.initial_profile().to_string())
        .unwrap_or_else(|| {
            std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "z-ai/glm-5.3-flash".to_string())
        });
    let model_variant =
        std::env::var("OPENROUTER_MODEL_VARIANT").unwrap_or_else(|_| "high".to_string());

    let provider = if let Some(scenario) = dev_provider_scenario {
        eprintln!(
            "warning: agent-workbench development provider scenario enabled: {}",
            scenario.as_str()
        );
        scenario.provider()
    } else {
        let provider_url = std::env::var("AGENT_WORKBENCH_PROVIDER_URL")
            .unwrap_or_else(|_| OPENROUTER_BASE_URL.to_owned());
        reqwest::Url::parse(&provider_url).context("invalid AGENT_WORKBENCH_PROVIDER_URL")?;
        ProviderHandle::new(crate::e2e_live_budget::install(
            OpenAiCompatibleProvider::new(api_key, provider_url)
                .with_compat(OpenAiCompat::openrouter())
                .into_components(),
        )?)
    };
    #[cfg(feature = "e2e-tools")]
    let provider = if let Some(fixture) = &tool_fixture {
        fixture.provider(match protocol {
            crate::session_protocol::SessionProtocol::Standard => {
                crate::e2e_tools::provider::FixtureProtocol::Standard
            }
            crate::session_protocol::SessionProtocol::Rlm => {
                crate::e2e_tools::provider::FixtureProtocol::Rlm
            }
        })?
    } else {
        provider
    };
    let selection = LlmProfileSelection {
        model: model.clone(),
        model_variant: Some(model_variant.clone()),
    };
    // A bad context window refuses startup rather than the first session.
    workbench_recorded_llm_profile(&selection.key())
        .map_err(|err| anyhow!("invalid OPENROUTER_MODEL metadata: {err}"))?;
    let database_url = std::env::var("AGENT_WORKBENCH_DATABASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let stores = WorkbenchStores::open(&data_dir, database_url.as_deref()).await?;
    #[cfg(feature = "e2e-tools")]
    let stores = match tool_fixture
        .as_ref()
        .map(|fixture| fixture.receiver_hold())
        .transpose()?
        .flatten()
    {
        Some(hold) => WorkbenchStores {
            backend: stores.backend,
            stores: crate::e2e_receiver_hold::hold_stores(stores.stores, hold),
        },
        None => stores,
    };
    eprintln!("agent-workbench durable store: {}", stores.backend);
    let core_store_factory = stores.stores.session_store_factory();
    let trigger_store = stores.stores.trigger_store();
    let subagent_registry = Arc::new(lash::subagents::default_registry(&BTreeMap::new()));
    let mail_world = mail::MailWorld::new();
    let sessions = WorkbenchSessions::persistent(data_dir.join("session-id"))?;
    // The boot session joins the roster so the selector lists it. A roster row
    // that already exists wins.
    sessions.ensure(&sessions.current());
    let event_tx = SessionEventRegistry::persistent(data_dir.join("product-events.json"), 1024)?;
    let restate_http = lash::http_transport::build_http_client();
    let active_turns = ActiveTurns::persistent(data_dir.join("active-turns.json"))?;
    let deferred_tools =
        deferred_tools::WorkbenchDeferredTools::open(data_dir.join("deferred-tool-grants.db"))
            .context("open workbench deferred-tool grants")?;
    let approvals = approvals::WorkbenchApprovals::open(data_dir.join("approvals.db"))
        .context("open workbench approval ledger")?;
    // Best-effort freshness feed for appended process events (ADR 0017). The
    // sink is a freshness overlay on the durable event log, never truth: no
    // delivery guarantee, and a consumer needing completeness reconciles from
    // paged event reads. Terminal observation still rides `await_terminal`.
    // `emit` must be fast, so it only hands each event to this channel; the
    // consumer task does the projection off the append path.
    let (host_shutdown, _) = tokio::sync::watch::channel(false);
    let (process_event_tx, mut process_event_rx) =
        mpsc::channel::<lash::process::ProcessEvent>(256);
    let mut process_event_shutdown = host_shutdown.subscribe();
    let process_event_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                changed = process_event_shutdown.changed() => {
                    if changed.is_err() || *process_event_shutdown.borrow() {
                        break;
                    }
                }
                event = process_event_rx.recv() => match event {
                    Some(event) => eprintln!(
                        "agent-workbench process event: process={} seq={} type={}",
                        event.process_id, event.sequence, event.event_type
                    ),
                    None => break,
                }
            }
        }
    });
    let process_event_sink = Arc::new(ChannelProcessEventSink::new(process_event_tx))
        as Arc<dyn lash::process::ProcessEventSink>;
    // One Restate backend over the store set: the engine host journals the
    // turns' effects, runs the background processes, whose appended events
    // reach the sink best-effort after their durable write, and executes each
    // session's accepted input through its `LashSession` service.
    let backend = Arc::new(lash::restate::RestateEngine::new(
        Arc::clone(&stores.stores),
        lash::restate::RestateConfig::new(
            lash::restate::RestateConnection::with_client_and_config(
                restate_ingress_url.clone(),
                restate_http.clone(),
                lash::restate::RestateConnectionConfig {
                    control_timeout_ms: 30_000,
                    attach_ceiling_ms: 6 * 60 * 60 * 1_000,
                    ..lash::restate::RestateConnectionConfig::default()
                },
            ),
            lash::restate::RestateConnection::with_client(
                restate_admin_url.clone(),
                restate_http.clone(),
            ),
            restate_authority_id.clone(),
        )
        .with_namespace(workbench_restate_namespace()?)
        .with_process_event_sink(Arc::clone(&process_event_sink)),
    ));
    let attachment_store = stores.stores.attachment_store();

    let host_backend = lash::Backend::new(backend.clone());
    let tracing = lash::runtime::TraceRuntime::new(host_backend.clock())
        .with_product_observer(Arc::clone(&lashlang_execution_sink));
    // FIG-1407: the workbench used to run `TurnBudget::Unbounded` with no
    // second bound, so a turn whose cells never committed re-called the
    // provider until someone noticed — one measured send bought 1,223 calls.
    // The turn budget is the outer bound on how much work one send may do; the
    // no-progress budget is the inner bound on how long it may fail to do any.
    // Both are host policy, and a host that wants a turn to run unbounded now
    // has to say so.
    let output_token_cap = std::env::var("AGENT_WORKBENCH_OUTPUT_TOKEN_CAP")
        .ok()
        .map(|value| value.parse::<std::num::NonZeroUsize>())
        .transpose()
        .map_err(|error| anyhow!("invalid AGENT_WORKBENCH_OUTPUT_TOKEN_CAP: {error}"))?;
    let shutdown_provider = provider.clone();
    let session_defaults = lash::SessionSpec::new(
        selection.key(),
        lash::TurnBudget::bounded(WORKBENCH_MAX_TURNS),
        lash::MaxToolCalls::new(1024),
    )
    .reasoning(selection.reasoning())
    .no_progress_budget(lash::NoProgressBudget::bounded(
        WORKBENCH_MAX_NO_PROGRESS_ATTEMPTS,
    ))
    .generation(lash::direct::GenerationOptions {
        output_token_cap,
        ..Default::default()
    })
    .attachment_acceptance(Arc::new(workbench_attachment_acceptance()));
    // Web search/fetch ride the free Parallel Search MCP server, attached with
    // no API key and no auth headers. Construction never fails on an
    // unreachable server: the pool keeps reconnecting in the background and
    // the model-facing tools appear once it is up, so an offline boot degrades
    // to "no web tools" rather than refusing to start.
    let search_url = std::env::var("AGENT_WORKBENCH_SEARCH_MCP_URL")
        .unwrap_or_else(|_| WORKBENCH_SEARCH_MCP_URL.into());
    let mcp_search =
        crate::mcp_host::factory(&search_url, provider.clone(), model.clone(), &data_dir).await?;
    for status in mcp_search.server_statuses() {
        eprintln!(
            "agent-workbench MCP server {}: connected={}, tools={}, last_error={}",
            status.server_name,
            status.health.is_connected(),
            status.tool_count,
            status.health.error().unwrap_or_else(|| "none".into())
        );
    }
    let tool_provider =
        dev_provider_scenario.and_then(failure_provider::DevProviderScenario::tool_provider);
    #[cfg(feature = "e2e-tools")]
    let tool_provider = match &tool_fixture {
        Some(fixture) => Some(fixture.tools()?),
        None => tool_provider,
    };
    #[cfg(feature = "e2e-tools")]
    let operation_controls = Arc::new(crate::e2e_operation::Controls::default());
    let plugins = WorkbenchCorePlugins {
        rlm_workers,
        tool_provider,
        mail_world: mail_world.clone(),
        subagent_registry,
        deferred_tools,
        approvals: approvals.clone(),
        mcp: Arc::clone(&mcp_search) as Arc<dyn PluginFactory>,
        #[cfg(feature = "e2e-tools")]
        operation: operation_controls.clone(),
        #[cfg(feature = "e2e-tools")]
        worker_engine: tool_fixture
            .as_ref()
            .and_then(crate::e2e_tools::Fixture::worker_engine),
    };
    // Deployment policy example. Choose these limits for the host's workload
    // before build(); session settings instead use recorded config commands.
    // let builder = builder
    //     .output_retention(lash::attachments::OutputRetentionPolicy {
    //         inline_limit_bytes: 64 * 1024, witness_bytes: 4 * 1024,
    //     })
    //     .max_attachment_bytes(Some(32 * 1024 * 1024))
    //     .attachment_read_policy(lash::attachments::AttachmentReadPolicy::DEFAULT)
    //     .attachment_upload_expiry(Duration::from_secs(24 * 60 * 60))
    //     .recovery_lease(lash::RecoveryLeaseConfig {
    //         generation_rank: 1, ..Default::default()
    //     })
    //     .recovery_pass_budget(lash::RecoveryPassBudget {
    //         attempt: Duration::from_secs(30), tick_wait: Duration::from_secs(1),
    //     })
    //     .termination(lash::runtime::TerminationPolicy::default())
    //     .abort_drain_grace(Duration::from_secs(2))
    //     .trigger_route_restorer(host_trigger_route_restorer)
    //     .process_observation_config(lash::process_observation::ProcessObservationConfig::default())
    //     .live_replay_store(Arc::new(lash::observe::InMemoryLiveReplayStore::new(
    //         lash::observe::InMemoryLiveReplayStoreConfig::default(),
    //     )))
    //     .trace_context(TraceContext::default());
    // host_trigger_route_restorer is the host's Arc<dyn lash::triggers::TriggerRouteRestorer>.
    let core = workbench_core_builder(host_backend, rlm_channel, context_window_tokens, plugins)
        .await?
        .trace_runtime(tracing)
        .trace_sink(Arc::clone(&trace_sink))
        .trace_level(TraceLevel::Extended)
        .llm_profiles(Arc::new(WorkbenchLlmProfiles {
            provider: provider.clone(),
        }))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "agent-workbench",
            process_incarnation_id(),
        ))
        .context("build Lash core")?;
    let shutdown_core = core.clone();
    // A stalled obligation is the durable, operator-actionable face of what
    // the removed worker-fault channel reported: a delivery the relay refused
    // or exhausted stays on the ledger with its reason, attempts and last
    // error until someone rearms it. The workbench polls the ledgers and
    // writes each new stall to the stderr process log — the same sink the
    // fault notices used — because a stall is an operator signal, not a UI
    // row. First-attempt failures stay retryable, so they warn at the relay
    // and leave the row `Due`; only the durable stall reaches this feed.
    let stalled_core = core.clone();
    let mut stalled_shutdown = host_shutdown.subscribe();
    let stalled_task = tokio::spawn(async move {
        let mut reported = std::collections::HashSet::new();
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = stalled_shutdown.changed() => {
                    if changed.is_err() || *stalled_shutdown.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    for kind in lash::ObligationKind::ALL {
                        let stalled = stalled_core
                            .stalled_obligations(kind, None, std::num::NonZeroUsize::MAX)
                            .await;
                        match stalled {
                            Ok(stalled) => {
                                for stalled in stalled {
                                    if reported.insert((kind, stalled.id.clone())) {
                                        eprintln!(
                                            "agent-workbench obligation stalled: kind={} id={} reason={:?} attempts={} last_error={}",
                                            kind.label(),
                                            stalled.id.as_str(),
                                            stalled.reason,
                                            stalled.attempts,
                                            stalled.last_error.as_ref().map_or_else(
                                                || "none".to_owned(),
                                                ToString::to_string,
                                            ),
                                        );
                                    }
                                }
                            }
                            Err(error) => eprintln!(
                                "agent-workbench obligation ledger fault: {} list_stalled failed: {error}",
                                kind.label()
                            ),
                        }
                    }
                }
            }
        }
    });
    let operation = async {
        let process_worker = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .context("build Restate process worker config")?,
        )
        .context("validate Restate process worker config")?;
        let process_observer = core
            .processes()
            .observer()
            .context("process observer was configured for the workbench core")?;

        let state = AppState {
            core,
            session_defaults,
            unknown_turn_terminals: UnknownTurnTerminals::default(),
            attachment_store,
            session_store_factory: Arc::clone(&core_store_factory),
            trigger_store,
            process_observer,
            sessions,
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_llm_profile: Arc::new(Mutex::new(LlmProfileSelection {
                model,
                model_variant: Some(model_variant),
            })),
            trace_sink: Some(Arc::clone(&trace_sink)),
            lashlang_execution,
            event_tx,
            restate_ingress_url,
            restate_admin_url,
            restate_http,
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world,
            active_turns,
            authorization: WorkbenchAuthorization::allow_all(),
            approvals,
        };
        state
            .ensure_current_session()
            .await
            .context("create the workbench's current session")?;
        reconcile_decided_approvals(&state).await;
        // The turns a previous incarnation was following are settled by the
        // session's engine whoever follows them; this process takes them up.
        restate::resume_turn_followers(&state).await;
        restate::watch_session_runs(&state, &state.current_session_id()).await;
        emit_workbench_trace(
            &state.trace_sink,
            None,
            "startup",
            json!({
                "addr": addr.to_string(),
                "data_dir": data_dir.display().to_string(),
                "trace_path": trace_path_display,
                "lashlang_execution_path": lashlang_execution_path.display().to_string(),
                // The served RLM language, read from the constant the session
                // list, the settings panel and the rendered system prompt all
                // select from, so the record cannot disagree with what the host
                // actually serves (FIG-3165).
                "dialect": RLM_LANGUAGE_ID,
                "model": serde_json::to_value(state.selected_llm_profile()).unwrap_or(Value::Null),
                "dev_provider_scenario": dev_provider_scenario.map(|scenario| scenario.as_str()),
                "store_backend": stores.backend,
                "restate_endpoint_addr": restate_endpoint_addr.to_string(),
                "restate_ingress_url": state.restate_ingress_url,
            }),
        );

        let event_stream_shutdown = host_shutdown.clone();
        let observation_stream_shutdown = host_shutdown.clone();
        let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/state", get(app_state))
        .route("/api/sessions/{session_id}/waits", get(list_session_waits))
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{key}/approve", post(approve_wait))
        .route("/api/approvals/{key}/deny", post(deny_wait))
        .route(
            "/api/events",
            get(move |state, query| {
                session_events_with_shutdown(
                    state,
                    query,
                    Some(event_stream_shutdown.subscribe()),
                )
            }),
        )
        .route(
            "/api/observations",
            get(move |state, query, headers| {
                session_observations_with_shutdown(
                    state,
                    query,
                    headers,
                    Some(observation_stream_shutdown.subscribe()),
                )
            }),
        )
        .route("/api/turn", post(send_turn))
        .route("/api/attachments", post(upload_attachment))
        .route("/api/attachments/{attachment_id}", get(retrieve_attachment))
        .route("/api/turn/input", post(enqueue_turn_input))
        .route("/api/turn/cancel", post(cancel_turn))
        .route("/api/session", delete(reset_chat))
        .route("/api/reset", post(reset_chat))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/select", post(select_session))
        .route("/api/sessions/{session_id}", delete(delete_session))
        .route("/api/button-trigger", post(button_trigger))
        .route("/api/triggers", get(list_triggers))
        .route(
            "/api/triggers/{subscription_key}/enabled",
            put(set_trigger_enabled),
        )
        .route("/api/triggers/{subscription_key}", delete(delete_trigger))
        .merge(trigger_occurrence_admin_routes())
        // Deliberately absent from the UI, and deliberately unscheduled: see
        // the handler's contract.
        .route("/api/admin/store-maintenance", post(run_store_maintenance))
        .route("/api/accounts", get(list_accounts).post(add_account))
        .route("/api/accounts/{slug}", delete(delete_account))
        .route("/api/accounts/{slug}/messages", post(inject_message))
        .route("/api/accounts/{slug}/messages/{id}", delete(delete_message))
        .route("/api/accounts/{slug}/inbox", get(account_inbox))
        .route("/api/work", get(list_work))
        .route("/api/queued-work", get(list_queued_work))
        .route(
            "/api/queued-work/{batch_id}",
            delete(cancel_queued_work_batch),
        )
        .route("/api/work/{process_id}/cancel", post(cancel_work))
        .route("/api/work/{process_id}/await", get(await_work))
        .route("/api/lashlang-graphs", get(list_lashlang_graphs))
        .route("/api/lashlang-graph/{graph_key}", get(lashlang_graph))
        .with_state(state.clone())
        .merge(crate::mcp_host::router(Arc::clone(&mcp_search)));
        #[cfg(feature = "e2e-tools")]
        let app = if let Some(fixture) = &tool_fixture {
            let (receiver, retained_path, event_type) = fixture.receiver_binding();
            app.merge(crate::e2e_receiver::routes(
                crate::e2e_receiver::ReceiverState {
                    app: state.clone(),
                    receiver,
                    retained_path,
                    event_type,
                },
            ))
        } else {
            app
        };
        #[cfg(feature = "e2e-tools")]
        let app = app.merge(crate::e2e_operation::routes(
            crate::e2e_operation::OperationState {
                app: state.clone(),
                controls: operation_controls.clone(),
            },
        ));
        #[cfg(feature = "provider-wire-fixtures")]
        let app = if dev_provider_scenario
            == Some(failure_provider::DevProviderScenario::ValidEmptyCompletion)
        {
            let fixture = crate::local_restate::LocalRestate {
                ingress_url: state.restate_ingress_url.clone(),
                admin_url: state.restate_admin_url.clone(),
                authority: restate_authority_id.clone(),
                namespace: lash::restate::RestateNamespace::default(),
            }
            .in_namespace(crate::valid_empty_completion::NAMESPACE)?;
            app.route(
                "/dev/valid-empty-completion",
                get(crate::valid_empty_completion::page)
                    .post(move || crate::valid_empty_completion::run(fixture.clone())),
            )
        } else {
            app
        };

        println!("agent-workbench listening on http://{addr}");
        println!("agent-workbench Restate endpoint listening on http://{restate_endpoint_addr}");
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("bind listener")?;
        let restate_listener = tokio::net::TcpListener::bind(restate_endpoint_addr)
            .await
            .context("bind Restate listener")?;
        let restate_task = restate::spawn_owned_restate_endpoint(
            restate_listener,
            state,
            Arc::clone(&backend),
            process_worker,
            host_shutdown.subscribe(),
        )
        .context("build the Restate endpoint")?;
        let signal_shutdown = host_shutdown.clone();
        let serve_result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                let _ = signal_shutdown.send(true);
            })
            .await
            .context("serve");
        let _ = host_shutdown.send(true);
        if let Err(error) = restate_task.await {
            eprintln!("agent-workbench: Restate endpoint task join failed: {error}");
        }
        serve_result
    }
    .await;
    let _ = host_shutdown.send(true);
    for (name, task) in [
        ("process event logger", process_event_task),
        ("stalled obligation logger", stalled_task),
    ] {
        if let Err(error) = task.await {
            eprintln!("agent-workbench: {name} task join failed: {error}");
        }
    }
    let cleanup = shutdown_workbench(&shutdown_core, &shutdown_provider).await;
    match (operation, cleanup) {
        (Err(primary), Err(cleanup_error)) => {
            eprintln!(
                "agent-workbench: cleanup failed after primary error `{primary:#}`: {cleanup_error:#}"
            );
            Err(primary)
        }
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Ok(()), Ok(())) => {
            println!("agent-workbench shutdown complete");
            Ok(())
        }
    }
}

#[cfg(test)]
mod worker_deployment_tests {
    use super::*;

    #[test]
    fn missing_vm_worker_refuses_startup_with_typed_deployment_fault() {
        let directory = tempfile::tempdir().expect("worker fixture directory");
        let executable = directory.path().join("lash-vm-worker");
        let result = prewarm_workbench_worker(lash::rlm::WorkerService::subprocess(&executable));
        let error = result.err().expect("a missing worker must refuse startup");
        let cause = error
            .downcast_ref::<lash::rlm::PoolError>()
            .expect("startup refusal retains the typed pool fault");
        assert!(matches!(
            cause,
            lash::rlm::PoolError::Infrastructure(
                lash::rlm::InfrastructureOutcome::WorkerDeployment {
                    executable: path,
                    fault: lash::rlm::WorkerDeploymentFault::NotFound,
                }
            ) if path == &executable
        ));
        assert!(error.to_string().contains("VM worker deployment"));
    }
}

async fn shutdown_workbench(core: &LashCore, provider: &ProviderHandle) -> AnyhowResult<()> {
    let mut first_error = None;
    if let Err(error) = core.shutdown().await {
        first_error = Some(anyhow!("core shutdown failed: {error}"));
    }
    if let Err(error) = provider.close().await {
        eprintln!("agent-workbench: provider close failed: {error}");
        if first_error.is_none() {
            first_error = Some(anyhow!("provider close failed: {error}"));
        }
    }
    if let Err(error) = core.flush_trace_sink() {
        eprintln!("agent-workbench: trace flush failed: {error}");
        if first_error.is_none() {
            first_error = Some(anyhow!("trace flush failed: {error}"));
        }
    }
    first_error.map_or(Ok(()), Err)
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
    println!("agent-workbench draining");
}

pub(crate) fn process_incarnation_id() -> &'static str {
    static PROCESS_INCARNATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PROCESS_INCARNATION
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .as_str()
}

pub(crate) fn validate_provider_credentials(
    dev_provider_scenario: Option<failure_provider::DevProviderScenario>,
    openrouter_api_key: &str,
) -> AnyhowResult<()> {
    if dev_provider_scenario.is_none() && openrouter_api_key.trim().is_empty() {
        return Err(anyhow!(
            "agent-workbench: {OPENROUTER_API_KEY_ENV} is not set and no dev provider scenario is active — refusing to start (set the key or {})",
            failure_provider::DEV_PROVIDER_SCENARIO_ENV
        ));
    }
    Ok(())
}

pub(crate) fn context_window_tokens_from_environment() -> AnyhowResult<usize> {
    context_window_tokens_from(|name| std::env::var(name))
}

pub(crate) fn continue_as_warn_tokens_from_environment(
    context_window_tokens: usize,
) -> AnyhowResult<Option<usize>> {
    continue_as_warn_tokens_from(context_window_tokens, |name| std::env::var(name))
}

pub(crate) fn continue_as_warn_tokens_from(
    context_window_tokens: usize,
    read_env: impl FnOnce(&str) -> Result<String, std::env::VarError>,
) -> AnyhowResult<Option<usize>> {
    let raw = match read_env(AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS_ENV) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(anyhow!(
                "agent-workbench: {AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS_ENV} is not valid Unicode"
            ));
        }
    };
    let warn_tokens = raw.trim().parse::<usize>().map_err(|_| {
        anyhow!(
            "agent-workbench: {AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS_ENV} must be a positive integer below the context window"
        )
    })?;
    if warn_tokens == 0 || warn_tokens >= context_window_tokens {
        return Err(anyhow!(
            "agent-workbench: {AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS_ENV} must be a positive integer below the context window"
        ));
    }
    Ok(Some(warn_tokens))
}

pub(crate) fn context_window_tokens_from(
    read_env: impl FnOnce(&str) -> Result<String, std::env::VarError>,
) -> AnyhowResult<usize> {
    let raw = match read_env(AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS_ENV) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(DEFAULT_CONTEXT_WINDOW_TOKENS),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(anyhow!(
                "agent-workbench: {AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS_ENV} is not valid Unicode"
            ));
        }
    };
    let context_window_tokens = raw.trim().parse::<usize>().map_err(|_| {
        invalid_context_window_error(format_args!(
            "must be an integer of at least {MIN_CONTEXT_WINDOW_TOKENS}"
        ))
    })?;
    if context_window_tokens < MIN_CONTEXT_WINDOW_TOKENS {
        return Err(invalid_context_window_error(format_args!(
            "must be at least {MIN_CONTEXT_WINDOW_TOKENS}"
        )));
    }
    Ok(context_window_tokens)
}

pub(crate) fn invalid_context_window_error(problem: std::fmt::Arguments<'_>) -> anyhow::Error {
    anyhow!(
        "agent-workbench: {AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS_ENV} {problem}: the workbench requires a context window of at least {MIN_CONTEXT_WINDOW_TOKENS} tokens"
    )
}

pub(crate) fn workbench_context_window_tokens() -> usize {
    let configured = WORKBENCH_CONTEXT_WINDOW_TOKENS.get().copied();
    // `async_main` initializes this before constructing AppState. Unit tests
    // that exercise pure model helpers may intentionally use the default.
    debug_assert!(
        configured.is_some() || cfg!(test),
        "workbench context window must be initialized before model selection"
    );
    configured.unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS)
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn continue_as_warning_override_accepts_a_bounded_threshold() {
        assert_eq!(
            continue_as_warn_tokens_from(41_000, |name| {
                assert_eq!(name, AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS_ENV);
                Ok("21000".to_string())
            })
            .expect("valid warning override"),
            Some(21_000)
        );
        assert_eq!(
            continue_as_warn_tokens_from(41_000, |_| Err(std::env::VarError::NotPresent))
                .expect("unset warning override"),
            None
        );
    }

    #[test]
    fn continue_as_warning_override_rejects_values_outside_the_context_window() {
        for value in ["0", "41000", "invalid"] {
            let error = continue_as_warn_tokens_from(41_000, |_| Ok(value.to_string()))
                .expect_err("invalid warning override");
            assert!(
                error
                    .to_string()
                    .contains("must be a positive integer below the context window")
            );
        }
    }

    #[test]
    fn startup_refuses_context_window_below_the_minimum() {
        let below_floor = (MIN_CONTEXT_WINDOW_TOKENS - 1).to_string();
        let error = context_window_tokens_from(|_| Ok(below_floor))
            .expect_err("a window below the minimum must refuse startup");

        let message = error.to_string();
        assert!(
            message.contains("AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS"),
            "unexpected startup refusal: {error:#}"
        );
        assert!(
            message.contains(&format!("must be at least {MIN_CONTEXT_WINDOW_TOKENS}")),
            "startup refusal must state the minimum: {error:#}"
        );
    }

    #[test]
    fn startup_refuses_unparseable_context_window_with_the_minimum() {
        let error = context_window_tokens_from(|_| Ok("tight".to_string()))
            .expect_err("an unparseable context-window override must refuse startup");

        let message = error.to_string();
        assert!(
            message.contains(&format!(
                "must be an integer of at least {MIN_CONTEXT_WINDOW_TOKENS}"
            )),
            "unexpected parse refusal: {error:#}"
        );
    }

    #[test]
    fn startup_accepts_context_window_at_the_minimum() {
        let value = context_window_tokens_from(|_| Ok(MIN_CONTEXT_WINDOW_TOKENS.to_string()))
            .expect("the minimum is accepted");
        assert_eq!(value, MIN_CONTEXT_WINDOW_TOKENS);
    }

    #[test]
    fn a_selected_llm_profile_mints_with_the_once_lock_context_window_override() {
        // OnceLock is process-global and can only be initialized once; keep the
        // override in this single test and do not assert an unset state elsewhere.
        let override_tokens = 84_000;
        WORKBENCH_CONTEXT_WINDOW_TOKENS
            .set(override_tokens)
            .expect("this is the sole test that initializes the process override");
        let selected = LlmProfileSelection {
            model: "test-model".to_string(),
            model_variant: None,
        };

        let selection = llm_profile_selection_for_request(&selected, None, None)
            .expect("the request keeps the selected model");
        let model = workbench_recorded_llm_profile(&selection.key())
            .expect("the catalog mints the selected model");

        assert_eq!(model.context_window_tokens(), override_tokens);
    }

    #[test]
    fn startup_refuses_without_openrouter_key_or_dev_provider_scenario() {
        let error = validate_provider_credentials(None, "   ")
            .expect_err("missing OpenRouter credentials must refuse startup");

        assert!(
            error
                .to_string()
                .starts_with("agent-workbench: OPENROUTER_API_KEY is not set"),
            "unexpected startup refusal: {error:#}"
        );
    }
}
