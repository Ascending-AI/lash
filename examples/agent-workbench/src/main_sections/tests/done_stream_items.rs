//! Transient `Done` stream items publish to live observers.

use super::*;

/// A turn's `Done` reaches its live observers, and its settlement retires it
/// from the replayable snapshot: a page that reloads after the turn settled
/// replays no stale busy-state change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_stream_items_are_transient_and_not_snapshotted() {
    let workbench =
        Workbench::replying("<typescript>\nawait control.finish(\"done\");\n</typescript>").await;
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
