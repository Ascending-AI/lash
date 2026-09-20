use super::tests::{
    explicit_durable_test_facets, in_memory_trigger_store, run_async_test_on_stack_budget,
    spawn_restate_ingress_capture, text_response,
};
use super::*;
use lash::TurnId;

struct ExpiringTerminalAttach {
    started: tokio::sync::mpsc::UnboundedSender<lash::TurnAddress>,
    release: tokio::sync::Mutex<tokio::sync::oneshot::Receiver<()>>,
}

#[async_trait::async_trait]
impl lash::TurnAttach for ExpiringTerminalAttach {
    async fn await_terminal(
        &self,
        address: &lash::TurnAddress,
    ) -> Result<lash::TurnTerminal, lash::runtime::RuntimeError> {
        self.started
            .send(address.clone())
            .expect("acknowledge attachment installation");
        (&mut *self.release.lock().await)
            .await
            .expect("explicit attachment completion");
        Err(lash::runtime::RuntimeError::new(
            lash::runtime::RuntimeErrorCode::TurnTerminalAwaitTimeout,
            "mock attachment completed without a terminal",
        ))
    }
}

/// Acknowledges the parent turn entering its `process.await_handle`
/// phase. Stop must land while the turn is parked on the child's await:
/// only that await cancels the child ("turn cancelled while awaiting
/// process"), so a Stop that arrives after `start` registered the child but
/// before the turn reached `await` leaves the child running by contract.
struct ProcessAwaitEntered {
    entered: tokio::sync::Notify,
}

impl lash::runtime::RuntimeTurnPhaseProbe for ProcessAwaitEntered {
    fn begin(&self, _phase: lash::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "process.await_handle" {
            self.entered.notify_one();
        }
    }
}

fn expiring_terminal_driver(
    state: &AppState,
) -> (lash::TurnWorkDriver, impl Future<Output = ()> + use<>) {
    let (started, mut installed) = tokio::sync::mpsc::unbounded_channel();
    let (complete, release) = tokio::sync::oneshot::channel();
    let driver = state
        .core
        .turn_work_driver()
        .expect("workbench core has a session catalog")
        .with_test_attach(Arc::new(ExpiringTerminalAttach {
            started,
            release: tokio::sync::Mutex::new(release),
        }));
    let stores = Arc::clone(&state.session_store_factory);
    (driver, async move {
        let address = installed.recv().await.expect("attachment started");
        let store = stores
            .open_existing_store_by_id(&address.session_id)
            .await
            .expect("read durable cancellation store")
            .expect("cancellation store exists");
        assert!(
            store
                .turn_cancel_request(&address)
                .await
                .expect("read durable cancel acknowledgement")
                .is_some(),
            "durable cancellation must precede terminal attachment"
        );
        complete.send(()).expect("complete mocked attachment");
    })
}

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
        &TurnId::from("running-turn"),
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
        injected.state.kind(),
        lash::persistence::TurnInputStateKind::PendingActive
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
        .durable()
        .pending_turn_inputs()
        .await
        .expect("list pending inputs");
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].input.input_id, injected.input_id);
    assert_eq!(pending[1].input.input_id, queued.input_id);
    session.close().await.expect("close session");

    let Json(snapshot) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("load state snapshot");
    assert!(snapshot.messages.iter().any(|message| {
        message.id == workbench_turn_user_message_id(&TurnId::from("running-turn"))
            && message.role == "user"
            && message.text == "restored active prompt"
    }));
    assert_eq!(snapshot.pending_turn_inputs.len(), 2);
    assert_eq!(
        snapshot.pending_turn_inputs[0].input.input_id,
        injected.input_id
    );
    assert_eq!(
        snapshot.pending_turn_inputs[1].input.input_id,
        queued.input_id
    );

    crate::restate::settle_workbench_turn(&state, &session_id, &TurnId::from("running-turn"))
        .await
        .expect("settle running turn");
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open session after turn settle");
    let after_settle = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("list pending inputs after turn settle");
    assert_eq!(after_settle.len(), 1);
    assert_eq!(after_settle[0].input.input_id, queued.input_id);
    session.close().await.expect("close session after settle");

    state.track_turn(&session_id, &TurnId::from("settle-race-turn"));
    let checked_ingress = lash::persistence::TurnInputIngress::active_turn(
        "settle-race-turn",
        lash::persistence::TurnInputCheckpointBoundary::AfterWork,
    );
    crate::restate::settle_workbench_turn(&state, &session_id, &TurnId::from("settle-race-turn"))
        .await
        .expect("settle turn between route check and enqueue");
    let raced = state
        .core
        .session(session_id.clone())
        .durable()
        .await
        .expect("durable handle for the raced session")
        .enqueue(lash::TurnInput::text("must not be stranded"))
        .ingress(checked_ingress)
        .id("settle-race-input")
        .send()
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
        .durable()
        .pending_turn_inputs()
        .await
        .expect("list pending inputs after settle race");
    assert_eq!(after_race.len(), 1);
    assert_eq!(after_race[0].input.input_id, queued.input_id);
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
    turn_cancel_test_state_with_ingress(data_dir, admin_url, "http://127.0.0.1:8080".to_string())
        .await
}

