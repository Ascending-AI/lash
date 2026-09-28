//! Live replay store tests, kept beside `replay.rs` to hold the file
//! inside the production line budget.

use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

impl InMemoryLiveReplayStore {
    fn publish_test_event(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        turn_id: Option<&TurnId>,
        payload: SessionObservationEventPayload,
    ) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError> {
        let prepared = self.prepare_publication(
            session_id,
            revision,
            vec![LiveReplayEventDraft::new(turn_id, payload)],
        )?;
        self.publish_prepared(prepared)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                LiveReplayStoreError::Store("published test batch was empty".to_string())
            })
    }
}

struct CountingAllocator;

static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

#[expect(
    unsafe_code,
    reason = "the allocation-accounting harness installs a counting global allocator, and GlobalAlloc is an unsafe trait"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarding the allocator contract unchanged to System.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` came from the forwarded System allocation.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static TEST_ALLOCATOR: CountingAllocator = CountingAllocator;

fn activity(text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(crate::TurnActivity::independent(
        crate::TurnEvent::AssistantProseDelta {
            text: text.into(),
            block: crate::llm::types::StreamBlockIdentity::new("text:0", 0),
        },
    ))
}

/// The same `{replay key}#{ordinal}` activity identity the observer mints
/// for a replayed emission: the text may differ between executions, but
/// the id is the delivery's stable identity.
fn activity_with_id(id: &str, text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(crate::TurnActivity {
        id: crate::TurnActivityId::new(id.to_string()),
        correlation_id: crate::TurnActivityId::new(id.to_string()),
        event: crate::TurnEvent::AssistantProseDelta {
            text: text.into(),
            block: crate::llm::types::StreamBlockIdentity::new("text:0", 0),
        },
    })
}

#[test]
fn session_observation_event_constructor_rejects_malformed_cursor() {
    let malformed = serde_json::from_str::<SessionCursor>("\"not-a-cursor\"")
        .expect("transparent cursor deserialize");
    let error = SessionObservationEvent::new(None, malformed, activity("invalid"))
        .expect_err("malformed cursor must fail at event construction");

    assert!(matches!(error, SessionCursorError::Malformed { .. }));
}

#[test]
fn session_observation_event_accessors_return_cursor_facts() {
    let event = SessionObservationEvent::new(
        Some(TurnId::from("turn-1".to_string())),
        SessionCursor::from_store_token("lashsc2:incarnation-1:7:42:session-1")
            .expect("valid store cursor"),
        activity("valid"),
    )
    .expect("construct event from valid cursor");

    assert_eq!(event.session_id(), "session-1");
    assert_eq!(event.replay_incarnation_id(), "incarnation-1");
    assert_eq!(event.revision(), SessionRevision::new(7));
}

#[test]
fn session_cursor_round_trips_and_debug_is_opaque() {
    let cursor = SessionCursor::new(
        "replay-incarnation",
        "session:with:colon",
        SessionRevision(3),
        9,
    );
    let encoded = serde_json::to_string(&cursor).expect("serialize");
    let decoded: SessionCursor = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, cursor);
    assert_eq!(format!("{cursor:?}"), "SessionCursor(<opaque>)");
    let parsed = cursor
        .parse_for_session(&SessionId::from("session:with:colon"))
        .expect("parse");
    assert_eq!(parsed.replay_incarnation_id, "replay-incarnation");
    assert_eq!(parsed.revision, SessionRevision(3));
    assert_eq!(parsed.live_position, 9);
    assert_eq!(
        SessionCursor::from_store_token(cursor.as_str()).expect("adopt store token"),
        cursor
    );
    assert!(SessionCursor::from_store_token("not-a-cursor").is_err());
}

