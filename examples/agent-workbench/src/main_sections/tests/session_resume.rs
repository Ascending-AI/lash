//! A restarted web process resumes a session from durable state alone.

use super::*;

/// The `(role, text)` rows the page renders for the current session.
async fn current_rows(state: &AppState) -> Vec<(String, String)> {
    state_rows(
        &read_state(state, None)
            .await
            .expect("project the transcript"),
    )
}

fn rows(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(role, text)| (role.to_string(), text.to_string()))
        .collect()
}

/// The committed transcript and the provider history survive the web
/// process: each turn commits its input and its one reply, marked with its
/// turn; a new web process over the same stores and data directory starts
/// with no local rows, renders the committed transcript from durable state,
/// and its next turn reaches the provider with the whole committed history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_transcript_and_provider_history_survive_web_process_reconstruction() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let session_id_path = data_dir.path().join("session-id");
    let first = Workbench::builder(scripted_cells_provider(vec![
        finish_cell("resume answer one"),
        finish_cell("resume answer two"),
    ]))
    .sessions(WorkbenchSessions::persistent(session_id_path.clone()).expect("the roster"))
    .build()
    .await;
    let session_id = first.state.current_session_id();
    let turn_one = run_turn(&first.state, "resume question one").await;
    let turn_two = run_turn(&first.state, "resume question two").await;

    let committed = first
        .state
        .open_session(&session_id, "test")
        .await
        .expect("open the session")
        .read_view();
    let messages = committed.messages();
    assert_eq!(
        messages
            .iter()
            .map(|message| (message.role, lash::message_text(message)))
            .collect::<Vec<_>>(),
        vec![
            (
                lash::messages::MessageRole::User,
                "resume question one".to_string()
            ),
            (
                lash::messages::MessageRole::Assistant,
                "resume answer one".to_string()
            ),
            (
                lash::messages::MessageRole::User,
                "resume question two".to_string()
            ),
            (
                lash::messages::MessageRole::Assistant,
                "resume answer two".to_string()
            ),
        ],
        "each turn commits its input and its one reply"
    );
    // The runtime commits each value-finished turn's reply itself, marked
    // with its turn (FIG-1493 §5.5); no host writer is involved.
    assert_eq!(
        messages
            .iter()
            .filter_map(|message| message.reply_marker.as_ref())
            .map(|reply| reply.turn_id().clone())
            .collect::<Vec<_>>(),
        vec![turn_one, turn_two]
    );
    assert_eq!(committed.turn_index(), 2);
    let stores = Arc::clone(&first.stores);
    first.shutdown().await;

    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider = {
        let requests = Arc::clone(&requests);
        lash::testing::TestProvider::builder()
            .kind("workbench-harness")
            .complete(move |request| {
                requests
                    .lock_recover()
                    .push(serde_json::to_string(&request).expect("serialize the request"));
                async { Ok(text_response(&finish_cell("resume answer three"))) }
            })
            .build()
            .into_handle()
    };
    let second = Workbench::builder(provider)
        .stores(stores)
        .sessions(WorkbenchSessions::persistent(session_id_path).expect("reopen the roster"))
        .build()
        .await;
    let state = &second.state;
    assert_eq!(state.current_session_id(), session_id);
    assert!(
        state.messages_snapshot().is_empty(),
        "the new web process begins with no local rows"
    );
    assert_eq!(
        current_rows(state).await,
        rows(&[
            ("user", "resume question one"),
            ("assistant", "resume answer one"),
            ("user", "resume question two"),
            ("assistant", "resume answer two"),
        ])
    );

    run_turn(state, "resume question three").await;
    {
        let requests = requests.lock_recover();
        let [request] = requests.as_slice() else {
            panic!("the resumed turn calls the provider once: {requests:?}");
        };
        for marker in [
            "resume question one",
            "resume answer one",
            "resume question two",
            "resume answer two",
            "resume question three",
        ] {
            assert!(
                request.contains(marker),
                "the resumed request omits committed history {marker:?}: {request}"
            );
        }
    }
    assert_eq!(
        current_rows(state).await,
        rows(&[
            ("user", "resume question one"),
            ("assistant", "resume answer one"),
            ("user", "resume question two"),
            ("assistant", "resume answer two"),
            ("user", "resume question three"),
            ("assistant", "resume answer three"),
        ])
    );
    second.shutdown().await;
}
