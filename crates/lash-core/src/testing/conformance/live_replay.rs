//! [`LiveReplayStore`] conformance: cursors, replay, subscriptions, trims.

use super::*;
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
    live_replay_store_appends_replays_and_isolates_sessions(make()).await;
    live_replay_store_subscribe_replays_then_yields_live_events(make()).await;
    live_replay_store_rejects_malformed_cursors(make()).await;
    live_replay_store_reports_unavailable_for_cursor_ahead_of_tail(make()).await;
}

/// Run the capacity-trim portion of the [`LiveReplayStore`] conformance suite.
///
/// `make` must return a fresh store configured to retain exactly one event per
/// session. Stores with a fixed larger capacity should expose a test
/// configuration rather than weakening this contract.
pub async fn live_replay_store_capacity_trim<F>(make: F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    let store = make();
    let revision = SessionRevision::new(1);
    let start = store.current_cursor("capacity-session", revision);
    let first = store
        .append(
            "capacity-session",
            revision,
            Some("capacity-turn"),
            live_replay_text_payload("capacity one"),
        )
        .expect("append first capacity event");
    store
        .append(
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
}

/// Run the TTL-trim portion of the [`LiveReplayStore`] conformance suite.
///
/// `make` must return a fresh store whose event TTL expires within
/// `expiration_wait`. The suite explicitly calls [`LiveReplayStore::trim_session`]
/// after waiting so implementations can keep trimming lazy and local.
pub async fn live_replay_store_ttl_trim<F>(make: F, expiration_wait: Duration)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    let store = make();
    let revision = SessionRevision::new(1);
    let start = store.current_cursor("ttl-session", revision);
    store
        .append(
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
}

async fn live_replay_store_appends_replays_and_isolates_sessions(store: Arc<dyn LiveReplayStore>) {
    let revision = SessionRevision::new(7);
    let start_a = store.current_cursor("session-a", revision);
    let start_b = store.current_cursor("session-b", revision);
    let empty = expect_live_replay_replayed(
        store.replay_after_cursor(&start_a),
        "empty replay from initial cursor",
    );
    assert!(empty.is_empty(), "initial cursor must replay no events");

    let first_a = store
        .append(
            "session-a",
            revision,
            Some("alpha-turn"),
            live_replay_text_payload("alpha one"),
        )
        .expect("append first session-a event");
    let first_b = store
        .append(
            "session-b",
            revision,
            None,
            SessionObservationEventPayload::ProcessChanged {
                kind: SessionProcessEventKind::Started,
                process_ids: vec!["proc-b".to_string()],
            },
        )
        .expect("append session-b event");
    let second_a = store
        .append(
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
}

async fn live_replay_store_subscribe_replays_then_yields_live_events(
    store: Arc<dyn LiveReplayStore>,
) {
    let revision = SessionRevision::new(3);
    let start = store.current_cursor("subscribe-session", revision);
    store
        .append(
            "subscribe-session",
            revision,
            Some("subscribe-turn"),
            live_replay_text_payload("buffered one"),
        )
        .expect("append first buffered event");
    store
        .append(
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

    store
        .append(
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

async fn live_replay_store_reports_unavailable_for_cursor_ahead_of_tail(
    store: Arc<dyn LiveReplayStore>,
) {
    let revision = SessionRevision::new(4);
    store
        .append(
            "ahead-session",
            revision,
            Some("ahead-turn"),
            live_replay_text_payload("existing"),
        )
        .expect("append existing event");
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

fn live_replay_text_payload(text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(TurnActivity::independent(
        TurnEvent::AssistantProseDelta { text: text.into() },
    ))
}

fn expect_live_replay_replayed(
    result: Result<LiveReplayResult, LiveReplayStoreError>,
    context: &str,
) -> Vec<Arc<SessionObservationEvent>> {
    match result.expect(context) {
        LiveReplayResult::Replayed(events) => events,
        LiveReplayResult::Gap(reason) => {
            panic!("{context}: expected replayed events, got gap {reason:?}")
        }
    }
}

fn expect_live_replay_gap(
    result: Result<LiveReplayResult, LiveReplayStoreError>,
    expected: LiveReplayGapReason,
    context: &str,
) {
    match result.expect(context) {
        LiveReplayResult::Gap(reason) => assert_eq!(reason, expected, "{context}"),
        LiveReplayResult::Replayed(events) => {
            panic!(
                "{context}: expected gap {expected:?}, got {} events",
                events.len()
            )
        }
    }
}

fn expect_live_replay_subscribed(
    result: Result<LiveReplaySubscribeResult, LiveReplayStoreError>,
    context: &str,
) -> crate::LiveReplaySubscription {
    match result.expect(context) {
        LiveReplaySubscribeResult::Subscribed(subscription) => subscription,
        LiveReplaySubscribeResult::Gap(reason) => {
            panic!("{context}: expected subscription, got gap {reason:?}")
        }
    }
}

fn expect_live_replay_subscribe_gap(
    result: Result<LiveReplaySubscribeResult, LiveReplayStoreError>,
    expected: LiveReplayGapReason,
    context: &str,
) {
    match result.expect(context) {
        LiveReplaySubscribeResult::Gap(reason) => assert_eq!(reason, expected, "{context}"),
        LiveReplaySubscribeResult::Subscribed(_) => {
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
