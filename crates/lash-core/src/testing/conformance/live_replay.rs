//! [`LiveReplayStore`] conformance: cursors, replay, subscriptions, trims.
//!
//! These vectors are the store-only portion of the ratified live-replay law
//! family. Laws which need the authoritative projection belong at the public
//! runtime seam rather than in a host-store fitness contract.

use super::*;
use crate::runtime::LiveReplayEventDraft;
use futures_util::StreamExt as _;

/// Run the full [`LiveReplayStore`] conformance suite against the backend
/// produced by `make`. `make` must return a fresh, empty store on each call.
///
/// This suite covers the non-durable live observation contract used for host
/// reconnects: cursors track per-session live positions, replay returns only
/// events after the cursor, subscriptions deliver buffered events before live
/// ones, malformed cursors fail before replay, and cursors ahead of the tail
/// report a recoverable unavailable gap.
pub async fn live_replay_store<F>(make: F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "live_replay_store");
    drop((first, second));
    exclusive_after_valid_cursor(make()).await;
    live_replay_store_cursor_preserves_newer_revisions(make()).await;
    live_replay_store_subscribe_replays_then_yields_live_events(make()).await;
    live_replay_store_rejects_malformed_cursors(make()).await;
    empty_is_proven_continuity_not_missing_history(make()).await;
    replay_cut_and_live_registration_are_linearizable(&make).await;
}

/// Run the capacity-trim portion of the [`LiveReplayStore`] conformance suite.
///
/// Together with [`live_replay_store_ttl_trim`], this states the store-owned
/// portion of `capacity_and_age_trim_force_snapshot`.
///
/// `make` must return a fresh store configured to retain exactly one event per
/// session. Stores with a fixed larger capacity should expose a test
/// configuration rather than weakening this contract.
pub async fn live_replay_store_capacity_trim<F>(make: F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "live_replay_store_capacity_trim");
    drop((first, second));
    let store = make();
    let revision = SessionRevision::new(1);
    let start = store.current_cursor("capacity-session", revision);
    let first = publish_one(
        &store,
        "capacity-session",
        revision,
        Some("capacity-turn"),
        live_replay_text_payload("capacity one"),
    )
    .expect("append first capacity event");
    publish_one(
        &store,
        "capacity-session",
        revision,
        Some("capacity-turn"),
        live_replay_text_payload("capacity two"),
    )
    .expect("append second capacity event");

    expect_live_replay_gap(
        store.replay_after_cursor(&start),
        LiveReplayGapReason::Trimmed,
        "capacity-trim replay from dropped cursor",
    );
    expect_live_replay_subscribe_gap(
        store.subscribe_after_cursor(&start),
        LiveReplayGapReason::Trimmed,
        "capacity-trim subscribe from dropped cursor",
    );
    let replay_after_first = expect_live_replay_replayed(
        store.replay_after_cursor(&first.cursor),
        "after first cursor",
    );
    assert_live_replay_labels(&replay_after_first, &["text:capacity two"]);
    let mut subscribe_after_first = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&first.cursor),
        "capacity-trim subscribe from retained boundary",
    );
    let retained =
        next_live_replay_event(&mut subscribe_after_first, "capacity-trim retained suffix").await;
    assert_live_replay_labels(&[retained], &["text:capacity two"]);

    let tail = store.current_cursor("capacity-session", revision);
    let tail_replay = expect_live_replay_replayed(
        store.replay_after_cursor(&tail),
        "capacity-trim replay from tail",
    );
    assert!(tail_replay.is_empty(), "capacity tail replay must be empty");
    let mut tail_subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&tail),
        "capacity-trim subscribe from tail",
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            futures_util::StreamExt::next(&mut tail_subscription),
        )
        .await
        .is_err(),
        "capacity tail subscription must wait for a future event"
    );
}

