#[cfg(test)]
mod process_work_tests {
    use super::tests::{
        explicit_durable_test_facets, in_memory_trigger_store, run_async_test_on_stack_budget,
        spawn_restate_ingress_capture,
    };
    use super::*;

    #[test]
    fn workbench_work_rail_exposes_process_cancellation() {
        assert!(ui::INDEX_HTML.contains("className = \"work-cancel\""));
        assert!(ui::INDEX_HTML.contains("/cancel\""));
        assert!(ui::INDEX_HTML.contains("Request cooperative process cancellation"));
        assert!(ui::INDEX_HTML.contains("error: \" + process.error"));
        assert!(
            ui::INDEX_HTML.contains("row.error ? \" error\""),
            "failed work must receive the work rail's visible error treatment"
        );
    }

    #[test]
    fn await_work_route_returns_terminal_outcome_and_reconciled_events() {
        run_async_test_on_stack_budget("workbench-await-work-test", || {
            await_work_route_returns_terminal_outcome_and_reconciled_events_inner()
        });
    }

    async fn await_work_route_returns_terminal_outcome_and_reconciled_events_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-await-work-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("await-work route test should not call the provider")
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::from_token_limits("test-model", Default::default(), 4096, None).expect("model spec");
        let event_tx = SessionEventRegistry::new(16);
        // The app sink, wired exactly as bootstrap wires it — through the
        // driver's watched decorator, feeding an mpsc channel.
        let (sink_tx, mut sink_rx) = mpsc::channel::<lash::process::ProcessEvent>(16);
        let driver = lash::process::ProcessWorkDriver::new_with_sink(
            Arc::clone(&process_registry),
            Arc::new(NoopProcessRunHandle),
            Some(Arc::new(ChannelProcessEventSink::new(sink_tx))),
        );
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .process_registry(Arc::clone(&process_registry))
            .build()
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: driver.clone(),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx,
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };

        // Register, append one non-terminal event, and complete — all through
        // the driver's decorated registry handle, the same one the sink watches.
        let watched = driver.process_registry();
        watched
            .register_process(
                lash::process::ProcessRegistration::new(
                    "await-route-proc",
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryDisposition::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "progress".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: Default::default(),
                }]),
            )
            .await
            .expect("register process");
        watched
            .append_event(
                "await-route-proc",
                lash::process::ProcessEventAppendRequest::new("progress", json!({ "step": 1 })),
            )
            .await
            .expect("append progress event");
        watched
            .complete_process(
                "await-route-proc",
                lash::process::ProcessAwaitOutput::Success {
                    value: json!("done"),
                    control: None,
                },
                lash::process::ProcessCompletionAuthority::external_owner("test"),
            )
            .await
            .expect("complete process");

        let Json(result) = await_work(
            AxumPath("await-route-proc".to_string()),
            State(state.clone()),
        )
        .await
        .expect("await work route");

        // Terminal outcome rides the await seam (ADR 0016)...
        assert!(matches!(
            &result.outcome,
            lash::process::ProcessAwaitOutput::Success { value, .. } if value == &json!("done")
        ));
        // ...and the event log reconciled from the durable store is complete.
        assert!(
            result
                .events
                .iter()
                .any(|event| event.event_type == "progress"),
            "reconciled events missing the appended progress event: {:?}",
            result.events
        );

        // The sink saw both appends (best-effort freshness)...
        let mut sunk = Vec::new();
        while let Ok(event) = sink_rx.try_recv() {
            sunk.push(event);
        }
        assert!(
            sunk.iter().any(|event| event.event_type == "progress"),
            "sink missed the non-terminal append: {sunk:?}"
        );
        assert!(
            sunk.iter()
                .any(|event| event.event_type == "process.completed"),
            "sink missed the terminal append: {sunk:?}"
        );

        watched
            .register_process(lash::process::ProcessRegistration::new(
                "failed-work-rail-proc",
                lash::process::ProcessInput::External {
                    metadata: Value::Null,
                },
                lash::process::RecoveryDisposition::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            ))
            .await
            .expect("register failed process");
        watched
            .complete_process(
                "failed-work-rail-proc",
                lash::process::ProcessAwaitOutput::Failure {
                    class: lash::tools::ToolFailureClass::External,
                    code: "deterministic_failure".to_string(),
                    message: "deterministic durable process failure".to_string(),
                    raw: None,
                    control: None,
                },
                lash::process::ProcessCompletionAuthority::external_owner("test"),
            )
            .await
            .expect("fail process");
        let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list failed work");
        let failed = work
            .iter()
            .find(|item| item.process.process_id == "failed-work-rail-proc")
            .expect("failed process in work API");
        assert_eq!(failed.process.status_label, "failed");
        assert!(failed.process.terminal);
        assert_eq!(
            failed.process.error.as_deref(),
            Some("deterministic durable process failure")
        );

        // An unknown process id errors instead of hanging.
        let missing = await_work(AxumPath("no-such-process".to_string()), State(state)).await;
        assert!(missing.is_err(), "unknown process id must error");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn work_api_keeps_orphaned_process_visible_and_routes_cancel_globally() {
        run_async_test_on_stack_budget("workbench-orphaned-process-api-test", || {
            work_api_keeps_orphaned_process_visible_and_routes_cancel_globally_inner()
        });
    }

    async fn work_api_keeps_orphaned_process_visible_and_routes_cancel_globally_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-orphaned-process-api-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("orphaned process API test should not call the provider")
            .build()
            .into_handle();
        let model = lash::ModelSpec::from_token_limits(
            "test-model",
            Default::default(),
            4096,
            None,
        )
        .expect("model spec");
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(core_store_factory)
            .process_registry(Arc::clone(&process_registry))
            .build()
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(16),
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url,
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };
        let session_id = state.current_session_id();
        let process_id = "process-survives-session-delete";
        process_registry
            .register_process(lash::process::ProcessRegistration::new(
                process_id,
                lash::process::ProcessInput::External {
                    metadata: json!({ "test": true }),
                },
                lash::process::RecoveryDisposition::ExternallyOwned,
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
                    &session_id,
                )),
            ))
            .await
            .expect("register process");
        process_registry
            .grant_handle(
                &lash::process::SessionScope::new(&session_id),
                process_id,
                lash::process::ProcessHandleDescriptor::new(
                    Some("test"),
                    Some("Session-independent process"),
                ),
            )
            .await
            .expect("grant process handle");
        let deletion = process_registry
            .delete_session_process_state(&session_id)
            .await
            .expect("delete session process edges");
        assert_eq!(deletion.orphaned_process_ids, vec![process_id]);
        let (_, current_session_id) = state.session_ids.rotate();
        assert_ne!(current_session_id, session_id);

        let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list runtime-wide work");
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].process.process_id, process_id);

        let Json(receipt) = cancel_work(
            AxumPath(process_id.to_string()),
            State(state.clone()),
        )
        .await
        .expect("submit process cancellation");
        assert!(receipt.accepted);
        assert_eq!(receipt.process_id, process_id);
        let request = tokio::time::timeout(Duration::from_secs(2), restate_requests.recv())
            .await
            .expect("Restate request timeout")
            .expect("Restate request");
        assert!(
            request
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.starts_with("WorkbenchProcessCancelWorkflow/")),
            "unexpected Restate request: {request:#}"
        );
        assert_eq!(
            request.pointer("/body/process_id").and_then(Value::as_str),
            Some(process_id)
        );
        assert_eq!(
            request.pointer("/body/session_id").and_then(Value::as_str),
            Some(session_id.as_str()),
            "process cancellation must retain its originating trace session"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
