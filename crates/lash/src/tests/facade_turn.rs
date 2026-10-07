//! L3 (FIG-5172): a sent input's turn runs on the core's node, through the
//! production turn driver, and its handle answers the committed reply.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sent_input_runs_on_the_cores_node_and_answers_its_reply() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let session_id = lash_sansio::SessionId::try_from("facade-turn".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("hello"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.assistant_message(), Some("echo: hello"));
    core.shutdown().await.expect("shutdown");
}

/// FIG-5219: a host attributes a model call to its turn, Run and attempt from
/// the typed scope its provider receives, never by parsing the request id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turns_model_call_carries_its_typed_turn_run_and_attempt() {
    let scopes = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let scopes = Arc::clone(&scopes);
            move |request| {
                scopes.lock_recover().push(request.scope.clone());
                async move { Ok(text_response("attributed")) }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session = core
        .session(lash_sansio::SessionId::try_from("attributed-turn".to_owned()).expect("id"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let turn = crate::TurnId::parse("host-attributed-turn").expect("turn id");
    let output = session
        .send(crate::TurnInput::text("hello"))
        .id(turn.clone())
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let scopes = scopes.lock_recover().clone();
    let [scope] = scopes.as_slice() else {
        panic!("one model call: {scopes:?}");
    };
    assert_eq!(
        scope.turn,
        Some(crate::provider::LlmTurnScope {
            run: crate::RunId::from(turn.clone()),
            turn_id: turn,
        })
    );
    assert_eq!(scope.attempt, Some(1));
    core.shutdown().await.expect("shutdown");
}
