//! Live replay store tests, kept beside `replay.rs` to hold the file
//! inside the production line budget.

use super::*;

impl InMemoryLiveReplayStore {
    async fn publish_test_event(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        turn_id: Option<&TurnId>,
        payload: SessionObservationEventPayload,
    ) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError> {
        self.publish(
            session_id,
            revision,
            vec![LiveReplayEventDraft::new(turn_id, payload)],
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| LiveReplayStoreError::Store("published test batch was empty".to_string()))
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

/// A journaled step re-executed after suspension re-publishes the
/// activities its first attempt already delivered, under the same
/// `{replay key}#{ordinal}` identities. The buffer keeps the first
/// delivery of each identity and drops the redelivery (FIG-3753).
#[tokio::test]
async fn a_redelivered_turn_activity_collapses_into_the_stored_copy() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig::standard());
    let session = SessionId::from("deduped");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);

    store
        .publish_test_event(&session, revision, None, activity_with_id("key#3", "prose"))
        .await
        .expect("first delivery");

    let published = store
        .publish(
            &session,
            revision,
            vec![
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("key#3", "prose")),
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("key#4", "tail")),
            ],
        )
        .await
        .expect("publish replayed batch");
    assert_eq!(
        published.len(),
        1,
        "the already-delivered activity collapses"
    );

    let LiveReplayOutcome::Replayed(events) =
        store.replay_after_cursor(&start).await.expect("replay")
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

/// A batch whose every draft is a redelivery takes no positions and
/// announces nothing; the next distinct publication still lands on the
/// position right behind what observers actually received.
#[tokio::test]
async fn a_fully_redelivered_publication_publishes_nothing() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig::standard());
    let session = SessionId::from("deduped-batch");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);

    store
        .publish_test_event(&session, revision, None, activity_with_id("key#0", "prose"))
        .await
        .expect("first delivery");
    let published = store
        .publish(
            &session,
            revision,
            vec![
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("key#0", "prose")),
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("key#0", "prose")),
            ],
        )
        .await
        .expect("publish redelivered batch");
    assert!(published.is_empty());

    store
        .publish_test_event(&session, revision, None, activity_with_id("key#1", "new"))
        .await
        .expect("distinct activity still delivers");
    let LiveReplayOutcome::Replayed(events) =
        store.replay_after_cursor(&start).await.expect("replay")
    else {
        panic!("replay must succeed");
    };
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[1].payload,
        SessionObservationEventPayload::TurnActivity(activity) if activity.id == crate::TurnActivityId::new("key#1")
    ));
}

/// Each replayed activity as `id=text`.
fn activity_texts(events: &[Arc<SessionObservationEvent>]) -> Vec<String> {
    events
        .iter()
        .map(|event| match &event.payload {
            SessionObservationEventPayload::TurnActivity(crate::TurnActivity {
                id,
                event: crate::TurnEvent::AssistantProseDelta { text, .. },
                ..
            }) => format!("{}={text}", id.0),
            other => format!("{other:?}"),
        })
        .collect()
}

/// A redrive that republishes a frame's deltas unmerged, or frames them
/// differently, names them inside the ranges already delivered: no text
/// lands twice, none is lost, and what follows still lands (FIG-5098).
#[tokio::test]
async fn a_redrive_framed_differently_adds_no_text_twice_and_loses_none() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig::standard());
    let session = SessionId::from("framed-redrive");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);
    for (id, text) in [("k#0", "a"), ("k#1..3", "bcd"), ("k#5..6", "fg")] {
        store
            .publish_test_event(&session, revision, None, activity_with_id(id, text))
            .await
            .expect("first delivery");
    }

    // The redrive: the frame's deltas unmerged, then framed another way.
    for (id, text) in [("k#0", "a"), ("k#1", "b"), ("k#2", "c"), ("k#3", "d")] {
        assert!(
            store
                .publish(
                    &session,
                    revision,
                    vec![LiveReplayEventDraft::new(
                        None::<TurnId>,
                        activity_with_id(id, text),
                    )],
                )
                .await
                .expect("redeliver an unmerged original")
                .is_empty(),
            "{id} lies inside a delivered frame"
        );
    }
    let reframed = store
        .publish(
            &session,
            revision,
            vec![
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("k#1..2", "bc")),
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("k#3..6", "dfg")),
                LiveReplayEventDraft::new(None::<TurnId>, activity_with_id("k#7..8", "hi")),
            ],
        )
        .await
        .expect("redeliver a different framing");
    assert_eq!(activity_texts(&reframed), vec!["k#7..8=hi"]);

    let LiveReplayOutcome::Replayed(events) =
        store.replay_after_cursor(&start).await.expect("replay")
    else {
        panic!("replay must succeed");
    };
    assert_eq!(
        activity_texts(&events),
        vec!["k#0=a", "k#1..3=bcd", "k#5..6=fg", "k#7..8=hi"]
    );
}

