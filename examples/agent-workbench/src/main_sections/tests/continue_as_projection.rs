#[tokio::test]
async fn two_continue_as_switches_keep_real_sends_and_hide_each_follow_task() {
    let data_dir = tempfile::tempdir().expect("multi-frame send projection tempdir");
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_completion = Arc::clone(&response_index);
    let provider = lash::testing::TestProvider::builder()
        .kind("multi-frame-workbench-send-projection")
        .complete(move |_| {
            let call = response_index_for_completion
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                Ok(match call {
                    0 => text_response(
                        "<lashlang>\nawait control.continue_as({ task: \"enter the middle follow frame\", seed: { middle_marker: \"hidden-middle-seed\" } })?\n</lashlang>",
                    ),
                    1 => text_response(
                        "<lashlang>\nawait control.continue_as({ task: \"enter the final follow frame\", seed: { final_marker: \"hidden-final-seed\" } })?\n</lashlang>",
                    ),
                    2 => text_response("<lashlang>\nfinish \"third frame answer\"\n</lashlang>"),
                    3 => text_response(
                        "<lashlang>\nfinish \"ordinary follow-frame answer\"\n</lashlang>",
                    ),
                    other => panic!("unexpected multi-frame provider call {other}"),
                })
            }
        })
        .build()
        .into_handle();
    let mut state =
        recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
    state.restate_ingress_url = restate_ingress_url;
    let session_id = state.current_session_id();
    let initial_prompt = "switch through three frames";

    let _ = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: initial_prompt.to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("submit initial real send");
    let initial_submission = restate_requests
        .recv()
        .await
        .expect("capture initial real send");
    let initial_turn_id = initial_submission
        .pointer("/body/turn_id")
        .and_then(Value::as_str)
        .expect("initial real-send turn id")
        .to_string();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open multi-frame session");
    let initial_turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let initial_output = session
        .turn(lash::TurnInput::text(initial_prompt))
        .turn_id(initial_turn_id.clone())
        .require_finish()
        .expect("require initial finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&initial_turn_state),
        })
        .await
        .expect("run real send across two frame switches");
    assert_eq!(initial_output.final_value(), Some(&json!("third frame answer")));
    crate::restate::record_turn_output(
        &state,
        &session,
        &initial_turn_id,
        initial_output,
        initial_turn_state,
        "test.continue_as.three_frames.completed",
    )
    .await
    .expect("record initial multi-frame send");
    crate::restate::settle_workbench_turn(&state, &session_id, &initial_turn_id)
        .await
        .expect("settle initial multi-frame send");
    session.close().await.expect("close after frame switches");

    let ordinary_prompt = "ordinary send inside the final follow frame";
    let _ = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: ordinary_prompt.to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("submit ordinary follow-frame send");
    let ordinary_submission = restate_requests
        .recv()
        .await
        .expect("capture ordinary follow-frame send");
    let ordinary_turn_id = ordinary_submission
        .pointer("/body/turn_id")
        .and_then(Value::as_str)
        .expect("ordinary real-send turn id")
        .to_string();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("reopen final follow frame");
    let ordinary_turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ordinary_output = session
        .turn(lash::TurnInput::text(ordinary_prompt))
        .turn_id(ordinary_turn_id.clone())
        .require_finish()
        .expect("require ordinary follow-frame finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&ordinary_turn_state),
        })
        .await
        .expect("run ordinary follow-frame send");
    assert_eq!(
        ordinary_output.final_value(),
        Some(&json!("ordinary follow-frame answer"))
    );
    crate::restate::record_turn_output(
        &state,
        &session,
        &ordinary_turn_id,
        ordinary_output,
        ordinary_turn_state,
        "test.continue_as.ordinary_follow_frame.completed",
    )
    .await
    .expect("record ordinary follow-frame send");
    crate::restate::settle_workbench_turn(&state, &session_id, &ordinary_turn_id)
        .await
        .expect("settle ordinary follow-frame send");

    assert!(session.read_view().messages().iter().any(|message| {
        matches!(
            message.origin.as_ref(),
            Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. })
                if turn_id == &ordinary_turn_id
        ) && lash::message_text(message) == ordinary_prompt
    }), "the asserted follow-frame send must carry runtime-stamped TurnInput provenance");
    let all_frame_turn_ids = session
        .read_view()
        .message_tree()
        .into_iter()
        .flat_map(|root| {
            let mut messages = vec![root.message];
            let mut pending = root.children;
            while let Some(node) = pending.pop() {
                messages.push(node.message);
                pending.extend(node.children);
            }
            messages
        })
        .filter_map(|message| match message.origin {
            Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. }) => Some(turn_id),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert!(all_frame_turn_ids.contains(&initial_turn_id));
    assert!(all_frame_turn_ids.contains(&ordinary_turn_id));
    session.close().await.expect("close projected follow frame");

    let Json(projected) = app_state(State(state), Query(SessionQuery::default()))
        .await
        .expect("project three-frame conversation");
    let expected_rows = vec![
        (
            workbench_turn_user_message_id(&initial_turn_id),
            "user".to_string(),
            initial_prompt.to_string(),
        ),
        (
            workbench_turn_assistant_message_id(&initial_turn_id),
            "assistant".to_string(),
            "third frame answer".to_string(),
        ),
        (
            workbench_turn_user_message_id(&ordinary_turn_id),
            "user".to_string(),
            ordinary_prompt.to_string(),
        ),
        (
            workbench_turn_assistant_message_id(&ordinary_turn_id),
            "assistant".to_string(),
            "ordinary follow-frame answer".to_string(),
        ),
    ];
    assert_eq!(
        projected
            .messages
            .iter()
            .map(|message| (message.id.clone(), message.role.clone(), message.text.clone()))
            .collect::<Vec<_>>(),
        expected_rows
    );
    assert_eq!(
        projected
            .transcript
            .iter()
            .filter_map(|row| match row {
                TranscriptRow::Message { message } => {
                    Some((message.id.clone(), message.role.clone(), message.text.clone()))
                }
                TranscriptRow::Reasoning { .. } | TranscriptRow::CodeBlock { .. } => None,
            })
            .collect::<Vec<_>>(),
        expected_rows
    );
    assert!(projected.messages.iter().all(|message| {
        !message.text.contains("enter the middle follow frame")
            && !message.text.contains("enter the final follow frame")
            && !message.text.contains("hidden-middle-seed")
            && !message.text.contains("hidden-final-seed")
    }));
}