/// Run the TTL-trim portion of the [`LiveReplayStore`] conformance suite.
///
/// Together with [`live_replay_store_capacity_trim`], this states the
/// store-owned portion of `capacity_and_age_trim_force_snapshot`.
///
/// `make` must return a fresh store whose event TTL expires within
/// `expiration_wait`. The suite explicitly calls [`LiveReplayStore::trim_session`]
/// after waiting so implementations can keep trimming lazy and local.
pub async fn live_replay_store_ttl_trim<F>(make: F, expiration_wait: Duration)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "live_replay_store_ttl_trim");
    drop((first, second));
    let store = make();
    let revision = SessionRevision::new(1);
    let start = store.current_cursor("ttl-session", revision);
    publish_one(
        &store,
        "ttl-session",
        revision,
        Some("ttl-turn"),
        live_replay_text_payload("ttl expired"),
    )
    .expect("append ttl event");
    tokio::time::sleep(expiration_wait).await;
    store.trim_session("ttl-session").expect("trim ttl session");

    expect_live_replay_gap(
        store.replay_after_cursor(&start),
        LiveReplayGapReason::Trimmed,
        "ttl-trim replay from expired cursor",
    );
    expect_live_replay_subscribe_gap(
        store.subscribe_after_cursor(&start),
        LiveReplayGapReason::Trimmed,
        "ttl-trim subscribe from expired cursor",
    );

    let tail = store.current_cursor("ttl-session", revision);
    let tail_replay = expect_live_replay_replayed(
        store.replay_after_cursor(&tail),
        "ttl-trim replay from latest cursor",
    );
    assert!(
        tail_replay.is_empty(),
        "latest cursor after ttl trim must replay no events"
    );
    let mut tail_subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&tail),
        "ttl-trim subscribe from latest cursor",
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            futures_util::StreamExt::next(&mut tail_subscription),
        )
        .await
        .is_err(),
        "ttl tail subscription must wait for a future event"
    );
}

async fn exclusive_after_valid_cursor(store: Arc<dyn LiveReplayStore>) {
    let revision = SessionRevision::new(7);
    let start_a = store.current_cursor("session-a", revision);
    let start_b = store.current_cursor("session-b", revision);
    let empty = expect_live_replay_replayed(
        store.replay_after_cursor(&start_a),
        "empty replay from initial cursor",
    );
    assert!(empty.is_empty(), "initial cursor must replay no events");

    let first_a = publish_one(
        &store,
        "session-a",
        revision,
        Some("alpha-turn"),
        live_replay_text_payload("alpha one"),
    )
    .expect("append first session-a event");
    let first_b = publish_one(
        &store,
        "session-b",
        revision,
        None,
        SessionObservationEventPayload::ProcessChanged {
            kind: SessionProcessEventKind::Started,
            process_ids: vec!["proc-b".to_string()],
        },
    )
    .expect("append session-b event");
    let second_a = publish_one(
        &store,
        "session-a",
        SessionRevision::new(8),
        None,
        SessionObservationEventPayload::QueueChanged {
            kind: SessionQueueEventKind::Enqueued,
            batch_ids: vec!["batch-a".to_string()],
        },
    )
    .expect("append second session-a event");

    assert_eq!(first_a.session_id, "session-a");
    assert_eq!(first_a.revision, revision);
    assert_eq!(first_a.turn_id.as_deref(), Some("alpha-turn"));
    assert_eq!(second_a.revision, SessionRevision::new(8));
    assert_eq!(first_b.turn_id, None);
    assert_eq!(second_a.turn_id, None);
    assert_eq!(
        first_a.replay_incarnation_id, second_a.replay_incarnation_id,
        "one store construction must stamp one stable replay incarnation"
    );
    assert_eq!(
        first_a.replay_incarnation_id, first_b.replay_incarnation_id,
        "the replay incarnation is store-scoped rather than session-scoped"
    );
    assert_ne!(
        first_a.cursor.as_str(),
        second_a.cursor.as_str(),
        "each appended event must receive a distinct cursor"
    );
    assert_eq!(first_b.session_id, "session-b");

    let replay_a =
        expect_live_replay_replayed(store.replay_after_cursor(&start_a), "session-a replay");
    assert_live_replay_labels(&replay_a, &["text:alpha one", "queue:Enqueued:batch-a"]);

    let replay_a_after_first = expect_live_replay_replayed(
        store.replay_after_cursor(&first_a.cursor),
        "session-a replay after first event",
    );
    assert_live_replay_labels(&replay_a_after_first, &["queue:Enqueued:batch-a"]);

    let replay_b =
        expect_live_replay_replayed(store.replay_after_cursor(&start_b), "session-b replay");
    assert_live_replay_labels(&replay_b, &["process:Started:proc-b"]);

    let tail_a = store.current_cursor("session-a", SessionRevision::new(9));
    let replay_from_tail = expect_live_replay_replayed(
        store.replay_after_cursor(&tail_a),
        "session-a replay from tail cursor",
    );
    assert!(
        replay_from_tail.is_empty(),
        "current tail cursor must not replay old events"
    );

    let mut from_start = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&start_a),
        "session-a subscribe from initial cursor",
    );
    let subscribed_first = next_live_replay_event(&mut from_start, "first exclusive event").await;
    let subscribed_second = next_live_replay_event(&mut from_start, "second exclusive event").await;
    assert_live_replay_labels(
        &[subscribed_first, subscribed_second],
        &["text:alpha one", "queue:Enqueued:batch-a"],
    );

    let mut after_first = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&first_a.cursor),
        "session-a subscribe after first cursor",
    );
    let subscribed_suffix =
        next_live_replay_event(&mut after_first, "exclusive suffix event").await;
    assert_live_replay_labels(&[subscribed_suffix], &["queue:Enqueued:batch-a"]);

    let mut at_tail = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&second_a.cursor),
        "session-a subscribe at tail",
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            futures_util::StreamExt::next(&mut at_tail),
        )
        .await
        .is_err(),
        "a valid tail subscription must wait rather than gap or replay"
    );
}

