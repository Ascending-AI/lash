//! Live replay store tests, kept beside `replay.rs` to hold the file
//! inside the production line budget.

use super::*;

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
fn reserved_cursors_are_valid_until_publication_and_abandonment_forces_gap() {
    let store = InMemoryLiveReplayStore::default();
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&SessionId::from("reserved"), revision);
    let prepared = store
        .prepare_publication(
            &SessionId::from("reserved"),
            revision,
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
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
            vec![LiveReplayEventDraft::new(None::<TurnId>, activity("first"))],
        )
        .expect("reserve first publication");
    let second = store
        .prepare_publication(
            &SessionId::from("ordered"),
            revision,
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
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
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("key#3", "prose")),
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("key#4", "tail")),
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
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("key#0", "prose")),
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("key#0", "prose")),
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
                None::<TurnId>,
                activity_with_id("key#0", "a"),
            )],
        )
        .expect("reserve first");
    let second = store
        .prepare_publication(
            &session,
            revision,
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
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
                None::<TurnId>,
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

#[tokio::test]
async fn invalidation_fences_pending_publications_and_recovers_after_the_gap() {
    use futures_util::StreamExt as _;
    let store = InMemoryLiveReplayStore::default();
    let session_id = SessionId::from("invalidation");
    let revision = SessionRevision::new(7);
    let start = store.current_cursor(&session_id, revision);
    let LiveReplaySubscribeOutcome::Subscribed(mut subscription) =
        store.subscribe_after_cursor(&start).unwrap()
    else {
        panic!("healthy subscription");
    };
    let pending = store
        .prepare_publication(
            &session_id,
            revision,
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
                activity("pending"),
            )],
        )
        .unwrap();
    let reserved = pending.latest_cursor().clone();
    store.invalidate_session(&session_id).unwrap();
    assert!(store.publish_prepared(pending).is_err());
    for cursor in [&start, &reserved] {
        assert!(matches!(
            store.replay_after_cursor(cursor),
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ));
        assert!(matches!(
            store.subscribe_after_cursor(cursor),
            Ok(LiveReplaySubscribeOutcome::Gap(
                LiveReplayGapReason::Unavailable
            ))
        ));
    }
    assert!(matches!(
        subscription.next().await,
        Some(Err(LiveReplayStoreError::Closed))
    ));
    let recovered = store.current_cursor(&session_id, revision);
    let live = store
        .prepare_publication(
            &session_id,
            revision,
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
                activity("after-resync"),
            )],
        )
        .unwrap();
    store.publish_prepared(live).unwrap();
    assert!(
        matches!(store.replay_after_cursor(&recovered), Ok(LiveReplayOutcome::Replayed(events)) if events.len() == 1 && events[0].revision() == revision)
    );
}

#[derive(Debug)]
struct ReplayClock(StdMutex<Instant>);

impl ReplayClock {
    fn advance(&self, duration: Duration) {
        *self.0.lock_recover() += duration;
    }
}

#[async_trait::async_trait]
impl crate::Clock for ReplayClock {
    fn now(&self) -> Instant {
        *self.0.lock_recover()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        crate::SystemClock.timestamp_datetime()
    }

    async fn sleep(&self, _: Duration) {
        panic!("replay expiry laws do not sleep");
    }

    async fn sleep_until(&self, _: Instant) {
        panic!("replay expiry laws do not sleep");
    }
}

