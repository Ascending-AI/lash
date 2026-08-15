#[tokio::test]
async fn interactive_bare_prose_termination_leaves_one_committed_agent_reply() {
    const BARE_PROSE_REPLY: &str = "bare prose answer";
    let data_dir = tempfile::tempdir().expect("bare prose tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-bare-prose")
        .complete(|_| async { Ok(text_response(BARE_PROSE_REPLY)) })
        .build()
        .into_handle();
    let state = recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id)
        .open()
        .await
        .expect("open bare prose session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    // Deliberately no `require_finish`: this is the termination an interactive
    // turn reaches when the send path does not force the answer through
    // `finish`, and the one every queued turn reaches.
    let output = session
        .turn(lash::TurnInput::text("answer in prose"))
        .turn_id("bare-prose-turn")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("run bare prose turn");
    assert!(
        matches!(
            &output.outcome,
            lash::TurnOutcome::Finished(lash::TurnFinish::AssistantMessage { text })
                if text == BARE_PROSE_REPLY
        ),
        "unexpected termination for a bare prose reply: {:?}",
        output.outcome
    );
    crate::restate::record_turn_output(
        &state,
        &session,
        "bare-prose-turn",
        output,
        turn_state,
        "test.bare_prose.completed",
    )
    .await
    .expect("record bare prose turn output");
    let committed_agent_replies = session
        .read_view()
        .messages()
        .iter()
        .filter(|message| {
            lash::message_role(message) == "assistant"
                && lash::message_text(message).contains(BARE_PROSE_REPLY)
        })
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        committed_agent_replies.len(),
        1,
        "a bare-prose termination must commit the agent reply exactly once, \
         got {committed_agent_replies:?}"
    );
    assert!(
        !committed_agent_replies[0].starts_with("workbench-assistant:"),
        "the runtime's own terminal message is the committed copy on this path, \
         got {committed_agent_replies:?}"
    );
}
