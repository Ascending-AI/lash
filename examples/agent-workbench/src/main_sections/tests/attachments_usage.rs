use super::*;
use lash::TurnId;
use lash::rlm::RlmSendBuilderExt;

const ATTACHMENT_USAGE_GATE_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

/// L12: retained MCP binary bytes must be served as binary, beside PNG uploads.
#[test]
fn retained_mcp_binary_retrieves_exact_bytes_with_octet_stream() {
    run_async_test_on_stack_budget("workbench-mcp-binary-retrieval", || async {
        let double = crate::tests::test_double_backend(0).await;
        let backend = double.lash_backend();
        let store = backend.attachment_store();
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-mcp-binary-retrieval")
            .complete(|_| async { Ok(usage_gate_response()) })
            .build()
            .into_handle();
        let state = attachment_usage_gate_state(
            attachment_usage_gate_core(
                GateBackend {
                    backend: backend.clone(),
                },
                provider,
                None,
            ),
            Arc::clone(&store),
            backend.session_store_factory(),
            WorkbenchSessions::fresh(),
        );
        let attachment = store
            .put(
                crate::mcp_fixture::BADGE_BYTES.to_vec(),
                lash::attachments::AttachmentCreateMeta::new(
                    lash::attachments::MediaType::parse("application/octet-stream").unwrap(),
                    None,
                    Some("workspace-badge.bin".into()),
                ),
            )
            .await
            .expect("retain the MCP resource bytes");
        let response = retrieve_attachment(AxumPath(attachment.id.to_string()), State(state))
            .await
            .expect("retrieve the retained resource");
        assert_eq!(response.status(), StatusCode::OK);
        let media = response.headers()[header::CONTENT_TYPE].clone();
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let bytes = axum::body::to_bytes(response.into_body(), MAX_WORKBENCH_ATTACHMENT_BYTES)
            .await
            .expect("read retained binary bytes");
        assert_eq!(bytes.as_ref(), crate::mcp_fixture::BADGE_BYTES);
        assert_eq!(media, "application/octet-stream");
    });
}

#[test]
fn attachment_usage_gate() {
    run_async_test_on_stack_budget("workbench-attachment-usage-gate", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-attachment-usage-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create gate data dir");
        // The gate's stores are the SQLite store set on disk, whose
        // attachment store is durable; the engine is the Restate double's
        // over them. Two handles on that engine: the first core and the core
        // the resumed web process builds over the same stores.
        let stores: Arc<dyn lash::StoreSet> = Arc::new(
            lash::sqlite::SqliteStoreSet::open(data_dir.join("lash-sessions.db"))
                .await
                .expect("open the gate's SQLite store set"),
        );
        let double = lash_restate_test::backend_with(
            0,
            lash_restate_test::ServerConfig::default(),
            move |_| stores,
        )
        .await
        .expect("build the Restate double over the gate's store set");

        Box::pin(run_attachment_usage_gate(
            &data_dir,
            GateBackend {
                backend: double.lash_backend(),
            },
            GateBackend {
                backend: double.lash_backend(),
            },
        ))
        .await;
        std::fs::remove_dir_all(&data_dir).expect("remove gate data dir");
    });
}

/// One handle on the gate's backend: the backend a core and its RLM
/// factory's Lashlang artifacts run on.
struct GateBackend {
    backend: lash::Backend,
}

