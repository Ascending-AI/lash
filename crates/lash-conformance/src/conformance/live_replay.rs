//! [`LiveReplayStore`] conformance: cursors, replay, subscriptions, trims.
//!
//! These vectors are the store-only portion of the ratified live-replay law
//! family. Laws which need the authoritative projection belong at the public
//! runtime seam rather than in a host-store fitness contract.

use super::*;
use crate::runtime::LiveReplayEventDraft;
use futures_util::StreamExt as _;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

/// `make` must return a fresh, empty store on each call.
///
/// This suite covers the non-durable live observation contract used for host
/// reconnects: cursors track per-session live positions, replay returns only
/// events after the cursor, the earliest cursor replays the whole window,
/// subscriptions deliver buffered events before live ones, malformed cursors
/// fail before replay, cursors ahead of the tail
/// report a recoverable unavailable gap, and a redrive of streamed deltas,
/// framed alike or not, adds no text twice and loses none silently.
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
    the_earliest_cursor_replays_the_whole_window(make()).await;
    live_replay_store_subscribe_replays_then_yields_live_events(make()).await;
    live_replay_store_rejects_malformed_cursors(make()).await;
    empty_is_proven_continuity_not_missing_history(make()).await;
    replay_cut_and_live_registration_are_linearizable(&make).await;
    a_redrive_adds_no_streamed_text_twice_and_loses_none(make()).await;
    concurrent_writers_share_one_gap_free_order(make()).await;
}

/// Writers [`concurrent_writers_share_one_gap_free_order`] races on one
/// session, as the run's runtime and a queue or process publisher on
/// another process do.
const CONCURRENT_WRITERS: usize = 2;
/// Batches each concurrent writer publishes; every third holds two events.
const CONCURRENT_BATCHES: usize = 120;

/// The store is each session's sequencer: writers racing on one session
/// receive contiguous positions in one total order, each writer's events
/// keep its own order, and a subscriber from before the race and a replay
/// after it see that same order, with nothing missing or repeated (FIG-5099).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn concurrent_writers_share_one_gap_free_order(store: Arc<dyn LiveReplayStore>) {
    let session_id = SessionId::from("concurrent-writers");
    let revision = SessionRevision::new(3);
    let start = store.current_cursor(&session_id, revision);
    let mut subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&start).await,
        "subscribe before the writers race",
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(CONCURRENT_WRITERS));
    let writers = (0..CONCURRENT_WRITERS)
        .map(|writer| {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                let mut published = Vec::new();
                for batch in 0..CONCURRENT_BATCHES {
                    let size = if batch % 3 == 0 { 2 } else { 1 };
                    let drafts = (0..size)
                        .map(|part| {
                            LiveReplayEventDraft::new(
                                None::<TurnId>,
                                live_replay_text_payload(&format!(
                                    "writer {writer}:{batch}.{part}"
                                )),
                            )
                        })
                        .collect();
                    let events = store
                        .publish(&session_id, revision, drafts)
                        .await
                        .expect("a racing writer publishes");
                    assert_eq!(events.len(), size, "a batch publishes whole");
                    published.extend(events);
                    if batch % 8 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
                published
            })
        })
        .collect::<Vec<_>>();
    let mut published = Vec::new();
    for writer in writers {
        published.push(writer.await.expect("join a racing writer"));
    }
    let total = published.iter().map(Vec::len).sum::<usize>();

    let position = |event: &SessionObservationEvent| {
        event
            .cursor
            .parse_for_session(&session_id)
            .expect("a published cursor names its session")
            .live_position
    };
    for events in &published {
        assert!(
            events
                .windows(2)
                .all(|pair| position(&pair[0]) < position(&pair[1])),
            "a writer's events keep the order it published them in"
        );
    }
    let replayed = expect_live_replay_replayed(
        store.replay_after_cursor(&start).await,
        "replay after the race",
    );
    assert_eq!(
        replayed.len(),
        total,
        "a replay holds every raced event once"
    );
    let first = position(&replayed[0]);
    assert!(
        replayed
            .iter()
            .enumerate()
            .all(|(offset, event)| position(event) == first + offset as u64),
        "raced writers share one contiguous position sequence"
    );
    let mut published_positions = published
        .iter()
        .flatten()
        .map(|event| position(event))
        .collect::<Vec<_>>();
    published_positions.sort_unstable();
    assert_eq!(
        published_positions,
        replayed
            .iter()
            .map(|event| position(event))
            .collect::<Vec<_>>(),
        "the replay holds exactly the events the writers published"
    );
    let mut live = Vec::with_capacity(total);
    while live.len() < total {
        live.push(next_live_replay_event(&mut subscription, "raced event").await);
    }
    assert_eq!(
        live.iter()
            .map(|event| event.cursor.clone())
            .collect::<Vec<_>>(),
        replayed
            .iter()
            .map(|event| event.cursor.clone())
            .collect::<Vec<_>>(),
        "a subscriber from before the race sees the replay's order"
    );
}