#[test]
fn expiry_tick_releases_one_hundred_thousand_idle_sessions() {
    let clock = Arc::new(ReplayClock(StdMutex::new(Instant::now())));
    let store = InMemoryLiveReplayStore::with_clock(
        InMemoryLiveReplayStoreConfig {
            max_sessions: 100_001,
            max_events_per_session: 1,
            max_retained_bytes: 1024 * 1024 * 1024,
            ..InMemoryLiveReplayStoreConfig::default()
        },
        clock.clone(),
    );
    let mut retained = Vec::new();
    for index in 0..100_000 {
        let session = SessionId::fixture(format!("idle-{index}"));
        let event = store
            .publish_test_event(&session, SessionRevision(1), None, activity("idle"))
            .expect("publish idle session");
        retained.push(Arc::downgrade(&event));
    }
    {
        let retention = store.sessions.lock_recover();
        assert_eq!(retention.buffers.len(), 100_000);
        assert_eq!(retention.expiry_entry_count(), 100_000);
    }
    clock.advance(DEFAULT_LIVE_REPLAY_TTL + Duration::from_secs(1));
    assert_eq!(store.expire_idle_sessions(), 100_000);
    assert!(
        store.sessions.lock_recover().buffers.is_empty(),
        "idle entries survive the expiry tick"
    );
    {
        let retention = store.sessions.lock_recover();
        assert_eq!(retention.expiry_entry_count(), 0);
        assert_eq!(retention.retained_bytes, 0);
        assert_eq!(retention.buffers.capacity(), 0);
    }
    assert!(
        retained.iter().all(|event| event.upgrade().is_none()),
        "idle events survive the expiry tick"
    );
}

#[test]
fn deployment_session_capacity_evicts_with_a_gap() {
    let store = InMemoryLiveReplayStore::default();
    let session = SessionId::from("capacity-victim");
    let old = store.current_cursor(&session, SessionRevision(1));
    for index in 0..4097 {
        store
            .publish_test_event(
                &SessionId::fixture(format!("pressure-{index}")),
                SessionRevision(1),
                None,
                activity("pressure"),
            )
            .expect("publish pressure");
    }
    store
        .publish_test_event(&session, SessionRevision(1), None, activity("recreated"))
        .expect("recreate victim");
    assert!(
        matches!(
            store.replay_after_cursor(&old),
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ),
        "an evicted cursor replays a recreated session"
    );
    assert!(
        store.sessions.lock_recover().buffers.len() <= 4096,
        "deployment session capacity is unbounded"
    );
}

#[test]
fn deployment_byte_capacity_evicts_with_a_gap() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig {
        max_events_per_session: 1,
        max_sessions: 100,
        max_retained_bytes: 8192,
        ..InMemoryLiveReplayStoreConfig::default()
    });
    let victim = SessionId::from("byte-victim");
    let old = store.current_cursor(&victim, SessionRevision(1));
    store
        .publish_test_event(
            &victim,
            SessionRevision(1),
            None,
            activity(&"a".repeat(5000)),
        )
        .expect("first payload");
    store
        .publish_test_event(
            &SessionId::from("byte-pressure"),
            SessionRevision(1),
            None,
            activity(&"b".repeat(5000)),
        )
        .expect("byte pressure");
    store
        .publish_test_event(&victim, SessionRevision(1), None, activity("recreated"))
        .expect("recreate");
    assert!(
        matches!(
            store.replay_after_cursor(&old),
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ),
        "byte pressure replays a recreated session"
    );
    assert!(matches!(
        store.subscribe_after_cursor(&old),
        Ok(LiveReplaySubscribeOutcome::Gap(
            LiveReplayGapReason::Unavailable
        ))
    ));
}

#[test]
fn capacity_eviction_retires_pending_reservations() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig {
        max_sessions: 1,
        ..InMemoryLiveReplayStoreConfig::default()
    });
    let victim = SessionId::from("pending-victim");
    let old = store
        .prepare_publication(
            &victim,
            SessionRevision(1),
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
                activity("pending"),
            )],
        )
        .expect("reserve");
    let old_cursor = old.latest_cursor().clone();
    store
        .publish_test_event(
            &SessionId::from("pending-pressure"),
            SessionRevision(1),
            None,
            activity("pressure"),
        )
        .expect("pressure");
    let fresh = store.current_cursor(&victim, SessionRevision(1));
    assert!(
        store.publish_prepared(old).is_err(),
        "evicted reservation can still publish"
    );
    store
        .publish_test_event(&victim, SessionRevision(1), None, activity("fresh"))
        .expect("fresh publication");
    assert!(matches!(
        store.replay_after_cursor(&old_cursor),
        Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
    ));
    assert!(
        matches!(store.replay_after_cursor(&fresh), Ok(LiveReplayOutcome::Replayed(events)) if events.len() == 1)
    );
}

