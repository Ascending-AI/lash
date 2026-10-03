use super::*;

use lash::triggers::TriggerStore;

#[test]
fn button_trigger_lifecycle_stays_visible_and_queues_wakes_during_active_turn() {
    run_async_test_on_stack_budget("workbench-button-trigger-lifecycle-test", || {
        button_trigger_lifecycle_stays_visible_and_queues_wakes_during_active_turn_inner()
    });
}
async fn button_trigger_lifecycle_stays_visible_and_queues_wakes_during_active_turn_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-processes-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let double = crate::tests::test_double_backend(0).await;
    let session_store_factory: Arc<dyn lash::persistence::DeploymentStore> =
        double.stores().session_store_factory();
    let core_store_factory: Arc<dyn lash::persistence::DeploymentStore> =
        session_store_factory.clone();
    let process_registry = double.engine_stores().process_registry();
    let trigger_store = double.stores().trigger_store();
    let provider = trigger_registration_provider();
    let model = lash::LlmProfileMetadata::builder("test-model")
        .context_window_tokens(4096)
        .build()
        .expect("model spec");
    let sessions = WorkbenchSessions::fresh();
    let session_id = sessions.current();
    let core = explicit_durable_test_facets_on(double.lash_backend())
        .serve_workbench_llm_profile(provider, model)
        .plugin(Arc::new(WorkbenchPluginFactory::new()))
        .build(crate::test_core_owner())
        .expect("build core");
    crate::tests::install_test_process_worker(&double, &core);
    let session = crate::created_session(&core, session_id.clone())
        .await
        .open()
        .await
        .expect("open session");
    register_test_trigger(&session).await;
    // The wakes the trigger's processes deliver stay queued while the host's
    // turn is active: the engine admits none of them during this test.
    let _hold = double.hold_session_shift(&session_id).await;
    let trigger_records = assert_remote_trigger_subscription_records_round_trip(
        double.stores().trigger_store().as_ref(),
        &session_id,
    )
    .await;
    assert_eq!(trigger_records.len(), 1);
    let trigger_record = &trigger_records[0];
    let tool_names = session
        .admin()
        .tools()
        .active_manifests()
        .await
        .expect("active tools")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    let removed_tool_name = ["attach", "button", "trigger"].join("_");
    assert!(!tool_names.iter().any(|name| name == &removed_tool_name));

    let active_turns = ActiveTurns::default();
    active_turns.insert(
        &session_id,
        "mid-turn-trigger-contract",
        WorkbenchTurnKind::User,
    );
    let first_report = emit_test_button_trigger(&double, &core, ButtonChoice::Red).await;
    let second_report = emit_test_button_trigger(&double, &core, ButtonChoice::Red).await;
    assert_remote_trigger_emit_report_round_trip(&first_report);
    assert_remote_trigger_emit_report_round_trip(&second_report);
    assert_eq!(first_report.started_process_ids().len(), 1);
    assert_eq!(second_report.started_process_ids().len(), 1);
    for process_id in first_report
        .started_process_ids()
        .into_iter()
        .chain(second_report.started_process_ids())
    {
        tokio::time::timeout(
            Duration::from_secs(5),
            core.processes().await_output(&process_id),
        )
        .await
        .expect("trigger process should finish promptly")
        .expect("trigger process should finish");
    }

    trigger_store
        .execute_command(
            "workbench-test-disable",
            lash::triggers::TriggerCommand::Disable {
                owner_scope: trigger_record.owner_scope.clone(),
                actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                    &session_id,
                )),
                subscription_key: trigger_record.subscription_key.clone(),
                expected_revision: trigger_record.revision,
            },
        )
        .await
        .expect("execute disable")
        .expect("disable trigger");
    let disabled_report = emit_test_button_trigger(&double, &core, ButtonChoice::Red).await;
    assert!(disabled_report.started_process_ids().is_empty());
    trigger_store
        .execute_command(
            "workbench-test-enable",
            lash::triggers::TriggerCommand::Enable {
                owner_scope: trigger_record.owner_scope.clone(),
                actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                    &session_id,
                )),
                subscription_key: trigger_record.subscription_key.clone(),
                expected_revision: trigger_record.revision + 1,
            },
        )
        .await
        .expect("execute enable")
        .expect("re-enable trigger");
    let reenabled_report = emit_test_button_trigger(&double, &core, ButtonChoice::Red).await;
    let reenabled_process_id = reenabled_report.started_process_ids()[0].clone();
    tokio::time::timeout(
        Duration::from_secs(5),
        core.processes().await_output(&reenabled_process_id),
    )
    .await
    .expect("re-enabled trigger process should finish promptly")
    .expect("re-enabled trigger process should finish");
    trigger_store
        .execute_command(
            "workbench-test-delete",
            lash::triggers::TriggerCommand::Delete {
                owner_scope: trigger_record.owner_scope.clone(),
                actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                    &session_id,
                )),
                subscription_key: trigger_record.subscription_key.clone(),
                expected_revision: trigger_record.revision + 2,
            },
        )
        .await
        .expect("execute delete")
        .expect("delete trigger");
    let deleted_report = emit_test_button_trigger(&double, &core, ButtonChoice::Red).await;
    assert!(deleted_report.started_process_ids().is_empty());

    let handles = session
        .admin()
        .processes()
        .list_all()
        .await
        .expect("list handles");
    assert_eq!(handles.len(), 3);
    assert!(handles.iter().all(|handle| handle.kind() == "lashlang"));
    // #1529 retired the source-level process name: a lifted process literal's
    // label is its lift digest (`__process_<hash>`). All three runs come from
    // the one registered definition, so the label is still pinned — as the one
    // digest they must share, rather than as the name the surface dropped.
    let first_label = handles[0].label().to_string();
    assert!(first_label.starts_with("__process_"), "{first_label}");
    assert!(handles.iter().all(|handle| handle.label() == first_label));
    session.close().await.expect("close session");

    let reopened = crate::created_session(&core, session_id.clone())
        .await
        .open()
        .await
        .expect("reopen session");
    let reopened_handles = reopened
        .admin()
        .processes()
        .list_all()
        .await
        .expect("list handles after reopen");
    assert_eq!(reopened_handles.len(), 3);
    assert!(
        reopened_handles
            .iter()
            .all(|handle| handle.status_label() == "completed")
    );
    drop(reopened);

    assert_remote_started_process_surface(
        &core,
        process_registry.as_ref(),
        &session_id,
        &first_report
            .started_process_ids()
            .into_iter()
            .chain(second_report.started_process_ids())
            .chain([reenabled_process_id])
            .collect::<Vec<_>>(),
    )
    .await;

    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        session_defaults: crate::tests::test_session_defaults(),
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store,
        process_observer,
        // Process work is resolved through the core.
        sessions,
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_llm_profile: Arc::new(Mutex::new(LlmProfileSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(1024),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: active_turns.clone(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    let target_session_id = state.current_session_id();
    session_store_factory
        .admit_session(&lash::persistence::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash::persistence::SessionRelation::Root,
            config: lash::runtime::SessionPolicy::new(
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .into(),
            head: lash::persistence::SessionCreationHead::Config,
        })
        .await
        .expect("open session store");
    let session_store = Arc::clone(&session_store_factory);
    let queued = session_store
        .list_queued_work(&session_id)
        .await
        .expect("list queued work");
    assert_eq!(queued.len(), 3);
    let lash::persistence::QueuedWorkPayload::ProcessWake { wake } = &queued[0].payload else {
        panic!("expected process wake queue payload");
    };
    assert!(wake.input.contains("button_pressed"));
    assert!(wake.input.contains("Red"));
    assert_eq!(
        wake.target_session_id.as_str(),
        target_session_id,
        "process wake should target the current session"
    );

    active_turns.remove(&session_id, &TurnId::from("mid-turn-trigger-contract"));
    let Json(work) = list_work(State(state), Query(SessionQuery::default()))
        .await
        .expect("list work");
    assert_eq!(work.len(), 3);
    assert!(
        work.iter()
            .all(|item| item.process.status_label == "completed")
    );
    assert!(
        work[0]
            .events
            .iter()
            .any(|event| event.event_type == "process.completed")
    );
    // The wake-carrying progress emission. ADR 0095 deleted the `wake` special
    // form this line was written against: the source now emits through
    // `processes.emit`, which appends under `process.yield`, and that event type
    // is what carries the wake to the declaring session. #1535 re-spelled the
    // identical assertions in `crates/lash/src/testing.rs` and missed this one;
    // the assertion is the same one, on the live event name.
    assert!(
        work[0]
            .events
            .iter()
            .any(|event| event.event_type == "process.yield")
    );
    let _ = std::fs::remove_dir_all(data_dir);
}
