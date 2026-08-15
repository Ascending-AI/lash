fn browser_projection_trigger_identities() -> serde_json::Value {
    serde_json::json!({
        "session_a": lash_core::triggers::deterministic_subscription_id(
            &lash_core::TriggerOwnerScope::session("session-a"),
            "derived/v2/content-address",
        ),
        "session_b": lash_core::triggers::deterministic_subscription_id(
            &lash_core::TriggerOwnerScope::session("session-b"),
            "derived/v2/content-address",
        ),
        "wired": lash_core::triggers::deterministic_subscription_id(
            &lash_core::TriggerOwnerScope::session("wired-session"),
            "wired-key",
        ),
    })
}

#[tokio::test]
async fn workbench_remote_recovery_facades_deliver_cursor_events_and_terminal_replacement() {
    let data_dir = tempfile::tempdir().expect("remote recovery facade tempdir");
    let state = recoverable_chat_test_state(data_dir.path(), 64).await;
    let session = state
        .core
        .session("workbench-remote-recovery-facades")
        .open()
        .await
        .expect("open remote recovery facade session");
    let observable = session.observe();
    let current = observable.current_remote_observation();
    assert_eq!(current.session_id, "workbench-remote-recovery-facades");
    assert_eq!(current.protocol_version, lash::remote::REMOTE_PROTOCOL_VERSION);
    assert_eq!(observable.current_remote_observation(), current);

    let snapshot = observable.recoverable_chat_snapshot();
    assert_eq!(snapshot.read_view.session_id(), current.session_id);
    assert_eq!(observable.recoverable_chat_snapshot().cursor, snapshot.cursor);
    let remote_cursor = lash::remote::observations::RemoteSessionCursor::new(
        snapshot.cursor.to_string(),
    );
    let mut direct = match observable
        .subscribe_from_remote_cursor(&remote_cursor)
        .expect("subscribe from remote cursor")
    {
        lash::observe::RemoteSessionObservationSubscription::Subscribed(stream) => stream,
        lash::observe::RemoteSessionObservationSubscription::Gap { .. } => {
            panic!("fresh remote cursor must subscribe without a gap")
        }
    };
    let mut recovering = observable
        .subscribe_and_recover_remote(remote_cursor)
        .expect("subscribe and recover remote");

    session
        .turn(lash::TurnInput::text("prove remote recovery facades"))
        .turn_id("remote-recovery-facade-turn")
        .require_finish()
        .expect("require finish")
        .run()
        .await
        .expect("run remote recovery facade turn");

    let direct_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = direct.next_event().await.expect("direct remote event");
            if matches!(
                &event.event,
                lash::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
                    activity,
                } if matches!(
                    &activity.event,
                    lash::remote::usage::RemoteTurnEvent::ModelCallRecorded { .. }
                )
            ) {
                return event;
            }
        }
    })
    .await
    .expect("direct model-call event timeout");
    assert_eq!(direct_event.session_id, current.session_id);
    assert_ne!(direct_event.cursor, current.cursor);
    let lash::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
        activity,
    } = &direct_event.event
    else {
        unreachable!("loop returns only turn activities")
    };
    let lash::remote::usage::RemoteTurnEvent::ModelCallRecorded { record } = &activity.event else {
        unreachable!("loop returns only model-call records")
    };
    assert!(!record.call_id.is_empty());
    assert!(!record.attempts.is_empty());

    let recovering_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let item = recovering
                .next()
                .await
                .expect("recovering remote stream closed")
                .expect("recovering remote event");
            if let lash::observe::RemoteSessionObservationStreamItem::Event(event) = item
                && matches!(
                    &event.event,
                    lash::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
                        activity,
                    } if matches!(
                        &activity.event,
                        lash::remote::usage::RemoteTurnEvent::ModelCallRecorded { .. }
                    )
                )
            {
                return event;
            }
        }
    })
    .await
    .expect("recovering model-call event timeout");
    assert_eq!(recovering_event.session_id, current.session_id);

    async fn next_terminal_replacement(
        mut chat: lash::recoverable_chat::RecoverableChatSubscription,
    ) -> lash::recoverable_chat::RecoverableChatUpdate {
        while let Some(item) = chat.next().await {
            let item = item.expect("recoverable chat event");
            if matches!(
                item,
                lash::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { .. }
            ) {
                return item;
            }
        }
        panic!("recoverable chat stream closed before terminal replacement");
    }
    let terminal = tokio::time::timeout(
        Duration::from_secs(5),
        next_terminal_replacement(observable.subscribe_recoverable_chat(snapshot.cursor)),
    )
    .await
    .expect("terminal replacement timeout");
    let lash::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
        snapshot,
        event,
        ..
    } = terminal
    else {
        unreachable!("loop returns only terminal replacements")
    };
    assert_eq!(event.turn_id.as_deref(), Some("remote-recovery-facade-turn"));
    assert_eq!(snapshot.cursor, event.cursor);
    drop(direct);
    drop(recovering);
    drop(observable);
    session.close().await.expect("close remote recovery facade session");
}
