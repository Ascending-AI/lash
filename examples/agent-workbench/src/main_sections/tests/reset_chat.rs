use super::*;

pub(super) async fn reset_chat_deletes_old_session_and_clears_trigger_started_work_inner() {
    let data_dir =
        std::env::temp_dir().join(format!("agent-workbench-reset-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let double = crate::tests::test_double_backend(0).await;
    let session_store_factory: Arc<dyn lash::persistence::DeploymentStore> =
        double.stores().session_store_factory();
    let core_store_factory: Arc<dyn lash::persistence::DeploymentStore> = session_store_factory;
    let process_registry = double.engine_stores().process_registry();
    let provider = trigger_registration_provider();
    let model = test_llm_profile();
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    let core = explicit_durable_test_facets_on(double.lash_backend())
        .serve_workbench_llm_profile(provider, model)
        .plugin(Arc::new(WorkbenchPluginFactory::new()))
        .build(crate::test_core_owner())
        .expect("build core");
    crate::tests::install_test_process_worker(&double, &core);
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        session_defaults: crate::tests::test_session_defaults(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store: detached_trigger_store(),
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
            client_nonce: None,
        }])),
        selected_llm_profile: Arc::new(Mutex::new(LlmProfileSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(1024),
        restate_ingress_url,
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };
    state
        .ensure_current_session()
        .await
        .expect("create the workbench's current session");
    let old_session_id = state.current_session_id();
    let _deleted_session_events = state.event_tx.subscribe(&old_session_id);
    assert!(state.event_tx.contains(&old_session_id));
    let session = crate::created_session(&state.core, old_session_id.clone())
        .await
        .open()
        .await
        .expect("open old session");
    register_test_trigger(&session).await;
    let started = emit_test_button_trigger(&double, &state.core, ButtonChoice::Red).await;
    assert_remote_trigger_emit_report_round_trip(&started);
    let trigger_records = assert_remote_trigger_subscription_records_round_trip(
        double.stores().trigger_store().as_ref(),
        &old_session_id,
    )
    .await;
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
            .collect::<Vec<_>>(),
        started.started_process_ids()
    );
    append_started_graph(
        &state.lashlang_execution,
        &test_graph(
            &format!("process:{}", ProcessId::fixture("old-reset-process")),
            &old_session_id,
            TraceRuntimeSubject::Process {
                process_id: ProcessId::fixture("old-reset-process"),
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
    assert!(snapshot.transcript.is_empty());
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
            .collect::<Vec<_>>(),
        started.started_process_ids(),
        "mock Restate ingress must not consume deletion work inline"
    );
    assert!(
        crate::created_session(&state.core, snapshot.state.settings.session_id)
            .await
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
        state
            .core
            .processes()
            .await_output(&started.started_process_ids()[0])
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
        let lookup = core_store_factory
            .lookup_session(&old_session_id)
            .await
            .expect("look up old session");
        assert!(matches!(lookup, lash::persistence::SessionLookup::Live(_)));
        let store = Arc::clone(&core_store_factory);
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

// FIG-3136: reset on a busy session must always leave the page on a live
// session. The roster rotation used to be a continuation of the browser's
// request, so a request that went away — or whose Restate attach result was
// lost — stopped between the durable delete and the rotation and left the
// roster's current on a tombstone every surface refuses.

use super::recoverable_chat_tests::recoverable_chat_test_state;

/// Enough terminal processes that the durable delete of this session is real
/// work: the reproduction carried 460 of them and 1843 events.
const BUSY_SESSION_PROCESS_COUNT: usize = 300;

async fn register_terminal_processes(
    double: &lash_restate_test::RestateTestBackend,
    session_id: &SessionId,
    count: usize,
) {
    // The registry the workbench state's engine runs over, so these rows are
    // the session's own work rather than a second registry's.
    let registry = double.engine_stores().process_registry();
    for _ in 0..count {
        let process_id = registry
            .register_process(lash::process::ProcessRegistration::new(
                lash::process::ProcessInput::External {
                    metadata: Value::Null,
                },
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
                    session_id.clone(),
                )),
                lash::process::Lifetime::Detached,
            ))
            .await
            .expect("register process")
            .id;
        registry
            .complete_process(
                &process_id,
                lash::process::ProcessAwaitOutput::from_tool_output(
                    lash::tools::ToolCallOutput::success(json!("done")),
                ),
                lash::process::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete process");
    }
}

fn live_cron_job_keys(state: &AppState, session_id: &SessionId) {
    state.restate_cron_job_keys.lock_recover().insert(
        session_id.clone(),
        [
            "workbench-cron-5s".to_string(),
            "workbench-cron-30s".to_string(),
        ]
        .into_iter()
        .collect(),
    );
}

fn captured_restate_paths(requests: &mut mpsc::UnboundedReceiver<Value>) -> Vec<String> {
    let mut paths = Vec::new();
    while let Ok(request) = requests.try_recv() {
        if let Some(path) = request.get("path").and_then(Value::as_str) {
            paths.push(path.to_string());
        }
    }
    paths
}

#[test]
fn resetting_a_busy_session_hands_the_page_a_replacement_session() {
    run_async_test_on_stack_budget("workbench-reset-busy-session", || async {
        let double = crate::tests::test_double_backend(0).await;
        let mut state = recoverable_chat_test_state(&double, 16).await;
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        state.restate_ingress_url = restate_ingress_url;
        let old_session_id = state.current_session_id();
        register_terminal_processes(&double, &old_session_id, BUSY_SESSION_PROCESS_COUNT).await;
        live_cron_job_keys(&state, &old_session_id);

        let Json(snapshot) = Box::pin(reset_chat(
            State(state.clone()),
            Query(SessionQuery {
                session_id: Some(old_session_id.clone()),
            }),
        ))
        .await
        .expect("a busy session's reset must hand back a replacement session");

        assert_ne!(snapshot.settings.session_id, old_session_id);
        assert_eq!(state.sessions.current(), snapshot.settings.session_id);
        assert_eq!(
            state.active_turns.retirement(&old_session_id),
            Some(SessionRetirement::Retired)
        );
        let paths = captured_restate_paths(&mut restate_requests);
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.starts_with("WorkbenchCronJob/") && path.ends_with("/cancel"))
                .count(),
            2,
            "both live cron jobs must be cancelled before the delete: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|path| path.starts_with("WorkbenchSessionDeleteWorkflow/")),
            "the durable delete must be submitted: {paths:?}"
        );
    });
}