/// A redelivered frame that is only partly delivered cannot be split: its
/// deltas' text carries no boundaries. Publishing it would repeat the
/// delivered part and dropping it would lose the rest unseen, so it is a
/// gap: every cursor into the session reloads its snapshot, and the
/// session's next activity starts fresh continuity (FIG-5098).
#[tokio::test]
async fn a_redelivery_straddling_the_delivered_range_is_a_gap() {
    use futures_util::FutureExt as _;
    use futures_util::StreamExt as _;

    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig::standard());
    let session = SessionId::from("straddled-redrive");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);
    for (id, text) in [("k#0", "a"), ("k#1..2", "bc")] {
        store
            .publish_test_event(&session, revision, None, activity_with_id(id, text))
            .await
            .expect("first delivery");
    }
    let LiveReplaySubscribeOutcome::Subscribed(mut subscription) = store
        .subscribe_after_cursor(&start)
        .await
        .expect("subscribe")
    else {
        panic!("the cursor is within the window");
    };

    let straddling = store
        .publish(
            &session,
            revision,
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
                activity_with_id("k#2..4", "cde"),
            )],
        )
        .await
        .expect("redeliver a straddling frame");
    assert!(
        straddling.is_empty(),
        "the straddling frame is not published"
    );

    assert!(matches!(
        store.replay_after_cursor(&start).await,
        Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
    ));
    let mut delivered = Vec::new();
    let closed = loop {
        match subscription.next().now_or_never() {
            Some(Some(Ok(event))) => delivered.push(event),
            Some(Some(Err(LiveReplayStoreError::Closed))) => break true,
            _ => break false,
        }
    };
    assert_eq!(activity_texts(&delivered), vec!["k#0=a", "k#1..2=bc"]);
    assert!(
        closed,
        "the live subscription closes: its observer reloads the snapshot"
    );

    let resumed = store.current_cursor(&session, revision);
    store
        .publish_test_event(&session, revision, None, activity_with_id("k#5..6", "fg"))
        .await
        .expect("the next frame publishes");
    let LiveReplayOutcome::Replayed(events) =
        store.replay_after_cursor(&resumed).await.expect("replay")
    else {
        panic!("continuity resumes after the gap");
    };
    assert_eq!(activity_texts(&events), vec!["k#5..6=fg"]);
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

#[tokio::test]
async fn current_cursor_for_stale_snapshot_replays_newer_revision_events() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig::standard());
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(2),
            None,
            activity("worker commit"),
        )
        .await
        .expect("append newer worker commit");

    // A runtime can finish loading durable revision 1 just before a separate
    // worker publishes revision 2. Its initial cursor must not skip that
    // newer event merely because the live-replay tail already advanced.
    let stale_snapshot_cursor = store.current_cursor(&SessionId::from("s"), SessionRevision(1));
    let LiveReplayOutcome::Replayed(events) = store
        .replay_after_cursor(&stale_snapshot_cursor)
        .await
        .expect("replay from stale snapshot")
    else {
        panic!("expected replay");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].revision(), SessionRevision(2));
}

#[tokio::test]
async fn in_memory_replay_subscription_yields_replay_then_live() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig::standard());
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .await
        .expect("append a");
    let LiveReplaySubscribeOutcome::Subscribed(mut subscription) = store
        .subscribe_after_cursor(&start)
        .await
        .expect("subscribe")
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
        .await
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
async fn in_memory_replay_subscription_reports_gap_after_capacity_trim() {
    let store = InMemoryLiveReplayStore::with_bounds(1, Duration::from_secs(120));
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .await
        .expect("append a");
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("b"),
        )
        .await
        .expect("append b");
    assert!(matches!(
        store
            .subscribe_after_cursor(&start)
            .await
            .expect("subscribe"),
        LiveReplaySubscribeOutcome::Gap(LiveReplayGapReason::Trimmed)
    ));
}

