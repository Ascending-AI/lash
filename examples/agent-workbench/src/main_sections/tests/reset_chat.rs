use super::*;
use lash::SessionId;

// FIG-3136: reset on a busy session must always leave the page on a live
// session. The roster rotation used to be a continuation of the browser's
// request, so a request that went away stopped between the durable delete and
// the rotation and left the roster's current on a tombstone every surface
// refuses.

/// Enough terminal processes that the durable delete of this session is real
/// work: the reproduction carried 460 of them and 1843 events.
const BUSY_SESSION_PROCESS_COUNT: usize = 300;

/// Register `count` finished processes `session_id` originated, in the
/// registry the workbench's engine runs over.
async fn register_terminal_processes(
    workbench: &Workbench,
    session_id: &SessionId,
    count: usize,
) -> Vec<lash::ProcessId> {
    let registry = workbench.stores.process_registry();
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        let process_id = registry
            .register_process(lash::testing::held_engine_registration(
                Value::Null,
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
                lash::persistence::ProcessCompletionAuthority::workflow_key(&process_id),
            )
            .await
            .expect("complete process");
        ids.push(process_id);
    }
    ids
}

/// The store's own answer: `session_id` is tombstoned.
pub(crate) async fn assert_tombstoned(state: &AppState, session_id: &SessionId) {
    assert!(
        matches!(
            state
                .session_store_factory
                .lookup_session(session_id)
                .await
                .expect("look up the deleted session"),
            lash::persistence::SessionLookup::Deleted
        ),
        "the session's close must have written its tombstone"
    );
}

