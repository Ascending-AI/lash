const ATTACHMENT_USAGE_GATE_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

#[test]
fn workbench_ui_renders_assistant_markdown() {
    assert!(ui::INDEX_HTML.contains("function renderMarkdownBlocks(markdown)"));
    assert!(ui::INDEX_HTML.contains("setMessageBody(body, message.role, message.text)"));
    assert!(ui::INDEX_HTML.contains("renderMarkdownBlocks(assistantDraftText)"));
    assert!(ui::INDEX_HTML.contains(".message.assistant .msg-body h1"));
}

#[test]
fn workbench_ui_exposes_attachment_and_usage_affordances() {
    for contract in [
        "id=\"attachmentInput\"",
        "/api/attachments",
        "attachment_id: attachment?.id || null",
        "id=\"usageTotal\"",
        "id=\"usageBreakdown\"",
        "renderUsage(state.usage)",
    ] {
        assert!(ui::INDEX_HTML.contains(contract), "missing UI contract: {contract}");
    }
}

#[test]
fn attachment_usage_gate_sqlite() {
    run_async_test_on_stack_budget("workbench-attachment-usage-sqlite-gate", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-attachment-usage-sqlite-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create SQLite gate data dir");
        let sessions = data_dir.join("lash-sessions");
        let first_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            sessions.clone(),
        )) as Arc<dyn lash::persistence::SessionStoreFactory>;
        let resumed_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(sessions))
            as Arc<dyn lash::persistence::SessionStoreFactory>;

        Box::pin(run_attachment_usage_gate(
            &data_dir,
            first_factory,
            resumed_factory,
        ))
        .await;
        std::fs::remove_dir_all(&data_dir).expect("remove SQLite gate data dir");
    });
}