#[tokio::test]
async fn invalidation_releases_events_queued_for_live_subscribers() {
    let store = InMemoryLiveReplayStore::default();
    let session = SessionId::from("queued-victim");
    let cursor = store.current_cursor(&session, SessionRevision(1));
    let LiveReplaySubscribeOutcome::Subscribed(mut subscription) =
        store.subscribe_after_cursor(&cursor).expect("subscribe")
    else {
        panic!("fresh cursor");
    };
    let event = store
        .publish_test_event(&session, SessionRevision(1), None, activity("queued"))
        .expect("publish");
    let retained = Arc::downgrade(&event);
    drop(event);
    store.invalidate_session(&session).expect("invalidate");
    assert!(
        retained.upgrade().is_none(),
        "broadcast channel owns invalidated payloads"
    );
    use futures_util::StreamExt;
    assert!(matches!(subscription.next().await, Some(Err(_))));
}

#[test]
fn an_oversized_publication_fences_continuity_without_reserving_positions() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig {
        max_events_per_session: 1,
        max_retained_bytes: 8192,
        ..InMemoryLiveReplayStoreConfig::default()
    });
    let session = SessionId::from("oversized");
    let before = store.current_cursor(&session, SessionRevision(1));
    assert!(
        store
            .prepare_publication(
                &session,
                SessionRevision(1),
                vec![LiveReplayEventDraft::new(
                    None::<TurnId>,
                    activity(&"x".repeat(20_000))
                )]
            )
            .is_err()
    );
    assert!(matches!(
        store.replay_after_cursor(&before),
        Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
    ));
    assert!(matches!(
        store.subscribe_after_cursor(&before),
        Ok(LiveReplaySubscribeOutcome::Gap(
            LiveReplayGapReason::Unavailable
        ))
    ));
    let fresh = store.current_cursor(&session, SessionRevision(1));
    assert!(
        fresh.parse().expect("fresh cursor").live_position
            > before.parse().expect("prior cursor").live_position
    );
    store
        .publish_test_event(&session, SessionRevision(1), None, activity("fits"))
        .expect("fresh publication");
    assert!(
        matches!(store.replay_after_cursor(&fresh), Ok(LiveReplayOutcome::Replayed(events)) if events.len() == 1)
    );
}

#[test]
fn pending_and_ready_publications_share_deployment_byte_capacity() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig {
        max_events_per_session: 1,
        max_retained_bytes: 16 * 1024,
        ..InMemoryLiveReplayStoreConfig::default()
    });
    let victim = SessionId::from("reserved-byte-victim");
    let first = store
        .prepare_publication(
            &victim,
            SessionRevision(1),
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
                activity(&"a".repeat(5000)),
            )],
        )
        .expect("first reservation");
    let old = first.latest_cursor().clone();
    let second = store
        .prepare_publication(
            &victim,
            SessionRevision(1),
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
                activity(&"b".repeat(5000)),
            )],
        )
        .expect("second reservation");
    let ready_event = Arc::downgrade(&second.events()[0]);
    store.publish_prepared(second).expect("ready suffix");
    store
        .publish_test_event(
            &SessionId::from("reserved-byte-pressure"),
            SessionRevision(1),
            None,
            activity(&"c".repeat(12_000)),
        )
        .expect("pressure");
    assert!(
        ready_event.upgrade().is_none(),
        "ready reservation survives byte eviction"
    );
    assert!(
        store.publish_prepared(first).is_err(),
        "pending reservation survives byte eviction"
    );
    assert!(matches!(
        store.replay_after_cursor(&old),
        Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
    ));
    let retention = store.sessions.lock_recover();
    assert!(retention.retained_bytes <= 16 * 1024);
    assert_eq!(retention.expiry_entry_count(), retention.buffers.len());
}