async fn live_replay_store_cursor_preserves_newer_revisions(store: Arc<dyn LiveReplayStore>) {
    publish_one(
        &store,
        "stale-snapshot-session",
        SessionRevision::new(2),
        Some("worker-turn"),
        live_replay_text_payload("newer worker commit"),
    )
    .expect("append newer worker event");

    let stale_snapshot_cursor =
        store.current_cursor("stale-snapshot-session", SessionRevision::new(1));
    let replay = expect_live_replay_replayed(
        store.replay_after_cursor(&stale_snapshot_cursor),
        "newer revision after stale snapshot",
    );
    assert_live_replay_labels(&replay, &["text:newer worker commit"]);
}

async fn live_replay_store_subscribe_replays_then_yields_live_events(
    store: Arc<dyn LiveReplayStore>,
) {
    let revision = SessionRevision::new(3);
    let start = store.current_cursor("subscribe-session", revision);
    publish_one(
        &store,
        "subscribe-session",
        revision,
        Some("subscribe-turn"),
        live_replay_text_payload("buffered one"),
    )
    .expect("append first buffered event");
    publish_one(
        &store,
        "subscribe-session",
        revision,
        Some("subscribe-turn"),
        live_replay_text_payload("buffered two"),
    )
    .expect("append second buffered event");

    let mut subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&start),
        "subscribe after initial cursor",
    );
    let first = next_live_replay_event(&mut subscription, "first buffered event").await;
    let second = next_live_replay_event(&mut subscription, "second buffered event").await;
    assert_live_replay_labels(
        &[first, second],
        &["text:buffered one", "text:buffered two"],
    );

    publish_one(
        &store,
        "subscribe-session",
        revision,
        Some("subscribe-turn"),
        live_replay_text_payload("live three"),
    )
    .expect("append live event");
    let live = next_live_replay_event(&mut subscription, "live event after replay").await;
    assert_live_replay_labels(&[live], &["text:live three"]);
}

