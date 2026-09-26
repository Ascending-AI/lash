use super::*;
use lash::ProcessId;
use lash::TurnId;

// Where one turn's committed transcript ends and the next one's begins, for
// the rule that admits a turn's protocol-authored reply (FIG-1406).
//
// These probes are message sequences rather than driven turns because the
// shapes they pin — a cause-only wake, an input injected into a running turn,
// a stale active-turn entry that outlived its process — are properties of the
// committed sequence, and the rule reads nothing else.

fn probe_turn_input(turn_id: &TurnId, message_id: &str) -> lash::messages::Message {
    lash::messages::Message {
        id: message_id.to_string(),
        role: lash::messages::MessageRole::User,
        parts: Arc::new(vec![lash::messages::Part::text(
            format!("{message_id}.p0"),
            "ask".to_string(),
            None,
        )]),
        origin: Some(lash::messages::MessageOrigin::TurnInput {
            turn_id: TurnId::from(turn_id.to_string()),
            input_id: None,
        }),
    }
}

/// The message a cause-only turn opens with: a process wake or queued drain
/// commits no turn input at all, only its typed cause.
fn probe_turn_cause(message_id: &str) -> lash::messages::Message {
    lash::messages::Message {
        id: message_id.to_string(),
        role: lash::messages::MessageRole::Event,
        parts: Arc::new(vec![lash::messages::Part::text(
            format!("{message_id}.p0"),
            "the producer woke this session".to_string(),
            None,
        )]),
        origin: Some(lash::messages::MessageOrigin::Process {
            process_id: ProcessId::fixture("probe-producer"),
            event_type: "producer.wake".to_string(),
            sequence: 1,
            wake_id: None,
            caused_by: None,
        }),
    }
}