#[tokio::test]
async fn in_memory_replay_subscription_reports_gap_after_ttl_trim() {
    let store = InMemoryLiveReplayStore::with_bounds(16, Duration::from_millis(1));
    let start = store.current_cursor(&SessionId::from("s"), SessionRevision(0));
    store
        .publish_test_event(
            &SessionId::from("s"),
            SessionRevision(0),
            None,
            activity("a"),
        )
        .await
        .expect("append a");
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(matches!(
        store
            .subscribe_after_cursor(&start)
            .await
            .expect("subscribe"),
        LiveReplaySubscribeOutcome::Gap(LiveReplayGapReason::Trimmed)
    ));
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

#[tokio::test]
async fn expiry_tick_releases_one_hundred_thousand_idle_sessions() {
    let clock = Arc::new(ReplayClock(StdMutex::new(Instant::now())));
    let store = InMemoryLiveReplayStore::with_clock(
        InMemoryLiveReplayStoreConfig {
            max_sessions: 100_001,
            max_events_per_session: 1,
            max_retained_bytes: 1024 * 1024 * 1024,
            ..InMemoryLiveReplayStoreConfig::standard()
        },
        clock.clone(),
    );
    let mut retained = Vec::new();
    for index in 0..100_000 {
        let session = SessionId::fixture(format!("idle-{index}"));
        let event = store
            .publish_test_event(&session, SessionRevision(1), None, activity("idle"))
            .await
            .expect("publish idle session");
        retained.push(Arc::downgrade(&event));
    }
    {
        let retention = store.sessions.lock_recover();
        assert_eq!(retention.buffers.len(), 100_000);
        assert_eq!(retention.expiry_entry_count(), 100_000);
    }
    clock.advance(STANDARD_LIVE_REPLAY_TTL + Duration::from_secs(1));
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

#[tokio::test]
async fn deployment_session_capacity_evicts_with_a_gap() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig::standard());
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
            .await
            .expect("publish pressure");
    }
    store
        .publish_test_event(&session, SessionRevision(1), None, activity("recreated"))
        .await
        .expect("recreate victim");
    assert!(
        matches!(
            store.replay_after_cursor(&old).await,
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ),
        "an evicted cursor replays a recreated session"
    );
    assert!(
        store.sessions.lock_recover().buffers.len() <= 4096,
        "deployment session capacity is unbounded"
    );
}

#[tokio::test]
async fn deployment_byte_capacity_evicts_with_a_gap() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig {
        max_events_per_session: 1,
        max_sessions: 100,
        max_retained_bytes: 8192,
        ..InMemoryLiveReplayStoreConfig::standard()
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
        .await
        .expect("first payload");
    store
        .publish_test_event(
            &SessionId::from("byte-pressure"),
            SessionRevision(1),
            None,
            activity(&"b".repeat(5000)),
        )
        .await
        .expect("byte pressure");
    store
        .publish_test_event(&victim, SessionRevision(1), None, activity("recreated"))
        .await
        .expect("recreate");
    assert!(
        matches!(
            store.replay_after_cursor(&old).await,
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ),
        "byte pressure replays a recreated session"
    );
    assert!(matches!(
        store.subscribe_after_cursor(&old).await,
        Ok(LiveReplaySubscribeOutcome::Gap(
            LiveReplayGapReason::Unavailable
        ))
    ));
}

#[tokio::test]
async fn invalidation_releases_events_queued_for_live_subscribers() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig::standard());
    let session = SessionId::from("queued-victim");
    let cursor = store.current_cursor(&session, SessionRevision(1));
    let LiveReplaySubscribeOutcome::Subscribed(mut subscription) = store
        .subscribe_after_cursor(&cursor)
        .await
        .expect("subscribe")
    else {
        panic!("fresh cursor");
    };
    let event = store
        .publish_test_event(&session, SessionRevision(1), None, activity("queued"))
        .await
        .expect("publish");
    let retained = Arc::downgrade(&event);
    drop(event);
    store
        .invalidate_session(&session)
        .await
        .expect("invalidate");
    assert!(
        retained.upgrade().is_none(),
        "broadcast channel owns invalidated payloads"
    );
    use futures_util::StreamExt;
    assert!(matches!(subscription.next().await, Some(Err(_))));
}

#[tokio::test]
async fn an_oversized_publication_fences_continuity_without_taking_positions() {
    let store = InMemoryLiveReplayStore::new(InMemoryLiveReplayStoreConfig {
        max_events_per_session: 1,
        max_retained_bytes: 8192,
        ..InMemoryLiveReplayStoreConfig::standard()
    });
    let session = SessionId::from("oversized");
    let before = store.current_cursor(&session, SessionRevision(1));
    assert!(
        store
            .publish(
                &session,
                SessionRevision(1),
                vec![LiveReplayEventDraft::new(
                    None::<TurnId>,
                    activity(&"x".repeat(20_000))
                )]
            )
            .await
            .is_err()
    );
    assert!(matches!(
        store.replay_after_cursor(&before).await,
        Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
    ));
    assert!(matches!(
        store.subscribe_after_cursor(&before).await,
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
        .await
        .expect("fresh publication");
    assert!(
        matches!(store.replay_after_cursor(&fresh).await, Ok(LiveReplayOutcome::Replayed(events)) if events.len() == 1)
    );
}