#[test]
fn a_reset_of_an_already_retired_session_hands_back_its_replacement() {
    run_async_test_on_stack_budget("workbench-reset-already-retired", || async {
        let double = crate::tests::test_double_backend(0).await;
        let mut state = recoverable_chat_test_state(&double, 16).await;
        let (restate_ingress_url, _restate_requests) = spawn_restate_ingress_capture().await;
        state.restate_ingress_url = restate_ingress_url;
        let old_session_id = state.current_session_id();
        let query = || {
            Query(SessionQuery {
                session_id: Some(old_session_id.clone()),
            })
        };

        let Json(first) = Box::pin(reset_chat(State(state.clone()), query()))
            .await
            .expect("the first reset retires the session");
        assert_eq!(
            state.active_turns.retirement(&old_session_id),
            Some(SessionRetirement::Retired)
        );

        // The page never saw that answer — the response was lost — so it asks
        // again with the only id it has. The fence refuses a retired id for
        // every use including a delete, which made this the dead end: the
        // repair for a tombstoned session was a reset, and reset was the one
        // thing that could not run.
        let Json(second) = Box::pin(reset_chat(State(state.clone()), query()))
            .await
            .expect("a reset of an already retired session must not dead-end");

        assert_eq!(second.settings.session_id, first.settings.session_id);
        assert_eq!(state.sessions.current(), first.settings.session_id);
    });
}

#[test]
fn a_reset_whose_request_goes_away_still_takes_the_roster_off_the_tombstone() {
    run_async_test_on_stack_budget_multi_thread("workbench-reset-abandoned-request", 2, || {
        a_reset_whose_request_goes_away_still_takes_the_roster_off_the_tombstone_inner()
    });
}

