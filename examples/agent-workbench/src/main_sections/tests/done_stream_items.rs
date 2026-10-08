//! Transient `Done` stream items publish to live observers, and a host
//! trigger's dispatch never clears the busy state of a foreground turn.

use super::*;

/// A turn's `Done` reaches its live observers, and its settlement retires it
/// from the replayable snapshot: a page that reloads after the turn settled
/// replays no stale busy-state change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_stream_items_are_transient_and_not_snapshotted() {
    let workbench = Workbench::replying("<typescript>\nfinish(\"done\");\n</typescript>").await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let mut events = state.event_tx.subscribe(&session_id);

    let turn_id = run_turn(state, "answer once").await;

    assert_eq!(
        received_dones(&mut events),
        vec![(Some(turn_id.to_string()), TurnDoneOutcome::Completed)],
        "the live observer receives the turn's Done"
    );
    // The claim is released just before the settled rows retire.
    tokio::time::timeout(Duration::from_secs(30), async {
        while state
            .event_tx
            .snapshot(&session_id)
            .events
            .iter()
            .any(|event| matches!(event.item, StreamItem::Done { .. }))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the settled turn's Done leaves the snapshot");
    workbench.shutdown().await;
}

/// The `Done` items `events` received since the last call.
fn received_dones(
    events: &mut broadcast::Receiver<ProductEvent>,
) -> Vec<(Option<String>, TurnDoneOutcome)> {
    let mut dones = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let StreamItem::Done { turn_id, outcome } = event.item {
            dones.push((turn_id.map(|turn_id| turn_id.to_string()), outcome));
        }
    }
    dones
}

/// A button press during a foreground turn publishes no `Done`, so the
/// page's busy state stays the turn's; a press with no turn running
/// publishes one turn-less `Done` for its own dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trigger_dispatch_done_does_not_clear_an_active_turn() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let mut events = state.event_tx.subscribe(&session_id);
    let press = || {
        button_trigger(
            State(state.clone()),
            Query(SessionQuery::default()),
            Json(ButtonEventRequest {
                button: ButtonChoice::Red,
                model: None,
                model_variant: None,
            }),
        )
    };

    state.track_turn(&session_id, &TurnId::from("foreground-turn"));
    let Json(accepted) = press().await.expect("press during the foreground turn");
    assert!(accepted.accepted);
    assert_eq!(
        received_dones(&mut events),
        Vec::new(),
        "a trigger dispatch must not publish Done while a foreground turn is active"
    );

    state
        .active_turns
        .remove(&session_id, &TurnId::from("foreground-turn"));
    let Json(accepted) = press().await.expect("press with no turn running");
    assert!(accepted.accepted);
    assert_eq!(
        received_dones(&mut events),
        vec![(None, TurnDoneOutcome::Completed)],
        "an idle session's dispatch publishes its own Done"
    );
    workbench.shutdown().await;
}
