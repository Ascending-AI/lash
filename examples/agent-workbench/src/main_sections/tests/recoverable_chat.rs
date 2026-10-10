use super::*;
use lash::SessionId;
use lash::TurnId;
use lash::rlm::RlmSendBuilderExt;

#[test]
fn attachment_urls_percent_encode_the_id_path_segment() {
    let attachment = ChatAttachment::from_id("sha256:folder/image 1.png");
    assert_eq!(
        attachment.retrieve_url,
        "/api/attachments/sha256%3Afolder%2Fimage%201%2Epng"
    );
}

#[test]
fn session_event_registry_isolates_channels_and_recreates_after_removal() {
    let registry = SessionEventRegistry::new(4);
    let mut session_a = registry.subscribe(&SessionId::from("session-a"));
    let mut session_b = registry.subscribe(&SessionId::from("session-b"));

    registry.publish(
        &SessionId::from("session-a"),
        StreamItem::Done {
            turn_id: None,
            outcome: TurnDoneOutcome::Completed,
        },
    );
    assert!(matches!(
        session_a.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done { .. },
            ..
        })
    ));
    assert!(matches!(
        session_b.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    registry.remove(&SessionId::from("session-a"));
    assert!(!registry.contains(&SessionId::from("session-a")));
    let mut replacement_a = registry.subscribe(&SessionId::from("session-a"));
    registry.publish(
        &SessionId::from("session-a"),
        StreamItem::Done {
            turn_id: None,
            outcome: TurnDoneOutcome::Completed,
        },
    );
    assert!(matches!(
        replacement_a.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done { .. },
            ..
        })
    ));
    assert!(matches!(
        session_a.try_recv(),
        Err(broadcast::error::TryRecvError::Closed)
    ));
}

#[test]
fn settled_product_reconciliation_keeps_the_cursor_monotonic() {
    let registry = SessionEventRegistry::new(4);
    let session_id = SessionId::from("reconciled-session");
    let committed_id = format!("fixture-user:{}", TurnId::from("reconciled-turn"));
    registry.publish_identified(
        &session_id,
        "provisional-message",
        StreamItem::Message {
            message: ChatMessage {
                id: committed_id.to_string(),
                role: "user".to_string(),
                text: "settled prompt".to_string(),
                at: String::new(),
                attachments: Vec::new(),
                provenance: Some(ChatMessageProvenance::TurnInput {
                    turn_id: TurnId::from("reconciled-turn"),
                }),
                client_nonce: None,
            },
        },
    );
    let expected_record = lash::LlmCallRecord {
        call_id: lash::LlmCallId("call-1".to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![
            lash::AttemptRecord {
                ordinal: 1,
                outcome: lash::provider::AttemptOutcome::Failed,
                protocol_position: lash::provider::ProtocolPosition::NoResponse,
                retry_budget_consumed: true,
                retry_decision: Some(lash::provider::RetryDecision::Scheduled {
                    delay: Duration::from_millis(1),
                    wait: lash::provider::RetryWait::Backoff,
                    class: lash::provider::RetryClass::NoResponse,
                }),
                error: Some(lash::provider::NormalizedError {
                    class: lash::provider::ProviderFailureKind::Transport,
                    code: Some(lash::provider::FailureCode::provider("connection_reset")),
                    http_status: Some(503),
                    provider_request_id: Some("request-1".to_string()),
                    retry_after: Some(Duration::from_millis(25)),
                }),
                evidence: Some(lash::provider::ExecutionEvidence {
                    collection_interruption: Some(
                        lash::provider::ExecutionEvidenceCollectionInterruption::ProtocolAbort,
                    ),
                    ..Default::default()
                }),
                generation_disposition: Some(lash::direct::GenerationReceipt {
                    output_token_cap: lash::direct::GenerationOptionOutcome::ClampedToCapacity,
                    temperature: lash::direct::GenerationOptionOutcome::Applied,
                    seed: lash::direct::GenerationOptionOutcome::NotRequested,
                    stop_sequences: lash::direct::GenerationOptionOutcome::SuppressedProtocolOwned,
                    cache: lash::direct::GenerationOptionOutcome::OmittedUnsupported,
                    ..Default::default()
                }),
                usage: Some(lash::usage::LlmUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 2,
                    reasoning_output_tokens: 5,
                }),
            },
            lash::AttemptRecord {
                ordinal: 2,
                outcome: lash::provider::AttemptOutcome::Completed,
                protocol_position: lash::provider::ProtocolPosition::TerminalObserved,
                retry_budget_consumed: true,
                retry_decision: None,
                error: None,
                evidence: None,
                generation_disposition: None,
                usage: None,
            },
        ],
    };
    registry.publish_identified(
        &session_id,
        "model-call",
        StreamItem::ModelCallRecorded {
            record: expected_record.clone(),
        },
    );
    registry.publish_identified(
        &session_id,
        "turn-done",
        StreamItem::Done {
            turn_id: Some(TurnId::from("reconciled-turn")),
            outcome: TurnDoneOutcome::Completed,
        },
    );

    registry.reconcile_settled(
        &session_id,
        &BTreeSet::new(),
        &BTreeSet::from([TurnId::from("reconciled-turn")]),
        &BTreeSet::new(),
    );
    let reconciled = registry.snapshot(&session_id);
    assert_eq!(reconciled.cursor, 3);
    assert_eq!(reconciled.events.len(), 2);
    assert!(matches!(
        &reconciled.events[0].item,
        StreamItem::Message { message }
            if message.id == format!("fixture-user:{}", TurnId::from("reconciled-turn"))
    ));
    let StreamItem::ModelCallRecorded { record } = &reconciled.events[1].item else {
        panic!("reconciliation must retain the model-call record");
    };
    assert_eq!(record, &expected_record);
    assert!(
        !registry.publish_identified(
            &session_id,
            "turn-done",
            StreamItem::Done {
                turn_id: Some(TurnId::from("reconciled-turn")),
                outcome: TurnDoneOutcome::Completed,
            },
        ),
        "compaction must retain event identity for idempotent workflow replay"
    );
    assert_eq!(registry.snapshot(&session_id).cursor, 3);

    registry.publish_identified(
        &session_id,
        "host-only-event",
        StreamItem::Message {
            message: ChatMessage {
                id: "host-only".to_string(),
                role: "event".to_string(),
                text: "host event".to_string(),
                at: String::new(),
                attachments: Vec::new(),
                provenance: None,
                client_nonce: None,
            },
        },
    );
    assert_eq!(
        registry.snapshot(&session_id).events[2].sequence,
        4,
        "compaction must not reuse a cursor already observed by a client"
    );
}

