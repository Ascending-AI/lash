// Where one turn's committed transcript ends and the next one's begins, for
// the rule that admits a turn's protocol-authored reply (FIG-1406).
//
// These probes are message sequences rather than driven turns because the
// shapes they pin — a cause-only wake, an input injected into a running turn,
// a stale active-turn entry that outlived its process — are properties of the
// committed sequence, and the rule reads nothing else.

fn probe_turn_input(turn_id: &str, message_id: &str) -> lash::messages::Message {
    lash::messages::Message {
        id: message_id.to_string(),
        role: lash::messages::MessageRole::User,
        parts: Arc::new(vec![lash::messages::Part::text(
            format!("{message_id}.p0"),
            "ask".to_string(),
            None,
        )]),
        origin: Some(lash::messages::MessageOrigin::TurnInput {
            turn_id: turn_id.to_string(),
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
            process_id: "probe-producer".to_string(),
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
        .map(|turn_id| (*turn_id).to_string())
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
                probe_turn_input("t1", "m_turn_t1_input"),
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
                probe_turn_input("t1", "m_turn_t1_input"),
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
                probe_turn_input("t1", "m_turn_t1_input"),
                probe_plugin_prose("m_rlm_t1_0_assistant_content", "thinking out loud"),
                probe_turn_input("t1", "m_ingress_injected"),
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
            probe_turn_input("t1", "m_turn_t1_input"),
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
                probe_turn_input("t1", "m_turn_t1_input"),
                probe_plugin_prose("m_rlm_t1_0_assistant_response", "first answer"),
                probe_turn_input("t2", "m_turn_t2_input"),
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