async fn turn_cancel_test_state_with_ingress(
    data_dir: &std::path::Path,
    admin_url: String,
    restate_ingress_url: String,
) -> AppState {
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
        restate_ingress_url,
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
    let mut state = turn_cancel_test_state(&data_dir, admin_url).await;
    let trace_path = data_dir.join("dangling-cancel.jsonl");
    state.trace_sink = Some(Arc::new(JsonlTraceSink::new(trace_path.clone())));
    let session_id = state.current_session_id();
    let mut events = state.event_tx.subscribe(&session_id);
    state.track_turn(&session_id, &TurnId::from("dangling-turn"));

    let (driver, acknowledge) = expiring_terminal_driver(&state);
    let receipts = tokio::time::timeout(Duration::from_secs(1), async {
        let cancel_session = session_id.clone();
        tokio::join!(
            state.cancel_turns_for_session_with_driver(
                &cancel_session,
                &driver,
                WorkbenchTurnCancelMode::Abort
            ),
            acknowledge
        )
        .0
    })
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
    assert!(state.active_turns.for_session(&session_id).is_none());
    assert!(
        events.try_recv().is_err(),
        "pruning a route is not terminal evidence"
    );
    assert!(ui::INDEX_HTML.contains("cancellation_recorded_terminal_pending"));
    assert!(ui::INDEX_HTML.contains("turn route cleared · terminal outcome unknown"));
    let recovered =
        ActiveTurns::persistent(data_dir.join("active-turns.json")).expect("reopen active turns");
    assert!(recovered.for_session(&session_id).is_none());

    // FIG-3163: the disclosure outlives the DOM node that first showed it. It
    // has to be readable on the state projection, which the timeline re-renders
    // from on every poll, and in the trace, which a later reader reaches
    // without the UI at all.
    let Json(snapshot) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("read the post-abort snapshot");
    let disclosed = snapshot
        .state
        .unknown_turn_terminals
        .iter()
        .map(|record| (record.turn_id.to_string(), record.note.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        disclosed,
        vec![(
            "dangling-turn".to_string(),
            "turn route cleared · terminal outcome unknown".to_string(),
        )],
        "the pruned turn's unknown terminal must ride the projection"
    );
    let note_rows = snapshot
        .transcript
        .iter()
        .filter_map(|row| match row {
            TranscriptRow::Note { turn_id, text, .. } => Some((turn_id.to_string(), text.clone())),
            TranscriptRow::Message { .. }
            | TranscriptRow::Reasoning { .. }
            | TranscriptRow::CodeBlock { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        note_rows,
        vec![(
            "dangling-turn".to_string(),
            "turn route cleared · terminal outcome unknown".to_string(),
        )],
        "the timeline renders the disclosure from the same transcript it renders every other row from"
    );
    assert!(
        ui::INDEX_HTML.contains("if (row.type === \"note\" && !renderedMessages.has(row.id))"),
        "the timeline must render a projected note row"
    );

    let traces = std::fs::read_to_string(&trace_path)
        .expect("read the cancel trace")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("trace record"))
        .filter(|record| record["name"] == "agent_workbench.turn.terminal_unknown")
        .collect::<Vec<_>>();
    assert_eq!(
        traces.len(),
        1,
        "the unknown terminal is recorded exactly once"
    );
    assert_eq!(traces[0]["payload"]["turn_id"], "dangling-turn");
    assert_eq!(
        traces[0]["payload"]["note"],
        "turn route cleared · terminal outcome unknown"
    );
    assert!(
        traces[0]["payload"]["cancellation"]["request_id"].is_string(),
        "the trace carries the cancellation that was recorded in place of a terminal"
    );

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
    state.track_turn(&session_id, &TurnId::from("live-turn"));

    let (driver, acknowledge) = expiring_terminal_driver(&state);
    let response = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(
            cancel_turn_with_driver(
                state.clone(),
                TurnCancelQuery {
                    session: SessionQuery {
                        session_id: Some(session_id.clone())
                    },
                    mode: WorkbenchTurnCancelMode::Abort,
                },
                &driver,
            ),
            acknowledge
        )
        .0
    })
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
    assert_eq!(cancellation["address"]["session_id"], session_id.as_str());
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
        "workbench Abort control"
    );
    assert!(
        cancellation["cancellation"]["cancellation"]
            .get("undelivered")
            .is_none()
    );
    assert!(cancellation.get("terminal").is_none());
    assert!(cancellation.get("terminal_error").is_none());
    assert_eq!(
        state
            .active_turns
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        Some(lash::TurnAddress::new(&session_id, "live-turn")),
        "an active Restate invocation remains routable while cancellation is pending"
    );
    let recovered =
        ActiveTurns::persistent(data_dir.join("active-turns.json")).expect("reopen active turns");
    assert_eq!(
        recovered
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        Some(lash::TurnAddress::new(session_id, "live-turn"))
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
                r#"<typescript>
const hold_for_stop = async () => {
  await sleep(600000);
  return "unreachable";
};
const handle = await processes.start({ definition: hold_for_stop });
finish(await handle);
</typescript>"#,
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
    let await_entered = Arc::new(ProcessAwaitEntered {
        entered: tokio::sync::Notify::new(),
    });
    session
        .set_turn_phase_probe(
            Arc::clone(&await_entered) as Arc<dyn lash::runtime::RuntimeTurnPhaseProbe>
        )
        .await;
    let run_turn_id = turn_id.clone();
    let turn = tokio::spawn(async move {
        session
            .turn(lash::TurnInput::text(turn_text))
            .turn_id(run_turn_id)
            .run()
            .await
    });

    let process_id = tokio::time::timeout(Duration::from_secs(10), async {
        await_entered.entered.notified().await;
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
        Query(TurnCancelQuery {
            session: SessionQuery {
                session_id: Some(session_id.clone()),
            },
            mode: WorkbenchTurnCancelMode::Abort,
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
    assert!(state.active_turns.for_session(&session_id).is_none());
    let _ = std::fs::remove_dir_all(data_dir);
}

/// Hold both receipts at terminal attachment so both HTTP handlers have
/// addressed the same gate before either can remove the active route.
struct ConcurrentCancelTerminal {
    state: AppState,
    attached: tokio::sync::Barrier,
}

#[async_trait::async_trait]
impl lash::TurnAttach for ConcurrentCancelTerminal {
    async fn await_terminal(
        &self,
        address: &lash::TurnAddress,
    ) -> Result<lash::TurnTerminal, lash::runtime::RuntimeError> {
        let leader = self.attached.wait().await.is_leader();
        let store = self
            .state
            .session_store_factory
            .open_existing_store_by_id(&address.session_id)
            .await
            .unwrap()
            .unwrap();
        let request = store
            .turn_cancel_request(address)
            .await
            .unwrap()
            .unwrap()
            .request;
        let evidence = lash::TurnCancellationEvidence {
            request_id: request.request_id,
            origin: request.origin,
            reason: request.reason,
            undelivered: request.undelivered,
            mode: request.mode,
            honoured_after_step: None,
        };
        // Exercise the product terminal publisher used by turn execution,
        // exactly once, while both cancellation handlers are attached.
        if leader {
            self.state
                .publish_turn_done(&address.session_id, &address.turn_id);
        }
        Ok(lash::TurnTerminal::Committed {
            outcome: lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { evidence }),
            session_revision: None,
        })
    }
}

#[test]
fn concurrent_stops_publish_one_done_and_trace_winning_request() {
    run_async_test_on_stack_budget("concurrent-stops", || async {
        let data_dir = tempfile::tempdir().unwrap();
        let mut state = turn_cancel_test_state(data_dir.path(), String::new()).await;
        let trace_path = data_dir.path().join("cancel.jsonl");
        state.trace_sink = Some(Arc::new(JsonlTraceSink::new(trace_path.clone())));
        let session_id = state.current_session_id();
        state.track_turn(&session_id, &TurnId::from("concurrent-stop"));
        let mut events = state.event_tx.subscribe(&session_id);
        let driver = state
            .core
            .turn_work_driver()
            .expect("workbench core has a session catalog")
            .with_test_attach(Arc::new(ConcurrentCancelTerminal {
                state: state.clone(),
                attached: tokio::sync::Barrier::new(2),
            }));
        let cancel = || {
            cancel_turn_with_driver(
                state.clone(),
                TurnCancelQuery {
                    session: SessionQuery {
                        session_id: Some(session_id.clone()),
                    },
                    mode: WorkbenchTurnCancelMode::Abort,
                },
                &driver,
            )
        };
        let (first, second) = tokio::join!(cancel(), cancel());
        let responses = [first.unwrap().1.0, second.unwrap().1.0];
        let cancellations = responses
            .iter()
            .map(|response| match response.cancellations.as_slice() {
                [TurnCancelReceipt::TerminalAttached { cancellation, .. }] => cancellation,
                other => panic!("expected terminal receipt: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cancellations
                .iter()
                .filter(|c| matches!(c, RecordedTurnCancellation::Requested(_)))
                .count(),
            1
        );
        assert_eq!(
            cancellations
                .iter()
                .filter(|c| matches!(c, RecordedTurnCancellation::AlreadyRequested(_)))
                .count(),
            1
        );
        let winner = &cancellations[0].evidence().request_id;
        assert_eq!(winner, &cancellations[1].evidence().request_id);
        let mut done_count = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(event.item, StreamItem::Done { .. }) {
                done_count += 1;
            }
        }
        assert_eq!(
            done_count, 1,
            "concurrent stops must publish one live terminal"
        );
        let traces = std::fs::read_to_string(trace_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["name"] == "agent_workbench.turn.cancel_requested")
            .collect::<Vec<_>>();
        assert_eq!(traces.len(), 2);
        for trace in traces {
            assert_eq!(
                trace["payload"]["request_id"], *winner,
                "both traces must identify the winning request"
            );
        }
    });
}

#[test]
fn turn_cancel_query_maps_stop_and_abort_onto_lash_modes() {
    let parse = |query: &str| {
        let uri: axum::http::Uri = format!("/api/turn/cancel?{query}")
            .parse()
            .expect("cancel route uri");
        axum::extract::Query::<TurnCancelQuery>::try_from_uri(&uri)
            .expect("cancel query parses")
            .0
    };
    let stop = parse("session_id=s-1&mode=stop");
    assert_eq!(stop.session.session_id.as_deref(), Some("s-1"));
    assert_eq!(stop.mode, WorkbenchTurnCancelMode::Stop);
    assert_eq!(stop.mode.lash_mode(), lash::TurnCancelMode::AfterStep);
    let abort = parse("session_id=s-1&mode=abort");
    assert_eq!(abort.mode, WorkbenchTurnCancelMode::Abort);
    assert_eq!(abort.mode.lash_mode(), lash::TurnCancelMode::Immediate);
    let legacy = parse("session_id=s-1");
    assert_eq!(
        legacy.mode,
        WorkbenchTurnCancelMode::Abort,
        "an unqualified Stop control keeps today's immediate abort"
    );
    assert!(ui::INDEX_HTML.contains("id=\"abort\""));
    assert!(ui::INDEX_HTML.contains("stop after step"));
    assert!(ui::INDEX_HTML.contains("\"/api/turn/cancel?mode=\" + mode"));
    assert!(ui::INDEX_HTML.contains("stopTurn(\"stop\")"));
    assert!(ui::INDEX_HTML.contains("stopTurn(\"abort\")"));
    assert!(ui::INDEX_HTML.contains("STOP_ESCALATION_MS"));
    assert!(ui::INDEX_HTML.contains("escalated"));
}

#[test]
fn stop_control_requests_after_step_and_abort_escalates_the_durable_record() {
    run_async_test_on_stack_budget("workbench-stop-mode-test", || {
        stop_control_requests_after_step_and_abort_escalates_the_durable_record_inner()
    });
}

async fn stop_control_requests_after_step_and_abort_escalates_the_durable_record_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-stop-mode-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let admin_url = spawn_restate_admin_with_workflow_status(None).await;
    let state = turn_cancel_test_state(&data_dir, admin_url).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(&session_id)
        .open()
        .await
        .expect("open stop-mode session");

    // Stop before the turn starts: the start gate honours the after-step
    // request; the route forwards the same strength and attaches to the
    // stopped terminal.
    state.track_turn(&session_id, &TurnId::from("stop-mode-turn"));
    let address = session.turn_address("stop-mode-turn");
    let (stopped, turn) = tokio::join!(
        cancel_turn(
            State(state.clone()),
            Query(TurnCancelQuery {
                session: SessionQuery {
                    session_id: Some(session_id.clone()),
                },
                mode: WorkbenchTurnCancelMode::Stop,
            }),
        ),
        async {
            let recorded = await_durable_turn_cancel_request(&state, &address).await;
            assert_eq!(recorded.request.origin.as_deref(), Some("user"));
            assert_eq!(
                recorded.request.reason.as_deref(),
                Some("workbench Stop control")
            );
            assert_eq!(recorded.request.mode, lash::TurnCancelMode::AfterStep);
            session
                .turn(lash::TurnInput::text("stop after the step"))
                .turn_id("stop-mode-turn")
                .run()
                .await
        },
    );
    let (status, Json(stopped)) = stopped.expect("stop route");
    let turn = turn.expect("stopped turn commits");
    assert_eq!(status, StatusCode::OK);
    assert!(stopped.accepted);
    match stopped.cancellations.as_slice() {
        [
            TurnCancelReceipt::TerminalAttached {
                cancellation: RecordedTurnCancellation::Requested(evidence),
                terminal:
                    lash::TurnTerminal::Committed {
                        outcome:
                            lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled {
                                evidence: committed,
                            }),
                        ..
                    },
                ..
            },
        ] => {
            assert_eq!(evidence.origin.as_deref(), Some("user"));
            assert_eq!(evidence.reason.as_deref(), Some("workbench Stop control"));
            assert_eq!(evidence.mode, lash::TurnCancelMode::AfterStep);
            assert_eq!(committed.mode, lash::TurnCancelMode::AfterStep);
            assert_eq!(committed.honoured_after_step, None);
        }
        other => panic!("stop route must attach an after-step cancellation: {other:?}"),
    }
    assert!(matches!(
        turn.result.outcome,
        lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { ref evidence })
            if evidence.mode == lash::TurnCancelMode::AfterStep
    ));

    // Escalate: a routed turn already holding an after-step request is
    // upgraded in place by the Abort control.
    state.track_turn(&session_id, &TurnId::from("escalate-turn"));
    let seeded = state
        .core
        .turn_work_driver()
        .expect("workbench core has a session catalog")
        .request_cancel(
            lash::TurnCancelRequest::new(
                session.turn_address("escalate-turn"),
                "host-stop",
                Some("user".to_string()),
            )
            .mode(lash::TurnCancelMode::AfterStep),
        )
        .await
        .expect("seed the after-step request");
    assert!(matches!(
        seeded.outcome,
        lash::TurnCancelOutcome::Requested(_)
    ));
    let (driver, acknowledge) = expiring_terminal_driver(&state);
    let receipts = tokio::time::timeout(Duration::from_secs(1), async {
        let cancel_session = session_id.clone();
        tokio::join!(
            state.cancel_turns_for_session_with_driver(
                &cancel_session,
                &driver,
                WorkbenchTurnCancelMode::Abort
            ),
            acknowledge
        )
        .0
    })
    .await
    .expect("Abort must not hang on a routed turn")
    .expect("escalate routed turn");
    match receipts.as_slice() {
        [
            TurnCancelReceipt::CancellationRecordedTerminalPending {
                cancellation: RecordedTurnCancellation::Escalated(evidence),
                ..
            },
        ] => {
            assert_eq!(evidence.mode, lash::TurnCancelMode::Immediate);
            assert_eq!(evidence.reason.as_deref(), Some("workbench Abort control"));
        }
        other => panic!("Abort after Stop must report an escalation: {other:?}"),
    }
    let durable = state
        .session_store_factory
        .open_existing_store_by_id(&session_id)
        .await
        .expect("open store")
        .expect("store exists")
        .turn_cancel_request(&session.turn_address("escalate-turn"))
        .await
        .expect("read durable request")
        .expect("durable request recorded");
    assert_eq!(durable.request.mode, lash::TurnCancelMode::AfterStep);
    assert_eq!(durable.request.request_id, "host-stop");
    assert_eq!(durable.request.origin.as_deref(), Some("user"));
    assert_eq!(durable.request.reason, None);
    let _ = std::fs::remove_dir_all(data_dir);
}

#[test]
fn both_cancel_modes_request_cancellation_of_the_turns_awaited_process() {
    run_async_test_on_stack_budget("workbench-turn-cancel-awaited-process-test", || {
        both_cancel_modes_request_cancellation_of_the_turns_awaited_process_inner()
    });
}

/// FIG-3155: an API turn cancellation over a foreground process await used to
/// commit the turn terminal and leave the process running. Only the browser's
/// escalation reached it, and only as a side effect of dropping the in-flight
/// `/api/turn` request; `stop` never reached it at all. Both modes must now
/// request the awaited subject's cancellation from the turn-cancel path
/// itself.
async fn both_cancel_modes_request_cancellation_of_the_turns_awaited_process_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-turn-cancel-awaited-process-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let admin_url = spawn_restate_admin_with_workflow_status(None).await;
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    let state =
        turn_cancel_test_state_with_ingress(&data_dir, admin_url, restate_ingress_url).await;
    let session_id = state.current_session_id();
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &data_dir.join("processes.db"),
            data_dir.join("lash-sessions"),
        )
        .await
        .expect("open the registry the workbench state shares"),
    ) as Arc<dyn lash::process::ProcessRegistry>;

    for (mode, turn_id, process_id) in [
        (WorkbenchTurnCancelMode::Stop, "stop-turn", "stop-awaited"),
        (
            WorkbenchTurnCancelMode::Abort,
            "abort-turn",
            "abort-awaited",
        ),
    ] {
        // A process the turn is the durable parent of — the shape a
        // `processes.start` followed by `await handle` leaves behind, down to
        // the `Abandon` parent-end policy that keeps the parent-end sweep from
        // touching it.
        register_turn_child(&registry, &session_id, turn_id, process_id).await;
        // A second process parented by a different turn proves the cancel is
        // addressed, not a session-wide sweep.
        register_turn_child(&registry, &session_id, "other-turn", "other-awaited").await;
        state.track_turn(&session_id, &TurnId::from(turn_id));

        let (driver, acknowledge) = expiring_terminal_driver(&state);
        let (_status, Json(_receipt)) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                cancel_turn_with_driver(
                    state.clone(),
                    TurnCancelQuery {
                        session: SessionQuery {
                            session_id: Some(session_id.clone())
                        },
                        mode,
                    },
                    &driver,
                ),
                acknowledge
            )
            .0
        })
        .await
        .expect("turn cancellation must return")
        .expect("cancel the turn awaiting a process");

        let request = tokio::time::timeout(Duration::from_secs(5), restate_requests.recv())
            .await
            .expect("a process cancellation must be submitted")
            .expect("Restate request");
        assert!(
            request
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.starts_with("WorkbenchProcessCancelWorkflow/")),
            "{mode:?} must submit a process cancellation: {request:#}"
        );
        assert_eq!(
            request.pointer("/body/process_id").and_then(Value::as_str),
            Some(process_id),
            "{mode:?} must cancel the process its own turn awaited"
        );
        assert_eq!(
            request.pointer("/body/session_id").and_then(Value::as_str),
            Some(session_id.as_str())
        );
        assert!(
            restate_requests.try_recv().is_err(),
            "{mode:?} must not cancel a process another turn parents"
        );
    }
    let _ = std::fs::remove_dir_all(data_dir);
}

