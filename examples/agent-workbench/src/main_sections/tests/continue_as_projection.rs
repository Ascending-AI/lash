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
    let mut state = recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
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
    assert_eq!(
        initial_output.final_value(),
        Some(&json!("third frame answer"))
    );
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

    assert!(
        session.read_view().messages().iter().any(|message| {
            matches!(
                message.origin.as_ref(),
                Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. })
                    if turn_id == &ordinary_turn_id
            ) && lash::message_text(message) == ordinary_prompt
        }),
        "the asserted follow-frame send must carry runtime-stamped TurnInput provenance"
    );
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
            .map(|message| (
                message.id.clone(),
                message.role.clone(),
                message.text.clone()
            ))
            .collect::<Vec<_>>(),
        expected_rows
    );
    assert_eq!(
        projected
            .transcript
            .iter()
            .filter_map(|row| match row {
                TranscriptRow::Message { message } => {
                    Some((
                        message.id.clone(),
                        message.role.clone(),
                        message.text.clone(),
                    ))
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

#[tokio::test]
async fn continue_as_frame_switch_keeps_committed_user_rows_in_api_and_transcript() {
    let data_dir = tempfile::tempdir().expect("frame-switch projection tempdir");
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_completion = Arc::clone(&response_index);
    let provider = lash::testing::TestProvider::builder()
        .kind("frame-switch-committed-user-rows")
        .complete(move |_| {
            let call = response_index_for_completion
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                Ok(match call {
                    0 => text_response(
                        "<lashlang>\nawait control.continue_as({ task: \"continue in the next frame\", seed: { marker: \"protocol-only\" } })?\n</lashlang>",
                    ),
                    1 => text_response("<lashlang>\nfinish \"switched frame answer\"\n</lashlang>"),
                    other => panic!("unexpected frame-switch provider call {other}"),
                })
            }
        })
        .build()
        .into_handle();
    let mut state = recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let product_events_path = data_dir.path().join("product-events.json");
    state.event_tx = SessionEventRegistry::persistent(product_events_path, 16)
        .expect("open persistent product event registry");
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open frame-switch session");

    let mut committed_inputs = Vec::new();
    let mut committed_turn_ids = BTreeSet::new();
    for index in 0..6 {
        let turn_id = format!("committed-before-switch-{index}");
        let prompt = format!("committed prompt before switch {index}");
        committed_turn_ids.insert(turn_id.clone());
        state.push_message_with_id_for_session(
            &session_id,
            workbench_turn_user_message_id(&turn_id),
            "user",
            &prompt,
        );
        committed_inputs.push(
            lash::plugins::PluginMessage::text(lash::messages::MessageRole::User, &prompt)
                .with_id(format!("runtime-{turn_id}"))
                .with_origin(lash::messages::MessageOrigin::TurnInput {
                    turn_id,
                    input_id: Some(format!("input-{index}")),
                }),
        );
    }
    session
        .admin()
        .state()
        .append_messages(committed_inputs)
        .await
        .expect("commit pre-switch user inputs");
    state.event_tx.reconcile_settled(
        &session_id,
        &BTreeSet::new(),
        &committed_turn_ids,
        &BTreeSet::new(),
    );

    let switch_turn_id = "frame-switch-turn";
    let switch_prompt = "switch frames now";
    state.track_turn_prompt(&session_id, switch_turn_id, switch_prompt.to_string(), None);
    state.push_message_with_id_for_session(
        &session_id,
        workbench_turn_user_message_id(switch_turn_id),
        "user",
        switch_prompt,
    );
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text(switch_prompt))
        .turn_id(switch_turn_id)
        .require_finish()
        .expect("require switched-frame finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("run frame switch");
    assert_eq!(output.final_value(), Some(&json!("switched frame answer")));

    crate::restate::record_turn_output(
        &state,
        &session,
        switch_turn_id,
        output,
        turn_state,
        "test.frame_switch_committed_user_rows.completed",
    )
    .await
    .expect("record switched-frame turn");

    // The next state read can be scoped to the new frame and therefore carry
    // no old-frame input ids. Once the workbench has observed a row's typed
    // durable provenance, that rebuild must not retire the UI-owned row.
    state.event_tx.reconcile_settled(
        &session_id,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeSet::from([switch_turn_id.to_string()]),
    );

    let Json(boundary) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("read state at frame-switch boundary");
    let api_user_rows = boundary
        .state
        .messages
        .iter()
        .filter(|message| message.role == "user")
        .count();
    let transcript_user_rows = boundary
        .transcript
        .iter()
        .filter(|row| matches!(row, TranscriptRow::Message { message } if message.role == "user"))
        .count();
    let product_user_rows = boundary
        .state
        .product_events
        .events
        .iter()
        .filter(|event| {
            matches!(&event.item, StreamItem::Message { message } if message.role == "user")
        })
        .count();
    assert_eq!(
        api_user_rows, 7,
        "committed user rows disappeared from /api/state"
    );
    assert_eq!(
        transcript_user_rows, 7,
        "committed user rows disappeared from the rendered transcript"
    );
    assert_eq!(
        product_user_rows, 7,
        "product user rows were retired at the switch"
    );

    crate::restate::settle_workbench_turn(&state, &session_id, switch_turn_id)
        .await
        .expect("settle switched-frame turn");
    session.close().await.expect("close frame-switch session");
}
