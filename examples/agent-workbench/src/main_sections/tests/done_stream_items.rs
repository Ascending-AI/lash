//! Transient `Done` stream items: they publish to live observers and never
//! enter the snapshotted product-event log.
//!
//! Extracted from `main_sections/tests.rs` unchanged, which was on its
//! test-file line budget.

use super::*;

#[test]
fn done_stream_items_are_transient_and_not_snapshotted() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-transient-done-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let process_registry = Arc::new(sync_await({
        let path = data_dir.join("processes.db");
        async move {
            lash_sqlite_store::SqliteProcessRegistry::open(&path, path.with_extension("sessions"))
                .await
                .expect("open registry")
        }
    })) as Arc<dyn lash::process::ProcessRegistry>;
    let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.join("lash-sessions"),
    ));
    let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = session_store_factory;
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete_error("transient done test should not call the provider")
        .build()
        .into_handle();
    let model = test_model();
    let event_tx = SessionEventRegistry::new(16);
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model)
        .store_factory(Arc::clone(&core_store_factory))
        .process_registry(Arc::clone(&process_registry))
        .build(crate::test_core_owner())
        .expect("build core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store: in_memory_trigger_store(),
        process_observer,
        // Process work is resolved through the core.
        sessions: WorkbenchSessions::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx,
        queued_work_driver: inert_queued_work(),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    let session_id = state.current_session_id();
    let mut events = state.event_tx.subscribe(&session_id);

    state.publish_turn_done(&session_id, &TurnId::from("transient-turn"));

    assert!(matches!(
        events.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done {
                turn_id: Some(turn_id),
                outcome: TurnDoneOutcome::Completed,
            },
            ..
        }) if turn_id == "transient-turn"
    ));
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
fn trigger_dispatch_done_does_not_clear_an_active_turn() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-trigger-dispatch-done-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let process_registry = Arc::new(sync_await({
        let path = data_dir.join("processes.db");
        async move {
            lash_sqlite_store::SqliteProcessRegistry::open(&path, path.with_extension("sessions"))
                .await
                .expect("open registry")
        }
    })) as Arc<dyn lash::process::ProcessRegistry>;
    let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
    );
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-trigger-dispatch-done-test")
        .complete_error("trigger dispatch done test should not call the provider")
        .build()
        .into_handle();
    let model = test_model();
    let event_tx = SessionEventRegistry::new(16);
    let core = explicit_durable_test_facets(&data_dir)
        .provider(provider)
        .model(model)
        .store_factory(Arc::clone(&store_factory))
        .process_registry(Arc::clone(&process_registry))
        .build(crate::test_core_owner())
        .expect("build core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&store_factory),
        trigger_store: in_memory_trigger_store(),
        process_observer,
        // Process work is resolved through the core.
        sessions: WorkbenchSessions::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx,
        queued_work_driver: inert_queued_work(),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    let session_id = state.current_session_id();
    let mut events = state.event_tx.subscribe(&session_id);

    state.track_turn(&session_id, &TurnId::from("foreground-turn"));
    state.publish_trigger_dispatch_done(&session_id, "trigger-running");
    assert!(
        matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "trigger dispatch must not publish Done while a foreground turn is active"
    );

    state
        .active_turns
        .remove(&session_id, &TurnId::from("foreground-turn"));
    state.publish_trigger_dispatch_done(&session_id, "trigger-settled");
    assert!(matches!(
        events.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done {
                turn_id: None,
                outcome: TurnDoneOutcome::Completed,
            },
            ..
        })
    ));
    let _ = std::fs::remove_dir_all(data_dir);
}
