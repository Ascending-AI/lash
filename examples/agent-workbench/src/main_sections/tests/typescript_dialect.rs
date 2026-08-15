
/// One turn through the workbench's own session-opening path.
///
/// Deliberately `state.session_builder(...)`, which is what `run_user_turn` and
/// every route use — opening `state.core.session(...)` directly would bypass
/// the very code this file exists to test.
async fn run_turn_through_the_workbench_open_path(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
    text: &str,
) {
    let session = state
        .session_builder(session_id.to_string())
        .open()
        .await
        .expect("open through the workbench path");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ui_events = ChannelTurnEvents {
        turn_state: Arc::clone(&turn_state),
    };
    session
        .turn(lash::TurnInput::text(text))
        .turn_id(turn_id.to_string())
        .require_finish()
        .expect("require finish")
        .stream_to(&ui_events)
        .await
        .expect("run the turn");
}

// The workbench's TypeScript branch, driven end to end.
//
// Every other fixture here builds a Lashlang `AppState`, so nothing reached the
// branch that a `LASH_RUNBOOK_DIALECT=typescript` deployment actually runs. The
// dialect field exists precisely so a test can set it; these do.

/// A served turn must reach the model with the TypeScript prompt, and the
/// session must record `typescript` durably.
///
/// The first version of this fix applied the ambient dialect only on opens that
/// create, and those two call sites open and drop without running a turn. A
/// dialect becomes durable at the session's first commit, so the pin evaporated
/// with the handle and the first real turn — opening with no dialect, finding
/// nothing recorded — committed `lashlang` permanently. Asserting the prompt
/// alone would not have caught it either; the durable read is what pins the
/// mechanism.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_typescript_workbench_serves_typescript_turns_and_records_the_dialect() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let served_prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = {
        let served_prompts = Arc::clone(&served_prompts);
        lash::testing::TestProvider::builder()
            .kind("typescript-workbench-test")
            .complete(move |request: lash::provider::LlmRequest| {
                let served_prompts = Arc::clone(&served_prompts);
                async move {
                    // The request's own rendering, so this fixture needs no
                    // message-vocabulary types the facade does not export.
                    let rendered = format!("{request:?}");
                    served_prompts.lock_recover().push(rendered.clone());
                    Ok(text_response(
                        "<typescript>\nfinish(\"canonical answer\");\n</typescript>",
                    ))
                }
            })
            .build()
            .into_handle()
    };

    let mut state = queued_send_test_state(data_dir.path(), provider).await;
    state.rlm_dialect = lash::rlm::RlmDialect::Typescript;
    let session_id = state.current_session_id();

    run_turn_through_the_workbench_open_path(
        &state,
        &session_id,
        "typescript-dialect-turn",
        "say the canonical answer",
    )
    .await;

    let prompts = served_prompts.lock_recover().clone();
    assert!(
        !prompts.is_empty(),
        "the turn must have reached the provider"
    );
    assert!(
        prompts
            .iter()
            .all(|prompt| prompt.contains("## TypeScript execution")),
        "every served prompt must be the TypeScript one: {prompts:#?}"
    );

    // The durable half. A prompt can be right for one turn and still leave the
    // session recorded as Lashlang, which is the shape that shipped.
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("reopen the served session");
    assert_eq!(
        session.read_view().protocol_turn_options().payload["dialect"],
        serde_json::json!("typescript"),
        "the served session must have recorded its dialect durably"
    );
}

/// The same fixture on the default dialect, so the assertions above cannot pass
/// by the workbench being TypeScript for everyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lashlang_workbench_still_serves_lashlang_turns() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let served_prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = {
        let served_prompts = Arc::clone(&served_prompts);
        lash::testing::TestProvider::builder()
            .kind("lashlang-workbench-test")
            .complete(move |request: lash::provider::LlmRequest| {
                let served_prompts = Arc::clone(&served_prompts);
                async move {
                    // The request's own rendering, so this fixture needs no
                    // message-vocabulary types the facade does not export.
                    served_prompts.lock_recover().push(format!("{request:?}"));
                    Ok(text_response(
                        "<lashlang>\nfinish \"canonical answer\"\n</lashlang>",
                    ))
                }
            })
            .build()
            .into_handle()
    };

    let state = queued_send_test_state(data_dir.path(), provider).await;
    let session_id = state.current_session_id();

    run_turn_through_the_workbench_open_path(
        &state,
        &session_id,
        "lashlang-dialect-turn",
        "say the canonical answer",
    )
    .await;

    let prompts = served_prompts.lock_recover().clone();
    assert!(!prompts.is_empty(), "the turn must have reached the provider");
    assert!(
        prompts
            .iter()
            .all(|prompt| !prompt.contains("## TypeScript execution")),
        "the default workbench must not serve the TypeScript prompt: {prompts:#?}"
    );

    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("reopen the served session");
    assert_eq!(
        session.read_view().protocol_turn_options().payload["dialect"],
        serde_json::json!("lashlang"),
    );
}