async fn a_reset_whose_request_goes_away_still_takes_the_roster_off_the_tombstone_inner() {
    let double = crate::tests::test_double_backend(0).await;
    let mut state = recoverable_chat_test_state(&double, 16).await;
    let (restate_ingress_url, mut restate_requests, delete_gate) =
        spawn_restate_ingress_capture_with_delete_gate().await;
    state.restate_ingress_url = restate_ingress_url;
    let old_session_id = state.current_session_id();
    live_cron_job_keys(&state, &old_session_id);

    let reset = tokio::spawn({
        let state = state.clone();
        let old_session_id = old_session_id.clone();
        async move {
            Box::pin(reset_chat(
                State(state),
                Query(SessionQuery {
                    session_id: Some(old_session_id),
                }),
            ))
            .await
            .map(|Json(snapshot)| snapshot.state.settings.session_id)
        }
    });

    let attached = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let request = restate_requests
                .recv()
                .await
                .expect("mock Restate ingress request");
            let path = request
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if path.starts_with("WorkbenchSessionDeleteWorkflow/") {
                return path;
            }
        }
    })
    .await
    .expect("the delete workflow is attached");
    assert!(attached.ends_with("/run"), "unexpected attach: {attached}");

    // The browser goes away: a reload, a closed tab, an abandoned fetch. The
    // durable delete does not care, and neither may the rotation.
    reset.abort();
    delete_gate.notify_one();

    tokio::time::timeout(Duration::from_secs(10), async {
        while state.sessions.current() == old_session_id {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a delete that completed must take the roster off the tombstone");

    assert_ne!(state.sessions.current(), old_session_id);
    assert_eq!(
        state.active_turns.retirement(&old_session_id),
        Some(SessionRetirement::Retired)
    );
}

/// FIG-5022: every reset response is a settled state the page can render.
#[test]
fn reset_response_carries_the_replacement_sessions_durable_transcript() {
    run_async_test_on_stack_budget("workbench-reset-transcript", || async {
        let double = crate::tests::test_double_backend(0).await;
        let mut state = recoverable_chat_test_state(&double, 16).await;
        let (restate_ingress_url, _requests) = spawn_restate_ingress_capture().await;
        state.restate_ingress_url = restate_ingress_url;
        let app = Router::new()
            .route("/api/reset", post(reset_chat))
            .route("/api/session", delete(reset_chat))
            .route("/api/state", get(app_state))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        for (method, path) in [
            (reqwest::Method::POST, "/api/reset"),
            (reqwest::Method::DELETE, "/api/session"),
        ] {
            let old_session_id = state.current_session_id();
            let response = client
                .request(method, format!("http://{address}{path}"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            assert_eq!(
                response.get("transcript"),
                Some(&json!([])),
                "{path} must carry the fresh session's durable transcript"
            );
            assert_ne!(response["settings"]["session_id"], json!(old_session_id));
            let settled = client
                .get(format!("http://{address}/api/state"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            assert_eq!(
                response, settled,
                "reset and GET must project the same state"
            );

            // Exercise the page's actual timeline with the HTTP response,
            // including the iteration that threw on the missing transcript.
            let node =
                std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
            let output = std::process::Command::new(node)
                .arg("-e")
                .arg(format!(
                    "{}\nconst host = {{ firstChild: null, insertBefore() {{}}, replaceChildren() {{}} }};\n\
                     createWorkbenchTimeline({{ list: host, footer: host, empty: {{}} }}).applySnapshot({response});",
                    ui::TIMELINE_JS
                ))
                .output()
                .expect("Node.js is required for the reset renderer regression");
            assert!(
                output.status.success(),
                "reset response failed to render: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        server.abort();
        let _ = server.await;
    });
}

/// FIG-5036: deleting a chat from the sidebar retires that one session, takes
/// its row off the roster, and hands the page the most recent chat left. The
/// other chats keep their rows and their state.
#[test]
fn deleting_a_chat_removes_only_its_row_and_hands_back_the_most_recent_one() {
    run_async_test_on_stack_budget("workbench-delete-chat", || async {
        let double = crate::tests::test_double_backend(0).await;
        let mut state = recoverable_chat_test_state(&double, 16).await;
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        state.restate_ingress_url = restate_ingress_url;
        let first = state.current_session_id();
        state.sessions.ensure(&first);
        let create = |name: &str| {
            create_session(
                State(state.clone()),
                Json(SessionCreateRequest {
                    name: Some(name.to_string()),
                }),
            )
        };
        let Json(kept) = create("kept").await.expect("create the kept chat");
        let Json(doomed) = create("doomed").await.expect("create the doomed chat");
        tokio::time::sleep(Duration::from_millis(5)).await;
        state
            .sessions
            .record_prompt(&kept.session_id, "the latest prompt");
        state
            .sessions
            .select(&doomed.session_id)
            .expect("select the doomed chat");

        let Json(deleted) = Box::pin(delete_session(
            AxumPath(doomed.session_id.to_string()),
            State(state.clone()),
        ))
        .await
        .expect("delete the open chat");

        assert_eq!(deleted.session_id, doomed.session_id);
        assert_eq!(deleted.successor_session_id, kept.session_id);
        assert_eq!(state.sessions.current(), kept.session_id);
        let listed = state
            .sessions
            .list()
            .into_iter()
            .map(|entry| entry.session_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            listed,
            BTreeSet::from([first.clone(), kept.session_id.clone()]),
            "only the deleted chat leaves the roster, and no replacement joins it"
        );
        assert_eq!(
            state.active_turns.retirement(&doomed.session_id),
            Some(SessionRetirement::Retired)
        );
        assert!(
            Box::pin(app_state(
                State(state.clone()),
                Query(SessionQuery {
                    session_id: Some(doomed.session_id.clone()),
                }),
            ))
            .await
            .is_err(),
            "the deleted chat's state is gone"
        );
        for survivor in [&first, &kept.session_id] {
            assert_eq!(state.active_turns.retirement(survivor), None);
            let Json(snapshot) = Box::pin(app_state(
                State(state.clone()),
                Query(SessionQuery {
                    session_id: Some(survivor.clone()),
                }),
            ))
            .await
            .expect("an untouched chat still reads");
            assert_eq!(&snapshot.state.settings.session_id, survivor);
        }
        let paths = captured_restate_paths(&mut restate_requests);
        assert!(
            paths
                .iter()
                .any(|path| path.starts_with("WorkbenchSessionDeleteWorkflow/")),
            "the durable delete must be submitted: {paths:?}"
        );
    });
}