async fn live_replay_store_rejects_malformed_cursors(store: Arc<dyn LiveReplayStore>) {
    let malformed: crate::SessionCursor =
        serde_json::from_value(serde_json::json!("not-a-session-cursor"))
            .expect("construct malformed cursor through public serde surface");
    assert!(
        matches!(
            store.replay_after_cursor(&malformed),
            Err(LiveReplayStoreError::Cursor(
                crate::SessionCursorError::Malformed { .. }
            ))
        ),
        "replay must reject malformed cursors before reading replay state"
    );
    assert!(
        matches!(
            store.subscribe_after_cursor(&malformed),
            Err(LiveReplayStoreError::Cursor(
                crate::SessionCursorError::Malformed { .. }
            ))
        ),
        "subscribe must reject malformed cursors before reading replay state"
    );
}

async fn empty_is_proven_continuity_not_missing_history(store: Arc<dyn LiveReplayStore>) {
    let revision = SessionRevision::new(4);
    let existing = publish_one(
        &store,
        "ahead-session",
        revision,
        Some("ahead-turn"),
        live_replay_text_payload("existing"),
    )
    .expect("append existing event");
    let tail_replay = expect_live_replay_replayed(
        store.replay_after_cursor(&existing.cursor),
        "replay from proven tail",
    );
    assert!(tail_replay.is_empty(), "only a valid tail may replay empty");
    let mut tail_subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&existing.cursor),
        "subscribe from proven tail",
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            futures_util::StreamExt::next(&mut tail_subscription),
        )
        .await
        .is_err(),
        "a proven tail subscription must remain live and empty"
    );

    let ahead = crate::SessionCursor::new("ahead-session", revision, 99);

    expect_live_replay_gap(
        store.replay_after_cursor(&ahead),
        LiveReplayGapReason::Unavailable,
        "replay from cursor ahead of tail",
    );
    expect_live_replay_subscribe_gap(
        store.subscribe_after_cursor(&ahead),
        LiveReplayGapReason::Unavailable,
        "subscribe from cursor ahead of tail",
    );
}

async fn replay_cut_and_live_registration_are_linearizable<F>(make: &F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    const RACES: usize = 64;
    for race in 0..RACES {
        let store = make();
        let session_id = format!("subscribe-race-{race}");
        let revision = SessionRevision::new(5);
        let start = store.current_cursor(&session_id, revision);
        let prior = publish_one(
            &store,
            &session_id,
            revision,
            Some("race-turn"),
            live_replay_text_payload("prior"),
        )
        .expect("append prior event");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let subscribe_store = Arc::clone(&store);
        let subscribe_cursor = start.clone();
        let subscribe_barrier = Arc::clone(&barrier);
        let subscribe = tokio::task::spawn_blocking(move || {
            subscribe_barrier.wait();
            subscribe_store.subscribe_after_cursor(&subscribe_cursor)
        });
        let append_store = Arc::clone(&store);
        let append_session_id = session_id.clone();
        let append_barrier = Arc::clone(&barrier);
        let append = tokio::task::spawn_blocking(move || {
            append_barrier.wait();
            publish_one(
                &append_store,
                &append_session_id,
                revision,
                Some("race-turn"),
                live_replay_text_payload("raced"),
            )
        });
        barrier.wait();

        let mut subscription = expect_live_replay_subscribed(
            subscribe.await.expect("join racing subscription"),
            "racing subscription",
        );
        let raced = append
            .await
            .expect("join racing append")
            .expect("racing append");
        let first = next_live_replay_event(&mut subscription, "prior racing event").await;
        let second = next_live_replay_event(&mut subscription, "concurrent racing event").await;
        assert_eq!(first.cursor, prior.cursor, "prior event must remain first");
        assert_eq!(
            second.cursor, raced.cursor,
            "raced event must appear exactly once"
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(2),
                futures_util::StreamExt::next(&mut subscription),
            )
            .await
            .is_err(),
            "the append racing subscription creation must not be duplicated"
        );
    }
}

fn live_replay_text_payload(text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(TurnActivity::independent(
        TurnEvent::AssistantProseDelta { text: text.into() },
    ))
}

