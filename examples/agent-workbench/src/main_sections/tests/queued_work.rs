//! A session's queued work on the workbench: the page lists and cancels
//! individual batches.

use super::*;
use lash::SessionId;

/// A context compaction queued for `session_id` under `source_key`, each its
/// own batch. It applies only at a turn boundary, so it stays pending while
/// a foreground turn holds the session.
fn queued_command_draft(
    session_id: &SessionId,
    source_key: &str,
) -> lash::persistence::QueuedWorkBatchDraft {
    lash::persistence::QueuedWorkBatchDraft::new(
        session_id.clone(),
        lash::persistence::DeliveryPolicy::AfterCurrentTurnCommit,
        lash::persistence::QueuedWorkPayload::session_command(
            lash::SessionCommand::CompactContext { instructions: None },
        ),
    )
    .with_source_key(source_key)
}

/// The page lists a session's pending batches in order and cancels one of
/// them: the cancelled batch leaves the queue, its cancellation is a queue
/// event a recent cursor replays, and the other batch stays. The engine
/// executes every pending batch; the page offers no run-one control. A
/// foreground turn holds the session while the page acts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workbench_lists_and_controls_individual_queued_batches() {
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let foreground = send_text(state, None, "hold the session")
        .await
        .expect("the foreground send is admitted");
    let foreground = started_turn_id(&foreground);
    assert_eq!(gate.next_call().await, 0);
    let session = state
        .open_session_for_observation(&session_id)
        .await
        .expect("observe the session");
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("a durable snapshot")
        .cursor;
    let first = state
        .session_store_factory
        .enqueue_queued_work(queued_command_draft(&session_id, "workbench-control-first"))
        .await
        .expect("enqueue the first batch");
    let second = state
        .session_store_factory
        .enqueue_queued_work(queued_command_draft(
            &session_id,
            "workbench-control-second",
        ))
        .await
        .expect("enqueue the second batch");

    let Json(listed) = list_queued_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list the queued work");
    assert_eq!(
        listed
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()]
    );
    let Json(cancelled) = cancel_queued_work_batch(
        AxumPath(first.batch_id.to_string()),
        State(state.clone()),
        Query(SessionQuery::default()),
    )
    .await
    .expect("cancel the first batch");
    assert!(cancelled.accepted);
    assert_eq!(cancelled.batch_id, first.batch_id.as_str());
    let Json(remaining) = list_queued_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list after the cancel");
    assert_eq!(
        remaining
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.batch_id.as_str()]
    );
    let lash::observe::SessionResume::Replayed { events } = session
        .observe()
        .resume_from_cursor(&cursor)
        .await
        .expect("resume the queue events")
    else {
        panic!("a recent cursor replays the queue events");
    };
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        lash::observe::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash::observe::SessionQueueEventKind::Cancelled
                && batch_ids.as_slice() == std::slice::from_ref(&first.batch_id)
    )));
    let Json(_) = cancel_queued_work_batch(
        AxumPath(second.batch_id.to_string()),
        State(state.clone()),
        Query(SessionQuery::default()),
    )
    .await
    .expect("cancel the second batch");
    drop(session);

    let timeline = include_str!("../../../assets/timeline.js");
    assert!(!timeline.contains("Run only this queued-work batch now"));
    assert!(timeline.contains("Cancel this pending queued-work batch"));
    gate.release(1);
    wait_for_turn_released(state, &session_id, &foreground, Duration::from_secs(30)).await;
    workbench.shutdown().await;
}
