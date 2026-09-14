use super::*;
use lash::SessionId;

// The multi-session workbench: a roster of sessions surviving the web process
// that created them (FIG-1306).
//
// Every fixture here drives the production route handlers rather than the
// roster type, because the mechanism under test is not "does a map remember a
// string" — it is whether a session an operator created is still the session
// the *executor* runs after the handle that created it is gone.
//
// ADR 0096: TypeScript is the sole RLM language, so the halves of these
// fixtures that asserted a second dialect beside it are gone.

/// A created session takes its place beside the boot session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_created_session_runs_beside_the_ambient_default() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let provider = scripted_cells_provider(
        "workbench-multi-session",
        vec![
            "<typescript>\nfinish(\"typescript answer\");\n</typescript>".to_string(),
            "<typescript>\nfinish(\"ambient answer\");\n</typescript>".to_string(),
        ],
    );
    let state = queued_send_test_state(data_dir.path(), provider).await;
    let ambient_session_id = state.current_session_id();

    let Json(created) = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some("typescript work".to_string()),
        }),
    )
    .await
    .expect("a named session is created");
    assert_eq!(created.name, "typescript work");
    assert_ne!(created.session_id, ambient_session_id);

    run_turn_through_the_workbench_open_path(
        &state,
        &created.session_id,
        &TurnId::from("created-session-turn"),
        "say the canonical answer",
    )
    .await;
    run_turn_through_the_workbench_open_path(
        &state,
        &ambient_session_id,
        &TurnId::from("ambient-session-turn"),
        "say the canonical answer",
    )
    .await;

    // Each session's transcript labels its own cells.
    let Json(created_view) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(created.session_id.clone()),
        }),
    )
    .await
    .expect("project the created session");
    assert_eq!(created_view.settings.session_name, "typescript work");
    assert_eq!(
        transcript_code_languages(&created_view),
        vec!["typescript".to_string()]
    );

    let Json(ambient_view) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(ambient_session_id.clone()),
        }),
    )
    .await
    .expect("project the ambient session");
    assert_eq!(
        transcript_code_languages(&ambient_view),
        vec!["typescript".to_string()]
    );
}

// ADR 0096: the fixture that created a Lashlang session on a TypeScript
// deployment is gone with the second dialect.

/// A create request that still names a language does not decode.
///
/// The field is gone rather than pinned (ADR 0096), and a plain Serde struct
/// would drop an unknown key silently — so a stale form posting
/// `{"dialect": "lashscript"}` would be answered `201` and served TypeScript,
/// which is exactly the quiet substitution the removal is meant to prevent.
/// `deny_unknown_fields` is what makes it an answer instead.
#[test]
fn a_create_request_that_still_names_a_language_does_not_decode() {
    let error = serde_json::from_value::<SessionCreateRequest>(serde_json::json!({
        "name": "typo",
        "dialect": "lashscript",
    }))
    .expect_err("a create request naming a language must be refused");
    assert!(
        error.to_string().contains("dialect"),
        "the refusal names the retired field: {error}"
    );

    let accepted = serde_json::from_value::<SessionCreateRequest>(serde_json::json!({
        "name": "ok",
    }))
    .expect("a request carrying only a name still decodes");
    assert_eq!(accepted.name.as_deref(), Some("ok"));
}

