use super::*;

pub(super) async fn reset_chat_deletes_old_session_and_clears_trigger_started_work_inner() {
    let data_dir =
        std::env::temp_dir().join(format!("agent-workbench-reset-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.join("lash-sessions"),
    ));
    let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = session_store_factory;
    let process_registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &data_dir.join("processes.db"),
            data_dir.join("lash-sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn lash::process::ProcessRegistry>;
    let provider = trigger_registration_provider();
    let model = test_model();
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model)
        .store_factory(Arc::clone(&core_store_factory))
        .plugin(Arc::new(WorkbenchPluginFactory::new()))
        .process_registry(Arc::clone(&process_registry))
        .build(crate::test_core_owner())
        .expect("build core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        core,
        rlm_dialect: lash::rlm::RlmDialect::Lashlang,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store: in_memory_trigger_store(),
        process_observer,
        // Process work is resolved through the core.
        sessions: WorkbenchSessions::fresh(),
        messages: Arc::new(Mutex::new(vec![ChatMessage {
            id: "message".to_string(),
            role: "user".to_string(),
            text: "before reset".to_string(),
            at: "2026-05-27T00:00:00Z".to_string(),
            attachments: Vec::new(),
            provenance: None,
        }])),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(1024),
        queued_work_driver: inert_queued_work(),
        restate_ingress_url,
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    let old_session_id = state.current_session_id();
    let _deleted_session_events = state.event_tx.subscribe(&old_session_id);
    assert!(state.event_tx.contains(&old_session_id));
    let session = state
        .core
        .session(old_session_id.clone())
        .open()
        .await
        .expect("open old session");
    register_test_trigger(&session).await;
    let started = emit_test_button_trigger(&state.core, ButtonChoice::Red).await;
    assert_remote_trigger_emit_report_round_trip(&started);
    let trigger_records =
        assert_remote_trigger_subscription_records_round_trip(&data_dir, &old_session_id).await;
    assert_eq!(trigger_records.len(), 1);
    assert_eq!(started.started_process_ids().len(), 1);
    let old_work_before_reset = state
        .process_observer
        .snapshot_for_session(&old_session_id)
        .await
        .expect("old work before reset");
    assert_eq!(
        old_work_before_reset
            .visible_processes
            .into_iter()
            .map(|process_ref| process_ref.process_id)
            .collect::<Vec<_>>(),
        started.started_process_ids()
    );
    append_started_graph(
        &state.lashlang_execution,
        &test_graph(
            "process:old-reset-process",
            &old_session_id,
            TraceRuntimeSubject::Process {
                process_id: ProcessId::from("old-reset-process"),
            },
            Vec::new(),
        ),
    );
    assert_eq!(state.lashlang_execution.graphs().len(), 1);
    assert_remote_started_process_surface(
        &state.core,
        process_registry.as_ref(),
        &old_session_id,
        &started.started_process_ids(),
    )
    .await;
    state
        .mail_world
        .add_account("Reset Probe")
        .expect("add account before reset");
    let query = Query(SessionQuery {
        session_id: Some(old_session_id.clone()),
    });
    let Json(snapshot) = Box::pin(reset_chat(State(state.clone()), query))
        .await
        .expect("reset");

    assert_ne!(snapshot.settings.session_id, old_session_id);
    assert!(!state.event_tx.contains(&old_session_id));
    assert!(snapshot.messages.is_empty());
    assert!(state.messages_snapshot().is_empty());
    assert!(
        state.mail_world.account_summaries().is_empty(),
        "reset must clear mail accounts along with the chat session"
    );
    let request = tokio::time::timeout(Duration::from_secs(2), restate_requests.recv())
        .await
        .expect("Restate request")
        .expect("Restate request payload");
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .expect("request path");
    assert!(
        path.starts_with("WorkbenchSessionDeleteWorkflow/workbench-delete-"),
        "unexpected Restate path: {path}"
    );
    assert!(path.ends_with("/run"), "unexpected Restate path: {path}");
    assert_eq!(
        request.pointer("/body/session_id").and_then(Value::as_str),
        Some(old_session_id.as_str())
    );
    let old_work_after_reset = state
        .process_observer
        .snapshot_for_session(&old_session_id)
        .await
        .expect("old work after reset submission");
    assert_eq!(
        old_work_after_reset
            .visible_processes
            .into_iter()
            .map(|process_ref| process_ref.process_id)
            .collect::<Vec<_>>(),
        started.started_process_ids(),
        "mock Restate ingress must not consume deletion work inline"
    );
    assert!(
        state
            .core
            .session(snapshot.settings.session_id)
            .open()
            .await
            .expect("open new session")
            .admin()
            .processes()
            .list()
            .await
            .expect("new work")
            .is_empty()
    );
    let Json(graph_index) =
        list_lashlang_graphs(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list graphs after reset");
    assert!(
        graph_index.graphs.is_empty(),
        "new session graph index should be empty after reset: {graph_index:#?}"
    );
    // The captured Restate call did not execute deletion. Finish the trigger's
    // normal wake delivery before this test's manual retirement/route probe;
    // raw catalog deletion must continue to refuse a live closure pin.
    tokio::time::timeout(Duration::from_secs(20), async {
        lash::process::NativeProcessWork::for_registry(Arc::clone(&process_registry))
            .await_terminal(&started.started_process_ids()[0])
            .await
            .expect("trigger process finishes before manual retirement");
        loop {
            state
                .core
                .processes()
                .drive_wake_deliveries()
                .await
                .expect("deliver trigger wake");
            let report = state
                .core
                .processes()
                .wake_delivery_report()
                .await
                .expect("observe wake delivery");
            if report.pending == 0 && report.enqueuing == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let store = core_store_factory
            .open_existing_store_by_id(&old_session_id)
            .await
            .expect("open old session store")
            .expect("old session still exists");
        // The pending queue view omits live claims. The complete queue keeps
        // those rows until the final transaction consumes their closure pin.
        loop {
            if store
                .list_queued_work(&old_session_id)
                .await
                .expect("read all trigger wake batches, including live claims")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("trigger wake settles before manual retirement");
    assert!(
        core_store_factory
            .pending_turn_cancel_closure_pins(&old_session_id)
            .await
            .expect("read closure pins after trigger wake")
            .is_empty()
    );
    drop(session);
    core_store_factory
        .delete_session(&old_session_id)
        .await
        .expect("retire old session for route check");
    let retired_error = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(old_session_id.clone()),
        }),
    ))
    .await
    .expect_err("retired session state must be refused");
    assert_eq!(retired_error.status, StatusCode::CONFLICT);
    assert!(retired_error.message.contains(old_session_id.as_str()));
    assert!(retired_error.message.contains("was used and deleted"));
    let _ = std::fs::remove_dir_all(data_dir);
}