fn probe_plugin_prose(message_id: &str, prose: &str) -> lash::messages::Message {
    lash::messages::Message {
        id: message_id.to_string(),
        role: lash::messages::MessageRole::Assistant,
        parts: Arc::new(vec![
            lash::messages::Part::reasoning(
                format!("{message_id}.p0"),
                format!("reasoning for {message_id}"),
                None,
            ),
            lash::messages::Part::prose(format!("{message_id}.p1"), prose.to_string(), None),
        ]),
        origin: Some(lash::messages::MessageOrigin::Plugin {
            plugin_id: lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

fn probe_runtime_assistant(message_id: &str, prose: &str) -> lash::messages::Message {
    lash::messages::Message {
        id: message_id.to_string(),
        role: lash::messages::MessageRole::Assistant,
        parts: Arc::new(vec![lash::messages::Part::prose(
            format!("{message_id}.p0"),
            prose.to_string(),
            None,
        )]),
        origin: None,
    }
}

fn probe_replies(messages: Vec<lash::messages::Message>, running_turn_ids: &[&str]) -> Vec<String> {
    let running = running_turn_ids
        .iter()
        .map(|turn_id| TurnId::from(*turn_id))
        .collect::<BTreeSet<_>>();
    durable_rlm_reply_message_ids(&messages, &running)
        .into_iter()
        .collect()
}

/// A wake turn commits a cause, not a turn input, so the previous turn's
/// already-rendered reply must be settled by the cause that opens the next
/// turn. Reading only turn inputs as boundaries makes the wake's prose look
/// like a later candidate of the *earlier* turn and retracts an answer the
/// user already read.
#[test]
fn a_cause_only_turn_settles_the_previous_turn_reply() {
    assert_eq!(
        probe_replies(
            vec![
                probe_turn_input(&TurnId::from("t1"), "m_turn_t1_input"),
                probe_plugin_prose("m_rlm_t1_0_assistant_response", "first answer"),
                probe_turn_cause("m_cause_wake_1"),
                probe_plugin_prose("m_rlm_wake_0_assistant_response", "wake answer"),
            ],
            &[],
        ),
        vec![
            "m_rlm_t1_0_assistant_response".to_string(),
            "m_rlm_wake_0_assistant_response".to_string(),
        ],
        "each turn keeps its own reply across a cause-only turn boundary"
    );
    assert_eq!(
        probe_replies(
            vec![
                probe_turn_input(&TurnId::from("t1"), "m_turn_t1_input"),
                probe_plugin_prose("m_rlm_t1_0_assistant_response", "first answer"),
                probe_turn_cause("m_cause_wake_1"),
                probe_runtime_assistant("m_turn_wake_assistant", "wake answer"),
            ],
            &[],
        ),
        vec!["m_rlm_t1_0_assistant_response".to_string()],
        "a later turn's own runtime reply must not retract the previous turn's"
    );
}

/// An input injected into a running turn commits a turn-input message carrying
/// that same turn's id. It opens no turn, so it must not settle a candidate:
/// doing so renders the turn's mid-turn prose *and* its answer — one turn, two
/// agent rows, the shape FIG-984 closed.
#[test]
fn an_injected_input_does_not_open_a_turn() {
    assert_eq!(
        probe_replies(
            vec![
                probe_turn_input(&TurnId::from("t1"), "m_turn_t1_input"),
                probe_plugin_prose("m_rlm_t1_0_assistant_content", "thinking out loud"),
                probe_turn_input(&TurnId::from("t1"), "m_ingress_injected"),
                probe_plugin_prose("m_rlm_t1_1_assistant_response", "the answer"),
            ],
            &[],
        ),
        vec!["m_rlm_t1_1_assistant_response".to_string()],
        "one turn projects one reply however many inputs it absorbed"
    );
}

/// Withholding is a property of the candidate's own turn. The workbench's
/// active-turn registry is persistent, so an entry that never settles — its
/// process died mid-turn — would otherwise hide every later reply in the
/// session for good, and the registry is written *before* the next turn's input
/// commits, so a session-wide test blinks the previous answer out at every
/// send.
#[test]
fn only_the_running_turns_own_candidate_is_withheld() {
    let one_reasoned_turn = || {
        vec![
            probe_turn_input(&TurnId::from("t1"), "m_turn_t1_input"),
            probe_plugin_prose("m_rlm_t1_0_assistant_response", "the answer"),
        ]
    };
    assert!(
        probe_replies(one_reasoned_turn(), &["t1"]).is_empty(),
        "a running turn's own trailing candidate stays behind its live row"
    );
    assert_eq!(
        probe_replies(one_reasoned_turn(), &["t2"]),
        vec!["m_rlm_t1_0_assistant_response".to_string()],
        "another turn's liveness says nothing about this turn's reply"
    );
    assert_eq!(
        probe_replies(one_reasoned_turn(), &[]),
        vec!["m_rlm_t1_0_assistant_response".to_string()],
        "a settled turn renders its reply"
    );
    assert_eq!(
        probe_replies(
            vec![
                probe_turn_input(&TurnId::from("t1"), "m_turn_t1_input"),
                probe_plugin_prose("m_rlm_t1_0_assistant_response", "first answer"),
                probe_turn_input(&TurnId::from("t2"), "m_turn_t2_input"),
                probe_plugin_prose("m_rlm_t2_0_assistant_response", "second answer"),
            ],
            &["t1"],
        ),
        vec![
            "m_rlm_t1_0_assistant_response".to_string(),
            "m_rlm_t2_0_assistant_response".to_string(),
        ],
        "a stale active-turn entry that survived a restart hides nothing"
    );
}

/// Sam's screenshot: the button and mail EVENT rows sat at the bottom of the
/// chat, below turns that happened long after them and away from the queued-turn
/// replies they caused. The committed graph had those replies in the right
/// place; the projection appended every product-log row after all of them, so
/// each `/api/state` rebuild re-sank the event rows under the newest chat.
#[tokio::test]
async fn a_host_event_row_renders_where_it_happened_not_under_the_newest_turn() {
    let data_dir = tempfile::tempdir().expect("event ordering tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let first_turn = TurnId::from("workbench-turn-before-the-event");
    let later_turn = TurnId::from("workbench-turn-after-the-event");

    // A committed turn, with the workbench's own user row for it in the product
    // log — the row that survives reconciliation and anchors what follows.
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open event-ordering session");
    for (turn, prompt, reply) in [
        (&first_turn, "watch the buttons", "watching the buttons"),
        (&later_turn, "anything else?", "nothing else"),
    ] {
        session
            .admin()
            .state()
            .append_messages(vec![
                lash::plugins::PluginMessage::text(lash::messages::MessageRole::User, prompt)
                    .with_id(workbench_turn_user_message_id(turn))
                    .with_origin(lash::messages::MessageOrigin::TurnInput {
                        turn_id: turn.clone(),
                        input_id: None,
                    }),
                lash::plugins::PluginMessage::text(lash::messages::MessageRole::Assistant, reply)
                    .with_id(workbench_turn_assistant_message_id(turn)),
            ])
            .await
            .expect("append committed turn");
    }
    session.close().await.expect("close event-ordering session");

    state.push_message_with_id_for_session(
        &session_id,
        workbench_turn_user_message_id(&first_turn),
        "user",
        "mirrored prompt",
    );

    // The occurrence the operator caused between the two turns. The product log
    // is pushed in arrival order, and that order is the only record of where it
    // belongs: after the first turn, before the second was ever sent.
    state.push_message_with_id_for_session(
        &session_id,
        "red-button-occurrence",
        "event",
        "red button trigger occurrence",
    );

    state.push_message_with_id_for_session(
        &session_id,
        workbench_turn_user_message_id(&later_turn),
        "user",
        "mirrored prompt",
    );

    let Json(snapshot) = Box::pin(app_state(State(state), Query(SessionQuery::default())))
        .await
        .expect("materialize the event-ordering snapshot");
    let ids = snapshot
        .messages
        .iter()
        .map(|message| message.id.as_str())
        .collect::<Vec<_>>();
    let event_at = ids
        .iter()
        .position(|id| *id == "red-button-occurrence")
        .expect("the event row must render");
    let later_user_at = ids
        .iter()
        .position(|id| *id == workbench_turn_user_message_id(&later_turn))
        .expect("the later turn must render");
    let first_reply_at = ids
        .iter()
        .position(|id| *id == workbench_turn_assistant_message_id(&first_turn))
        .expect("the first reply must render");
    assert!(
        first_reply_at < event_at,
        "the event happened after the first turn settled: {ids:?}"
    );
    assert!(
        event_at < later_user_at,
        "a reload must not re-sink the event row under a turn that happened later: {ids:?}"
    );
    assert_eq!(
        snapshot
            .transcript
            .iter()
            .filter_map(|row| match row {
                TranscriptRow::Message { message } => Some(message.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        ids,
        "the transcript must render the same order as the message list"
    );
}