/// Sessions [`live_replay_store_burst`] publishes to at once.
const BURST_SESSIONS: usize = 6;
/// Deltas each burst session publishes: within the default per-session
/// retention, so every subscriber can hold the whole burst.
const BURST_DELTAS: usize = 600;

/// A burst of deltas across several sessions reaches every subscriber with
/// no loss, no reordering, no duplicate and no other session's event: one
/// subscribed before the burst, one subscribed from the same cursor while it
/// runs, and a replay after it ends (FIG-5090).
///
/// `make` must return a fresh store whose per-session retention and
/// subscriber buffer hold [`BURST_DELTAS`] events.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn live_replay_store_burst<F>(make: F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    let store = make();
    let revision = SessionRevision::new(4);
    let sessions = (0..BURST_SESSIONS)
        .map(|index| SessionId::fixture(format!("burst-session-{index}")))
        .collect::<Vec<_>>();
    let expected = |index: usize| {
        (0..BURST_DELTAS)
            .map(|delta| format!("text:burst {index}:{delta}"))
            .collect::<Vec<_>>()
    };
    let starts = sessions
        .iter()
        .map(|session_id| store.current_cursor(session_id, revision))
        .collect::<Vec<_>>();
    let drain = |subscription: crate::LiveReplaySubscription| {
        tokio::spawn(async move {
            let mut subscription = subscription;
            let mut labels = Vec::with_capacity(BURST_DELTAS);
            while labels.len() < BURST_DELTAS {
                let event = next_live_replay_event(&mut subscription, "burst delta").await;
                labels.push(live_replay_event_label(&event));
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(20), subscription.next())
                    .await
                    .is_err(),
                "a burst subscriber receives no event past the burst"
            );
            labels
        })
    };
    let mut early = Vec::with_capacity(starts.len());
    for start in &starts {
        early.push(drain(expect_live_replay_subscribed(
            store.subscribe_after_cursor(start).await,
            "subscribe before the burst",
        )));
    }

    let publishers = sessions
        .iter()
        .enumerate()
        .map(|(index, session_id)| {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            tokio::spawn(async move {
                for delta in 0..BURST_DELTAS {
                    publish_one(
                        &store,
                        &session_id,
                        revision,
                        Some(&TurnId::from("burst-turn")),
                        live_replay_text_payload(&format!("burst {index}:{delta}")),
                    )
                    .await
                    .expect("publish a burst delta");
                    if delta % 64 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    tokio::task::yield_now().await;
    let mut late = Vec::with_capacity(starts.len());
    for start in &starts {
        late.push(drain(expect_live_replay_subscribed(
            store.subscribe_after_cursor(start).await,
            "subscribe during the burst",
        )));
    }
    for publisher in publishers {
        publisher.await.expect("join a burst publisher");
    }

    for (index, (early, late)) in early.into_iter().zip(late).enumerate() {
        assert_eq!(
            early.await.expect("join an early burst subscriber"),
            expected(index),
            "a subscriber from before the burst sees every delta once, in order"
        );
        assert_eq!(
            late.await.expect("join a late burst subscriber"),
            expected(index),
            "a subscriber joining mid-burst sees every delta once, in order"
        );
        let replayed = expect_live_replay_replayed(
            store.replay_after_cursor(&starts[index]).await,
            "replay after the burst",
        )
        .iter()
        .map(|event| live_replay_event_label(event))
        .collect::<Vec<_>>();
        assert_eq!(
            replayed,
            expected(index),
            "a replay after the burst holds every delta once, in order"
        );
    }
}

/// Together with [`live_replay_store_ttl_trim`], this states the store-owned
/// portion of `capacity_and_age_trim_force_snapshot`.
///
/// `make` must return a fresh store configured to retain exactly one event per
/// session. Stores with a fixed larger capacity should expose a test
/// configuration rather than weakening this contract.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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
    let start = store.current_cursor(&SessionId::from("capacity-session"), revision);
    let first = publish_one(
        &store,
        &SessionId::from("capacity-session"),
        revision,
        Some(&TurnId::from("capacity-turn")),
        live_replay_text_payload("capacity one"),
    )
    .await
    .expect("append first capacity event");
    publish_one(
        &store,
        &SessionId::from("capacity-session"),
        revision,
        Some(&TurnId::from("capacity-turn")),
        live_replay_text_payload("capacity two"),
    )
    .await
    .expect("append second capacity event");

    expect_live_replay_gap(
        store.replay_after_cursor(&start).await,
        LiveReplayGapReason::Trimmed,
        "capacity-trim replay from dropped cursor",
    );
    expect_live_replay_subscribe_gap(
        store.subscribe_after_cursor(&start).await,
        LiveReplayGapReason::Trimmed,
        "capacity-trim subscribe from dropped cursor",
    );
    let replay_after_first = expect_live_replay_replayed(
        store.replay_after_cursor(&first.cursor).await,
        "after first cursor",
    );
    assert_live_replay_labels(&replay_after_first, &["text:capacity two"]);
    let mut subscribe_after_first = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&first.cursor).await,
        "capacity-trim subscribe from retained boundary",
    );
    let retained =
        next_live_replay_event(&mut subscribe_after_first, "capacity-trim retained suffix").await;
    assert_live_replay_labels(&[retained], &["text:capacity two"]);

    let tail = store.current_cursor(&SessionId::from("capacity-session"), revision);
    let tail_replay = expect_live_replay_replayed(
        store.replay_after_cursor(&tail).await,
        "capacity-trim replay from tail",
    );
    assert!(tail_replay.is_empty(), "capacity tail replay must be empty");
    let mut tail_subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&tail).await,
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

/// Together with [`live_replay_store_capacity_trim`], this states the
/// store-owned portion of `capacity_and_age_trim_force_snapshot`.
///
/// `make` must return a fresh store whose event TTL expires within
/// `expiration_wait`. The suite explicitly calls [`LiveReplayStore::trim_session`]
/// after waiting so implementations can keep trimming lazy and local.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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
    let start = store.current_cursor(&SessionId::from("ttl-session"), revision);
    publish_one(
        &store,
        &SessionId::from("ttl-session"),
        revision,
        Some(&TurnId::from("ttl-turn")),
        live_replay_text_payload("ttl expired"),
    )
    .await
    .expect("append ttl event");
    tokio::time::sleep(expiration_wait).await;
    store
        .trim_session(&SessionId::from("ttl-session"))
        .await
        .expect("trim ttl session");

    expect_live_replay_gap(
        store.replay_after_cursor(&start).await,
        LiveReplayGapReason::Trimmed,
        "ttl-trim replay from expired cursor",
    );
    expect_live_replay_subscribe_gap(
        store.subscribe_after_cursor(&start).await,
        LiveReplayGapReason::Trimmed,
        "ttl-trim subscribe from expired cursor",
    );

    let tail = store.current_cursor(&SessionId::from("ttl-session"), revision);
    let tail_replay = expect_live_replay_replayed(
        store.replay_after_cursor(&tail).await,
        "ttl-trim replay from latest cursor",
    );
    assert!(
        tail_replay.is_empty(),
        "latest cursor after ttl trim must replay no events"
    );
    let mut tail_subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&tail).await,
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