fn reset_query(session_id: &SessionId) -> Query<SessionQuery> {
    Query(SessionQuery {
        session_id: Some(session_id.clone()),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resetting_a_busy_session_hands_the_page_a_replacement_session() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let old_session_id = state.current_session_id();
    let finished =
        register_terminal_processes(&workbench, &old_session_id, BUSY_SESSION_PROCESS_COUNT).await;

    let Json(snapshot) = Box::pin(reset_chat(
        State(state.clone()),
        reset_query(&old_session_id),
    ))
    .await
    .expect("a busy session's reset must hand back a replacement session");

    assert_ne!(snapshot.settings.session_id, old_session_id);
    assert_eq!(state.sessions.current(), snapshot.settings.session_id);
    assert_eq!(
        state.active_turns.retirement(&old_session_id),
        Some(SessionRetirement::Retired)
    );
    assert_tombstoned(state, &old_session_id).await;
    // The delete's retention half reclaimed the session's finished work
    // (FIG-989).
    for process_id in &finished[..3] {
        assert!(
            matches!(
                workbench
                    .stores
                    .process_registry()
                    .get_process(process_id)
                    .await,
                Err(lash::plugins::PluginError::ProcessNoLongerRetained { .. })
            ),
            "a deleted session's finished work is reclaimed"
        );
    }
    workbench.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reset_of_an_already_retired_session_hands_back_its_replacement() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let old_session_id = state.current_session_id();

    let Json(first) = Box::pin(reset_chat(
        State(state.clone()),
        reset_query(&old_session_id),
    ))
    .await
    .expect("the first reset retires the session");
    assert_eq!(
        state.active_turns.retirement(&old_session_id),
        Some(SessionRetirement::Retired)
    );

    // The page never saw that answer — the response was lost — so it asks
    // again with the only id it has. The fence refuses a retired id for every
    // use including a delete, which made this the dead end: the repair for a
    // tombstoned session was a reset, and reset was the one thing that could
    // not run.
    let Json(second) = Box::pin(reset_chat(
        State(state.clone()),
        reset_query(&old_session_id),
    ))
    .await
    .expect("a reset of an already retired session must not dead-end");

    assert_eq!(second.settings.session_id, first.settings.session_id);
    assert_eq!(state.sessions.current(), first.settings.session_id);
    workbench.shutdown().await;
}

/// Holds the delete at its first durable step: the trace the delete writes
/// once the session's turns are cancelled, just before it requests the close.
struct DeleteGate {
    entered: std::sync::mpsc::SyncSender<()>,
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl TraceSink for DeleteGate {
    fn append(
        &self,
        record: &TraceRecord,
    ) -> std::result::Result<(), lash::tracing::TraceSinkError> {
        if matches!(
            &record.event,
            TraceEvent::Custom { name, .. }
                if name == "agent_workbench.api.session.delete.turns_cancelled"
        ) {
            let _ = self.entered.send(());
            let (released, condition) = &*self.release;
            let mut released = released.lock().unwrap_or_else(|error| error.into_inner());
            while !*released {
                released = condition
                    .wait(released)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_whose_request_goes_away_still_takes_the_roster_off_the_tombstone() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::new(DeleteGate {
            entered: entered_tx,
            release: Arc::clone(&release),
        }))
        .build()
        .await;
    let state = &workbench.state;
    let old_session_id = state.current_session_id();

    let reset = tokio::spawn({
        let state = state.clone();
        let old_session_id = old_session_id.clone();
        async move {
            Box::pin(reset_chat(State(state), reset_query(&old_session_id)))
                .await
                .map(|Json(snapshot)| snapshot.state.settings.session_id)
        }
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(10)))
        .await
        .expect("gate wait")
        .expect("the delete reaches its first durable step");

    // The browser goes away: a reload, a closed tab, an abandoned fetch. The
    // durable delete does not care, and neither may the rotation.
    reset.abort();
    let (released, condition) = &*release;
    *released.lock().unwrap_or_else(|error| error.into_inner()) = true;
    condition.notify_all();

    tokio::time::timeout(Duration::from_secs(30), async {
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
    assert_tombstoned(state, &old_session_id).await;
    workbench.shutdown().await;
}

/// FIG-5022: every reset response is a settled state the page can render.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_response_carries_the_replacement_sessions_durable_transcript() {
    let workbench = Workbench::silent().await;
    let state = workbench.state.clone();
    let app = Router::new()
        .route("/api/reset", post(reset_chat))
        .route("/api/session", delete(reset_chat))
        .route("/api/state", get(app_state))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the workbench routes");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
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
            .expect("reset request")
            .error_for_status()
            .expect("reset succeeds")
            .json::<Value>()
            .await
            .expect("reset body");
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
            .expect("state request")
            .error_for_status()
            .expect("state succeeds")
            .json::<Value>()
            .await
            .expect("state body");
        assert_eq!(
            response, settled,
            "reset and GET must project the same state"
        );

        // Exercise the page's actual timeline with the HTTP response,
        // including the iteration that threw on the missing transcript.
        let node = std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
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
    workbench.shutdown().await;
}

/// FIG-5036: deleting a chat from the sidebar retires that one session, takes
/// its row off the roster, and hands the page the most recent chat left. The
/// other chats keep their rows and their state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_chat_removes_only_its_row_and_hands_back_the_most_recent_one() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
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
        read_state(state, Some(&doomed.session_id)).await.is_err(),
        "the deleted chat's state is gone"
    );
    for survivor in [&first, &kept.session_id] {
        assert_eq!(state.active_turns.retirement(survivor), None);
        let snapshot = read_state(state, Some(survivor))
            .await
            .expect("an untouched chat still reads");
        assert_eq!(&snapshot.state.settings.session_id, survivor);
    }
    assert_tombstoned(state, &doomed.session_id).await;
    workbench.shutdown().await;
}

/// A reset retires the old session and hands the page a fresh session with
/// no work, rows, graphs or accounts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_chat_deletes_old_session_and_hands_back_a_fresh_one() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let old_session_id = state.current_session_id();
    let _deleted_session_events = state.event_tx.subscribe(&old_session_id);
    assert!(state.event_tx.contains(&old_session_id));
    state
        .mail_world
        .add_account("Reset Probe")
        .expect("add account before reset");

    let Json(snapshot) = Box::pin(reset_chat(
        State(state.clone()),
        reset_query(&old_session_id),
    ))
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
    assert_tombstoned(state, &old_session_id).await;
    assert!(
        state
            .create_or_open_session(&snapshot.state.settings.session_id, "test")
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
        list_lash_vm_graphs(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list graphs after reset");
    assert!(
        graph_index.graphs.is_empty(),
        "new session graph index should be empty after reset: {graph_index:#?}"
    );
    let retired_error = read_state(state, Some(&old_session_id))
        .await
        .expect_err("retired session state must be refused");
    assert_eq!(retired_error.status, StatusCode::CONFLICT);
    assert!(retired_error.message.contains(old_session_id.as_str()));
    workbench.shutdown().await;
}