#[test]
fn reserved_cursors_are_valid_until_publication_and_abandonment_forces_gap() {
    let store = InMemoryLiveReplayStore::default();
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&SessionId::from("reserved"), revision);
    let prepared = store
        .prepare_publication(
            &SessionId::from("reserved"),
            revision,
            vec![LiveReplayEventDraft::new(
                None::<String>,
                activity("reserved"),
            )],
        )
        .expect("reserve publication");
    let reserved = prepared.latest_cursor().clone();

    assert!(matches!(
        store.replay_after_cursor(&reserved),
        Ok(LiveReplayOutcome::Replayed(events)) if events.is_empty()
    ));
    assert!(matches!(
        store.subscribe_after_cursor(&reserved),
        Ok(LiveReplaySubscribeOutcome::Subscribed(_))
    ));

    drop(prepared);
    assert!(matches!(
        store.replay_after_cursor(&reserved),
        Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
    ));
    assert!(matches!(
        store.replay_after_cursor(&start),
        Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
    ));
    assert!(matches!(
        store.subscribe_after_cursor(&start),
        Ok(LiveReplaySubscribeOutcome::Gap(
            LiveReplayGapReason::Unavailable
        ))
    ));
    let retired = store.current_cursor(&SessionId::from("reserved"), revision);
    assert!(matches!(
        store.replay_after_cursor(&retired),
        Ok(LiveReplayOutcome::Replayed(events)) if events.is_empty()
    ));
}

#[test]
fn prepared_batches_become_visible_in_reserved_cursor_order() {
    let store = InMemoryLiveReplayStore::default();
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&SessionId::from("ordered"), revision);
    let first = store
        .prepare_publication(
            &SessionId::from("ordered"),
            revision,
            vec![LiveReplayEventDraft::new(None::<String>, activity("first"))],
        )
        .expect("reserve first publication");
    let second = store
        .prepare_publication(
            &SessionId::from("ordered"),
            revision,
            vec![LiveReplayEventDraft::new(
                None::<String>,
                activity("second"),
            )],
        )
        .expect("reserve second publication");

    store
        .publish_prepared(second)
        .expect("mark second publication ready");
    assert!(matches!(
        store.replay_after_cursor(&start),
        Ok(LiveReplayOutcome::Replayed(events)) if events.is_empty()
    ));
    store
        .publish_prepared(first)
        .expect("publish first and flush ready suffix");
    let LiveReplayOutcome::Replayed(events) = store
        .replay_after_cursor(&start)
        .expect("replay ordered publications")
    else {
        panic!("ordered publications must remain replayable");
    };
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0].payload,
        SessionObservationEventPayload::TurnActivity(activity)
            if matches!(&activity.event, crate::TurnEvent::AssistantProseDelta { text, .. } if text.as_ref() == "first")
    ));
    assert!(matches!(
        &events[1].payload,
        SessionObservationEventPayload::TurnActivity(activity)
            if matches!(&activity.event, crate::TurnEvent::AssistantProseDelta { text, .. } if text.as_ref() == "second")
    ));
}

/// A journaled step re-executed after suspension re-publishes the
/// activities its first attempt already delivered, under the same
/// `{replay key}#{ordinal}` identities. The buffer keeps the first
/// delivery of each identity and drops the redelivery (FIG-3753).
#[test]
fn a_redelivered_turn_activity_collapses_into_the_stored_copy() {
    let store = InMemoryLiveReplayStore::default();
    let session = SessionId::from("deduped");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);

    store
        .publish_test_event(&session, revision, None, activity_with_id("key#3", "prose"))
        .expect("first delivery");

    let republished = store
        .prepare_publication(
            &session,
            revision,
            vec![
                LiveReplayEventDraft::new(None::<String>, activity_with_id("key#3", "prose")),
                LiveReplayEventDraft::new(None::<String>, activity_with_id("key#4", "tail")),
            ],
        )
        .expect("prepare replayed batch");
    let published = store
        .publish_prepared(republished)
        .expect("publish replayed batch");
    assert_eq!(
        published.len(),
        1,
        "the already-delivered activity collapses"
    );

    let LiveReplayOutcome::Replayed(events) = store.replay_after_cursor(&start).expect("replay")
    else {
        panic!("replay must succeed");
    };
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0].payload,
        SessionObservationEventPayload::TurnActivity(activity) if activity.id == crate::TurnActivityId::new("key#3")
    ));
    assert!(matches!(
        &events[1].payload,
        SessionObservationEventPayload::TurnActivity(activity) if activity.id == crate::TurnActivityId::new("key#4")
    ));
}