/// Law 9: a fresh replay incarnation invalidates an old cursor, while a store
/// which genuinely preserves both history and incarnation may continue it.
///
/// `original` and `preserved` must be distinct handles over the same replay
/// substrate. `fresh` must start empty with a new incarnation. The vector
/// deliberately creates the same numeric position in `fresh`, so a backend
/// which compares offsets but not incarnation returns a forbidden clean empty
/// replay and fails deterministically.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn incarnation_change_invalidates_cursor(
    original: Arc<dyn LiveReplayStore>,
    fresh: Arc<dyn LiveReplayStore>,
    preserved: Arc<dyn LiveReplayStore>,
) {
    assert!(
        !Arc::ptr_eq(&original, &preserved),
        "preserved-history conformance requires a distinct reopened handle"
    );
    let revision = SessionRevision::new(9);
    let session_id = "incarnation-change-session";
    let old_event = publish_one(
        &original,
        &SessionId::from(session_id),
        revision,
        Some(&TurnId::from("old-turn")),
        live_replay_text_payload("old incarnation"),
    )
    .await
    .expect("publish old-incarnation event");

    let fresh_event = publish_one(
        &fresh,
        &SessionId::from(session_id),
        revision,
        Some(&TurnId::from("fresh-turn")),
        live_replay_text_payload("fresh incarnation numeric collision"),
    )
    .await
    .expect("publish fresh-incarnation event");
    assert_ne!(
        old_event.cursor, fresh_event.cursor,
        "equal numeric positions in different incarnations must produce distinct cursors"
    );
    expect_live_replay_gap(
        fresh.replay_after_cursor(&old_event.cursor).await,
        LiveReplayGapReason::Unavailable,
        "fresh incarnation replay from old cursor",
    );
    expect_live_replay_subscribe_gap(
        fresh.subscribe_after_cursor(&old_event.cursor).await,
        LiveReplayGapReason::Unavailable,
        "fresh incarnation subscribe from old cursor",
    );

    let preserved_tail = expect_live_replay_replayed(
        preserved.replay_after_cursor(&old_event.cursor).await,
        "preserved incarnation replay from old tail",
    );
    assert!(
        preserved_tail.is_empty(),
        "a preserved incarnation may prove its old tail is clean empty"
    );
    publish_one(
        &preserved,
        &SessionId::from(session_id),
        revision,
        Some(&TurnId::from("continued-turn")),
        live_replay_text_payload("preserved continuation"),
    )
    .await
    .expect("publish through reopened preserved store");
    let continuation = expect_live_replay_replayed(
        preserved.replay_after_cursor(&old_event.cursor).await,
        "preserved incarnation continuation",
    );
    assert_live_replay_labels(&continuation, &["text:preserved continuation"]);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn exclusive_after_valid_cursor(store: Arc<dyn LiveReplayStore>) {
    let revision = SessionRevision::new(7);
    let start_a = store.current_cursor(&SessionId::from("session-a"), revision);
    let start_b = store.current_cursor(&SessionId::from("session-b"), revision);
    let empty = expect_live_replay_replayed(
        store.replay_after_cursor(&start_a).await,
        "empty replay from initial cursor",
    );
    assert!(empty.is_empty(), "initial cursor must replay no events");

    let first_a = publish_one(
        &store,
        &SessionId::from("session-a"),
        revision,
        Some(&TurnId::from("alpha-turn")),
        live_replay_text_payload("alpha one"),
    )
    .await
    .expect("append first session-a event");
    let first_b = publish_one(
        &store,
        &SessionId::from("session-b"),
        revision,
        None,
        SessionObservationEventPayload::ProcessChanged {
            kind: SessionProcessEventKind::Started { sequence: 1 },
            process_ids: vec![crate::ProcessId::fixture("proc-b")],
        },
    )
    .await
    .expect("append session-b event");
    let second_a = publish_one(
        &store,
        &SessionId::from("session-a"),
        SessionRevision::new(8),
        None,
        SessionObservationEventPayload::QueueChanged {
            kind: SessionQueueEventKind::Enqueued,
            batch_ids: vec!["batch-a".to_string()],
        },
    )
    .await
    .expect("append second session-a event");

    assert_eq!(first_a.session_id(), "session-a");
    assert_eq!(first_a.revision(), revision);
    assert_eq!(first_a.turn_id.as_deref(), Some("alpha-turn"));
    assert_eq!(second_a.revision(), SessionRevision::new(8));
    assert_eq!(first_b.turn_id, None);
    assert_eq!(second_a.turn_id, None);
    assert_eq!(
        first_a.replay_incarnation_id(),
        second_a.replay_incarnation_id(),
        "one store construction must stamp one stable replay incarnation"
    );
    assert_eq!(
        first_a.replay_incarnation_id(),
        first_b.replay_incarnation_id(),
        "the replay incarnation is store-scoped rather than session-scoped"
    );
    assert_ne!(
        first_a.cursor.as_str(),
        second_a.cursor.as_str(),
        "each appended event must receive a distinct cursor"
    );
    assert_eq!(first_b.session_id(), "session-b");

    let replay_a = expect_live_replay_replayed(
        store.replay_after_cursor(&start_a).await,
        "session-a replay",
    );
    assert_live_replay_labels(&replay_a, &["text:alpha one", "queue:Enqueued:batch-a"]);

    let replay_a_after_first = expect_live_replay_replayed(
        store.replay_after_cursor(&first_a.cursor).await,
        "session-a replay after first event",
    );
    assert_live_replay_labels(&replay_a_after_first, &["queue:Enqueued:batch-a"]);

    let replay_b = expect_live_replay_replayed(
        store.replay_after_cursor(&start_b).await,
        "session-b replay",
    );
    let started_b = format!(
        "process:Started {{ sequence: 1 }}:{}",
        crate::ProcessId::fixture("proc-b")
    );
    assert_live_replay_labels(&replay_b, &[started_b.as_str()]);

    let tail_a = store.current_cursor(&SessionId::from("session-a"), SessionRevision::new(9));
    let replay_from_tail = expect_live_replay_replayed(
        store.replay_after_cursor(&tail_a).await,
        "session-a replay from tail cursor",
    );
    assert!(
        replay_from_tail.is_empty(),
        "current tail cursor must not replay old events"
    );

    let mut from_start = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&start_a).await,
        "session-a subscribe from initial cursor",
    );
    let subscribed_first = next_live_replay_event(&mut from_start, "first exclusive event").await;
    let subscribed_second = next_live_replay_event(&mut from_start, "second exclusive event").await;
    assert_live_replay_labels(
        &[subscribed_first, subscribed_second],
        &["text:alpha one", "queue:Enqueued:batch-a"],
    );

    let mut after_first = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&first_a.cursor).await,
        "session-a subscribe after first cursor",
    );
    let subscribed_suffix =
        next_live_replay_event(&mut after_first, "exclusive suffix event").await;
    assert_live_replay_labels(&[subscribed_suffix], &["queue:Enqueued:batch-a"]);

    let mut at_tail = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&second_a.cursor).await,
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn live_replay_store_cursor_preserves_newer_revisions(store: Arc<dyn LiveReplayStore>) {
    publish_one(
        &store,
        &SessionId::from("stale-snapshot-session"),
        SessionRevision::new(2),
        Some(&TurnId::from("worker-turn")),
        live_replay_text_payload("newer worker commit"),
    )
    .await
    .expect("append newer worker event");

    let stale_snapshot_cursor = store.current_cursor(
        &SessionId::from("stale-snapshot-session"),
        SessionRevision::new(1),
    );
    let replay = expect_live_replay_replayed(
        store.replay_after_cursor(&stale_snapshot_cursor).await,
        "newer revision after stale snapshot",
    );
    assert_live_replay_labels(&replay, &["text:newer worker commit"]);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn live_replay_store_subscribe_replays_then_yields_live_events(
    store: Arc<dyn LiveReplayStore>,
) {
    let revision = SessionRevision::new(3);
    let start = store.current_cursor(&SessionId::from("subscribe-session"), revision);
    publish_one(
        &store,
        &SessionId::from("subscribe-session"),
        revision,
        Some(&TurnId::from("subscribe-turn")),
        live_replay_text_payload("buffered one"),
    )
    .await
    .expect("append first buffered event");
    publish_one(
        &store,
        &SessionId::from("subscribe-session"),
        revision,
        Some(&TurnId::from("subscribe-turn")),
        live_replay_text_payload("buffered two"),
    )
    .await
    .expect("append second buffered event");

    let mut subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&start).await,
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
        &SessionId::from("subscribe-session"),
        revision,
        Some(&TurnId::from("subscribe-turn")),
        live_replay_text_payload("live three"),
    )
    .await
    .expect("append live event");
    let live = next_live_replay_event(&mut subscription, "live event after replay").await;
    assert_live_replay_labels(&[live], &["text:live three"]);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn live_replay_store_rejects_malformed_cursors(store: Arc<dyn LiveReplayStore>) {
    let malformed: crate::SessionCursor =
        serde_json::from_value(serde_json::json!("not-a-session-cursor"))
            .expect("construct malformed cursor through public serde surface");
    assert!(
        matches!(
            store.replay_after_cursor(&malformed).await,
            Err(LiveReplayStoreError::Cursor(
                crate::SessionCursorError::Malformed { .. }
            ))
        ),
        "replay must reject malformed cursors before reading replay state"
    );
    assert!(
        matches!(
            store.subscribe_after_cursor(&malformed).await,
            Err(LiveReplayStoreError::Cursor(
                crate::SessionCursorError::Malformed { .. }
            ))
        ),
        "subscribe must reject malformed cursors before reading replay state"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn empty_is_proven_continuity_not_missing_history(store: Arc<dyn LiveReplayStore>) {
    let revision = SessionRevision::new(4);
    let existing = publish_one(
        &store,
        &SessionId::from("ahead-session"),
        revision,
        Some(&TurnId::from("ahead-turn")),
        live_replay_text_payload("existing"),
    )
    .await
    .expect("append existing event");
    let tail_replay = expect_live_replay_replayed(
        store.replay_after_cursor(&existing.cursor).await,
        "replay from proven tail",
    );
    assert!(tail_replay.is_empty(), "only a valid tail may replay empty");
    let mut tail_subscription = expect_live_replay_subscribed(
        store.subscribe_after_cursor(&existing.cursor).await,
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

    let ahead = crate::SessionCursor::new(
        existing.replay_incarnation_id(),
        "ahead-session",
        revision,
        99,
    );

    expect_live_replay_gap(
        store.replay_after_cursor(&ahead).await,
        LiveReplayGapReason::Unavailable,
        "replay from cursor ahead of tail",
    );
    expect_live_replay_subscribe_gap(
        store.subscribe_after_cursor(&ahead).await,
        LiveReplayGapReason::Unavailable,
        "subscribe from cursor ahead of tail",
    );
}

/// A redrive republishes the activities its first attempt delivered under
/// the ids the same observations derive: `{key}#{ordinal}` for one delta,
/// `{key}#{first}..{last}` for a frame of them (FIG-5098). It may frame them
/// differently. A store drops every redelivery inside what it delivered of
/// that key, so no text lands twice; publishes what lies beyond it, so none
/// is lost; and answers a frame straddling its edge, whose undelivered text
/// cannot be cut from its delivered text, with a gap rather than either.
async fn a_redrive_adds_no_streamed_text_twice_and_loses_none(store: Arc<dyn LiveReplayStore>) {
    let session = SessionId::from("framed-redrive");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);

    assert_eq!(
        deliver_framed(
            &store,
            &session,
            revision,
            &[("k#0", "a"), ("k#1..3", "bcd")]
        )
        .await,
        vec!["text:a", "text:bcd"]
    );
    assert!(
        deliver_framed(
            &store,
            &session,
            revision,
            &[("k#0", "a"), ("k#1", "b"), ("k#2", "c"), ("k#3", "d")]
        )
        .await
        .is_empty(),
        "the unmerged originals of a delivered frame are redeliveries"
    );
    assert!(
        deliver_framed(&store, &session, revision, &[("k#1..2", "bc")])
            .await
            .is_empty(),
        "a different framing inside the delivered range is a redelivery"
    );
    assert_eq!(
        deliver_framed(
            &store,
            &session,
            revision,
            &[("k#3", "d"), ("k#4..5", "ef")]
        )
        .await,
        vec!["text:ef"],
        "what lies beyond the delivered range is published"
    );
    let replayed = expect_live_replay_replayed(
        store.replay_after_cursor(&start).await,
        "replay after the redrive",
    );
    assert_live_replay_labels(&replayed, &["text:a", "text:bcd", "text:ef"]);

    assert!(
        deliver_framed(&store, &session, revision, &[("k#5..7", "fgh")])
            .await
            .is_empty(),
        "a frame straddling the delivered range is not published"
    );
    expect_live_replay_gap(
        store.replay_after_cursor(&start).await,
        LiveReplayGapReason::Unavailable,
        "replay across a straddling redelivery",
    );
}

/// Publish `drafts` as framed text deltas, answering the published labels.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn deliver_framed(
    store: &Arc<dyn LiveReplayStore>,
    session: &SessionId,
    revision: SessionRevision,
    drafts: &[(&str, &str)],
) -> Vec<String> {
    let drafts = drafts
        .iter()
        .map(|(id, text)| LiveReplayEventDraft::new(None::<TurnId>, framed_text_payload(id, text)))
        .collect();
    store
        .publish(session, revision, drafts)
        .await
        .expect("publish streamed deltas")
        .iter()
        .map(|event| live_replay_event_label(event))
        .collect()
}

fn framed_text_payload(id: &str, text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(TurnActivity {
        id: crate::TurnActivityId::new(id),
        correlation_id: crate::TurnActivityId::new("text:0"),
        event: TurnEvent::AssistantProseDelta {
            text: text.into(),
            block: crate::llm::types::StreamBlockIdentity::new("text:0", 0),
        },
    })
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn replay_cut_and_live_registration_are_linearizable<F>(make: &F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    const RACES: usize = 64;
    for race in 0..RACES {
        let store = make();
        let session_id = SessionId::fixture(format!("subscribe-race-{race}"));
        let revision = SessionRevision::new(5);
        let start = store.current_cursor(&session_id, revision);
        let prior = publish_one(
            &store,
            &session_id,
            revision,
            Some(&TurnId::from("race-turn")),
            live_replay_text_payload("prior"),
        )
        .await
        .expect("append prior event");
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let subscribe_store = Arc::clone(&store);
        let subscribe_cursor = start.clone();
        let subscribe_barrier = Arc::clone(&barrier);
        let subscribe = tokio::spawn(async move {
            subscribe_barrier.wait().await;
            subscribe_store
                .subscribe_after_cursor(&subscribe_cursor)
                .await
        });
        let append_store = Arc::clone(&store);
        let append_session_id = session_id.clone();
        let append_barrier = Arc::clone(&barrier);
        let append = tokio::spawn(async move {
            append_barrier.wait().await;
            publish_one(
                &append_store,
                &append_session_id,
                revision,
                Some(&TurnId::from("race-turn")),
                live_replay_text_payload("raced"),
            )
            .await
        });
        barrier.wait().await;

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

/// The earliest cursor sits before every retained event, whatever its
/// revision: a session that never committed publishes at revision zero,
/// which no current cursor reaches behind. A session with nothing retained
/// replays empty from it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn the_earliest_cursor_replays_the_whole_window(store: Arc<dyn LiveReplayStore>) {
    let session = SessionId::from("earliest-session");
    let empty = expect_live_replay_replayed(
        store
            .replay_after_cursor(&store.earliest_cursor(&session))
            .await,
        "replay from an empty session's earliest cursor",
    );
    assert!(empty.is_empty(), "nothing is retained yet");
    for (revision, text) in [(0, "before the first commit"), (2, "after a commit")] {
        publish_one(
            &store,
            &session,
            SessionRevision::new(revision),
            Some(&TurnId::from("earliest-turn")),
            live_replay_text_payload(text),
        )
        .await
        .expect("append an event");
    }
    let replay = expect_live_replay_replayed(
        store
            .replay_after_cursor(&store.earliest_cursor(&session))
            .await,
        "replay from the earliest cursor",
    );
    assert_live_replay_labels(
        &replay,
        &["text:before the first commit", "text:after a commit"],
    );
}

fn live_replay_text_payload(text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(TurnActivity::independent(
        TurnEvent::AssistantProseDelta {
            text: text.into(),
            block: crate::llm::types::StreamBlockIdentity::new("text:0", 0),
        },
    ))
}

async fn publish_one(
    store: &Arc<dyn LiveReplayStore>,
    session_id: &SessionId,
    revision: SessionRevision,
    turn_id: Option<&TurnId>,
    payload: SessionObservationEventPayload,
) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError> {
    let event = store
        .publish(
            session_id,
            revision,
            vec![LiveReplayEventDraft::new(turn_id, payload)],
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| LiveReplayStoreError::Store("published batch was empty".to_string()))?;
    assert_event_readers_match_cursor(&event, session_id);
    Ok(event)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn assert_event_readers_match_cursor(
    event: &SessionObservationEvent,
    expected_session_id: &SessionId,
) {
    let parsed = event
        .cursor
        .parse_for_session(expected_session_id)
        .expect("every emitted live replay event must carry a valid cursor for its session");
    assert_eq!(event.session_id(), parsed.session_id);
    assert_eq!(event.replay_incarnation_id(), parsed.replay_incarnation_id);
    assert_eq!(event.revision(), parsed.revision);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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

/// The subscription's next event, however long the store takes to deliver
/// it. A store's delivery is not bounded by how loaded its host is: a
/// PostgreSQL store under a full workspace run stalls past any wall-clock
/// deadline a law could pick (FIG-5148). An event that never arrives leaves
/// the law to the test target's timeout.
async fn next_live_replay_event(
    subscription: &mut crate::LiveReplaySubscription,
    context: &str,
) -> Arc<SessionObservationEvent> {
    subscription
        .next()
        .await
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
            TurnEvent::AssistantProseDelta { text, .. } => format!("text:{text}"),
            other => format!("turn:{other:?}"),
        },
        SessionObservationEventPayload::Committed { .. } => "committed".to_string(),
        SessionObservationEventPayload::ResidentChanged => "resident_changed".to_string(),
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
