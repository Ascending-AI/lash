#[test]
fn concurrent_sessions_isolate_transcripts_triggers_and_processes() {
    run_async_test_on_stack_budget("workbench-session-isolation-test", || {
        concurrent_sessions_isolate_transcripts_triggers_and_processes_inner()
    });
}
async fn concurrent_sessions_isolate_transcripts_triggers_and_processes_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-session-isolation-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.join("lash-sessions"),
    ));
    let process_registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &data_dir.join("processes.db"),
            data_dir.join("lash-sessions"),
        )
        .await
        .expect("open process registry"),
    ) as Arc<dyn lash::process::ProcessRegistry>;
    let trigger_store = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
            .await
            .expect("open trigger store"),
    );
    let artifact_store = Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
            .await
            .expect("open artifact store"),
    ) as Arc<dyn lashlang::LashlangArtifactStore>;
    let process_env_store = Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
            .await
            .expect("open process env store"),
    );
    let core = test_workbench_core(
        session_store_factory,
        Arc::clone(&process_registry),
        Arc::clone(&trigger_store),
        artifact_store,
        Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )),
        process_env_store,
    );
    let session_a_id = "workbench-isolation-a";
    let session_b_id = "workbench-isolation-b";
    let session_a = core
        .session(session_a_id)
        .open()
        .await
        .expect("open session A");
    let session_b = core
        .session(session_b_id)
        .open()
        .await
        .expect("open session B");

    let (turn_a, turn_b) = tokio::join!(
        session_a
            .turn(lash::TurnInput::text("isolation-marker-A"))
            .turn_id("isolation-turn-a")
            .run(),
        session_b
            .turn(lash::TurnInput::text("isolation-marker-B"))
            .turn_id("isolation-turn-b")
            .run(),
    );
    assert_eq!(
        turn_a.expect("session A turn").final_value(),
        Some(&json!("registered"))
    );
    assert_eq!(
        turn_b.expect("session B turn").final_value(),
        Some(&json!("registered"))
    );
    let transcript_a = session_a
        .read_view()
        .messages()
        .iter()
        .map(lash::message_text)
        .collect::<Vec<_>>();
    let transcript_b = session_b
        .read_view()
        .messages()
        .iter()
        .map(lash::message_text)
        .collect::<Vec<_>>();
    assert!(transcript_a.iter().any(|text| text == "isolation-marker-A"));
    assert!(!transcript_a.iter().any(|text| text == "isolation-marker-B"));
    assert!(transcript_b.iter().any(|text| text == "isolation-marker-B"));
    assert!(!transcript_b.iter().any(|text| text == "isolation-marker-A"));

    let registrations_a = lash::triggers::TriggerStore::list_subscriptions(
        trigger_store.as_ref(),
        lash::triggers::TriggerSubscriptionFilter::for_session(session_a_id),
    )
    .await
    .expect("session A triggers");
    let registrations_b = lash::triggers::TriggerStore::list_subscriptions(
        trigger_store.as_ref(),
        lash::triggers::TriggerSubscriptionFilter::for_session(session_b_id),
    )
    .await
    .expect("session B triggers");
    assert_eq!(registrations_a.len(), 1);
    assert_eq!(registrations_b.len(), 1);
    assert_eq!(registrations_a[0].source_key, registrations_b[0].source_key);

    let (report_a, report_b) = tokio::join!(
        emit_test_button_trigger_for_session(&core, ButtonChoice::Blue, session_a_id),
        emit_test_button_trigger_for_session(&core, ButtonChoice::Blue, session_b_id),
    );
    assert_eq!(report_a.started_process_ids().len(), 1);
    assert_eq!(report_b.started_process_ids().len(), 1);
    assert_ne!(
        report_a.started_process_ids()[0],
        report_b.started_process_ids()[0]
    );
    let awaiter = lash::process::NativeProcessWork::for_registry(Arc::clone(&process_registry));
    for process_id in report_a
        .started_process_ids()
        .into_iter()
        .chain(report_b.started_process_ids())
    {
        awaiter
            .await_terminal(&process_id)
            .await
            .expect("isolated trigger process terminal");
    }
    let processes_a = session_a
        .processes()
        .list_all()
        .await
        .expect("session A work");
    let processes_b = session_b
        .processes()
        .list_all()
        .await
        .expect("session B work");
    assert_eq!(processes_a.len(), 1);
    assert_eq!(processes_b.len(), 1);
    assert_eq!(processes_a[0].process_id, report_a.started_process_ids()[0]);
    assert_eq!(processes_b[0].process_id, report_b.started_process_ids()[0]);
    assert_ne!(processes_a[0].process_id, processes_b[0].process_id);

    session_a.close().await.expect("close session A");
    session_b.close().await.expect("close session B");
    let _ = std::fs::remove_dir_all(data_dir);
}