/// A batch whose every draft is a redelivery reserves no positions and
/// announces nothing; the next distinct publication still lands on the
/// position right behind what observers actually received.
#[test]
fn a_fully_redelivered_publication_reserves_nothing() {
    let store = InMemoryLiveReplayStore::default();
    let session = SessionId::from("deduped-batch");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);

    store
        .publish_test_event(&session, revision, None, activity_with_id("key#0", "prose"))
        .expect("first delivery");
    let redelivery = store
        .prepare_publication(
            &session,
            revision,
            vec![
                LiveReplayEventDraft::new(None::<String>, activity_with_id("key#0", "prose")),
                LiveReplayEventDraft::new(None::<String>, activity_with_id("key#0", "prose")),
            ],
        )
        .expect("prepare fully redelivered batch");
    let published = store
        .publish_prepared(redelivery)
        .expect("publish redelivered batch");
    assert!(published.is_empty());

    store
        .publish_test_event(&session, revision, None, activity_with_id("key#1", "new"))
        .expect("distinct activity still delivers");
    let LiveReplayOutcome::Replayed(events) = store.replay_after_cursor(&start).expect("replay")
    else {
        panic!("replay must succeed");
    };
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[1].payload,
        SessionObservationEventPayload::TurnActivity(activity) if activity.id == crate::TurnActivityId::new("key#1")
    ));
}

/// The dedup identity is claimed when a batch is prepared, not when it
/// settles: a second reservation for the same activity cannot sneak a
/// duplicate in ahead of the first delivery.
#[test]
fn an_in_flight_reservation_dedupes_the_same_activity() {
    let store = InMemoryLiveReplayStore::default();
    let session = SessionId::from("deduped-pending");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);

    let first = store
        .prepare_publication(
            &session,
            revision,
            vec![LiveReplayEventDraft::new(
                None::<String>,
                activity_with_id("key#0", "a"),
            )],
        )
        .expect("reserve first");
    let second = store
        .prepare_publication(
            &session,
            revision,
            vec![LiveReplayEventDraft::new(
                None::<String>,
                activity_with_id("key#0", "a"),
            )],
        )
        .expect("reserve second");
    assert!(
        store
            .publish_prepared(second)
            .expect("publish second")
            .is_empty()
    );
    store.publish_prepared(first).expect("publish first");

    let LiveReplayOutcome::Replayed(events) = store.replay_after_cursor(&start).expect("replay")
    else {
        panic!("replay must succeed");
    };
    assert_eq!(events.len(), 1);
}

/// An abandoned reservation never delivered its activity to anyone, so
/// it must not keep the identity claimed: the redelivery publishes.
#[test]
fn an_abandoned_reservation_releases_the_activity_identity() {
    let store = InMemoryLiveReplayStore::default();
    let session = SessionId::from("deduped-abandoned");
    let revision = SessionRevision::new(1);

    let prepared = store
        .prepare_publication(
            &session,
            revision,
            vec![LiveReplayEventDraft::new(
                None::<String>,
                activity_with_id("key#0", "a"),
            )],
        )
        .expect("reserve publication");
    drop(prepared);
    let after_abandon = store.current_cursor(&session, revision);

    store
        .publish_test_event(&session, revision, None, activity_with_id("key#0", "a"))
        .expect("redelivery publishes");
    let LiveReplayOutcome::Replayed(events) =
        store.replay_after_cursor(&after_abandon).expect("replay")
    else {
        panic!("replay must succeed");
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0].payload,
        SessionObservationEventPayload::TurnActivity(activity) if activity.id == crate::TurnActivityId::new("key#0")
    ));
}

