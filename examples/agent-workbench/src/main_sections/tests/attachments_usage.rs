use super::*;

const ATTACHMENT_USAGE_GATE_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

/// L12: retained MCP binary bytes must be served as binary, beside PNG uploads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_mcp_binary_retrieves_exact_bytes_with_octet_stream() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let attachment = state
        .attachment_store
        .put(
            crate::mcp_fixture::BADGE_BYTES.to_vec(),
            lash::attachments::AttachmentCreateMeta::new(
                lash::attachments::MediaType::parse("application/octet-stream")
                    .expect("a binary media type"),
                None,
                Some("workspace-badge.bin".into()),
            ),
        )
        .await
        .expect("retain the MCP resource bytes");
    let response = retrieve_attachment(AxumPath(attachment.id.to_string()), State(state.clone()))
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
}

/// An uploaded PNG is stored durably with its metadata, served back exactly,
/// reaches the model as the turn's one attachment, is accounted with the
/// call's usage, stays on the committed row, and survives a restart of the
/// web process over the same on-disk stores.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_usage_gate() {
    let data_dir = tempfile::tempdir().expect("create gate data dir");
    let stores: Arc<dyn lash::StoreSet> = Arc::new(
        lash::sqlite::SqliteStoreSet::open(data_dir.path().join("lash-sessions.db"))
            .await
            .expect("open the gate's SQLite store set"),
    );
    let session_id_path = data_dir.path().join("session-id");
    let sessions =
        WorkbenchSessions::persistent(session_id_path.clone()).expect("create gate session id");
    let session_id = sessions.current();
    let attachment_store = stores.attachment_store();
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
    let missing_id = lash::attachments::AttachmentId::parse(
        "ee2862941553e4d152e9c547c018e45952a7029424159d05ca82a39ab689b7f2",
    )
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
    // The model send carries the upload ref after the host fills its slots.
    let lowered_attachments = Arc::new(Mutex::new(Vec::new()));
    let provider_requests = Arc::new(Mutex::new(Vec::new()));
    // The workbench catalogue names its serving provider exactly.
    let provider = lash::testing::TestProvider::builder()
        .kind("openai-compatible")
        .send({
            let lowered_attachments = Arc::clone(&lowered_attachments);
            let provider_requests = Arc::clone(&provider_requests);
            move |request, _wire| {
                lowered_attachments
                    .lock_recover()
                    .push(request.attachments().cloned().collect::<Vec<_>>());
                provider_requests.lock_recover().push(request);
                async { Ok(usage_gate_response()) }
            }
        })
        .build()
        .into_handle();
    let trace = Arc::new(RecordingTrace::default());
    let first = Workbench::builder(provider)
        .stores(Arc::clone(&stores))
        .sessions(sessions)
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &first.state;
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
    assert_eq!(uploaded.attachment.media_type().as_str(), "image/png");
    assert_eq!(uploaded.attachment.byte_len, png_bytes.len() as u64);
    assert_eq!(uploaded.attachment.label.as_deref(), Some("usage-gate.png"));
    match uploaded.attachment.type_metadata.as_ref() {
        Some(lash::attachments::AttachmentTypeMetadata::Image { width, height }) => {
            assert_eq!((*width, *height), (Some(1), Some(1)));
        }
        other => panic!("uploaded PNG must retain image dimensions, got {other:?}"),
    }
    let stored = attachment_store
        .get(
            &uploaded.attachment.id,
            lash::persistence::AttachmentReadPolicy::DEFAULT.max_blob_bytes,
        )
        .await
        .expect("read uploaded bytes from workbench attachment store");
    assert_eq!(stored.bytes, png_bytes);
    let metadata: lash::attachments::AttachmentRef = serde_json::from_value(
        serde_json::to_value(lash::attachments::AttachmentRef::new(
            uploaded.attachment.id.clone(),
            uploaded.attachment.media_type.clone(),
            uploaded.attachment.byte_len,
            uploaded.attachment.type_metadata.clone(),
            uploaded.attachment.label.clone(),
        ))
        .expect("serialize uploaded attachment metadata"),
    )
    .expect("deserialize uploaded attachment metadata");
    assert_eq!(metadata, uploaded.attachment);
    assert_eq!(
        uploaded.retrieve_url,
        format!("/api/attachments/{}", uploaded.attachment.id)
    );
    assert_retrieved_attachment(state, &uploaded.attachment.id, &png_bytes).await;

    let runtime_window_start_ms = chrono::Utc::now().timestamp_millis() as u64;
    let accepted = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            attachment: Some(uploaded.attachment.clone()),
            ..turn_request("Describe the attached PNG briefly.")
        }),
    )
    .await
    .expect("send the attachment turn through the chat route")
    .0;
    let turn_id = started_turn_id(&accepted);
    wait_for_turn_released(state, &session_id, &turn_id, Duration::from_secs(30)).await;
    let runtime_window_end_ms = chrono::Utc::now().timestamp_millis() as u64;
    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open gate session");
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
        "runtime timestamp {latest_node_ms} must come from the system clock window {runtime_window_start_ms}..={runtime_window_end_ms}"
    );
    drop(session);

    assert_eq!(
        provider_requests.lock_recover().len(),
        1,
        "gate must make exactly one LLM call"
    );
    assert_eq!(
        *lowered_attachments.lock_recover(),
        vec![vec![uploaded.attachment.clone()]],
        "the one call carries the uploaded PNG's ref"
    );

    let before_restart = read_state(state, None)
        .await
        .expect("read pre-restart workbench state API");
    assert_snapshot_attachment(&before_restart, &uploaded.attachment.id);
    let call_usage = trace
        .records()
        .into_iter()
        .filter_map(|record| match record.event {
            TraceEvent::LlmCallCompleted { usage, .. } => usage,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(call_usage.len(), 1);
    let call_total = call_usage.iter().map(trace_usage_total).sum::<i64>();
    assert!(
        call_total > 0,
        "the deterministic LLM call must report usage"
    );
    let attachment_id = uploaded.attachment.id.clone();

    tokio::time::timeout(Duration::from_secs(30), state.core.drain())
        .await
        .expect("the first web process drains")
        .expect("the first web process releases its sessions");
    let resumed_sessions =
        WorkbenchSessions::persistent(session_id_path).expect("reopen gate session id");
    assert_eq!(resumed_sessions.current(), session_id);
    let resumed = Workbench::builder(silent_provider())
        .stores(stores)
        .sessions(resumed_sessions)
        .build()
        .await;
    assert_retrieved_attachment(&resumed.state, &attachment_id, &png_bytes).await;
    let after_restart = read_state(&resumed.state, None)
        .await
        .expect("read post-restart workbench state API");
    assert_snapshot_attachment(&after_restart, &attachment_id);
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

fn trace_usage_total(usage: &lash::tracing::TraceTokenUsage) -> i64 {
    usage.input_tokens
        + usage.output_tokens
        + usage.cache_read_input_tokens
        + usage.cache_write_input_tokens
}