async fn run_attachment_usage_gate(
    data_dir: &std::path::Path,
    first: GateBackend,
    resumed: GateBackend,
) {
    let trace_path = data_dir.join("trace.jsonl");
    let session_id_path = data_dir.join("session-id");
    let sessions =
        WorkbenchSessions::persistent(session_id_path.clone()).expect("create gate session id");
    let session_id = sessions.current();
    let first_factory = first.backend.session_store_factory();
    let resumed_factory = resumed.backend.session_store_factory();
    let attachment_store = first.backend.attachment_store();
    assert_eq!(
        attachment_store.persistence(),
        lash::persistence::AttachmentStorePersistence::Durable
    );
    let normalized_media_type = lash::attachments::MediaType::parse(" IMAGE/PNG ")
        .expect("normalize host-supplied media type");
    assert!(normalized_media_type.is_image());
    assert_eq!(normalized_media_type.family(), "image");
    assert_eq!(normalized_media_type.as_str(), "image/png");
    assert!(lash::attachments::MediaType::parse("image//png").is_err());
    // A host route hands whatever the caller typed to `AttachmentId::parse`;
    // an id that is not a single namespace component is rejected there rather
    // than reaching the store.
    assert!(lash::attachments::AttachmentId::parse("../escape").is_err());
    let missing_id = lash::attachments::AttachmentId::parse("missing-workbench-attachment")
        .expect("valid attachment id");
    match attachment_store
        .get(
            &missing_id,
            lash::persistence::AttachmentReadPolicy::DEFAULT.max_blob_bytes,
        )
        .await
    {
        Err(lash::persistence::AttachmentStoreError::NotFound(id)) => {
            assert_eq!(id, missing_id);
        }
        other => panic!("missing attachment must return NotFound, got {other:?}"),
    }
    let provider_requests = Arc::new(Mutex::new(Vec::new()));
    let provider_requests_for_call = Arc::clone(&provider_requests);
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-attachment-usage-gate")
        .complete(move |request| {
            let provider_requests = Arc::clone(&provider_requests_for_call);
            async move {
                provider_requests.lock_recover().push(request);
                Ok(usage_gate_response())
            }
        })
        .build()
        .into_handle();
    let system_clock = Arc::new(lash::runtime::SystemClock);
    let core = attachment_usage_gate_core(
        first,
        provider,
        Some(Arc::new(JsonlTraceSink::new(trace_path.clone())) as Arc<dyn TraceSink>),
    );
    let state =
        attachment_usage_gate_state(core, Arc::clone(&attachment_store), first_factory, sessions);
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
    let uploaded_ref: &lash::attachments::AttachmentRef = &uploaded.attachment;
    assert_eq!(uploaded_ref.media_type().as_str(), "image/png");
    assert_eq!(uploaded.attachment.byte_len, png_bytes.len() as u64);
    assert_eq!(uploaded.attachment.label.as_deref(), Some("usage-gate.png"));
    match uploaded.attachment.type_metadata.as_ref() {
        Some(lash::attachments::AttachmentTypeMetadata::Image { width, height }) => {
            assert_eq!((*width, *height), (Some(1), Some(1)));
        }
        other => panic!("uploaded PNG must retain image dimensions, got {other:?}"),
    }
    let stored: lash::persistence::StoredAttachment = attachment_store
        .get(
            &uploaded.attachment.id,
            lash::persistence::AttachmentReadPolicy::DEFAULT.max_blob_bytes,
        )
        .await
        .expect("read uploaded bytes from workbench attachment store");
    assert_eq!(stored.bytes, png_bytes);
    let metadata = lash::attachments::AttachmentRef::new(
        uploaded.attachment.id.clone(),
        uploaded.attachment.media_type.clone(),
        uploaded.attachment.byte_len,
        uploaded.attachment.type_metadata.clone(),
        uploaded.attachment.label.clone(),
    );
    let metadata: lash::attachments::AttachmentRef = serde_json::from_value(
        serde_json::to_value(metadata).expect("serialize uploaded attachment metadata"),
    )
    .expect("deserialize uploaded attachment metadata");
    assert_eq!(metadata.id, uploaded.attachment.id);
    assert_eq!(metadata.media_type.as_str(), "image/png");
    assert_eq!(metadata.byte_len, png_bytes.len() as u64);
    assert_eq!(metadata.type_metadata, uploaded.attachment.type_metadata);
    assert_eq!(metadata.label.as_deref(), Some("usage-gate.png"));
    assert_eq!(metadata, uploaded.attachment);
    assert_eq!(
        uploaded.retrieve_url,
        format!("/api/attachments/{}", uploaded.attachment.id)
    );
    assert_retrieved_attachment(&state, &uploaded.attachment.id, &png_bytes).await;

    let runtime_window_start_ms = lash::runtime::ClockWallTime::timestamp_ms(system_clock.as_ref());
    let turn_id = TurnId::fixture(format!("attachment-usage-gate-{}", uuid::Uuid::new_v4()));
    let request = restate::UserTurnRequest {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        text: "Describe the attached PNG briefly.".to_string(),
        model: LlmProfileSelection {
            model: "test-model".to_string(),
            model_variant: None,
        },
        attachment_id: Some(uploaded.attachment.id.to_string()),
    };
    let input = restate::workbench_turn_input(&state, &request)
        .await
        .expect("build attachment turn input through workbench adapter");
    let session = crate::created_session(&state.core, session_id.clone())
        .await
        .open()
        .await
        .expect("open gate session");
    let output = session
        .send(input)
        .id(turn_id)
        .require_finish()
        .expect("require deterministic finish")
        .output()
        .await
        .expect("run deterministic attachment turn");
    assert_eq!(output.final_value(), Some(&json!("attachment accounted")));
    let runtime_window_end_ms = lash::runtime::ClockWallTime::timestamp_ms(system_clock.as_ref());
    let read_view = session.read_view();
    let graph = read_view.session_graph();
    let latest_node_ms = graph
        .leaf_node_id
        .as_deref()
        .and_then(|node_id| graph.find_node(node_id))
        .map(|node| node.timestamp.timestamp_millis() as u64)
        .expect("runtime-stamped graph node timestamp");
    assert!(
        (runtime_window_start_ms..=runtime_window_end_ms).contains(&latest_node_ms),
        "runtime timestamp {latest_node_ms} must come from the injected SystemClock window {runtime_window_start_ms}..={runtime_window_end_ms}"
    );
    session.close().await.expect("close gate session");

    {
        let requests = provider_requests.lock_recover();
        assert_eq!(requests.len(), 1, "gate must make exactly one LLM call");
        assert_eq!(requests[0].attachments().len(), 1);
        let source = &requests[0].attachments()[0];
        assert_eq!(
            source
                .media_type()
                .map(lash::attachments::MediaType::as_str),
            Some("image/png")
        );
        assert_eq!(
            requests[0].attachment_bytes(source),
            Some(png_bytes.as_slice())
        );
        assert_eq!(
            source.stored_ref().map(|reference| &reference.id),
            Some(&uploaded.attachment.id)
        );
    }

    let Json(before_restart) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("read pre-restart workbench state API");
    assert_snapshot_attachment(&before_restart, &uploaded.attachment.id);
    let call_usage = completed_llm_call_usage(&trace_path);
    assert_eq!(call_usage.len(), 1);
    let call_total = call_usage.iter().map(trace_usage_total).sum::<i64>();
    assert!(
        call_total > 0,
        "the deterministic LLM call must report usage"
    );
    let attachment_id = uploaded.attachment.id.clone();
    drop(state);
    drop(attachment_store);

    let resumed_attachment_store = resumed.backend.attachment_store();
    let resumed_provider = lash::testing::TestProvider::builder()
        .kind("workbench-attachment-usage-gate")
        .complete_error("restart verification must not call the provider")
        .build()
        .into_handle();
    let resumed_core = attachment_usage_gate_core(resumed, resumed_provider, None);
    let resumed_session_ids =
        WorkbenchSessions::persistent(session_id_path).expect("reopen gate session id");
    assert_eq!(resumed_session_ids.current(), session_id);
    let resumed_state = attachment_usage_gate_state(
        resumed_core,
        Arc::clone(&resumed_attachment_store),
        resumed_factory,
        resumed_session_ids,
    );
    assert_retrieved_attachment(&resumed_state, &attachment_id, &png_bytes).await;
    let Json(after_restart) = Box::pin(app_state(
        State(resumed_state),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("read post-restart workbench state API");
    assert_snapshot_attachment(&after_restart, &attachment_id);

    println!(
        "workbench attachment/usage gate passed: session={session_id} attachment={attachment_id} total_tokens={}",
        call_total
    );
}

fn assert_snapshot_attachment(
    snapshot: &StateReadSnapshot,
    expected_id: &lash::attachments::AttachmentId,
) {
    let attached_rows = snapshot
        .transcript
        .iter()
        .filter(|row| row.suppressed.is_none() && !row.content.attachments.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        attached_rows.len(),
        1,
        "the snapshot must carry exactly one attached row"
    );
    assert_eq!(&attached_rows[0].content.attachments[0].id, expected_id);
}

fn attachment_usage_gate_core(
    backend: GateBackend,
    provider: ProviderHandle,
    trace_sink: Option<Arc<dyn TraceSink>>,
) -> LashCore {
    let model = with_workbench_llm_profile_capability(
        lash::LlmProfileMetadata::builder("test-model")
            .context_window_tokens(4096)
            .build()
            .expect("gate model spec"),
    );
    let mut builder = explicit_durable_test_facets_on(backend.backend)
        .serve_workbench_llm_profile(provider, model);
    if let Some(trace_sink) = trace_sink {
        builder = builder
            .trace_sink(trace_sink)
            .trace_level(TraceLevel::Extended);
    }
    builder
        .build(crate::test_core_owner())
        .expect("build attachment/usage gate core")
}

fn attachment_usage_gate_state(
    core: LashCore,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    store_factory: Arc<dyn lash::persistence::DeploymentStore>,
    sessions: WorkbenchSessions,
) -> AppState {
    let process_observer = core
        .processes()
        .observer()
        .expect("gate process observer configured");
    AppState {
        session_defaults: crate::tests::test_session_defaults(),
        core,
        attachment_store,
        session_store_factory: store_factory,
        trigger_store: detached_trigger_store(),
        process_observer,
        // Process work is resolved through the core.
        sessions,
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_llm_profile: Arc::new(Mutex::new(LlmProfileSelection {
            model: "test-model".to_string(),
            model_variant: None,
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(16),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    }
}

fn usage_gate_response() -> lash::provider::LlmResponse {
    let mut response =
        text_response("<typescript>\nfinish(\"attachment accounted\");\n</typescript>");
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
    attachment_id: &lash::attachments::AttachmentId,
    expected: &[u8],
) {
    let response = retrieve_attachment(AxumPath(attachment_id.to_string()), State(state.clone()))
        .await
        .expect("retrieve attachment through workbench API handler");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("image/png")
    );
    assert_eq!(
        response
            .headers()
            .get("x-content-type-options")
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        response
            .headers()
            .get("x-lash-attachment-id")
            .and_then(|value| value.to_str().ok()),
        Some(attachment_id.to_string().as_str())
    );
    let bytes = axum::body::to_bytes(response.into_body(), MAX_WORKBENCH_ATTACHMENT_BYTES)
        .await
        .expect("read retrieved attachment body");
    assert_eq!(bytes.as_ref(), expected);
}

fn completed_llm_call_usage(trace_path: &std::path::Path) -> Vec<lash::tracing::TraceTokenUsage> {
    lash::tracing::parse_jsonl_records::<TraceRecord>(
        &std::fs::read_to_string(trace_path).expect("read gate trace"),
    )
    .expect("decode gate trace records")
    .into_iter()
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
