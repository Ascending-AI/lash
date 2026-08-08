    use lash::persistence::SessionStoreFactory;
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
        let db_path = data_dir.join("processes.db");
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory.clone();
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&db_path, db_path.with_extension("sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let trigger_store = Arc::new(
            lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
                .await
                .expect("open trigger store"),
        );
        let provider = trigger_registration_provider();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let session_ids = WorkbenchSessionIds::fresh();
        let session_id = session_ids.current();
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .plugin(Arc::new(WorkbenchPluginFactory::new("")))
            .process_registry(Arc::clone(&process_registry))
            .trigger_store(trigger_store.clone())
            .disable_queued_work_driver()
            .build()
            .expect("build core");
        let session = core
            .session(session_id.clone())
            .open()
            .await
            .expect("open session");
        register_test_trigger(&session).await;
        let trigger_records =
            assert_remote_trigger_subscription_records_round_trip(&data_dir, &session_id).await;
        assert_eq!(trigger_records.len(), 1);
        let trigger_record = &trigger_records[0];
        let tool_names = session
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
        active_turns.insert(&session_id, "mid-turn-trigger-contract");
        let first_report = emit_test_button_trigger(&core, ButtonChoice::Red).await;
        let second_report = emit_test_button_trigger(&core, ButtonChoice::Red).await;
        assert_remote_trigger_emit_report_round_trip(&first_report);
        assert_remote_trigger_emit_report_round_trip(&second_report);
        assert_eq!(first_report.started_process_ids().len(), 1);
        assert_eq!(second_report.started_process_ids().len(), 1);
        let awaiter = lash::process::ProcessAwaiter::polling(Arc::clone(&process_registry));
        for process_id in first_report
            .started_process_ids()
            .into_iter()
            .chain(second_report.started_process_ids())
        {
            tokio::time::timeout(Duration::from_secs(5), awaiter.await_terminal(&process_id))
                .await
                .expect("trigger process should finish promptly")
                .expect("trigger process should finish");
        }

        trigger_store
            .execute_command(
                "workbench-test-disable",
                lash::triggers::TriggerCommand::Disable {
                    owner_scope: trigger_record.owner_scope.clone(),
                    actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(&session_id)),
                    subscription_key: trigger_record.subscription_key.clone(),
                    expected_revision: trigger_record.revision,
                },
            )
            .await
            .expect("execute disable")
            .expect("disable trigger");
        let disabled_report = emit_test_button_trigger(&core, ButtonChoice::Red).await;
        assert!(disabled_report.started_process_ids().is_empty());
        trigger_store
            .execute_command(
                "workbench-test-enable",
                lash::triggers::TriggerCommand::Enable {
                    owner_scope: trigger_record.owner_scope.clone(),
                    actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(&session_id)),
                    subscription_key: trigger_record.subscription_key.clone(),
                    expected_revision: trigger_record.revision + 1,
                },
            )
            .await
            .expect("execute enable")
            .expect("re-enable trigger");
        let reenabled_report = emit_test_button_trigger(&core, ButtonChoice::Red).await;
        let reenabled_process_id = reenabled_report.started_process_ids()[0].clone();
        tokio::time::timeout(
            Duration::from_secs(5),
            awaiter.await_terminal(&reenabled_process_id),
        )
        .await
        .expect("re-enabled trigger process should finish promptly")
        .expect("re-enabled trigger process should finish");
        trigger_store
            .execute_command(
                "workbench-test-delete",
                lash::triggers::TriggerCommand::Delete {
                    owner_scope: trigger_record.owner_scope.clone(),
                    actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(&session_id)),
                    subscription_key: trigger_record.subscription_key.clone(),
                    expected_revision: trigger_record.revision + 2,
                },
            )
            .await
            .expect("execute delete")
            .expect("delete trigger");
        let deleted_report = emit_test_button_trigger(&core, ButtonChoice::Red).await;
        assert!(deleted_report.started_process_ids().is_empty());

        let handles = session.processes().list_all().await.expect("list handles");
        assert_eq!(handles.len(), 3);
        assert!(handles.iter().all(|handle| handle.kind == "lashlang"));
        assert!(handles.iter().all(|handle| handle.label == "remember"));
        session.close().await.expect("close session");

        let reopened = core
            .session(session_id.clone())
            .open()
            .await
            .expect("reopen session");
        let reopened_handles = reopened
            .processes()
            .list_all()
            .await
            .expect("list handles after reopen");
        assert_eq!(reopened_handles.len(), 3);
        assert!(
            reopened_handles
                .iter()
                .all(|handle| handle.status_label == "completed")
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
            core,
            attachment_store: test_attachment_store(),
            trigger_store,
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids,
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(1024),
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: active_turns.clone(),
            authorization: WorkbenchAuthorization::allow_all(),
        };
        let target_session_id = state.current_session_id();
        let session_store =
            session_store_factory
                .create_store(&lash::persistence::SessionStoreCreateRequest {
                    session_id: session_id.clone(),
                    relation: lash::persistence::SessionRelation::Root,
                    policy: lash::runtime::SessionPolicy::default(),
                })
                .await
                .expect("open session store");
        let queued = session_store
            .list_queued_work(&session_id)
            .await
            .expect("list queued work");
        assert_eq!(queued.len(), 3);
        assert!(queued.iter().all(|batch| batch.items.len() == 1));
        let lash::persistence::QueuedWorkPayload::ProcessWake { wake } =
            &queued[0].items[0].payload
        else {
            panic!("expected process wake queue payload");
        };
        assert!(wake.input.contains("button_pressed"));
        assert!(wake.input.contains("Red"));
        assert_eq!(
            wake.target_session_id.as_str(),
            target_session_id,
            "process wake should target the current session"
        );

        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        let submitter = WorkbenchQueuedWorkSubmitter {
            session_ids: state.session_ids.clone(),
            store_factory: Arc::clone(&core_store_factory),
            restate_ingress_url,
            restate_http: reqwest::Client::new(),
            active_turns: active_turns.clone(),
        };
        lash::runtime::QueuedWorkRunHandle::claim_and_run_pending(
            &submitter,
            Some(&session_id),
            "trigger_fired_mid_turn",
        )
        .await
        .expect("active-turn queued-work deferral");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), restate_requests.recv())
                .await
                .is_err(),
            "trigger wake must not submit a competing queued turn while the active turn owns ingress"
        );
        active_turns.remove(&session_id, "mid-turn-trigger-contract");
        lash::runtime::QueuedWorkRunHandle::claim_and_run_pending(
            &submitter,
            Some(&session_id),
            "active_turn_settled",
        )
        .await
        .expect("post-settle queued turn submission");
        let queued_turn_request = tokio::time::timeout(Duration::from_secs(1), restate_requests.recv())
            .await
            .expect("queued turn submission after settle")
            .expect("queued turn request body");
        assert!(
            queued_turn_request["path"]
                .as_str()
                .is_some_and(|path| path.starts_with("WorkbenchQueuedTurnWorkflow/")),
            "unexpected post-settle request: {queued_turn_request:#?}"
        );
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
        assert!(
            work[0]
                .events
                .iter()
                .any(|event| event.event_type == "process.wake")
        );
        let _ = std::fs::remove_dir_all(data_dir);
    }

    /// Pins what the operator is actually buying when they call the admin
    /// prune route.
    ///
    /// A mutation receipt is the only thing that makes a redriven trigger
    /// command a replay instead of a second evaluation. While the receipt
    /// exists, Restate can redrive the register handler forever and it keeps
    /// answering exactly what it answered the first time, even after the
    /// subscription it created was deliberately deleted. Prune the receipt and
    /// the same redrive is re-evaluated against current state — where the
    /// tombstone now makes it a hard, terminal conflict. A retry that was a
    /// safe no-op becomes a permanent handler failure, and nothing about the
    /// call site changed.
    ///
    /// This is the cost the route's contract makes its caller own, written
    /// down as a behavior rather than a warning.
    #[tokio::test]
    async fn pruning_a_mutation_receipt_turns_a_safe_redrive_into_a_terminal_conflict() {
        let data_dir = tempfile::tempdir().expect("receipt prune tempdir");
        let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
        let state = recoverable_chat_test_state_with_trigger_store(
            data_dir.path(),
            Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
        )
        .await;
        let session_id = state.current_session_id();
        let register = workbench_receipt_register_command(&session_id);
        // Restate redrives this handler under one stable operation id.
        let register_operation_id = "workbench-receipt-register";

        let created = workbench_trigger_mutation(
            trigger_store.as_ref(),
            register_operation_id,
            register.clone(),
        )
        .await
        .expect("first register");
        assert_eq!(
            created.disposition,
            lash::triggers::TriggerMutationDisposition::Created
        );
        workbench_trigger_mutation(
            trigger_store.as_ref(),
            "workbench-receipt-delete",
            lash::triggers::TriggerCommand::Delete {
                owner_scope: lash::triggers::TriggerOwnerScope::session(&session_id),
                actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                    &session_id,
                )),
                subscription_key: created.subscription_key.clone(),
                expected_revision: created.revision,
            },
        )
        .await
        .expect("operator deletes the subscription");
        assert!(
            workbench_live_trigger_keys(&state).await.is_empty(),
            "the operator deleted the subscription"
        );

        let replayed = workbench_trigger_mutation(
            trigger_store.as_ref(),
            register_operation_id,
            register.clone(),
        )
        .await
        .expect("a receipted redrive replays its recorded outcome");
        assert_eq!(
            replayed.disposition,
            lash::triggers::TriggerMutationDisposition::Created,
            "the retained receipt answers the redrive with the outcome the \
             handler already observed"
        );
        assert!(
            workbench_live_trigger_keys(&state).await.is_empty(),
            "and the replay writes nothing, so the delete stands"
        );

        let Json(pruned) = prune_trigger_mutation_receipts(
            State(state.clone()),
            Json(PruneTriggerMutationReceiptsRequest {
                before_epoch_ms: u64::MAX,
            }),
        )
        .await
        .expect("operator-invoked receipt prune");
        assert!(
            pruned.pruned >= 1,
            "the prune must report the receipts it destroyed: {pruned:?}"
        );

        let refused =
            workbench_trigger_mutation(trigger_store.as_ref(), register_operation_id, register)
                .await
                .expect_err("the unreceipted redrive is evaluated afresh");
        assert!(
            matches!(
                &refused,
                lash::triggers::TriggerOperationError::Conflict { subscription_key, .. }
                    if *subscription_key == created.subscription_key
            ),
            "this is the cost the route's caller owns: with the receipt gone, \
             a redrive that was a safe no-op fails terminally against the \
             tombstone it originally created: {refused:?}"
        );
    }

    async fn workbench_trigger_mutation(
        trigger_store: &dyn lash::triggers::TriggerStore,
        operation_id: &str,
        command: lash::triggers::TriggerCommand,
    ) -> Result<
        lash::triggers::TriggerMutationReceipt,
        lash::triggers::TriggerOperationError,
    > {
        let outcome = trigger_store
            .execute_command(operation_id, command)
            .await
            .expect("workbench trigger mutation reaches the store");
        match outcome? {
            lash::triggers::TriggerCommandOutcome::Mutation { receipt } => Ok(*receipt),
            other => panic!("expected a mutation outcome for `{operation_id}`: {other:?}"),
        }
    }

    async fn workbench_live_trigger_keys(state: &AppState) -> Vec<String> {
        let Json(records) = list_triggers(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list workbench triggers");
        records
            .iter()
            .map(|record| record.registration.subscription_key.clone())
            .collect()
    }

    fn workbench_receipt_register_command(session_id: &str) -> lash::triggers::TriggerCommand {
        lash::triggers::TriggerCommand::Register {
            owner_scope: lash::triggers::TriggerOwnerScope::session(session_id),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(session_id)),
            draft: lash::triggers::TriggerSubscriptionDraft {
                subscription_key: "workbench-receipt-prune".to_string(),
                env_ref: lash::process::ProcessExecutionEnvRef::new(format!(
                    "process-env:{session_id}"
                )),
                wake_target: Some(lash::process::SessionScope::new(session_id)),
                name: Some("receipt prune demo".to_string()),
                source_type: BUTTON_TRIGGER_EVENT.to_string(),
                source_key: "workbench-receipt-prune-source".to_string(),
                source: json!({ "button": "Blue" }),
                payload_schema: button_trigger_payload_schema(),
                target: lash::process::ProcessInput::Engine {
                    kind: "test".to_string(),
                    payload: json!({ "process": "receipt_prune_demo" }),
                },
                target_identity: lash::process::ProcessIdentity::new("test")
                    .with_label(Some("receipt prune demo".to_string()))
                    .with_definition(Some(json!({ "process_name": "receipt_prune_demo" }))),
                event_types: Vec::new(),
                input_template: std::collections::BTreeMap::from([(
                    "event".to_string(),
                    lash::triggers::TriggerInputBinding::Event,
                )]),
                target_label: Some("receipt prune demo".to_string()),
            },
        }
    }
