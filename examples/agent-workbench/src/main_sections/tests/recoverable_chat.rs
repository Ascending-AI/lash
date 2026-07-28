async fn recoverable_chat_test_state(
    data_dir: &std::path::Path,
    channel_capacity: usize,
) -> AppState {
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-workbench-test")
        .complete(|_| async {
            Ok(text_response(
                "<lashlang>\nfinish \"canonical answer\"\n</lashlang>",
            ))
        })
        .build()
        .into_handle();
    recoverable_chat_test_state_with_provider(data_dir, channel_capacity, provider).await
}

async fn recoverable_chat_test_state_with_provider(
    data_dir: &std::path::Path,
    channel_capacity: usize,
    provider: ProviderHandle,
) -> AppState {
    let process_registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &data_dir.join("processes.db"),
            data_dir.join("lash-sessions"),
        )
        .await
        .expect("open process registry"),
    ) as Arc<dyn lash::process::ProcessRegistry>;
    let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
    );
    let model = with_workbench_model_capability(
        lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None)
            .expect("model spec"),
    );
    let core = explicit_durable_test_facets(data_dir)
        .provider(provider)
        .model(model)
        .store_factory(store_factory)
        .process_registry(Arc::clone(&process_registry))
        .build()
        .expect("build test core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    AppState {
        core,
        attachment_store: test_attachment_store(),
        trigger_store: in_memory_trigger_store(),
        process_observer,
        process_work_driver: inert_process_work_driver(process_registry),
        session_ids: WorkbenchSessionIds::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        web_configured: false,
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(channel_capacity),
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

#[test]
fn workbench_browser_recovery_projection_preserves_rows_and_scopes_session_cursors() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/browser_projection.mjs");
    let output = std::process::Command::new("node")
        .arg("--test")
        .arg(&script)
        .output()
        .expect("Node.js is required for the agent-workbench browser projection gate");
    assert!(
        output.status.success(),
        "browser projection gate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn session_event_registry_isolates_channels_and_recreates_after_removal() {
    let registry = SessionEventRegistry::new(4);
    let mut session_a = registry.subscribe("session-a");
    let mut session_b = registry.subscribe("session-b");

    registry.publish("session-a", StreamItem::Done { turn_id: None });
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

    registry.remove("session-a");
    assert!(!registry.contains("session-a"));
    let mut replacement_a = registry.subscribe("session-a");
    registry.publish("session-a", StreamItem::Done { turn_id: None });
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

#[tokio::test]
async fn product_event_route_lag_emits_durable_ordered_resync() {
    let data_dir = tempfile::tempdir().expect("workbench lag tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 1).await;
    let session_id = state.current_session_id();
    let response = session_events(
        State(state.clone()),
        Query(ProductEventsQuery {
            session_id: None,
            cursor: Some(0),
        }),
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
                },
            },
        );
    }

    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(2), async {
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

#[tokio::test]
async fn workbench_state_snapshot_merges_canonical_history_with_partial_product_log() {
    let data_dir = tempfile::tempdir().expect("workbench state merge tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open canonical session");
    session
        .admin()
        .state()
        .append_messages(vec![
            lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::User,
                "canonical question",
            )
            .with_id("canonical-user"),
            lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::Assistant,
                "canonical answer",
            )
            .with_id("canonical-assistant"),
        ])
        .await
        .expect("append canonical history");
    session.close().await.expect("close canonical session");

    state.push_message_with_id_for_session(
        &session_id,
        "canonical-assistant",
        "assistant",
        "stale mirrored answer",
    );
    state.push_message_with_id_for_session(
        &session_id,
        "host-only-event",
        "event",
        "host-only row",
    );

    let Json(snapshot) = Box::pin(app_state(State(state), Query(SessionQuery::default())))
        .await
        .expect("materialize merged state");
    assert_eq!(
        snapshot
            .messages
            .iter()
            .map(|message| (message.id.as_str(), message.text.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("canonical-user", "canonical question"),
            ("canonical-assistant", "canonical answer"),
            ("host-only-event", "host-only row"),
        ],
        "the Lash read view remains authoritative and product-only rows supplement it"
    );
}

#[tokio::test]
async fn send_turn_state_projection_stays_readable_while_turn_runs() {
    let data_dir = tempfile::tempdir().expect("send turn projection tempdir");
    let (provider_entered_tx, mut provider_entered_rx) = mpsc::unbounded_channel();
    let provider_release = Arc::new(tokio::sync::Notify::new());
    let provider_release_for_completion = Arc::clone(&provider_release);
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_completion = Arc::clone(&response_index);
    let provider = lash::testing::TestProvider::builder()
        .kind("send-turn-state-projection")
        .complete(move |_| {
            let provider_entered_tx = provider_entered_tx.clone();
            let provider_release = Arc::clone(&provider_release_for_completion);
            let response_index = Arc::clone(&response_index_for_completion);
            async move {
                let call = response_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = provider_entered_tx.send(call);
                if call == 0 {
                    provider_release.notified().await;
                }
                Ok(match call {
                    0 => text_response(
                        "<lashlang>\nprint(\"durable execution disclosure\")\n</lashlang>",
                    ),
                    1 => text_response("<lashlang>\nfinish \"settled answer\"\n</lashlang>"),
                    other => panic!("unexpected provider call {other}"),
                })
            }
        })
        .build()
        .into_handle();
    let mut state =
        recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    state.restate_ingress_url = restate_ingress_url;
    let session_id = state.current_session_id();
    let turn_text = "exercise the user-facing send path";

    let _ = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: turn_text.to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("send turn through the production handler");
    let submitted = restate_requests
        .recv()
        .await
        .expect("capture submitted Restate turn");
    let turn_id = submitted
        .pointer("/body/turn_id")
        .and_then(Value::as_str)
        .expect("submitted turn id")
        .to_string();

    let run_state = state.clone();
    let run_turn_id = turn_id.clone();
    let turn = tokio::spawn(async move {
        let session = run_state
            .core
            .session(session_id)
            .open()
            .await
            .expect("open submitted turn session");
        let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let output = session
            .turn(lash::TurnInput::text(turn_text))
            .turn_id(run_turn_id.clone())
            .require_finish()
            .expect("require finish")
            .stream_to(&ChannelTurnEvents {
                turn_state: Arc::clone(&turn_state),
            })
            .await
            .expect("run submitted turn");
        crate::restate::record_turn_output(
            &run_state,
            &session,
            &run_turn_id,
            output,
            turn_state,
            "test.send_turn.completed",
        )
        .await
        .expect("record submitted turn output");
        crate::restate::settle_workbench_turn(&run_state, &session.session_id(), &run_turn_id)
            .await
            .expect("settle submitted turn");
    });

    assert_eq!(
        provider_entered_rx.recv().await,
        Some(0),
        "the first provider call must be blocked before the mid-turn read"
    );
    let Json(running) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("/api/state must remain readable while the turn lease is held");
    assert_eq!(running.active_turns.len(), 1);

    provider_release.notify_one();
    turn.await.expect("submitted turn task");
    assert_eq!(
        provider_entered_rx.recv().await,
        Some(1),
        "the turn must execute the terminal provider iteration"
    );
}

#[tokio::test]
async fn workbench_sequential_settled_turn_cancels_each_emit_done() {
    let data_dir = tempfile::tempdir().expect("workbench cancel identity tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open cancel identity session");

    for turn_id in ["settled-turn-a", "settled-turn-b"] {
        session
            .turn(lash::TurnInput::text(format!("complete {turn_id}")))
            .turn_id(turn_id)
            .require_finish()
            .expect("require finish")
            .run()
            .await
            .expect("complete turn before stale cancel");
        state.track_turn(&session_id, turn_id);
        let receipts = state
            .cancel_turns_for_session(&session_id)
            .await
            .expect("cancel settled turn");
        assert!(matches!(
            receipts.as_slice(),
            [TurnCancelReceipt {
                outcome: lash::TurnCancelOutcome::CompletionWonRace,
                ..
            }]
        ));
    }

    let done_ids = state
        .event_tx
        .snapshot(&session_id)
        .events
        .into_iter()
        .filter_map(|event| {
            matches!(event.item, StreamItem::Done { .. }).then_some(event.event_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        done_ids.len(),
        2,
        "distinct evidence-free cancel operations must not collide"
    );
    assert_ne!(done_ids[0], done_ids[1]);
}

#[tokio::test]
async fn product_event_identity_deduplicates_real_live_and_canonical_turn_output() {
    let data_dir = tempfile::tempdir().expect("product event tempdir");
    let path = data_dir.path().join("product-events.json");
    let mut state = recoverable_chat_test_state(data_dir.path(), 4).await;
    state.event_tx =
        SessionEventRegistry::persistent(path.clone(), 4).expect("persistent product events");
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open real turn session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text("produce one stable answer"))
        .turn_id("stable-turn")
        .require_finish()
        .expect("require finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("run real streamed turn");
    crate::restate::record_turn_output(
        &state,
        &session,
        "stable-turn",
        output,
        turn_state,
        "test.real_turn.completed",
    )
    .await
    .expect("record real turn output");
    assert!(
        session
            .read_view()
            .messages()
            .iter()
            .any(|message| message.id == "workbench-assistant:stable-turn"),
        "the production turn recorder must commit the canonical assistant row"
    );
    session.close().await.expect("close real turn session");

    let reopened =
        SessionEventRegistry::persistent(path, 4).expect("reopen persistent product events");
    let assistant_events = reopened
        .snapshot(&session_id)
        .events
        .into_iter()
        .filter(|event| {
            matches!(
                &event.item,
                StreamItem::Message { message }
                    if message.id == "workbench-assistant:stable-turn"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_events.len(),
        1,
        "the real live product row must persist exactly once across reload"
    );
}

#[tokio::test]
async fn workbench_provider_failure_emits_only_fixed_public_product_copy() {
    const INTERNAL_PROVIDER_FAILURE: &str = "provider rejected credentials for secret account";
    let data_dir = tempfile::tempdir().expect("provider failure tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-provider-failure")
        .complete_error(INTERNAL_PROVIDER_FAILURE)
        .build()
        .into_handle();
    let state =
        recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open provider failure session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text("fail through the provider"))
        .turn_id("provider-failure-turn")
        .require_finish()
        .expect("require finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("provider failure is represented as a stopped turn");
    assert!(
        output
            .errors
            .iter()
            .any(|error| error.message.contains(INTERNAL_PROVIDER_FAILURE)),
        "the real provider diagnostic must reach the internal turn result"
    );
    crate::restate::record_turn_output(
        &state,
        &session,
        "provider-failure-turn",
        output,
        turn_state,
        "test.provider.failed",
    )
    .await
    .expect("project provider failure through the production recorder");

    let serialized = serde_json::to_string(&state.event_tx.snapshot(&session_id))
        .expect("serialize provider failure projection");
    assert!(serialized.contains(PUBLIC_TURN_FAILURE_MESSAGE));
    assert!(!serialized.contains(INTERNAL_PROVIDER_FAILURE));

    let response = AppError::internal(INTERNAL_PROVIDER_FAILURE).into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("read internal error response");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).expect("decode internal error response"),
        json!({ "error": "internal server error" })
    );
}

#[test]
fn authorization_seam_can_deny_observation_without_product_specific_auth() {
    struct DenyObservation;

    impl WorkbenchAuthorizer for DenyObservation {
        fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError> {
            match action {
                WorkbenchAuthorizationAction::Observe { .. } => {
                    Err(AppError::forbidden("observation denied by host policy"))
                }
                _ => Ok(()),
            }
        }
    }

    let authorization = WorkbenchAuthorization::with_authorizer(Arc::new(DenyObservation));
    let denied = authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: "auth-session".to_string(),
        })
        .expect_err("host policy must be able to deny observation");
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: "auth-session".to_string(),
        })
        .expect("independent enqueue policy remains pluggable");
}
