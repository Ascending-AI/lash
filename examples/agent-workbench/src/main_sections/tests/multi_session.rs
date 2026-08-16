// The multi-session workbench: a roster of sessions, each created with its own
// dialect, surviving the web process that created them (FIG-1306).
//
// Every fixture here drives the production route handlers rather than the
// roster type, because the mechanism under test is not "does a map remember a
// string" — it is whether the dialect an operator picked at creation is still
// the dialect the *executor* runs after the handle that created it is gone. A
// session's dialect only becomes durable at its first commit, so a roster that
// is consulted anywhere except the open path is a roster that loses the choice
// on the first turn.

/// Both dialects, in one workbench, on one ambient default.
///
/// The ambient `LASH_RUNBOOK_DIALECT` stays Lashlang throughout: the boot
/// session must still record `lashlang` — that is the compatibility every
/// runbook driver depends on — while a session created as TypeScript records
/// `typescript`. Asserting only the created session would pass on a workbench
/// that had simply flipped everything to TypeScript.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_created_session_runs_its_own_dialect_beside_the_ambient_default() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let provider = scripted_cells_provider(
        "workbench-multi-session-dialects",
        vec![
            "<typescript>\nfinish(\"typescript answer\");\n</typescript>".to_string(),
            "<lashlang>\nfinish \"lashlang answer\"\n</lashlang>".to_string(),
        ],
    );
    let state = queued_send_test_state(data_dir.path(), provider).await;
    assert_eq!(
        state.rlm_dialect,
        lash::rlm::RlmDialect::Lashlang,
        "this fixture's premise is an ambient Lashlang deployment"
    );
    let ambient_session_id = state.current_session_id();

    let Json(created) = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some("typescript work".to_string()),
            dialect: Some("typescript".to_string()),
        }),
    )
    .await
    .expect("a registered dialect is accepted at creation");
    assert_eq!(created.dialect, "typescript");
    assert_eq!(created.name, "typescript work");
    assert_ne!(created.session_id, ambient_session_id);

    run_turn_through_the_workbench_open_path(
        &state,
        &created.session_id,
        "created-session-turn",
        "say the canonical answer",
    )
    .await;
    run_turn_through_the_workbench_open_path(
        &state,
        &ambient_session_id,
        "ambient-session-turn",
        "say the canonical answer",
    )
    .await;

    // The durable half: what each session recorded, read from the store.
    assert_eq!(
        recorded_dialect_payload(&state, &created.session_id).await,
        serde_json::json!("typescript"),
        "the created session must have recorded the dialect it was created with"
    );
    assert_eq!(
        recorded_dialect_payload(&state, &ambient_session_id).await,
        serde_json::json!("lashlang"),
        "creating a TypeScript session must not move the ambient default"
    );

    // The rendered half: each session's transcript labels its own cells, and
    // `/api/state` badges the dialect that session recorded.
    let Json(created_view) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(created.session_id.clone()),
        }),
    )
    .await
    .expect("project the created session");
    assert_eq!(created_view.settings.rlm_dialect, "typescript");
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
    assert_eq!(ambient_view.settings.rlm_dialect, "lashlang");
    assert_eq!(
        transcript_code_languages(&ambient_view),
        vec!["lashlang".to_string()]
    );
}

/// The same mechanism the other way round: a TypeScript deployment must be able
/// to create a Lashlang session, or the roster is just the ambient setting
/// spelled twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_typescript_workbench_can_create_a_lashlang_session() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let provider = scripted_cells_provider(
        "workbench-multi-session-lashlang-on-typescript",
        vec!["<lashlang>\nfinish \"lashlang answer\"\n</lashlang>".to_string()],
    );
    let mut state = queued_send_test_state(data_dir.path(), provider).await;
    state.rlm_dialect = lash::rlm::RlmDialect::Typescript;

    let Json(created) = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: None,
            dialect: Some("lashlang".to_string()),
        }),
    )
    .await
    .expect("the non-ambient dialect is accepted at creation");
    assert_eq!(created.dialect, "lashlang");
    assert_eq!(
        created.name, created.session_id,
        "a session created without a name is named by its id"
    );

    run_turn_through_the_workbench_open_path(
        &state,
        &created.session_id,
        "lashlang-on-typescript-turn",
        "say the canonical answer",
    )
    .await;

    assert_eq!(
        recorded_dialect_payload(&state, &created.session_id).await,
        serde_json::json!("lashlang"),
        "the created session must record its own dialect, not the ambient one"
    );
}