#[test]
#[ignore = "requires Postgres; use `just agent-workbench-attachment-usage-gate <port>`"]
fn attachment_usage_gate_postgres() {
    run_async_test_on_stack_budget_multi_thread(
        "workbench-attachment-usage-postgres-gate",
        4,
        || async {
            let database_url = std::env::var("AGENT_WORKBENCH_USAGE_GATE_DATABASE_URL")
                .expect("AGENT_WORKBENCH_USAGE_GATE_DATABASE_URL is required");
            let first_storage = lash_postgres_store::PostgresStorage::connect(&database_url)
                .await
                .expect("connect first Postgres gate storage");
            let resumed_storage = lash_postgres_store::PostgresStorage::connect(&database_url)
                .await
                .expect("connect resumed Postgres gate storage");
            let first_factory = Arc::new(first_storage.session_store_factory())
                as Arc<dyn lash::persistence::SessionStoreFactory>;
            let resumed_factory = Arc::new(resumed_storage.session_store_factory())
                as Arc<dyn lash::persistence::SessionStoreFactory>;
            let data_dir = std::env::temp_dir().join(format!(
                "agent-workbench-attachment-usage-postgres-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&data_dir).expect("create Postgres gate data dir");

            Box::pin(run_attachment_usage_gate(
                &data_dir,
                first_factory,
                resumed_factory,
            ))
            .await;
            std::fs::remove_dir_all(&data_dir).expect("remove Postgres gate data dir");
        },
    );
}

async fn run_attachment_usage_gate(
    data_dir: &std::path::Path,
    first_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    resumed_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
) {
    let trace_path = data_dir.join("trace.jsonl");
    let session_id_path = data_dir.join("session-id");
    let session_ids = WorkbenchSessionIds::persistent(session_id_path.clone())
        .expect("create gate session id");
    let session_id = session_ids.current();
    let process_registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
            .await
            .expect("open gate process registry"),
    ) as Arc<dyn lash::process::ProcessRegistry>;
    let attachment_store = Arc::new(lash::persistence::FileAttachmentStore::new(
        data_dir.join("attachments"),
    )) as Arc<dyn lash::persistence::AttachmentStore>;
    let provider_requests = Arc::new(Mutex::new(Vec::new()));
    let provider_requests_for_call = Arc::clone(&provider_requests);
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-attachment-usage-gate")
        .complete(move |request| {
            let provider_requests = Arc::clone(&provider_requests_for_call);
            async move {
                provider_requests
                    .lock()
                    .expect("provider request lock")
                    .push(request);
                Ok(usage_gate_response())
            }
        })
        .build()
        .into_handle();
    let core = attachment_usage_gate_core(
        data_dir,
        first_factory,
        Arc::clone(&process_registry),
        Arc::clone(&attachment_store),
        provider,
        Some(Arc::new(JsonlTraceSink::new(trace_path.clone())) as Arc<dyn TraceSink>),
    );
    let state = attachment_usage_gate_state(
        core,
        Arc::clone(&attachment_store),
        Arc::clone(&process_registry),
        session_ids,
    );
    let png_bytes = base64::engine::general_purpose::STANDARD
        .decode(ATTACHMENT_USAGE_GATE_PNG_BASE64)
        .expect("decode gate PNG");

    let Json(uploaded) = upload_attachment(
        State(state.clone()),
        Json(AttachmentUploadRequest {
            name: "usage-gate.png".to_string(),
            mime: "image/png".to_string(),
            data_base64: ATTACHMENT_USAGE_GATE_PNG_BASE64.to_string(),
        }),
    )
    .await
    .expect("upload attachment through workbench API handler");
    assert_eq!(uploaded.attachment.byte_len, png_bytes.len() as u64);
    assert_eq!(
        uploaded.retrieve_url,
        format!("/api/attachments/{}", uploaded.attachment.id)
    );
    assert_retrieved_attachment(&state, &uploaded.attachment.id, &png_bytes).await;

    let turn_id = format!("attachment-usage-gate-{}", uuid::Uuid::new_v4());
    let request = restate::WorkbenchTurnWorkflowRequest {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        text: "Describe the attached PNG briefly.".to_string(),
        model: ModelSelection {
            model: "test-model".to_string(),
            model_variant: None,
        },
        attachment_id: Some(uploaded.attachment.id.to_string()),
    };
    let mut input = restate::workbench_turn_input(&state, &request)
        .await
        .expect("build attachment turn input through workbench adapter");
    input.trace_turn_id = Some(turn_id.clone());
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open gate session");
    let output = session
        .turn(input)
        .turn_id(turn_id)
        .require_finish()
        .expect("require deterministic finish")
        .run()
        .await
        .expect("run deterministic attachment turn");
    assert_eq!(output.final_value(), Some(&json!("attachment accounted")));
    session.close().await.expect("close gate session");

    {
        let requests = provider_requests.lock().expect("provider request lock");
        assert_eq!(requests.len(), 1, "gate must make exactly one LLM call");
        assert_eq!(requests[0].attachments.len(), 1);
        let source = &requests[0].attachments[0];
        assert_eq!(source.media_type().map(lash_core::MediaType::as_str), Some("image/png"));
        assert_eq!(requests[0].attachment_bytes(source), Some(png_bytes.as_slice()));
        assert_eq!(
            source.stored_ref().map(|reference| &reference.id),
            Some(&uploaded.attachment.id)
        );
    }

    let Json(before_restart) =
        Box::pin(app_state(State(state.clone()), Query(SessionQuery::default())))
            .await
            .expect("read pre-restart workbench state API");
    assert_usage_report_consistent(&before_restart.usage);
    let call_usage = completed_llm_call_usage(&trace_path);
    assert_eq!(call_usage.len(), 1);
    let call_total = call_usage.iter().map(trace_usage_total).sum::<i64>();
    assert!(call_total > 0, "the deterministic LLM call must report usage");
    assert!(before_restart.usage.usage.total_tokens >= call_total);
    let persisted_usage = before_restart.usage.clone();
    let attachment_id = uploaded.attachment.id.clone();
    drop(state);
    drop(attachment_store);

    let resumed_attachment_store = Arc::new(lash::persistence::FileAttachmentStore::new(
        data_dir.join("attachments"),
    )) as Arc<dyn lash::persistence::AttachmentStore>;
    let resumed_provider = lash::testing::TestProvider::builder()
        .kind("workbench-attachment-usage-gate")
        .complete_error("restart verification must not call the provider")
        .build()
        .into_handle();
    let resumed_core = attachment_usage_gate_core(
        data_dir,
        resumed_factory,
        Arc::clone(&process_registry),
        Arc::clone(&resumed_attachment_store),
        resumed_provider,
        None,
    );
    let resumed_session_ids =
        WorkbenchSessionIds::persistent(session_id_path).expect("reopen gate session id");
    assert_eq!(resumed_session_ids.current(), session_id);
    let resumed_state = attachment_usage_gate_state(
        resumed_core,
        Arc::clone(&resumed_attachment_store),
        Arc::clone(&process_registry),
        resumed_session_ids,
    );
    assert_retrieved_attachment(&resumed_state, &attachment_id, &png_bytes).await;
    let Json(after_restart) =
        Box::pin(app_state(State(resumed_state), Query(SessionQuery::default())))
            .await
            .expect("read post-restart workbench state API");
    assert_eq!(after_restart.usage, persisted_usage);
    assert_usage_report_consistent(&after_restart.usage);

    println!(
        "workbench attachment/usage gate passed: session={session_id} attachment={attachment_id} total_tokens={}",
        after_restart.usage.usage.total_tokens
    );
}

fn attachment_usage_gate_core(
    data_dir: &std::path::Path,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    process_registry: Arc<dyn lash::process::ProcessRegistry>,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    provider: ProviderHandle,
    trace_sink: Option<Arc<dyn TraceSink>>,
) -> LashCore {
    let model = with_workbench_model_capability(
        lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None)
            .expect("gate model spec"),
    );
    let mut builder = explicit_durable_test_facets(data_dir)
        .provider(provider)
        .model(model)
        .store_factory(store_factory)
        .process_registry(process_registry)
        .attachment_store(attachment_store)
        .disable_queued_work_driver();
    if let Some(trace_sink) = trace_sink {
        builder = builder.trace_sink(trace_sink).trace_level(TraceLevel::Extended);
    }
    builder.build().expect("build attachment/usage gate core")
}

