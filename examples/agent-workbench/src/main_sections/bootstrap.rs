fn main() -> AnyhowResult<()> {
    let stack_bytes = std::env::var("AGENT_WORKBENCH_TOKIO_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOKIO_THREAD_STACK_BYTES);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack_bytes)
        .build()
        .context("build agent-workbench tokio runtime")?
        .block_on(async_main())
}

async fn async_main() -> AnyhowResult<()> {
    let _ = dotenvy::dotenv();
    let context_window_tokens = context_window_tokens_from_environment()?;
    WORKBENCH_CONTEXT_WINDOW_TOKENS
        .set(context_window_tokens)
        .map_err(|_| anyhow!("agent-workbench context window was initialized more than once"))?;

    let dev_provider_scenario = failure_provider::DevProviderScenario::from_environment()?;
    let api_key = std::env::var(OPENROUTER_API_KEY_ENV).unwrap_or_default();
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

    let tavily_api_key = std::env::var("TAVILY_API_KEY").unwrap_or_default();
    if tavily_api_key.trim().is_empty() {
        eprintln!("warning: TAVILY_API_KEY is empty; web tools will return configuration errors");
    }
    let model = dev_provider_scenario
        .map(|_| "dev/failure-paths".to_string())
        .unwrap_or_else(|| {
            std::env::var("OPENROUTER_MODEL")
                .unwrap_or_else(|_| "anthropic/claude-sonnet-4.6".to_string())
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
        ProviderHandle::new(
            OpenAiCompatibleProvider::new(api_key, OPENROUTER_BASE_URL)
                .with_compat(OpenAiCompat::openrouter())
                .with_options(ProviderOptions {
                    expose_thinking: true,
                    ..ProviderOptions::default()
                })
                .into_components(),
        )
    };
    let model_spec = lash::ModelSpec::from_token_limits(
        model.clone(),
        lash::provider::ReasoningSelection::Effort(model_variant.clone()),
        context_window_tokens,
        None,
    )
    .map_err(|err| anyhow!("invalid OPENROUTER_MODEL metadata: {err}"))?;
    let model_spec = with_workbench_model_capability(model_spec);
    let database_url = std::env::var("AGENT_WORKBENCH_DATABASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let stores = WorkbenchStores::open(&data_dir, database_url.as_deref()).await?;
    eprintln!("agent-workbench durable store: {}", stores.backend);
    let core_store_factory = Arc::clone(&stores.session_store_factory);
    let process_registry = Arc::clone(&stores.process_registry);
    let process_continuations = Arc::clone(&stores.process_continuations);
    let trigger_store = Arc::clone(&stores.trigger_store);
    let artifact_store = Arc::clone(&stores.artifact_store);
    let subagent_registry = Arc::new(lash_subagents::default_registry(&BTreeMap::new()));
    let mail_world = mail::MailWorld::new();
    let session_ids = WorkbenchSessionIds::persistent(data_dir.join("session-id"))?;
    let event_tx =
        SessionEventRegistry::persistent(data_dir.join("product-events.json"), 1024)?;
    let restate_http = reqwest::Client::new();
    let active_turns = ActiveTurns::persistent(data_dir.join("active-turns.json"))?;
    // Best-effort freshness feed for appended process events (ADR 0017). The
    // sink is a freshness overlay on the durable event log, never truth: no
    // delivery guarantee, and a consumer needing completeness reconciles from
    // `events_after`. Terminal observation still rides `await_terminal`.
    // `emit` must be fast, so it only hands each event to this channel; the
    // consumer task does the projection off the append path.
    let (process_event_tx, mut process_event_rx) =
        mpsc::channel::<lash::process::ProcessEvent>(256);
    tokio::spawn(async move {
        while let Some(event) = process_event_rx.recv().await {
            eprintln!(
                "agent-workbench process event: process={} seq={} type={}",
                event.process_id, event.sequence, event.event_type
            );
        }
    });
    let process_event_sink = Arc::new(ChannelProcessEventSink::new(process_event_tx))
        as Arc<dyn lash::process::ProcessEventSink>;
    let process_deployment = lash_restate::RestateProcessDeployment::new_with_sink(
        restate_ingress_url.clone(),
        process_registry,
        process_continuations,
        Some(process_event_sink),
    );
    // Retained so a host-facing "wait for the work item" flow can route through
    // `ProcessWorkDriver::await_terminal` (see the `/api/work/{id}/await` route).
    let process_work_driver = process_deployment.process_work_driver();
    let queued_work_driver =
        lash::runtime::QueuedWorkDriver::new(Arc::new(WorkbenchQueuedWorkSubmitter {
            session_ids: session_ids.clone(),
            store_factory: Arc::clone(&core_store_factory),
            restate_ingress_url: restate_ingress_url.clone(),
            restate_http: restate_http.clone(),
            active_turns: active_turns.clone(),
        }));

    let turn_deployment = lash_restate::RestateTurnDeployment::new(
        lash_restate::RestateConnection::with_client(
            restate_ingress_url.clone(),
            restate_http.clone(),
        ),
    );

    let attachment_store = Arc::new(lash::persistence::FileAttachmentStore::new(
        data_dir.join("attachments"),
    )) as Arc<dyn lash::persistence::AttachmentStore>;
    let runtime_host_config = lash::durability::RuntimeHostConfig::new(
        turn_deployment.effect_host(),
        Arc::clone(&attachment_store),
        Arc::clone(&stores.process_env_store),
    );

    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default()
            .with_lashlang_abilities(workbench_lashlang_abilities()),
        Arc::clone(&artifact_store),
    )
    .with_lashlang_execution_sink(Arc::clone(&lashlang_execution_sink));
    let core = LashCore::rlm_builder(factory)
        .provider(provider)
        .model(model_spec)
        .store_factory(Arc::clone(&core_store_factory))
        .trigger_store(Arc::clone(&trigger_store))
        .trace_sink(Arc::clone(&trace_sink))
        .trace_level(TraceLevel::Extended)
        .configure_plugins(|plugins| {
            plugins.push(Arc::new(
                WorkbenchPluginFactory::new(tavily_api_key.clone())
                    .with_mail_world(mail_world.clone()),
            ));
            plugins.push(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
            ));
            plugins.push(Arc::new(
                lash_subagents::SubagentsPluginFactory::new(subagent_registry)
                    .with_session_spec(SessionSpec::inherit()),
            ));
        })
        .process_work_driver(process_work_driver.clone())
        .queued_work_driver(queued_work_driver.clone())
        .advanced()
        .runtime_host_config(runtime_host_config)
        .build()
        .context("build Lash core")?;
    let process_worker = lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .context("build Restate process worker config")?,
    );
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");

    let state = AppState {
        core,
        attachment_store,
        trigger_store,
        process_observer,
        process_work_driver,
        session_ids,
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model,
            model_variant: Some(model_variant),
        })),
        web_configured: !tavily_api_key.trim().is_empty(),
        trace_sink: Some(Arc::clone(&trace_sink)),
        lashlang_execution,
        event_tx,
        queued_work_driver,
        restate_ingress_url,
        restate_admin_url,
        restate_http,
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
        mail_world,
        active_turns,
        authorization: WorkbenchAuthorization::allow_all(),
    };
    restate::spawn_restate_endpoint(
        restate_endpoint_addr,
        state.clone(),
        process_deployment,
        process_worker,
    );
    emit_workbench_trace(
        &state.trace_sink,
        None,
        "startup",
        json!({
            "addr": addr.to_string(),
            "data_dir": data_dir.display().to_string(),
            "trace_path": trace_path_display,
            "lashlang_execution_path": lashlang_execution_path.display().to_string(),
            "model": serde_json::to_value(state.selected_model()).unwrap_or(Value::Null),
            "dev_provider_scenario": dev_provider_scenario.map(|scenario| scenario.as_str()),
            "web_configured": state.web_configured,
            "store_backend": stores.backend,
            "restate_endpoint_addr": restate_endpoint_addr.to_string(),
            "restate_ingress_url": state.restate_ingress_url,
        }),
    );

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/state", get(app_state))
        .route("/api/events", get(session_events))
        .route("/api/observations", get(session_observations))
        .route("/api/turn", post(send_turn))
        .route("/api/attachments", post(upload_attachment))
        .route("/api/attachments/{attachment_id}", get(retrieve_attachment))
        .route("/api/turn/input", post(enqueue_turn_input))
        .route("/api/turn/cancel", post(cancel_turn))
        .route("/api/session", delete(reset_chat))
        .route("/api/reset", post(reset_chat))
        .route("/api/button-trigger", post(button_trigger))
        .route("/api/triggers", get(list_triggers))
        .route(
            "/api/triggers/{subscription_key}/enabled",
            put(set_trigger_enabled),
        )
        .route(
            "/api/triggers/{subscription_key}",
            delete(delete_trigger),
        )
        // Deliberately absent from the UI: see the handler's contract.
        .route(
            "/api/admin/trigger-mutation-receipts/prune",
            post(prune_trigger_mutation_receipts),
        )
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
        .route(
            "/api/queued-work/{batch_id}/run",
            post(run_queued_work_batch),
        )
        .route("/api/work/{process_id}/cancel", post(cancel_work))
        .route("/api/work/{process_id}/await", get(await_work))
        .route("/api/lashlang-graphs", get(list_lashlang_graphs))
        .route("/api/lashlang-graph/{graph_key}", get(lashlang_graph))
        .with_state(state);

    println!("agent-workbench listening on http://{addr}");
    println!("agent-workbench Restate endpoint listening on http://{restate_endpoint_addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("bind listener")?;
    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}

