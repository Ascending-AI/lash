#[cfg(test)]
mod turn_control_timeout_tests {
    use super::tests::{
        explicit_durable_test_facets, in_memory_trigger_store, run_async_test_on_stack_budget,
        spawn_restate_ingress_capture, text_response,
    };
    use super::*;

    #[test]
    fn turn_input_route_records_exact_active_and_next_turn_ingress() {
        run_async_test_on_stack_budget("workbench-turn-input-route-test", || {
            turn_input_route_records_exact_active_and_next_turn_ingress_inner()
        });
    }

    async fn turn_input_route_records_exact_active_and_next_turn_ingress_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-turn-input-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(
                &data_dir.join("processes.db"),
                data_dir.join("lash-sessions"),
            )
            .await
            .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("turn input route test should not call the provider")
            .build()
            .into_handle();
        let model = lash::ModelSpec::builder("test-model")
            .context_window_tokens(4096)
            .build()
            .expect("model spec");
        let event_tx = SessionEventRegistry::new(16);
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&store_factory))
            .process_registry(Arc::clone(&process_registry))
            .without_queued_work()
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
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx,
            queued_work_driver: inert_queued_work(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::persistent(data_dir.join("active-turns.json"))
                .expect("open active turns"),
            authorization: WorkbenchAuthorization::allow_all(),
            approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
        };
        let session_id = state.current_session_id();

        let no_active = enqueue_turn_input(
            State(state.clone()),
            Query(SessionQuery::default()),
            Json(TurnInputRequest {
                text: "too early".to_string(),
                ingress: TurnInputIngressRequest::ActiveTurn,
            }),
        )
        .await
        .expect_err("active-turn ingress without a running turn must fail");
        assert_eq!(no_active.status, StatusCode::CONFLICT);

        state.track_turn_prompt(
            &session_id,
            "running-turn",
            "restored active prompt".to_string(),
            None,
        );
        let Json(injected) = enqueue_turn_input(
            State(state.clone()),
            Query(SessionQuery::default()),
            Json(TurnInputRequest {
                text: "inject exactly once".to_string(),
                ingress: TurnInputIngressRequest::ActiveTurn,
            }),
        )
        .await
        .expect("enqueue active-turn input");
        assert!(matches!(
            injected.ingress,
            lash::persistence::TurnInputIngress::ActiveTurn {
                ref turn_id,
                min_boundary: lash::persistence::TurnInputCheckpointBoundary::AfterWork,
            } if turn_id.as_str() == "running-turn"
        ));
        assert_eq!(
            injected.state,
            lash::persistence::TurnInputState::PendingActive
        );

        let Json(queued) = enqueue_turn_input(
            State(state.clone()),
            Query(SessionQuery::default()),
            Json(TurnInputRequest {
                text: "run after settle".to_string(),
                ingress: TurnInputIngressRequest::NextTurn,
            }),
        )
        .await
        .expect("enqueue next-turn input");
        assert!(matches!(
            queued.ingress,
            lash::persistence::TurnInputIngress::NextTurn
        ));
        assert_eq!(
            queued.state,
            lash::persistence::TurnInputState::DeferredNextTurn
        );

        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open session for pending input evidence");
        let pending = session
            .pending_turn_inputs()
            .await
            .expect("list pending inputs");
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].input_id, injected.input_id);
        assert_eq!(pending[1].input_id, queued.input_id);
        session.close().await.expect("close session");

        let Json(snapshot) = Box::pin(app_state(
            State(state.clone()),
            Query(SessionQuery::default()),
        ))
        .await
        .expect("load state snapshot");
        assert!(snapshot.messages.iter().any(|message| {
            message.id == workbench_turn_user_message_id("running-turn")
                && message.role == "user"
                && message.text == "restored active prompt"
        }));
        assert_eq!(snapshot.pending_turn_inputs.len(), 2);
        assert_eq!(snapshot.pending_turn_inputs[0].input_id, injected.input_id);
        assert_eq!(snapshot.pending_turn_inputs[1].input_id, queued.input_id);

        crate::restate::settle_workbench_turn(&state, &session_id, "running-turn")
            .await
            .expect("settle running turn");
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open session after turn settle");
        let after_settle = session
            .pending_turn_inputs()
            .await
            .expect("list pending inputs after turn settle");
        assert_eq!(after_settle.len(), 1);
        assert_eq!(after_settle[0].input_id, queued.input_id);
        session.close().await.expect("close session after settle");

        state.track_turn(&session_id, "settle-race-turn");
        let checked_ingress = lash::persistence::TurnInputIngress::active_turn(
            "settle-race-turn",
            lash::persistence::TurnInputCheckpointBoundary::AfterWork,
        );
        crate::restate::settle_workbench_turn(&state, &session_id, "settle-race-turn")
            .await
            .expect("settle turn between route check and enqueue");
        let raced = state
            .core
            .enqueue_turn_input(
                session_id.clone(),
                lash::TurnInput::text("must not be stranded"),
                checked_ingress,
                Some("settle-race-input".to_string()),
            )
            .await
            .expect("enqueue after the checked turn settled");
        let race_error = reject_if_active_turn_settled(&state, &raced)
            .await
            .expect_err("settled active-turn input must be rejected");
        assert_eq!(race_error.status, StatusCode::CONFLICT);
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open session after settle race");
        let after_race = session
            .pending_turn_inputs()
            .await
            .expect("list pending inputs after settle race");
        assert_eq!(after_race.len(), 1);
        assert_eq!(after_race[0].input_id, queued.input_id);
        session
            .close()
            .await
            .expect("close session after settle race");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    async fn spawn_restate_admin_with_workflow_status(status: Option<&str>) -> String {
        async fn query_status(
            State(status): State<Option<String>>,
            Json(_query): Json<Value>,
        ) -> Json<Value> {
            let rows = status
                .map(|status| {
                    vec![json!({
                        "id": "inv_test_turn",
                        "target": "workflow/WorkbenchTurnWorkflow/test/run",
                        "target_service_name": "WorkbenchTurnWorkflow",
                        "target_service_key": "test",
                        "target_handler_name": "run",
                        "status": status,
                    })]
                })
                .unwrap_or_default();
            Json(json!({ "rows": rows }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock Restate admin");
        let addr = listener.local_addr().expect("mock Restate admin addr");
        let app = Router::new()
            .route("/query", post(query_status))
            .with_state(status.map(str::to_string));
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                eprintln!("mock Restate admin stopped: {err}");
            }
        });
        format!("http://{addr}")
    }

    async fn turn_cancel_test_state(data_dir: &std::path::Path, admin_url: String) -> AppState {
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(
                &data_dir.join("processes.db"),
                data_dir.join("lash-sessions"),
            )
            .await
            .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("turn cancellation routing test should not call the provider")
            .build()
            .into_handle();
        let model = lash::ModelSpec::builder("test-model")
            .context_window_tokens(4096)
            .build()
            .expect("model spec");
        let event_tx = SessionEventRegistry::new(16);
        let core = explicit_durable_test_facets(data_dir)
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
        AppState {
            core,
            rlm_dialect: lash::rlm::RlmDialect::Lashlang,
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
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx,
            queued_work_driver: inert_queued_work(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: admin_url,
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::persistent(data_dir.join("active-turns.json"))
                .expect("open active turns"),
            authorization: WorkbenchAuthorization::allow_all(),
            approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
        }
    }

    #[test]
    fn dangling_routed_turn_does_not_hang_stop_and_is_pruned() {
        run_async_test_on_stack_budget("workbench-dangling-turn-cancel-test", || {
            dangling_routed_turn_does_not_hang_stop_and_is_pruned_inner()
        });
    }

    async fn dangling_routed_turn_does_not_hang_stop_and_is_pruned_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-dangling-turn-cancel-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let admin_url = spawn_restate_admin_with_workflow_status(None).await;
        let state = turn_cancel_test_state(&data_dir, admin_url).await;
        let session_id = state.current_session_id();
        let mut events = state.event_tx.subscribe(&session_id);
        state.track_turn(&session_id, "dangling-turn");

        let receipts = tokio::time::timeout(
            Duration::from_secs(1),
            state.cancel_turns_for_session(&session_id),
        )
        .await
        .expect("Stop must not hang on a dangling routed turn")
        .expect("cancel dangling turn");

        assert!(matches!(
            receipts.as_slice(),
            [TurnCancelReceipt::CancellationRecordedTerminalPending {
                cancellation: RecordedTurnCancellation::Requested(_),
                ..
            }]
        ));
        assert!(state.active_turns.for_session(&session_id).is_empty());
        let done = events.try_recv().expect("receive break-glass Done event");
        assert_eq!(
            serde_json::to_value(done.item).expect("serialize break-glass Done item"),
            json!({
                "type": "done",
                "turn_id": "dangling-turn",
                "outcome": "failed",
            })
        );
        assert!(ui::INDEX_HTML.contains("cancellation_recorded_terminal_pending"));
        assert!(ui::INDEX_HTML.contains("turn route cleared · terminal outcome unknown"));
        let recovered = ActiveTurns::persistent(data_dir.join("active-turns.json"))
            .expect("reopen active turns");
        assert!(recovered.for_session(&session_id).is_empty());
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn live_restate_turn_timeout_retains_routing_as_pending() {
        run_async_test_on_stack_budget("workbench-live-turn-cancel-timeout-test", || {
            live_restate_turn_timeout_retains_routing_as_pending_inner()
        });
    }

    async fn live_restate_turn_timeout_retains_routing_as_pending_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-live-turn-cancel-timeout-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let admin_url = spawn_restate_admin_with_workflow_status(Some("suspended")).await;
        let state = turn_cancel_test_state(&data_dir, admin_url).await;
        let session_id = state.current_session_id();
        let mut events = state.event_tx.subscribe(&session_id);
        state.track_turn(&session_id, "live-turn");

        let response = tokio::time::timeout(
            Duration::from_secs(1),
            cancel_turn(
                State(state.clone()),
                Query(SessionQuery {
                    session_id: Some(session_id.clone()),
                }),
            ),
        )
        .await
        .expect("Stop must return after the bounded terminal attachment")
        .expect("cancel live turn")
        .into_response();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("read pending Stop receipt");
        let body = serde_json::from_slice::<Value>(&body).expect("decode pending Stop receipt");
        let cancellation = &body["cancellations"][0];
        assert_eq!(
            cancellation["status"],
            "cancellation_recorded_terminal_pending"
        );
        assert_eq!(cancellation["address"]["session_id"], session_id);
        assert_eq!(cancellation["address"]["turn_id"], "live-turn");
        assert_eq!(cancellation["cancellation"]["outcome"], "requested");
        assert!(
            cancellation["cancellation"]["cancellation"]["request_id"]
                .as_str()
                .is_some_and(|request_id| request_id.starts_with("workbench-stop-"))
        );
        assert_eq!(
            cancellation["cancellation"]["cancellation"]["origin"],
            "user"
        );
        assert_eq!(
            cancellation["cancellation"]["cancellation"]["reason"],
            "workbench Stop control"
        );
        assert!(
            cancellation["cancellation"]["cancellation"]
                .get("undelivered")
                .is_none()
        );
        assert!(cancellation.get("terminal").is_none());
        assert!(cancellation.get("terminal_error").is_none());
        assert_eq!(
            state.active_turns.for_session(&session_id),
            vec![lash::TurnAddress::new(&session_id, "live-turn")],
            "an active Restate invocation remains routable while cancellation is pending"
        );
        let recovered = ActiveTurns::persistent(data_dir.join("active-turns.json"))
            .expect("reopen active turns");
        assert_eq!(
            recovered.for_session(&session_id),
            vec![lash::TurnAddress::new(session_id, "live-turn")]
        );
        assert!(
            events.try_recv().is_err(),
            "a still-active turn must not receive a terminal Done item"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn stop_over_real_process_await_commits_cancelled_terminal() {
        run_async_test_on_stack_budget("workbench-stop-over-process-await-test", || {
            stop_over_real_process_await_commits_cancelled_terminal_inner()
        });
    }

    async fn stop_over_real_process_await_commits_cancelled_terminal_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-stop-over-process-await-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create Stop-over-process data dir");
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
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-stop-over-process-await")
            .complete(|_| async {
                Ok(text_response(
                    r#"<lashlang>
process hold_for_stop() {
  sleep for "10m"
  finish "unreachable"
}
handle = start hold_for_stop()
finish (await handle)?
</lashlang>"#,
                ))
            })
            .build()
            .into_handle();
        let model = lash::ModelSpec::builder("test-model")
            .context_window_tokens(4096)
            .build()
            .expect("model spec");
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&store_factory))
            .process_registry(Arc::clone(&process_registry))
            .build(crate::test_core_owner())
            .expect("build Stop-over-process core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        let state = AppState {
            core,
            rlm_dialect: lash::rlm::RlmDialect::Lashlang,
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
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(16),
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
        let session_id = state.current_session_id();
        let turn_text = "start and await the held process";
        let Json(accepted) = send_turn(
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
        .expect("send process-await turn through the production handler");
        assert!(accepted.accepted);
        let submitted = restate_requests
            .recv()
            .await
            .expect("capture submitted process-await Restate turn");
        let turn_id = submitted
            .pointer("/body/turn_id")
            .and_then(Value::as_str)
            .expect("submitted process-await turn id")
            .to_string();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open submitted Stop-over-process session");
        let run_turn_id = turn_id.clone();
        let turn = tokio::spawn(async move {
            session
                .turn(lash::TurnInput::text(turn_text))
                .turn_id(run_turn_id)
                .run()
                .await
        });

        let process_id = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let live = process_registry
                    .list_non_terminal_page(
                        std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                        None,
                    )
                    .await
                    .expect("list live process while turn awaits")
                    .records;
                if let [process] = live.as_slice() {
                    break process.id.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("turn must reach a real process await");

        let (status, Json(receipt)) = cancel_turn(
            State(state.clone()),
            Query(SessionQuery {
                session_id: Some(session_id.clone()),
            }),
        )
        .await
        .expect("production Stop handler must cancel the process-await turn");
        assert_eq!(status, StatusCode::OK);
        let turn = turn
            .await
            .expect("Stop-over-process turn joins")
            .expect("Stop-over-process turn commits");

        assert!(receipt.accepted);
        assert!(matches!(
            receipt.cancellations.as_slice(),
            [TurnCancelReceipt::TerminalAttached {
                terminal: lash::TurnTerminal::Committed {
                    outcome: lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { .. }),
                    ..
                },
                ..
            }]
        ));
        assert!(matches!(
            turn.result.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { .. })
        ));
        assert!(matches!(
            process_registry
                .get_process(&process_id)
                .await
                .expect("process record after Stop")
                .expect("Stop keeps the process record present")
                .outcome,
            Some(lash::process::ProcessAwaitOutput::Settled { ref output })
                if !output.is_success()
                    && output.value_for_projection()["source"] == "cancellation"
        ));
        assert!(state.active_turns.for_session(&session_id).is_empty());
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