fn publish_one(
    store: &Arc<dyn LiveReplayStore>,
    session_id: &str,
    revision: SessionRevision,
    turn_id: Option<&str>,
    payload: SessionObservationEventPayload,
) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError> {
    let prepared = store.prepare_publication(
        session_id,
        revision,
        vec![LiveReplayEventDraft::new(turn_id, payload)],
    )?;
    store
        .publish_prepared(prepared)?
        .into_iter()
        .next()
        .ok_or_else(|| LiveReplayStoreError::Store("published batch was empty".to_string()))
}

fn expect_live_replay_replayed(
    result: Result<LiveReplayOutcome, LiveReplayStoreError>,
    context: &str,
) -> Vec<Arc<SessionObservationEvent>> {
    match result.expect(context) {
        LiveReplayOutcome::Replayed(events) => events,
        LiveReplayOutcome::Gap(reason) => {
            panic!("{context}: expected replayed events, got gap {reason:?}")
        }
    }
}

fn expect_live_replay_gap(
    result: Result<LiveReplayOutcome, LiveReplayStoreError>,
    expected: LiveReplayGapReason,
    context: &str,
) {
    match result.expect(context) {
        LiveReplayOutcome::Gap(reason) => assert_eq!(reason, expected, "{context}"),
        LiveReplayOutcome::Replayed(events) => {
            panic!(
                "{context}: expected gap {expected:?}, got {} events",
                events.len()
            )
        }
    }
}

fn expect_live_replay_subscribed(
    result: Result<LiveReplaySubscribeOutcome, LiveReplayStoreError>,
    context: &str,
) -> crate::LiveReplaySubscription {
    match result.expect(context) {
        LiveReplaySubscribeOutcome::Subscribed(subscription) => subscription,
        LiveReplaySubscribeOutcome::Gap(reason) => {
            panic!("{context}: expected subscription, got gap {reason:?}")
        }
    }
}

fn expect_live_replay_subscribe_gap(
    result: Result<LiveReplaySubscribeOutcome, LiveReplayStoreError>,
    expected: LiveReplayGapReason,
    context: &str,
) {
    match result.expect(context) {
        LiveReplaySubscribeOutcome::Gap(reason) => assert_eq!(reason, expected, "{context}"),
        LiveReplaySubscribeOutcome::Subscribed(_) => {
            panic!("{context}: expected subscribe gap {expected:?}, got subscription")
        }
    }
}

async fn next_live_replay_event(
    subscription: &mut crate::LiveReplaySubscription,
    context: &str,
) -> Arc<SessionObservationEvent> {
    tokio::time::timeout(Duration::from_secs(1), subscription.next())
        .await
        .unwrap_or_else(|_| panic!("{context}: timed out waiting for live replay event"))
        .unwrap_or_else(|| panic!("{context}: live replay subscriber closed"))
        .unwrap_or_else(|err| panic!("{context}: live replay subscriber failed: {err}"))
}

fn assert_live_replay_labels(events: &[Arc<SessionObservationEvent>], expected: &[&str]) {
    let labels = events
        .iter()
        .map(|event| live_replay_event_label(event))
        .collect::<Vec<_>>();
    let expected = expected
        .iter()
        .map(|label| label.to_string())
        .collect::<Vec<_>>();
    assert_eq!(labels, expected, "replayed event payloads must match");
}

fn live_replay_event_label(event: &SessionObservationEvent) -> String {
    match &event.payload {
        SessionObservationEventPayload::TurnActivity(activity) => match &activity.event {
            TurnEvent::AssistantProseDelta { text } => format!("text:{text}"),
            other => format!("turn:{other:?}"),
        },
        SessionObservationEventPayload::Committed { .. } => "committed".to_string(),
        SessionObservationEventPayload::ResidentChanged { .. } => "resident_changed".to_string(),
        SessionObservationEventPayload::AgentFrameSwitched { frame_id } => {
            format!("frame:{frame_id}")
        }
        SessionObservationEventPayload::QueueChanged { kind, batch_ids } => {
            format!("queue:{kind:?}:{}", batch_ids.join(","))
        }
        SessionObservationEventPayload::ProcessChanged { kind, process_ids } => {
            format!("process:{kind:?}:{}", process_ids.join(","))
        }
    }
}