fn attachment_usage_gate_state(
    core: LashCore,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    process_registry: Arc<dyn lash::process::ProcessRegistry>,
    session_ids: WorkbenchSessionIds,
) -> AppState {
    let process_observer = core
        .processes()
        .observer()
        .expect("gate process observer configured");
    AppState {
        core,
        attachment_store,
        trigger_store: in_memory_trigger_store(),
        process_observer,
        process_work_driver: inert_process_work_driver(process_registry),
        session_ids,
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: None,
        })),
        web_configured: false,
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(16),
        queued_work_driver: inert_queued_work_driver(),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
    }
}

fn usage_gate_response() -> lash::provider::LlmResponse {
    let mut response = text_response(
        "<lashlang>\nfinish \"attachment accounted\"\n</lashlang>",
    );
    response.usage = lash::direct::LlmUsage {
        input_tokens: 21,
        output_tokens: 8,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 4,
    };
    response
}

async fn assert_retrieved_attachment(
    state: &AppState,
    attachment_id: &lash_core::AttachmentId,
    expected: &[u8],
) {
    let response = retrieve_attachment(
        AxumPath(attachment_id.to_string()),
        State(state.clone()),
    )
    .await
    .expect("retrieve attachment through workbench API handler");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
        Some("image/png")
    );
    let bytes = axum::body::to_bytes(response.into_body(), MAX_WORKBENCH_ATTACHMENT_BYTES)
        .await
        .expect("read retrieved attachment body");
    assert_eq!(bytes.as_ref(), expected);
}

fn assert_usage_report_consistent(report: &lash::usage::SessionUsageReport) {
    assert!(report.entry_count > 0);
    assert!(report.usage.total_tokens > 0);
    assert!(report.usage.usage.input_tokens > 0);
    assert!(report.usage.usage.output_tokens > 0);
    let rows_total = report
        .by_source_model
        .iter()
        .map(|row| row.usage.total_tokens)
        .sum::<i64>();
    assert_eq!(report.usage.total_tokens, rows_total);
    assert_eq!(report.entry_count, report.by_source_model.len());
    for row in &report.by_source_model {
        assert!(report.usage.total_tokens >= row.usage.total_tokens);
    }
}

fn completed_llm_call_usage(trace_path: &std::path::Path) -> Vec<lash::tracing::TraceTokenUsage> {
    std::fs::read_to_string(trace_path)
        .expect("read gate trace")
        .lines()
        .map(|line| serde_json::from_str::<TraceRecord>(line).expect("decode gate trace record"))
        .filter_map(|record| match record.event {
            TraceEvent::LlmCallCompleted { usage, .. } => usage,
            _ => None,
        })
        .collect()
}

fn trace_usage_total(usage: &lash::tracing::TraceTokenUsage) -> i64 {
    usage.input_tokens
        + usage.output_tokens
        + usage.cache_read_input_tokens
        + usage.cache_write_input_tokens
}