async fn register_turn_child(
    registry: &Arc<dyn lash::process::ProcessRegistry>,
    session_id: &str,
    turn_id: &str,
    process_id: &str,
) {
    registry
        .register_process(lash::process::ProcessRegistration::new(
            process_id,
            lash::process::ProcessInput::External {
                metadata: json!({ "awaited": true }),
            },
            lash::process::RecoveryContract::ExternallyOwned,
            lash::process::ProcessProvenance::session(lash::process::SessionScope::new(session_id)),
            lash::process::ProcessLifecyclePolicy::new(
                lash::process::ParentScope::Turn {
                    session_id: lash::SessionId::from(session_id),
                    turn_id: TurnId::from(turn_id),
                },
                lash::process::OnParentEnd::Abandon,
            ),
        ))
        .await
        .expect("register the awaited process");
}

#[test]
fn a_confirmed_tombstone_retires_the_route_a_cancel_had_to_keep() {
    run_async_test_on_stack_budget("workbench-retired-session-route-pruned", || {
        a_confirmed_tombstone_retires_the_route_a_cancel_had_to_keep_inner()
    });
}

/// FIG-3018: a delete whose cancel could not attach a terminal leaves the turn
/// routed on purpose — the turn may still commit its own terminal, so
/// `turn.cancel_liveness_unknown` retains the route rather than invent an
/// outcome. Nothing then removed it: the delete went on to tombstone the
/// session, and `for_session` stayed non-empty for an id every surface refuses.
///
/// Under load that is what made
/// `session_fence_tests::deleting_a_session_with_a_running_turn_cancels_it_before_retiring`
/// fail its `active_turns` assertion 4-7 times in 30 runs: the cancel's
/// terminal attach is bounded, and whether the route survived the delete was
/// decided by whether the box met that bound. The ordering here is driven by
/// the `TurnAttach` seam instead, so the branch is reached every run.
async fn a_confirmed_tombstone_retires_the_route_a_cancel_had_to_keep_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-retired-route-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    // The admin reports the invocation still running, so the cancel's liveness
    // check keeps the route: this is the retained branch, not the pruned one.
    let admin_url = spawn_restate_admin_with_workflow_status(Some("running")).await;
    let state = turn_cancel_test_state(&data_dir, admin_url).await;
    let session_id = state.current_session_id();
    let turn_id = TurnId::from("retained-route-turn");
    state.track_turn_prompt(
        &session_id,
        &turn_id,
        "held by a pending terminal".to_string(),
        None,
    );

    let (driver, acknowledge) = expiring_terminal_driver(&state);
    let receipts = tokio::time::timeout(Duration::from_secs(5), async {
        let cancel_session = session_id.clone();
        tokio::join!(
            state.cancel_turns_for_session_with_driver(
                &cancel_session,
                &driver,
                WorkbenchTurnCancelMode::Abort
            ),
            acknowledge
        )
        .0
    })
    .await
    .expect("the delete's cancel must not hang")
    .expect("cancel the routed turn");
    assert!(
        matches!(
            receipts.as_slice(),
            [TurnCancelReceipt::CancellationRecordedTerminalPending { .. }]
        ),
        "the seam must produce a pending terminal: {receipts:?}"
    );
    // The precondition this regression needs: the cancel legitimately kept the
    // route, so the delete is about to tombstone a session that still routes.
    assert_eq!(
        state
            .active_turns
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        Some(lash::TurnAddress::new(&session_id, &turn_id)),
        "a pending terminal with a live invocation keeps its route"
    );
    assert!(
        state
            .active_turns
            .prompt_for(&session_id, &turn_id)
            .is_some()
    );

    // The delete settles: the durable tombstone is a fact.
    state.settle_retirement_mark(&session_id, &Ok(())).await;

    assert_eq!(
        state.active_turns.retirement(&session_id),
        Some(SessionRetirement::Retired)
    );
    assert!(
        state.active_turns.for_session(&session_id).is_none(),
        "a tombstoned session keeps no routes"
    );
    assert!(
        state
            .active_turns
            .prompt_for(&session_id, &turn_id)
            .is_none(),
        "the route's prompt goes with it"
    );
    // The registry is persisted, so the next boot must not resurrect the route.
    let persisted: Value = serde_json::from_slice(
        &std::fs::read(data_dir.join("active-turns.json")).expect("read persisted active turns"),
    )
    .expect("decode persisted active turns");
    assert_eq!(
        persisted.pointer("/turns").and_then(Value::as_array),
        Some(&Vec::new()),
        "the persisted snapshot drops the retired session's route: {persisted:#}"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// A mock Restate admin that reports one named workflow as running and records
/// every workflow the workbench asked about.
async fn spawn_restate_admin_recording_probes(
    running_workflow: &'static str,
) -> (String, Arc<Mutex<Vec<String>>>) {
    const WORKFLOWS: [&str; 2] = ["WorkbenchTurnWorkflow", "WorkbenchQueuedTurnWorkflow"];

    #[derive(Clone)]
    struct Probes {
        running_workflow: &'static str,
        seen: Arc<Mutex<Vec<String>>>,
    }

    async fn query_status(State(probes): State<Probes>, Json(query): Json<Value>) -> Json<Value> {
        let sql = query
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let asked = WORKFLOWS
            .into_iter()
            .find(|workflow| sql.contains(&format!("target_service_name = '{workflow}'")));
        if let Some(asked) = asked {
            probes.seen.lock_recover().push(asked.to_string());
        }
        let rows = if asked == Some(probes.running_workflow) {
            vec![json!({
                "id": "inv_test_turn",
                "target": format!("workflow/{}/test/run", probes.running_workflow),
                "target_service_name": probes.running_workflow,
                "target_service_key": "test",
                "target_handler_name": "run",
                "status": "running",
            })]
        } else {
            Vec::new()
        };
        Json(json!({ "rows": rows }))
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind probe-recording Restate admin");
    let addr = listener.local_addr().expect("probe-recording admin addr");
    let app = Router::new().route(
        "/query",
        post(query_status).with_state(Probes {
            running_workflow,
            seen: Arc::clone(&seen),
        }),
    );
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("probe-recording Restate admin stopped: {err}");
        }
    });
    (format!("http://{addr}"), seen)
}