/// Switching moves what a query-less call resolves to, durably.
///
/// The workbench's current session is one durable fact — `/api/state` with no
/// `session_id`, the `<data-dir>/session-id` file the drivers read, and the
/// selector all read it — so a switch that only changed a browser variable
/// would leave the three disagreeing after a reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selecting_a_session_moves_the_query_less_default() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let session_id_path = data_dir.path().join("session-id");
    let provider = scripted_cells_provider("workbench-session-switch", Vec::new());
    let mut state = queued_send_test_state(data_dir.path(), provider).await;
    state.sessions = WorkbenchSessions::persistent(session_id_path.clone()).expect("roster");
    let boot_session_id = state.current_session_id();
    state.sessions.ensure(&boot_session_id);

    let Json(created) = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some("second".to_string()),
        }),
    )
    .await
    .expect("create the session to switch to");
    assert_eq!(
        state.current_session_id(),
        boot_session_id,
        "creating a session must not silently move the current one"
    );

    let Json(selected) = select_session(
        State(state.clone()),
        Json(SessionSelectRequest {
            session_id: created.session_id.clone(),
        }),
    )
    .await
    .expect("a rostered session can be selected");
    assert!(selected.current);
    assert_eq!(state.current_session_id(), created.session_id);
    assert_eq!(
        std::fs::read_to_string(&session_id_path).expect("read the selection file"),
        created.session_id,
        "the selection is the same durable fact the drivers read"
    );

    let Json(defaulted) = app_state(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("project the query-less default");
    assert_eq!(defaulted.settings.session_id, created.session_id);
    assert_eq!(defaulted.settings.session_name, "second");

    let error = select_session(
        State(state.clone()),
        Json(SessionSelectRequest {
            session_id: SessionId::from("workbench-not-on-the-roster"),
        }),
    )
    .await
    .expect_err("selecting an unknown session must be refused");
    assert_eq!(error.status, StatusCode::NOT_FOUND);
    assert_eq!(
        state.current_session_id(),
        created.session_id,
        "a refused selection must not move the current session"
    );
}

/// The roster is durable: a restarted web process lists the same sessions and
/// keeps serving them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_roster_survives_the_web_process() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let session_id_path = data_dir.path().join("session-id");
    let created_ids = {
        let provider = scripted_cells_provider("workbench-roster-restart-first", Vec::new());
        let mut state = queued_send_test_state(data_dir.path(), provider).await;
        state.sessions = WorkbenchSessions::persistent(session_id_path.clone()).expect("roster");
        state.sessions.ensure(&state.current_session_id());
        let mut created = Vec::new();
        // ADR 0096: the second row of this table was the Lashlang room.
        for name in ["ts room", "other room"] {
            let Json(summary) = create_session(
                State(state.clone()),
                Json(SessionCreateRequest {
                    name: Some(name.to_string()),
                }),
            )
            .await
            .expect("create a session to reload");
            created.push((summary.session_id, name.to_string()));
        }
        created
    };

    // A new web process over the same data directory: new AppState, new roster
    // handle, same files.
    let provider = scripted_cells_provider(
        "workbench-roster-restart-second",
        vec!["<typescript>\nfinish(\"restarted\");\n</typescript>".to_string()],
    );
    let mut state = queued_send_test_state(data_dir.path(), provider).await;
    state.sessions = WorkbenchSessions::persistent(session_id_path).expect("reopen the roster");

    let Json(listing) = list_sessions(State(state.clone()))
        .await
        .expect("list the reloaded roster");
    for (session_id, name) in &created_ids {
        let listed = listing
            .sessions
            .iter()
            .find(|summary| summary.session_id == session_id)
            .unwrap_or_else(|| panic!("`{session_id}` must survive the restart: {listing:#?}"));
        assert_eq!(&listed.name, name);
    }

    let (typescript_session_id, _) = created_ids
        .first()
        .cloned()
        .expect("the TypeScript session was created first");
    run_turn_through_the_workbench_open_path(
        &state,
        &typescript_session_id,
        &TurnId::from("post-restart-turn"),
        "say the canonical answer",
    )
    .await;
    let Json(restarted_view) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(typescript_session_id.clone()),
        }),
    )
    .await
    .expect("project the restarted session");
    assert_eq!(
        transcript_code_languages(&restarted_view),
        vec!["typescript".to_string()],
        "a restarted process must serve a rostered session"
    );
}

/// A reset replaces the session behind a roster slot, and the replacement keeps
/// the slot the operator named.
///
/// ADR 0096: the dialect half of this fixture is gone with the second dialect;
/// the slot itself still has to survive the rotation.
#[test]
fn a_reset_carries_the_slot_name_to_the_rotated_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = WorkbenchSessions::persistent(temp.path().join("session-id")).expect("roster");
    let original = sessions.current();
    sessions.record(original.clone(), "typescript work".to_string());

    let (old, new) = sessions.rotate();

    assert_eq!(old, original);
    assert_eq!(
        sessions.entry(&new).map(|entry| entry.name),
        Some("typescript work".to_string())
    );
    assert!(
        sessions.entry(&old).is_none(),
        "the retired session leaves the roster with the slot it held"
    );
}

// ADR 0096: the typed dialect-pin conflict fixture (FIG-1555) is gone with
// `RlmSessionConfigConflict::Dialect`.