fn validate_provider_credentials(
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

fn context_window_tokens_from_environment() -> AnyhowResult<usize> {
    context_window_tokens_from(|name| std::env::var(name))
}

fn context_window_tokens_from(
    read_env: impl FnOnce(&str) -> Result<String, std::env::VarError>,
) -> AnyhowResult<usize> {
    let raw = match read_env(AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS_ENV) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(DEFAULT_CONTEXT_WINDOW_TOKENS),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(invalid_context_window_error("is not valid Unicode"));
        }
    };
    let context_window_tokens = raw.trim().parse::<usize>().map_err(|_| {
        invalid_context_window_error("must be an integer greater than 20,000")
    })?;
    if context_window_tokens <= 20_000 {
        return Err(invalid_context_window_error(
            "must be greater than 20,000; a window <= 20,000 is trivially true",
        ));
    }
    Ok(context_window_tokens)
}

fn invalid_context_window_error(problem: &str) -> anyhow::Error {
    anyhow!(
        "agent-workbench: {AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS_ENV} {problem}: rolling-history compaction_needed fires at max_context - 20,000, so the context window must leave room beyond that buffer"
    )
}

fn workbench_context_window_tokens() -> usize {
    WORKBENCH_CONTEXT_WINDOW_TOKENS
        .get()
        .copied()
        .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS)
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn startup_honors_context_window_environment_override() {
        let context_window_tokens = context_window_tokens_from(|name| {
            assert_eq!(name, AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS_ENV);
            Ok("42000".to_string())
        })
        .expect("valid context-window override should be accepted");

        assert_eq!(context_window_tokens, 42_000);
    }

    #[test]
    fn startup_refuses_context_window_at_compaction_buffer() {
        let error = context_window_tokens_from(|_| Ok("20000".to_string()))
            .expect_err("a window equal to the compaction buffer must refuse startup");

        let message = error.to_string();
        assert!(
            message.contains("AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS"),
            "unexpected startup refusal: {error:#}"
        );
        assert!(
            message.contains("compaction_needed fires at max_context - 20,000"),
            "startup refusal must explain the buffer predicate: {error:#}"
        );
        assert!(
            message.contains("window <= 20,000 is trivially true"),
            "startup refusal must explain the invalid range: {error:#}"
        );
    }

    #[test]
    fn startup_refuses_unparseable_context_window_with_buffer_explanation() {
        let error = context_window_tokens_from(|_| Ok("tight".to_string()))
            .expect_err("an unparseable context-window override must refuse startup");

        let message = error.to_string();
        assert!(
            message.contains("must be an integer greater than 20,000"),
            "unexpected parse refusal: {error:#}"
        );
        assert!(
            message.contains("compaction_needed fires at max_context - 20,000"),
            "parse refusal must explain the buffer predicate: {error:#}"
        );
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

    #[test]
    fn startup_allows_dev_provider_scenario_without_openrouter_key() {
        validate_provider_credentials(
            Some(failure_provider::DevProviderScenario::AuthFailureOnce),
            "",
        )
        .expect("a dev provider scenario supports keyless startup");
    }
}
