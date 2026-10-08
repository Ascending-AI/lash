//! The remote recovery facades a workbench session serves: a fresh remote
//! cursor subscribes without a gap, both the direct and the recovering
//! remote stream deliver a routed turn's model-call record, and the
//! recoverable chat stream delivers its terminal replacement.

use super::*;

fn is_model_call(event: &lash::remote::observations::RemoteSessionObservationEvent) -> bool {
    matches!(
        &event.event,
        lash::remote::observations::RemoteSessionObservationEventPayload::TurnActivity { activity }
            if matches!(
                &activity.event,
                lash::remote::usage::RemoteTurnEvent::ModelCallRecorded { .. }
            )
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workbench_remote_recovery_facades_deliver_cursor_events_and_terminal_replacement() {
    let workbench =
        Workbench::replying("<typescript>\nfinish(\"remote answer\");\n</typescript>").await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let session = state
        .open_session_for_observation(&session_id)
        .await
        .expect("open the session for observation");
    let observable = session.observe();
    let current = observable
        .remote_snapshot()
        .await
        .expect("a durable remote snapshot");
    assert_eq!(current.session_id, session_id.as_str());
    assert_eq!(
        observable
            .remote_snapshot()
            .await
            .expect("a durable remote snapshot"),
        current,
        "a remote snapshot is a pure read"
    );
    let snapshot = observable
        .recoverable_chat_snapshot()
        .await
        .expect("a durable chat snapshot");
    assert_eq!(snapshot.read_view.session_id(), session_id);
    let remote_cursor =
        lash::remote::observations::RemoteSessionCursor::new(snapshot.cursor.to_string());
    let mut direct = match observable
        .subscribe_from_remote_cursor(&remote_cursor)
        .await
        .expect("subscribe from the remote cursor")
    {
        lash::observe::RemoteSessionObservationSubscription::Subscribed(stream) => stream,
        lash::observe::RemoteSessionObservationSubscription::Gap { .. } => {
            panic!("a fresh remote cursor must subscribe without a gap")
        }
    };
    let mut recovering = observable
        .subscribe_and_recover_remote(remote_cursor)
        .expect("subscribe and recover");
    let mut chat = observable.subscribe_recoverable_chat(snapshot.cursor);

    let turn_id = run_turn(state, "prove the remote recovery facades").await;

    let direct_event = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = direct.next_event().await.expect("a direct remote event");
            if is_model_call(&event) {
                return event;
            }
        }
    })
    .await
    .expect("the direct stream delivers the model call");
    assert_eq!(direct_event.session_id, current.session_id);
    assert_ne!(direct_event.cursor, current.cursor);
    let lash::remote::observations::RemoteSessionObservationEventPayload::TurnActivity { activity } =
        &direct_event.event
    else {
        unreachable!("the loop returns only turn activity")
    };
    let lash::remote::usage::RemoteTurnEvent::ModelCallRecorded { record } = &activity.event else {
        unreachable!("the loop returns only model-call records")
    };
    assert!(!record.call_id.is_empty());
    assert!(!record.attempts.is_empty());

    let recovering_event = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let item = recovering
                .next()
                .await
                .expect("the recovering stream stays open")
                .expect("a recovering remote event");
            if let lash::observe::RemoteSessionObservationStreamItem::Event(event) = item
                && is_model_call(&event)
            {
                return event;
            }
        }
    })
    .await
    .expect("the recovering stream delivers the model call");
    assert_eq!(recovering_event.session_id, current.session_id);
    assert_eq!(recovering_event.cursor, direct_event.cursor);

    // The route's model selection commits first, with no turn; the turn's
    // own commit follows it.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let update = chat
                .next()
                .await
                .expect("the chat stream stays open")
                .expect("a chat update");
            if let lash::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
                event, ..
            } = update
                && event.turn_id.as_deref() == Some(turn_id.as_str())
            {
                return;
            }
        }
    })
    .await
    .expect("the chat stream delivers the turn's terminal replacement");
    drop((direct, recovering, chat, observable, session));
    workbench.shutdown().await;
}