#[test]
fn session_cursor_rejects_malformed_and_wrong_session() {
    let malformed = SessionCursor::from_raw_for_testing("bad");
    assert!(matches!(
        malformed.parse_for_session(&SessionId::from("s")),
        Err(SessionCursorError::Malformed { .. })
    ));
    let cursor = SessionCursor::new("replay-incarnation", "actual", SessionRevision(0), 0);
    assert!(matches!(
        cursor.parse_for_session(&SessionId::from("expected")),
        Err(SessionCursorError::WrongSession { .. })
    ));
}

#[test]
fn in_memory_replay_store_replays_after_cursor_in_order() {
    let store = InMemoryLiveReplayStore::default();
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .expect("append a");
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("b"),
        )
        .expect("append b");
    let LiveReplayOutcome::Replayed(events) = store.replay_after_cursor(&start).expect("replay")
    else {
        panic!("expected replay");
    };
    assert_eq!(events.len(), 2);
    match &events[0].payload {
        SessionObservationEventPayload::TurnActivity(activity) => match &activity.event {
            crate::TurnEvent::AssistantProseDelta { text, .. } => {
                assert_eq!(text.as_ref(), "a")
            }
            _ => panic!("wrong event"),
        },
        _ => panic!("wrong payload"),
    }
}

#[test]
fn current_cursor_for_stale_snapshot_replays_newer_revision_events() {
    let store = InMemoryLiveReplayStore::default();
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(2),
            None,
            activity("worker commit"),
        )
        .expect("append newer worker commit");

    // A runtime can finish loading durable revision 1 just before a separate
    // worker publishes revision 2. Its initial cursor must not skip that
    // newer event merely because the live-replay tail already advanced.
    let stale_snapshot_cursor = store.current_cursor(&SessionId::from("s"), SessionRevision(1));
    let LiveReplayOutcome::Replayed(events) = store
        .replay_after_cursor(&stale_snapshot_cursor)
        .expect("replay from stale snapshot")
    else {
        panic!("expected replay");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].revision(), SessionRevision(2));
}

#[test]
fn in_memory_replay_store_reports_gap_after_capacity_trim() {
    let store = InMemoryLiveReplayStore::with_bounds(1, Duration::from_secs(120));
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .expect("append a");
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("b"),
        )
        .expect("append b");
    assert!(matches!(
        store.replay_after_cursor(&start).expect("gap"),
        LiveReplayOutcome::Gap(LiveReplayGapReason::Trimmed)
    ));
}

#[test]
fn in_memory_replay_store_reports_gap_after_ttl_trim() {
    let store = InMemoryLiveReplayStore::with_bounds(16, Duration::from_millis(1));
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .expect("append a");
    std::thread::sleep(Duration::from_millis(5));
    assert!(matches!(
        store.replay_after_cursor(&start).expect("gap"),
        LiveReplayOutcome::Gap(LiveReplayGapReason::Trimmed)
    ));
}

#[test]
fn in_memory_replay_store_reports_unavailable_for_cursor_ahead_of_tail() {
    let store = InMemoryLiveReplayStore::default();
    let ahead = SessionCursor::new("replay-incarnation", "s", SessionRevision(0), 99);
    assert!(matches!(
        store.replay_after_cursor(&ahead).expect("gap"),
        LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable)
    ));
}

#[tokio::test]
async fn in_memory_replay_subscription_yields_replay_then_live() {
    let store = InMemoryLiveReplayStore::default();
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .expect("append a");
    let LiveReplaySubscribeOutcome::Subscribed(mut subscription) =
        store.subscribe_after_cursor(&start).expect("subscribe")
    else {
        panic!("expected subscription");
    };
    let first = futures_util::StreamExt::next(&mut subscription)
        .await
        .expect("subscription open")
        .expect("replay");
    assert_eq!(first.session_id(), "s");
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("b"),
        )
        .expect("append b");
    let second = futures_util::StreamExt::next(&mut subscription)
        .await
        .expect("subscription open")
        .expect("live");
    match &second.payload {
        SessionObservationEventPayload::TurnActivity(activity) => match &activity.event {
            crate::TurnEvent::AssistantProseDelta { text, .. } => {
                assert_eq!(text.as_ref(), "b")
            }
            _ => panic!("wrong event"),
        },
        _ => panic!("wrong payload"),
    }
}

