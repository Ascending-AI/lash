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

/// The task the frame switch hands the new frame.
const TASK: &str = "carry on from the summary";
/// A tool that switches the agent frame and hands the new frame [`TASK`].
struct SwitchFrame;

#[async_trait::async_trait]
impl crate::tools::StaticToolExecute for SwitchFrame {
    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true }))
            .with_control(lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("facade-frame-switch")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some(TASK.to_owned()),
            })
            .into()
    }
}

/// FIG-5232: compaction's continuation through `send()`. A turn whose tool
/// switches the agent frame with a task answers its send with the switch,
/// and the session runs the task as its next turn, on the new frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_switch_through_send_completes_and_its_follow_on_runs_next() {
    let definition = lash_core::ToolDefinition::raw(
        "switch_frame",
        "switch_frame",
        "Switches the agent frame and hands the new frame a task.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("switch_frame's schemas");
    // The model compacts by calling `switch_frame`, and answers the task it
    // finds on the new frame.
    let provider = crate::testing::TestProvider::builder()
        .kind("facade-frame-switch")
        .requires_streaming(true)
        .complete(|request: LlmRequest| async move {
            let user_text = last_user_text(&request);
            if user_text.contains(TASK) {
                return Ok(text_response(&format!("done: {user_text}")));
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "facade-frame-switch-call".to_owned(),
                    tool_name: "switch_frame".to_owned(),
                    input_json: "{}".to_owned(),
                    replay: None,
                }],
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(Arc::new(crate::tools::StaticToolProvider::new(
        vec![definition],
        SwitchFrame,
    )))
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session_id =
        lash_sansio::SessionId::try_from("facade-frame-switch".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");

    let switched = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        session
            .send(crate::TurnInput::text("compact, then carry on"))
            .output(),
    )
    .await
    .expect("the switching send completes")
    .expect("the switching turn answers");
    assert_eq!(
        switched.status(),
        crate::TurnStatus::Answered,
        "{switched:?}"
    );
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
        frame_key, task, ..
    } = &switched.result.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.result.outcome);
    };
    assert_eq!(task, TASK);

    // The switch's commit mailed the task: the session's next turn runs it
    // on the new frame, and answers under its own run.
    let follow_on = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        session
            .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
                frame_key,
            ))
            .output(),
    )
    .await
    .expect("the follow-on completes")
    .expect("the follow-on answers");
    assert_eq!(
        follow_on.assistant_message(),
        Some(format!("done: {TASK}").as_str()),
        "{follow_on:?}"
    );
    core.shutdown().await.expect("shutdown");
}
