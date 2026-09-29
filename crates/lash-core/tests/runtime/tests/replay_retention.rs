//! FIG-4059: the live replay retains every committed observation with its
//! own read view. Those views keep exactly what their commit published, and
//! share the frame's buffers instead of each holding a copy of the frame.

use super::*;
use lash_core::testing::TestTurnDrive as _;
use std::collections::HashSet;

const SEED: u64 = 0x4059;

fn text_call(text: String) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text,
                response_meta: None,
            }],
            usage: LlmUsage::default(),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn retained_committed_views_keep_their_commit_and_share_the_frame() {
    const TURNS: usize = 48;
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(
            (0..TURNS)
                .map(|turn| text_call(format!("answer {turn}")))
                .collect(),
        ),
        test_host_config(&backend),
        store as Arc<dyn lash_core::RuntimeStore>,
    )
    .await;
    let handle = RuntimeHandle::new(runtime);
    let cursor = handle.observe().cursor().clone();

    for turn in 0..TURNS {
        let mut runtime = handle.runtime.lock().await;
        let handler = double
            .open_handler(AdmittedScope::turn(
                SessionId::from("root"),
                TurnId::from(format!("retention-turn-{turn}")),
            ))
            .await
            .expect("open the scope's handler");
        runtime
            .drive_turn(
                TurnInput::text(format!("question {turn}")),
                lash_core::facade_support::TurnOptions::new(
                    CancellationToken::new(),
                    handler.scoped(),
                ),
            )
            .await
            .expect("the turn commits");
        handler.close().await.expect("close the scope's handler");
        handle.publish_from(&runtime);
    }

    let SessionResume::Replayed { events } = handle
        .resume_session_observation(&cursor)
        .expect("resume from the first cursor")
    else {
        panic!("every commit stays replayable");
    };
    let views = events
        .iter()
        .filter_map(|event| match &event.payload {
            SessionObservationEventPayload::Committed { read_view } => Some(read_view),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(views.len(), TURNS);

    // Held-reader isolation: each retained view is its own commit, a strict
    // prefix of the next, ending with that turn's exchange.
    for (turn, view) in views.iter().enumerate() {
        let texts = view
            .messages()
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| part.text_content())
            .collect::<Vec<_>>();
        assert!(
            texts.contains(&format!("question {turn}").as_str()),
            "view {turn} holds its own question"
        );
        assert!(
            !texts.contains(&format!("question {}", turn + 1).as_str()),
            "view {turn} never sees a later commit"
        );
        if let Some(next) = views.get(turn + 1) {
            assert!(view.messages().len() < next.messages().len());
            for (held, later) in view.messages().iter().zip(next.messages()) {
                assert!(lash_sansio::same_message(held, later));
            }
        }
    }

    // Flat retention: the retained views share a bounded set of message
    // and node buffers rather than one copy of the frame each.
    let message_buffers = views
        .iter()
        .map(|view| view.messages().as_ptr())
        .collect::<HashSet<_>>();
    let node_buffers = views
        .iter()
        .map(|view| view.session_graph().nodes.as_ptr())
        .collect::<HashSet<_>>();
    assert!(
        message_buffers.len() <= 10,
        "{} message buffers for {TURNS} retained views",
        message_buffers.len()
    );
    assert!(
        node_buffers.len() <= 10,
        "{} node buffers for {TURNS} retained views",
        node_buffers.len()
    );
}
