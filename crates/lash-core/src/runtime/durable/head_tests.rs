use super::*;

/// Commit `count` more messages over `state`, the session's head.
async fn advance(backend: &Backend, state: &mut RuntimeSessionState, count: usize) {
    state.ensure_agent_frame_initialized();
    for ordinal in 0..count {
        state.session_graph.append_message(crate::Message {
            id: format!("held-base-{}-{ordinal}", state.head_revision),
            role: crate::MessageRole::User,
            parts: crate::shared_parts(vec![crate::Part::text(
                format!("held-base-{}-{ordinal}.p0", state.head_revision),
                format!("message {ordinal} over revision {}", state.head_revision),
                None,
            )]),
            origin: None,
            reply_marker: None,
        });
    }
    let receipt = backend
        .session_store_factory()
        .commit_runtime_state(crate::store::RuntimeCommit::persisted_state_for_test(state))
        .await
        .expect("commit the messages");
    state.apply_persisted_commit_result(receipt);
}

fn encoded(messages: &crate::MessageSequence) -> serde_json::Value {
    serde_json::to_value(messages).expect("messages encode")
}

/// FIG-6634: a prelude restores over the base its caller holds only when
/// that base is the window the prelude names. A base held at a head the
/// session has since advanced past is never served for the later head's
/// prelude: that prelude's base is read again from the store.
#[tokio::test]
async fn a_held_base_serves_only_the_window_it_is() {
    let backend = crate::testing::sqlite_recording_backend().await;
    let session = SessionId::fixture("held-prelude-base");
    crate::testing::runtime_helpers::recording_session_store(&backend, session.clone()).await;
    let mut state = SessionHead::load(&backend, &session)
        .await
        .expect("the created head")
        .state()
        .clone();
    advance(&backend, &mut state, 3).await;
    let earlier = SessionHead::load(&backend, &session)
        .await
        .expect("the earlier head");
    let (earlier_head, earlier_base) = earlier.prelude_base();
    let mut state = earlier.state().clone();
    advance(&backend, &mut state, 2).await;
    let (later_head, later_base) = SessionHead::load(&backend, &session)
        .await
        .expect("the later head")
        .prelude_base();
    assert_ne!(earlier_head, later_head, "the head advanced");
    assert_ne!(encoded(&earlier_base), encoded(&later_base));

    let mut prelude = crate::runtime::effect::TurnPrelude::new(
        crate::EffectAddress::new(
            crate::ExecutionScope::turn(
                session.clone(),
                lash_vocabulary::identity::TurnId::from("held-base"),
            ),
            "turn-config:held-base",
        )
        .expect("an effect address"),
        Vec::new(),
        later_head.clone(),
        None,
    );
    prelude.capture_messages(&later_base, &later_base, &later_base);
    let store = session_store(&backend, &session)
        .await
        .expect("the session's store");

    // The base held at the earlier head is not the later head's window.
    let (base, history, _) =
        restore_prelude(&prelude, Some((&earlier_head, &earlier_base)), &store)
            .await
            .expect("restore over the store's window");
    assert_eq!(
        encoded(&base),
        encoded(&later_base),
        "the later head's window"
    );
    assert_eq!(encoded(&history), encoded(&later_base));

    // A base held at the prelude's own head is the base, whatever checkpoint
    // its head carries: the store is not read for it.
    let held: crate::MessageSequence = later_base
        .iter()
        .take(1)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    let mut pinned = later_head.clone();
    pinned.checkpoint = Some(crate::BlobRef::for_content(b"held-base-checkpoint"));
    let (base, _, _) = restore_prelude(&prelude, Some((&pinned, &held)), &store)
        .await
        .expect("restore over the held base");
    assert_eq!(
        encoded(&base),
        encoded(&held),
        "the held base serves its own window"
    );
}
