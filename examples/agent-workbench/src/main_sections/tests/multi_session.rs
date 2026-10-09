use super::*;
use lash::SessionId;

// The multi-session workbench: a roster of sessions surviving the web process
// that created them (FIG-1306).
//
// Every fixture here executes the production route handlers rather than the
// roster type, because the mechanism under test is not "does a map remember a
// string" — it is whether a session an operator created is still the session
// the *executor* runs after the handle that created it is gone.
//
// ADR 0096: TypeScript is the sole RLM language, so the halves of these
// fixtures that asserted a second dialect beside it are gone.

// ADR 0096: the fixture that created a Lash VM session on a TypeScript
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

/// An unnamed session takes its first prompt as its sidebar title; a named
/// one, and a session already titled, keep theirs. Selecting a session does
/// not reorder the list: only a sent prompt makes a session recently active.
#[test]
fn the_first_prompt_titles_an_unnamed_session_and_selection_keeps_the_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = WorkbenchSessions::persistent(temp.path().join("session-id")).expect("roster");
    let unnamed = new_session_id();
    sessions.record(unnamed.clone(), unnamed.to_string());
    let named = new_session_id();
    sessions.record(named.clone(), "typescript work".to_string());
    let carried = new_session_id();
    sessions.record(carried.clone(), new_session_id().to_string());

    sessions.record_prompt(
        &unnamed,
        "\n  Fix   the flaky   cron test\nand explain why it flaked",
    );
    sessions.record_prompt(&unnamed, "a later prompt never retitles it");
    sessions.record_prompt(&named, "this prompt is not a title");
    sessions.record_prompt(&carried, &"long ".repeat(40));

    let name = |id: &SessionId| sessions.entry(id).expect("rostered").name;
    assert_eq!(name(&unnamed), "Fix the flaky cron test");
    assert_eq!(name(&named), "typescript work");
    let long_title = name(&carried);
    assert_eq!(long_title.chars().count(), 60);
    assert!(long_title.ends_with('…'));

    let active_before = sessions.entry(&named).expect("rostered").last_active_ms;
    std::thread::sleep(std::time::Duration::from_millis(5));
    sessions.select(&named).expect("rostered session selects");
    assert_eq!(
        sessions.entry(&named).expect("rostered").last_active_ms,
        active_before,
        "selecting is not use"
    );
}

// The route laws below run on the in-process durable workbench, whose engine
// runs every session's turns.

/// The language of every code-block row of the rendered transcript.
fn transcript_code_languages(snapshot: &StateReadSnapshot) -> Vec<String> {
    snapshot
        .transcript
        .iter()
        .filter(|row| row.kind == crate::ChatRowKind::CodeBlock)
        .filter_map(|row| row.content.language.clone())
        .collect()
}

async fn create_named(state: &AppState, name: &str) -> SessionView {
    create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some(name.to_string()),
        }),
    )
    .await
    .expect("a named session is created")
    .0
}

/// A created session takes its place beside the boot session: each runs its
/// own turn, and each transcript holds only its own cell.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_created_session_runs_beside_the_ambient_default() {
    let workbench = Workbench::builder(scripted_cells_provider(vec![
        finish_cell("created answer"),
        finish_cell("ambient answer"),
    ]))
    .build()
    .await;
    let state = &workbench.state;
    let ambient_session_id = state.current_session_id();
    let created = create_named(state, "typescript work").await;
    assert_eq!(created.name, "typescript work");
    assert_ne!(created.session_id, ambient_session_id);

    run_turn_in(state, &created.session_id, "answer in the created session").await;
    run_turn_in(state, &ambient_session_id, "answer in the ambient session").await;

    let created_view = read_state(state, Some(&created.session_id))
        .await
        .expect("project the created session");
    assert_eq!(created_view.settings.session_name, "typescript work");
    assert_eq!(
        transcript_code_languages(&created_view),
        vec!["typescript".to_string()]
    );
    assert_eq!(
        state_rows(&created_view),
        [
            ("user", "answer in the created session"),
            ("assistant", "created answer")
        ]
        .map(|(role, text)| (role.to_string(), text.to_string()))
    );
    let ambient_view = read_state(state, Some(&ambient_session_id))
        .await
        .expect("project the ambient session");
    assert_eq!(
        transcript_code_languages(&ambient_view),
        vec!["typescript".to_string()]
    );
    assert_eq!(
        state_rows(&ambient_view),
        [
            ("user", "answer in the ambient session"),
            ("assistant", "ambient answer")
        ]
        .map(|(role, text)| (role.to_string(), text.to_string()))
    );
    workbench.shutdown().await;
}

/// Selecting moves what a query-less call resolves to, durably: the
/// current session is one fact that `/api/state` with no `session_id`, the
/// `<data-dir>/session-id` file the drivers read and the selector all read,
/// and a refused selection moves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selecting_a_session_moves_the_query_less_default() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let session_id_path = data_dir.path().join("session-id");
    let workbench = Workbench::builder(silent_provider())
        .sessions(WorkbenchSessions::persistent(session_id_path.clone()).expect("roster"))
        .build()
        .await;
    let state = &workbench.state;
    let boot_session_id = state.current_session_id();
    let created = create_named(state, "second").await;
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
    let defaulted = read_state(state, None)
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
    workbench.shutdown().await;
}

/// The roster is durable: a restarted web process over the same stores and
/// data directory lists the same sessions and keeps serving them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_roster_survives_the_web_process() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let session_id_path = data_dir.path().join("session-id");
    let first = Workbench::builder(silent_provider())
        .sessions(WorkbenchSessions::persistent(session_id_path.clone()).expect("roster"))
        .build()
        .await;
    let mut created = Vec::new();
    for name in ["ts room", "other room"] {
        let view = create_named(&first.state, name).await;
        created.push((view.session_id, name.to_string()));
    }
    let stores = Arc::clone(&first.stores);
    first.shutdown().await;

    let second = Workbench::builder(scripted_cells_provider(vec![finish_cell("restarted")]))
        .stores(stores)
        .sessions(WorkbenchSessions::persistent(session_id_path).expect("reopen the roster"))
        .build()
        .await;
    let state = &second.state;
    let Json(listing) = list_sessions(State(state.clone()))
        .await
        .expect("list the reloaded roster");
    for (session_id, name) in &created {
        let listed = listing
            .sessions
            .iter()
            .find(|summary| summary.session_id == *session_id)
            .unwrap_or_else(|| panic!("`{session_id}` must survive the restart: {listing:#?}"));
        assert_eq!(&listed.name, name);
    }
    let (session_id, _) = &created[0];
    run_turn_in(state, session_id, "answer after the restart").await;
    let restarted = read_state(state, Some(session_id))
        .await
        .expect("project the restarted session");
    assert_eq!(
        transcript_code_languages(&restarted),
        vec!["typescript".to_string()],
        "a restarted process must serve a rostered session"
    );
    second.shutdown().await;
}