pub(crate) fn assert_deleted_session_conflict(error: &AppError, session_id: &SessionId) {
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(error.message, deleted_session_message(session_id));
    assert_eq!(error.verdict, crate::AppErrorVerdict::Terminal);
}

fn session_query(session_id: &SessionId) -> Query<SessionQuery> {
    Query(SessionQuery {
        session_id: Some(session_id.clone()),
    })
}

/// The chat route admits the session before it reads the named attachment
/// or submits anything: a retired id is the typed conflict, not the
/// malformed attachment id's bad request, and nothing reaches the page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_session_admission_precedes_attachment_reads_and_submission() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    tombstone_session(state, &session_id).await;

    let error = send_turn(
        State(state.clone()),
        session_query(&session_id),
        Json(TurnRequest {
            attachment: Some(lash::attachments::AttachmentRef::new(
                lash::attachments::AttachmentId::parse(
                    "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                )
                .expect("digest"),
                lash::attachments::MediaType::parse("image/png").expect("mime"),
                3,
                None,
                None,
            )),
            ..turn_request("must not be accepted")
        }),
    )
    .await
    .expect_err("retired session turn must be refused");

    assert_deleted_session_conflict(&error, &session_id);
    assert!(state.messages_snapshot().is_empty());
    assert!(product_rows(state, &session_id, "user").is_empty());
    assert!(state.active_turns.for_session(&session_id).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_session_http_refusals_record_structured_admission_evidence() {
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    tombstone_session(state, &session_id).await;

    let state_error = read_state(state, Some(&session_id))
        .await
        .expect_err("retired session state read must be refused");
    assert_deleted_session_conflict(&state_error, &session_id);

    let observation_error = session_observations_with_shutdown(
        State(state.clone()),
        Query(EventsQuery {
            cursor: None,
            session_id: Some(session_id.clone()),
        }),
        None,
    )
    .await
    .expect_err("retired session observation must be refused");
    assert_deleted_session_conflict(&observation_error, &session_id);

    let turn_error = send_text(state, Some(&session_id), "must not be accepted")
        .await
        .expect_err("retired session turn must be refused");
    assert_deleted_session_conflict(&turn_error, &session_id);

    let input_error = enqueue_turn_input(
        State(state.clone()),
        session_query(&session_id),
        Json(TurnInputRequest {
            text: "must not be queued".to_string(),
            ingress: TurnInputIngressRequest::NextTurn,
        }),
    )
    .await
    .expect_err("retired session turn input must be refused");
    assert_deleted_session_conflict(&input_error, &session_id);

    assert!(
        trace.custom("api.turn.request").is_empty(),
        "a refused turn must not be traced as accepted"
    );
    let surfaces = trace
        .custom("session.admission_refused")
        .into_iter()
        .map(|(_, payload)| {
            assert_eq!(
                payload.pointer("/session_id").and_then(Value::as_str),
                Some(session_id.as_str())
            );
            assert_eq!(
                payload
                    .pointer("/consulted_state/kind")
                    .and_then(Value::as_str),
                Some("session_store_tombstone")
            );
            assert_eq!(
                payload
                    .pointer("/consulted_state/freshness")
                    .and_then(Value::as_str),
                Some("admission_read")
            );
            assert_eq!(
                payload
                    .pointer("/tombstone_outcome")
                    .and_then(Value::as_str),
                Some("retired")
            );
            assert_eq!(
                payload.pointer("/outcome").and_then(Value::as_str),
                Some("refused")
            );
            payload
                .pointer("/surface")
                .and_then(Value::as_str)
                .expect("refusal surface")
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        surfaces,
        [
            "api.observations",
            "api.state",
            "api.turn",
            "api.turn.input"
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
}

/// However a turn's execution ended, settling it against a tombstoned
/// session ends in the session's typed terminal conflict and releases the
/// turn's claim, so its follower never retries a refused turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_terminalize_branch_makes_runtime_shaped_session_deletion_terminal() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    tombstone_session(state, &session_id).await;

    type TerminalizeResult = Result<Result<(), AppError>, Box<dyn std::any::Any + Send>>;
    let cases: Vec<(&str, TerminalizeResult)> = vec![
        ("successful_turn", Ok(Ok(()))),
        (
            "failed_turn",
            Ok(Err(AppError::internal("original turn failure"))),
        ),
        ("panicked_turn", Err(Box::new("original turn panic"))),
    ];
    for (case, result) in cases {
        let turn_id = TurnId::fixture(format!("{case}-turn"));
        state.track_turn(&session_id, &turn_id);
        let error = crate::turns::terminalize_turn_execution(
            state,
            &session_id,
            &turn_id,
            "test.turn.failed",
            result,
        )
        .await
        .expect_err("settlement against a retired session must fail");
        assert_deleted_session_conflict(&error, &session_id);
        assert!(
            state.active_turns.for_session(&session_id).is_none(),
            "{case} must release the refused turn's claim"
        );
    }
}

// One thread: the route's forwarder cannot run between the publications
// below, so its capacity-one receiver always lags. On two workers it could
// take each event as it was published and never lag (FIG-5605).
#[tokio::test]
async fn product_event_route_lag_emits_durable_ordered_resync() {
    let workbench = Workbench::builder(silent_provider())
        .event_tx(SessionEventRegistry::new(1))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let response = session_events_with_shutdown(
        State(state.clone()),
        Query(ProductEventsQuery {
            session_id: None,
            cursor: Some(0),
        }),
        None,
    )
    .await
    .expect("open production product-event route");
    let mut body = response.into_body().into_data_stream();

    for sequence in 1..=3 {
        state.event_tx.publish_identified(
            &session_id,
            format!("event-{sequence}"),
            StreamItem::Message {
                message: ChatMessage {
                    id: format!("message-{sequence}"),
                    role: "event".to_string(),
                    text: format!("event {sequence}"),
                    at: String::new(),
                    attachments: Vec::new(),
                    provenance: None,
                    client_nonce: None,
                },
            },
        );
    }

    let snapshot = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let bytes = body
                .next()
                .await
                .expect("product route remains open")
                .expect("product route bytes");
            let item: Value = serde_json::from_slice(&bytes).expect("product stream item");
            if item.get("type").and_then(Value::as_str) == Some("resync") {
                break serde_json::from_value::<ProductEventSnapshot>(
                    item.get("snapshot").cloned().expect("resync snapshot"),
                )
                .expect("decode resync snapshot");
            }
        }
    })
    .await
    .expect("lagged route never emitted a resync");

    assert_eq!(snapshot.cursor, 3);
    assert_eq!(
        snapshot
            .events
            .iter()
            .map(|event| (event.sequence, event.event_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "event-1"), (2, "event-2"), (3, "event-3")]
    );
}

/// An attachment store whose blobs vanish after the first read: the send's
/// admission finds the attachment, then the turn input built for the send
/// cannot, so the send is refused after its optimistic row was published.
/// Every message the session's durable history holds, across frames, as
/// `(role, text)`.
async fn durable_history_rows(
    session: &lash::LashSession,
) -> Vec<(lash::messages::MessageRole, String)> {
    let page = session
        .durable()
        .history(
            lash::persistence::HistoryAnchor::Head,
            lash::persistence::HistoryBudget {
                max_nodes: std::num::NonZeroU32::new(1_000).expect("non-zero"),
                max_bytes: std::num::NonZeroU64::new(64 * 1024 * 1024).expect("non-zero"),
            },
        )
        .await
        .expect("read the session's history");
    assert_eq!(
        page.stop,
        lash::persistence::HistoryStop::Root,
        "one page holds this session's history"
    );
    page.nodes
        .into_iter()
        .rev()
        .filter_map(|node| match node.record.payload {
            lash::persistence::SessionNodePayload::Event {
                event: lash::persistence::SessionHistoryRecord::Conversation(record),
            } => Some((
                record.role,
                record
                    .parts
                    .iter()
                    .map(|part| part.content())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            _ => None,
        })
        .collect()
}

/// A `continue_as` turn moves the session into a follow frame: the page shows
/// every row the session committed across both frames (ADR 0129: the
/// transcript walks retained ancestry across frame boundaries, and each
/// settled turn keeps its one committed reply) followed by the follow frame's
/// task and answer, the durable history keeps the old frame, and a restarted
/// workbench rebuilds the same projection without a model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_as_keeps_every_frames_rows_and_survives_reload() {
    let data_dir = tempfile::tempdir().expect("continue_as projection tempdir");
    let product_events_path = data_dir.path().join("product-events.json");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete(move |_| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                Ok(match call {
                    0 => text_response(&finish_cell("old frame answer")),
                    1 => text_response(
                        "<typescript>\nawait control.continue_as({ task: \"finish in the follow frame\", seed: { boundary_marker: \"protocol-only-seed\" } });\n</typescript>",
                    ),
                    2 => text_response(&finish_cell("follow frame answer")),
                    other => panic!("unexpected continue_as provider call {other}"),
                })
            }
        })
        .build()
        .into_handle();
    let workbench = Workbench::builder(provider)
        .event_tx(
            SessionEventRegistry::persistent(product_events_path.clone(), 16)
                .expect("open persistent product event registry"),
        )
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let first_prompt = "first submitted row";
    let switch_prompt = "switch frames now";
    run_turn(state, first_prompt).await;
    run_turn(state, switch_prompt).await;

    // The switch commits with its follow-on mailed to the session as the
    // next run (ADR 0101 §3); the session's engine starts it on its own, and
    // the page shows its rows once it settles.
    let expected_rows = [
        ("user", first_prompt),
        ("assistant", "old frame answer"),
        ("user", switch_prompt),
        ("user", "finish in the follow frame"),
        ("assistant", "follow frame answer"),
    ]
    .map(|(role, text)| (role.to_string(), text.to_string()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let boundary = loop {
        let boundary = read_state(state, None)
            .await
            .expect("project continue_as boundary state");
        if state_rows(&boundary) == expected_rows || tokio::time::Instant::now() >= deadline {
            break boundary;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(state_rows(&boundary), expected_rows);

    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open the switched session");
    let durable_rows = durable_history_rows(&session).await;
    drop(session);
    assert!(
        durable_rows.iter().any(|(role, text)| {
            *role == lash::messages::MessageRole::User && text == first_prompt
        }),
        "the durable graph must retain the pre-switch user input: {durable_rows:?}"
    );
    assert!(
        durable_rows.iter().any(|(role, text)| {
            *role == lash::messages::MessageRole::Assistant && text.contains("old frame answer")
        }),
        "the old frame's answer must remain in the durable graph: {durable_rows:?}"
    );
    let boundary_transcript = serde_json::to_value(&boundary.transcript).expect("canonical rows");

    let stores = Arc::clone(&workbench.stores);
    workbench.shutdown().await;
    let reloaded = Workbench::builder(silent_provider())
        .stores(stores)
        .event_tx(
            SessionEventRegistry::persistent(product_events_path, 16)
                .expect("reload persistent product event registry"),
        )
        .build()
        .await;
    let rebuilt = read_state(&reloaded.state, Some(&session_id))
        .await
        .expect("rebuild continue_as projection after reload");
    assert_eq!(
        serde_json::to_value(&rebuilt.transcript).expect("reload rows"),
        boundary_transcript,
        "reload must reproduce the same session-scoped projection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_attachment_ref_is_exposed_in_the_workbench_snapshot() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let attachment = lash::attachments::AttachmentRef {
        id: lash::attachments::AttachmentId::parse(
            "ebfd7953df27a2b8c3752686e96c066aedbeb45a67827f0265ecfd3d8629f565",
        )
        .expect("valid attachment id"),
        media_type: lash::attachments::MediaType::parse("image/png")
            .expect("valid test media type"),
        byte_len: 68,
        type_metadata: Some(lash::attachments::AttachmentTypeMetadata::image(
            Some(1),
            Some(1),
        )),
        label: Some("committed.png".to_string()),
    };
    let mut message =
        lash::plugins::PluginMessage::text(lash::messages::MessageRole::User, "see image")
            .with_id("committed-attachment-message");
    message.parts.push(lash::messages::Part::attachment_part(
        String::new(),
        String::new(),
        Some(lash::messages::PartAttachment {
            reference: attachment.clone(),
        }),
    ));
    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open committed attachment session");
    session
        .admin()
        .state()
        .append_messages(
            vec![message],
            "host:recoverable_chat:append_messages:755".to_string(),
        )
        .await
        .expect("append committed attachment message")
        .settle_with(
            &session.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .expect("fixture mutation settled");
    drop(session);

    let snapshot = read_state(state, None)
        .await
        .expect("read committed attachment snapshot");
    let wire = serde_json::to_value(snapshot).expect("serialize workbench snapshot");
    let committed_user = wire["transcript"]
        .as_array()
        .expect("the snapshot carries the committed transcript")
        .iter()
        .find(|row| row["kind"] == "user" && row["suppressed"].is_null())
        .expect("the committed user row is visible");
    assert_eq!(
        committed_user["content"]["attachments"][0]["id"],
        json!(attachment.id),
        "the committed row must expose the stored attachment reference"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_turn_state_projection_stays_readable_and_settles_to_durable_truth() {
    let (provider_entered_tx, mut provider_entered_rx) = mpsc::unbounded_channel();
    let provider_release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete({
            let provider_release = Arc::clone(&provider_release);
            move |_| {
                let provider_entered_tx = provider_entered_tx.clone();
                let provider_release = Arc::clone(&provider_release);
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let _ = provider_entered_tx.send(call);
                    if call == 0 {
                        provider_release.notified().await;
                    }
                    Ok(match call {
                        0 => {
                            let mut response = text_response(
                                "<typescript>\nconsole.log(\"durable execution disclosure\");\n</typescript>",
                            );
                            response.parts.insert(
                                0,
                                lash::direct::LlmOutputPart::Reasoning {
                                    text: "durable reasoning disclosure".to_string(),
                                    replay: None,
                                },
                            );
                            response
                        }
                        1 => text_response(&finish_cell("settled answer")),
                        other => panic!("unexpected provider call {other}"),
                    })
                }
            }
        })
        .build()
        .into_handle();
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let turn_text = "exercise the user-facing send path";

    let accepted = send_text(state, None, turn_text)
        .await
        .expect("send turn through the production handler");
    let turn_id = started_turn_id(&accepted);

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), provider_entered_rx.recv())
            .await
            .expect("the first provider call starts"),
        Some(0),
        "the first provider call must be blocked before the mid-turn read"
    );
    let running = read_state(state, None)
        .await
        .expect("/api/state must remain readable while the turn runs");
    assert_eq!(running.active_turns.len(), 1);
    let original_input_id = running
        .product_events
        .events
        .iter()
        .find_map(|event| match &event.item {
            StreamItem::Message { message }
                if matches!(
                    &message.provenance,
                    Some(ChatMessageProvenance::TurnInput { turn_id: owner }) if *owner == turn_id
                ) =>
            {
                Some(message.id.clone())
            }
            _ => None,
        })
        .expect("UI input exists before the commit");
    let runtime_store: Arc<dyn lash::persistence::RuntimeStore> =
        state.session_store_factory.clone();
    let in_flight_store = lash::persistence::SessionStore::new(runtime_store, session_id.clone())
        .expect("valid session id");
    let in_flight = lash::persistence::load_session_window_state(
        &in_flight_store,
        lash::persistence::WindowSelector::Current,
    )
    .await
    .expect("read the admitted in-flight durable state")
    .map(|loaded| loaded.state);
    assert!(
        in_flight.as_ref().is_none_or(|state| {
            state.read_view().messages().iter().all(|message| {
                !matches!(
                    message.origin.as_ref(),
                    Some(lash::messages::MessageOrigin::TurnInput {
                        turn_id: committed_turn_id,
                        ..
                    }) if *committed_turn_id == turn_id
                )
            })
        }),
        "the initial turn input is not committed while the first provider call is in flight"
    );

    provider_release.notify_one();
    wait_for_turn_released(state, &session_id, &turn_id).await;
    assert_eq!(
        provider_entered_rx.recv().await,
        Some(1),
        "the turn must execute the terminal provider iteration"
    );

    let settled = read_state(state, None)
        .await
        .expect("materialize settled state");
    assert_eq!(
        state_rows(&settled),
        vec![
            ("user".to_owned(), turn_text.to_owned()),
            ("assistant".to_owned(), "settled answer".to_owned())
        ],
        "the browser transcript projection must contain the committed message set once"
    );
    assert!(
        settled.transcript.iter().any(|row| row
            .content
            .reasoning
            .iter()
            .any(|text| text == "durable reasoning disclosure")),
        "settled state must reconstruct reasoning disclosure"
    );
    assert!(
        settled.transcript.iter().any(|row| row
            .content
            .code
            .as_ref()
            .is_some_and(|code| code.contains("durable execution disclosure"))
            && row
                .content
                .output
                .as_ref()
                .is_some_and(|output| output.contains("durable execution disclosure"))),
        "settled state must reconstruct code execution and output"
    );
    assert_eq!(
        settled
            .product_events
            .events
            .iter()
            .filter_map(|event| match &event.item {
                StreamItem::Message { message } => Some((
                    message.id.clone(),
                    message.role.clone(),
                    message.text.clone(),
                )),
                StreamItem::TurnInput { .. }
                | StreamItem::ModelCallRecorded { .. }
                | StreamItem::Done { .. } => None,
            })
            .collect::<Vec<_>>(),
        vec![(original_input_id, "user".to_string(), turn_text.to_string())],
        "settlement must retain only the session-scoped UI-owned user row"
    );
    assert!(
        settled.product_events.events.iter().all(|event| !matches!(
            &event.item,
            StreamItem::Done {
                turn_id: Some(done_turn_id),
                ..
            } if *done_turn_id == turn_id
        )),
        "settled Done rows must leave the product-event lane"
    );
}

/// A cancel aimed at a turn that already completed reaches no live turn and
/// adds no terminal event: each completed turn keeps exactly its one `Done`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workbench_settled_turn_cancels_preserve_execution_done() {
    let workbench = Workbench::replying(
        "<typescript>\nawait control.finish(\"canonical answer\");\n</typescript>",
    )
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let session = state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open cancel identity session");
    let driver = state.core.turn_work_driver();

    let done_ids = || {
        state
            .event_tx
            .snapshot(&session_id)
            .events
            .into_iter()
            .filter_map(|event| {
                matches!(event.item, StreamItem::Done { .. }).then_some(event.event_id)
            })
            .collect::<BTreeSet<_>>()
    };
    for turn_id in ["settled-turn-a", "settled-turn-b"] {
        session
            .send(lash::TurnInput::text(format!("complete {turn_id}")))
            .id(lash::TurnId::parse(turn_id).expect("nonblank host identity"))
            .require_finish()
            .expect("require finish")
            .output()
            .await
            .expect("complete turn before stale cancel");
        state.publish_turn_done(&session_id, &TurnId::from(turn_id));
        state.track_turn(&session_id, &TurnId::from(turn_id));
        let before = done_ids();
        let receipts = state
            .cancel_turns_for_session_with_driver(
                &session_id,
                &driver,
                WorkbenchTurnCancelMode::Abort,
                Duration::from_secs(5),
            )
            .await
            .expect("cancel settled turn");
        assert!(
            matches!(
                receipts.as_slice(),
                [TurnCancelReceipt::UnknownOrRevoked { address }]
                    if address.turn_id.as_str() == turn_id
            ),
            "a settled turn's cancel must reach no live turn: {receipts:?}"
        );
        // The session's run follower may retire a settled turn's `Done`
        // meanwhile; what the cancel must never do is add one.
        let added = done_ids().difference(&before).cloned().collect::<Vec<_>>();
        assert!(
            added.is_empty(),
            "stale cancels must not add terminal events to execution evidence: {added:?}"
        );
    }
}
