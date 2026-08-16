/// The projection of a bare-prose reply the runtime committed itself.
///
/// Read through the production `/api/state` handler so the assertion covers the
/// same projection the browser reads, settled — after the turn stopped running,
/// its live workbench-owned row has retired and only durable truth remains.
async fn settled_assistant_rows(state: &AppState, session_id: &str) -> (Vec<String>, Vec<String>) {
    let Json(snapshot) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.to_string()),
        }),
    )
    .await
    .expect("project settled workbench state");
    let assistant_texts = snapshot
        .state
        .messages
        .iter()
        .filter(|message| message.role == "assistant")
        .map(|message| message.text.clone())
        .collect::<Vec<_>>();
    let reasoning_rows = snapshot
        .transcript
        .iter()
        .filter_map(|row| match row {
            TranscriptRow::Reasoning { text, .. } => Some(text.clone()),
            TranscriptRow::Message { .. } | TranscriptRow::CodeBlock { .. } => None,
        })
        .collect::<Vec<_>>();
    (assistant_texts, reasoning_rows)
}

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
        .session(session_id.clone())
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
    crate::restate::settle_workbench_turn(&state, &session.session_id(), "bare-prose-turn")
        .await
        .expect("settle bare prose turn");
    drop(session);
    let (assistant_texts, _) = settled_assistant_rows(&state, &session_id).await;
    assert_eq!(
        assistant_texts,
        vec![BARE_PROSE_REPLY.to_string()],
        "the settled projection must render the committed bare-prose reply once"
    );
}

/// The same bare-prose termination, with reasoning attached to the answer.
///
/// A reasoning-carrying reply is committed by the RLM protocol itself, as an
/// assistant message with a `Reasoning` and a `Prose` part under
/// `MessageOrigin::Plugin`. The runtime then mints no terminal copy of its own —
/// `materialize_terminal_output` finds the answer already last in the
/// transcript — so that protocol-authored message is the *only* durable copy of
/// the user-visible reply. Treating every plugin-origin RLM message as internal
/// therefore dropped the answer entirely and re-admitted only its reasoning
/// (FIG-1406). Correlation stays typed — role, origin, and part kind — never a
/// node-id shape (FIG-972/984).
#[tokio::test]
async fn bare_prose_reply_with_reasoning_renders_its_committed_prose_once() {
    const REASONED_REPLY: &str = "FIG-1406 reasoned prose answer";
    const REPLY_REASONING: &str = "FIG-1406 private deliberation";
    let data_dir = tempfile::tempdir().expect("reasoned prose tempdir");
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-reasoned-prose")
        .complete(|_| async {
            let mut response = text_response(REASONED_REPLY);
            response.parts.insert(
                0,
                lash::direct::LlmOutputPart::Reasoning {
                    text: REPLY_REASONING.to_string(),
                    replay: None,
                },
            );
            Ok(response)
        })
        .build()
        .into_handle();
    let state = recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open reasoned prose session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    // No `require_finish`: the termination every queued turn reaches.
    let output = session
        .turn(lash::TurnInput::text("answer in prose, thinking first"))
        .turn_id("reasoned-prose-turn")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("run reasoned prose turn");
    assert!(
        matches!(
            &output.outcome,
            lash::TurnOutcome::Finished(lash::TurnFinish::AssistantMessage { text })
                if text == REASONED_REPLY
        ),
        "unexpected termination for a reasoned prose reply: {:?}",
        output.outcome
    );
    crate::restate::record_turn_output(
        &state,
        &session,
        "reasoned-prose-turn",
        output,
        turn_state,
        "test.reasoned_prose.completed",
    )
    .await
    .expect("record reasoned prose turn output");
    let committed_agent_replies = session
        .read_view()
        .messages()
        .iter()
        .filter(|message| {
            lash::message_role(message) == "assistant"
                && lash::message_text(message).contains(REASONED_REPLY)
        })
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        committed_agent_replies.len(),
        1,
        "a reasoned bare-prose termination must commit the agent reply exactly once, \
         got {committed_agent_replies:?}"
    );
    crate::restate::settle_workbench_turn(&state, &session.session_id(), "reasoned-prose-turn")
        .await
        .expect("settle reasoned prose turn");
    drop(session);
    let (assistant_texts, reasoning_rows) = settled_assistant_rows(&state, &session_id).await;
    assert_eq!(
        assistant_texts,
        vec![REASONED_REPLY.to_string()],
        "the settled projection must render the protocol-committed reply once"
    );
    assert!(
        !assistant_texts
            .iter()
            .any(|text| text.contains(REPLY_REASONING)),
        "reasoning stays collapsed out of the chat rows, got {assistant_texts:?}"
    );
    assert_eq!(
        reasoning_rows,
        vec![REPLY_REASONING.to_string()],
        "the reasoning keeps its own collapsed transcript row"
    );
}

/// The protocol prose a turn commits *before* its answer stays out of the chat.
///
/// Every RLM iteration commits the model's prose as a plugin-origin assistant
/// message, and only the last one is the reply. The live stream renders one
/// agent row per turn, so admitting the mid-turn copies would put rows in the
/// reload projection that the live path never had — the two-copies-of-one-turn
/// shape FIG-984 closed. Their reasoning still renders, collapsed, as before.
#[tokio::test]
async fn mid_turn_protocol_prose_stays_out_of_the_chat_rows() {
    const MID_TURN_PROSE: &str = "FIG-1406 mid-turn thinking out loud";
    const FINAL_REPLY: &str = "FIG-1406 answer after a code step";
    let data_dir = tempfile::tempdir().expect("mid-turn prose tempdir");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-mid-turn-prose")
        .complete(move |_| {
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut response = match call {
                    0 => text_response(&format!(
                        "{MID_TURN_PROSE}\n<lashlang>\nprint(\"step\")\n</lashlang>"
                    )),
                    _ => text_response(FINAL_REPLY),
                };
                response.parts.insert(
                    0,
                    lash::direct::LlmOutputPart::Reasoning {
                        text: format!("FIG-1406 reasoning {call}"),
                        replay: None,
                    },
                );
                Ok(response)
            }
        })
        .build()
        .into_handle();
    let state = recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open mid-turn prose session");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text("take a step, then answer"))
        .turn_id("mid-turn-prose-turn")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("run mid-turn prose turn");
    crate::restate::record_turn_output(
        &state,
        &session,
        "mid-turn-prose-turn",
        output,
        turn_state,
        "test.mid_turn_prose.completed",
    )
    .await
    .expect("record mid-turn prose turn output");
    crate::restate::settle_workbench_turn(&state, &session.session_id(), "mid-turn-prose-turn")
        .await
        .expect("settle mid-turn prose turn");
    drop(session);
    let (assistant_texts, reasoning_rows) = settled_assistant_rows(&state, &session_id).await;
    assert_eq!(
        assistant_texts,
        vec![FINAL_REPLY.to_string()],
        "one turn must project exactly one agent row: its reply"
    );
    assert_eq!(
        reasoning_rows,
        vec![
            "FIG-1406 reasoning 0".to_string(),
            "FIG-1406 reasoning 1".to_string(),
        ],
        "each iteration keeps its own collapsed reasoning row"
    );
}