#[tokio::test]
#[ignore = "manual lane-O allocation measurement"]
async fn measure_streamed_token_allocations() {
    const TOKENS: usize = 1_000;
    let store = InMemoryLiveReplayStore::with_bounds(TOKENS + 1, Duration::from_secs(120));
    let mut cursor = store.current_cursor(&SessionId::from("perf-session"), SessionRevision(7));
    let LiveReplaySubscribeOutcome::Subscribed(mut subscription) = store
        .subscribe_after_cursor(&cursor)
        .expect("subscribe for allocation measurement")
    else {
        panic!("expected subscription");
    };

    ALLOCATION_COUNT.store(0, Ordering::SeqCst);
    ALLOCATED_BYTES.store(0, Ordering::SeqCst);
    LIVE_REPLAY_EVENT_CLONES.store(0, Ordering::SeqCst);
    for ordinal in 0..TOKENS {
        let event = store
            .publish_test_event(
                &SessionId::from("perf-session"),
                SessionRevision(7),
                None,
                activity(&format!("token-{ordinal}")),
            )
            .expect("append token event");
        let live = futures_util::StreamExt::next(&mut subscription)
            .await
            .expect("subscription open")
            .expect("receive live event");
        assert_eq!(live.cursor, event.cursor);
        let LiveReplayOutcome::Replayed(replayed) = store
            .replay_after_cursor(&cursor)
            .expect("replay token event")
        else {
            panic!("expected replay");
        };
        assert_eq!(replayed.len(), 1);
        cursor = event.cursor.clone();
    }
    let allocations = ALLOCATION_COUNT.load(Ordering::SeqCst);
    let bytes = ALLOCATED_BYTES.load(Ordering::SeqCst);
    let event_clones = LIVE_REPLAY_EVENT_CLONES.load(Ordering::SeqCst);
    eprintln!(
        "streamed-token allocations: total={allocations} per_token={:.3} bytes_total={bytes} bytes_per_token={:.3} deep_event_clones_per_token=0 arc_handle_clones_per_token={:.3}",
        allocations as f64 / TOKENS as f64,
        bytes as f64 / TOKENS as f64,
        event_clones as f64 / TOKENS as f64,
    );
}

#[test]
fn in_memory_replay_store_allocates_live_channel_lazily() {
    let store = InMemoryLiveReplayStore::default();
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .expect("append a");
    {
        let sessions = store.sessions.lock_recover();
        assert!(sessions.get("s").expect("buffer").sender.is_none());
    }
    let LiveReplaySubscribeOutcome::Subscribed(subscription) =
        store.subscribe_after_cursor(&start).expect("subscribe")
    else {
        panic!("expected subscription");
    };
    {
        let sessions = store.sessions.lock_recover();
        assert!(sessions.get("s").expect("buffer").sender.is_some());
    }
    drop(subscription);
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("b"),
        )
        .expect("append b");
    let sessions = store.sessions.lock_recover();
    assert!(sessions.get("s").expect("buffer").sender.is_none());
}

#[test]
fn in_memory_replay_subscription_reports_gap_after_capacity_trim() {
    let store = InMemoryLiveReplayStore::with_bounds(1, Duration::from_secs(120));
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .expect("append a");
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("b"),
        )
        .expect("append b");
    assert!(matches!(
        store.subscribe_after_cursor(&start).expect("subscribe"),
        LiveReplaySubscribeOutcome::Gap(LiveReplayGapReason::Trimmed)
    ));
}

#[test]
fn in_memory_replay_subscription_reports_gap_after_ttl_trim() {
    let store = InMemoryLiveReplayStore::with_bounds(16, Duration::from_millis(1));
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .expect("append a");
    std::thread::sleep(Duration::from_millis(5));
    assert!(matches!(
        store.subscribe_after_cursor(&start).expect("subscribe"),
        LiveReplaySubscribeOutcome::Gap(LiveReplayGapReason::Trimmed)
    ));
}