/// An unregistered language id is refused at creation, and leaves no roster row.
///
/// Failing closed here is the whole reason the choice is typed: a dialect is
/// pinned for a session's durable lifetime, so a typo that quietly selected the
/// default would be undoable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unregistered_dialect_is_refused_at_creation() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let provider = scripted_cells_provider("workbench-unknown-dialect", Vec::new());
    let state = queued_send_test_state(data_dir.path(), provider).await;
    let before = state.sessions.list().len();

    let error = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some("typo".to_string()),
            dialect: Some("lashscript".to_string()),
        }),
    )
    .await
    .expect_err("an unregistered dialect must be refused");

    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(
        error.message.contains("lashscript")
            && error.message.contains("`lashlang`")
            && error.message.contains("`typescript`"),
        "the refusal must name the offending id and the registered ones: {}",
        error.message
    );
    assert_eq!(
        state.sessions.list().len(),
        before,
        "a refused creation must not leave a roster row"
    );
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
    state.sessions.ensure(&boot_session_id, state.rlm_dialect);

    let Json(created) = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some("second".to_string()),
            dialect: Some("typescript".to_string()),
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
    assert_eq!(
        defaulted.settings.rlm_dialect, "typescript",
        "a session that has committed nothing is still badged with the dialect it will run"
    );

    let error = select_session(
        State(state.clone()),
        Json(SessionSelectRequest {
            session_id: "workbench-not-on-the-roster".to_string(),
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

/// The roster is durable: a restarted web process lists the same sessions, with
/// the dialects they were created with, and keeps serving them in those
/// dialects.
///
/// The second half is the one that matters. A roster that survived as a list of
/// names but was not consulted on the open path would list a TypeScript session
/// and then run it as Lashlang on the first turn after the restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_roster_survives_the_web_process() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let session_id_path = data_dir.path().join("session-id");
    let created_ids = {
        let provider = scripted_cells_provider("workbench-roster-restart-first", Vec::new());
        let mut state = queued_send_test_state(data_dir.path(), provider).await;
        state.sessions = WorkbenchSessions::persistent(session_id_path.clone()).expect("roster");
        state
            .sessions
            .ensure(&state.current_session_id(), state.rlm_dialect);
        let mut created = Vec::new();
        for (name, dialect) in [("ts room", "typescript"), ("lash room", "lashlang")] {
            let Json(summary) = create_session(
                State(state.clone()),
                Json(SessionCreateRequest {
                    name: Some(name.to_string()),
                    dialect: Some(dialect.to_string()),
                }),
            )
            .await
            .expect("create a session to reload");
            created.push((summary.session_id, dialect.to_string()));
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
    for (session_id, dialect) in &created_ids {
        let listed = listing
            .sessions
            .iter()
            .find(|summary| &summary.session_id == session_id)
            .unwrap_or_else(|| panic!("`{session_id}` must survive the restart: {listing:#?}"));
        assert_eq!(&listed.dialect, dialect);
    }
    assert_eq!(
        listing.dialects,
        lash::rlm::RlmDialect::ALL
            .iter()
            .map(|dialect| dialect.language_id())
            .collect::<Vec<_>>(),
        "the create menu is the substrate's registered dialects"
    );
    assert_eq!(listing.default_dialect, "lashlang");

    let (typescript_session_id, _) = created_ids
        .first()
        .cloned()
        .expect("the TypeScript session was created first");
    run_turn_through_the_workbench_open_path(
        &state,
        &typescript_session_id,
        "post-restart-turn",
        "say the canonical answer",
    )
    .await;
    assert_eq!(
        recorded_dialect_payload(&state, &typescript_session_id).await,
        serde_json::json!("typescript"),
        "a restarted process must serve a rostered session in its own dialect"
    );
}

/// A reset replaces the session behind a roster slot, and the replacement keeps
/// the dialect the operator chose: pressing reset in a TypeScript session must
/// not drop the workbench back to the ambient default.
#[test]
fn a_reset_carries_the_slot_dialect_to_the_rotated_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = WorkbenchSessions::persistent(temp.path().join("session-id")).expect("roster");
    let original = sessions.current();
    sessions.record(
        original.clone(),
        "typescript work".to_string(),
        lash::rlm::RlmDialect::Typescript,
    );

    let (old, new) = sessions.rotate();

    assert_eq!(old, original);
    assert_eq!(
        sessions.dialect_for(&new),
        Some(lash::rlm::RlmDialect::Typescript)
    );
    assert_eq!(
        sessions.entry(&new).map(|entry| entry.name),
        Some("typescript work".to_string())
    );
    assert_eq!(
        sessions.dialect_for(&old),
        None,
        "the retired session leaves the roster with the slot it held"
    );
}

/// FIG-1398: `is_dialect_pin_conflict` matches the exact error message the
/// protocol plugin emits when a session is reopened with a different dialect.
#[test]
fn dialect_pin_conflict_matches_the_protocol_plugins_exact_message() {
    let error = lash::EmbedError::Session(lash_core::SessionError::Protocol(
        "RLM dialect is durably pinned to `typescript` and cannot be reopened as `lashlang`".to_string(),
    ));
    assert!(is_dialect_pin_conflict(&error));

    let unrelated = lash::EmbedError::Session(lash_core::SessionError::Protocol(
        "other protocol error".to_string(),
    ));
    assert!(!is_dialect_pin_conflict(&unrelated));
}

/// What a session recorded, as the store holds it.
async fn recorded_dialect_payload(state: &AppState, session_id: &str) -> serde_json::Value {
    let session = state
        .core
        .session(session_id.to_string())
        .open()
        .await
        .expect("reopen the session");
    let recorded = session.read_view().protocol_turn_options().payload["dialect"].clone();
    drop(session);
    recorded
}