#[test]
fn a_queued_turns_cancel_probes_the_queued_workflow_whatever_its_id_looks_like() {
    run_async_test_on_stack_budget("workbench-queued-turn-probe-kind", || {
        a_queued_turns_cancel_probes_the_queued_workflow_whatever_its_id_looks_like_inner()
    });
}

/// FIG-3292: the workflow that owns a turn used to be recovered by sniffing a
/// `workbench-queued-` prefix off the turn id, with every other shape falling
/// through to the user workflow.
///
/// That probe is what decides whether a cancel whose terminal is still pending
/// keeps or drops the turn's routing claim, so asking about the wrong workflow
/// answers "no such invocation" and drops a turn that is still running. The
/// kind now travels with the claim, so the id is free to say anything.
async fn a_queued_turns_cancel_probes_the_queued_workflow_whatever_its_id_looks_like_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-queued-probe-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let (admin_url, probed) =
        spawn_restate_admin_recording_probes("WorkbenchQueuedTurnWorkflow").await;
    let state = turn_cancel_test_state(&data_dir, admin_url).await;
    let session_id = state.current_session_id();
    // No `workbench-queued-` prefix. Only the claim knows what this is.
    let turn_id = TurnId::from("plainly-named-queued-turn");
    state.track_queued_turn(&session_id, &turn_id);

    let (driver, acknowledge) = expiring_terminal_driver(&state);
    let receipts = tokio::time::timeout(Duration::from_secs(5), async {
        let cancel_session = session_id.clone();
        tokio::join!(
            state.cancel_turns_for_session_with_driver(
                &cancel_session,
                &driver,
                WorkbenchTurnCancelMode::Abort
            ),
            acknowledge
        )
        .0
    })
    .await
    .expect("the cancel must not hang")
    .expect("cancel the queued turn");

    assert!(
        matches!(
            receipts.as_slice(),
            [TurnCancelReceipt::CancellationRecordedTerminalPending { .. }]
        ),
        "the seam must leave the terminal pending: {receipts:?}"
    );
    assert_eq!(
        probed.lock_recover().as_slice(),
        ["WorkbenchQueuedTurnWorkflow".to_string()],
        "the liveness probe asks the workflow the claim names"
    );
    assert!(
        state.active_turns.for_session(&session_id).is_some(),
        "a turn the probe found running keeps its routing claim"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}
