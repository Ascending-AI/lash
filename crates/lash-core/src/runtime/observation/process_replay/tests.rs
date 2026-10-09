//! The in-memory process replay store's own bounds. The contract every
//! process replay store keeps is certified in `lash-conformance`
//! (`process_replay_tests!`).

use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use lash_sansio::sync::MutexExt as _;

use super::*;
use crate::testing::{
    process_committed_event, process_language_observation, process_observation_label,
};

fn store(config: InMemoryProcessReplayStoreConfig) -> InMemoryProcessReplayStore {
    InMemoryProcessReplayStore::new(config)
}

fn node(process_id: &ProcessId, key: &str) -> ProcessReplayEventDraft {
    ProcessReplayEventDraft::language_execution(
        ProcessSequence::new(0),
        process_language_observation(process_id, key, key),
    )
}

async fn replay(
    store: &InMemoryProcessReplayStore,
    cursor: &ProcessObservationCursor,
) -> Result<Vec<String>, ProcessReplayGapReason> {
    match store
        .replay_after_cursor(cursor)
        .await
        .expect("replay reads")
    {
        ProcessReplayOutcome::Replayed(events) => Ok(events
            .iter()
            .map(|event| process_observation_label(event))
            .collect()),
        ProcessReplayOutcome::Gap(reason) => Err(reason),
    }
}

/// The bytes one fixture observation is charged.
async fn one_event_bytes() -> usize {
    let store = store(InMemoryProcessReplayStoreConfig::standard());
    let process = ProcessId::fixture("sizing");
    store
        .publish(&process, vec![node(&process, "event-0")])
        .await
        .expect("publish");
    store.event_bytes(&process)
}

#[test]
fn a_cursor_names_one_process() {
    let process = ProcessId::fixture("cursor-process");
    let cursor = ProcessObservationCursor::new("incarnation", &process, ProcessSequence::new(4), 9);
    let parsed = cursor.parse_for_process(&process).expect("its own process");
    assert_eq!(parsed.replay_incarnation_id, "incarnation");
    assert_eq!(parsed.sequence, ProcessSequence::new(4));
    assert_eq!(parsed.live_position, 9);
    assert!(matches!(
        cursor.parse_for_process(&ProcessId::fixture("another")),
        Err(ProcessObservationCursorError::WrongProcess { .. })
    ));
    assert!(matches!(
        ProcessObservationCursor::from_store_token("lashsc2:incarnation:4:9:session"),
        Err(ProcessObservationCursorError::Malformed { .. })
    ));
}

/// A stale cursor is bridged only by the committed fact of every sequence it
/// missed: one later commit proves nothing about the ones between, and a
/// provisional event stamped at a later sequence proves no commit at all.
#[tokio::test]
async fn a_bridge_needs_every_committed_sequence() {
    let store = store(InMemoryProcessReplayStoreConfig::standard());
    let process = ProcessId::fixture("bridge");
    let mut drafts = vec![
        ProcessReplayEventDraft::committed(process_committed_event(2, "operator")),
        ProcessReplayEventDraft::language_execution(
            ProcessSequence::new(5),
            process_language_observation(&process, "stamped-ahead", "stamped-ahead"),
        ),
        ProcessReplayEventDraft::committed(process_committed_event(4, "operator")),
    ];
    let events = store
        .publish(&process, drafts.clone())
        .await
        .expect("publish");
    let bridge = |events: &[Arc<ProcessObservationEvent>], held: u64, durable: u64| {
        commits_bridge(
            events.iter().map(Arc::as_ref),
            ProcessSequence::new(held),
            ProcessSequence::new(durable),
        )
    };
    assert!(bridge(&events, 1, 2));
    assert!(
        bridge(&events, 4, 4),
        "a cursor at the process needs nothing"
    );
    assert!(!bridge(&events, 1, 4), "sequence 3 is missing");
    assert!(
        !bridge(&events, 4, 5),
        "a provisional stamp is not a commit"
    );

    drafts.insert(
        2,
        ProcessReplayEventDraft::committed(process_committed_event(3, "operator")),
    );
    let other = ProcessId::fixture("bridge-complete");
    let events = store.publish(&other, drafts).await.expect("publish");
    assert!(bridge(&events, 1, 4));
}

/// A process's window is cut by its byte bound as well as its event count:
/// the oldest events go first and a cursor behind them is `Trimmed`.
#[tokio::test]
async fn the_per_process_byte_bound_drops_the_oldest_events() {
    let bytes = one_event_bytes().await;
    let store = store(InMemoryProcessReplayStoreConfig {
        max_bytes_per_process: bytes * 2,
        ..InMemoryProcessReplayStoreConfig::standard()
    });
    let process = ProcessId::fixture("byte-bound");
    let start = store
        .earliest_cursor(&process, ProcessSequence::new(0))
        .await
        .expect("cursor");
    let mut published = Vec::new();
    for index in 0..3 {
        published.extend(
            store
                .publish(&process, vec![node(&process, &format!("event-{index}"))])
                .await
                .expect("publish"),
        );
    }
    assert_eq!(
        replay(&store, &start).await,
        Err(ProcessReplayGapReason::Trimmed)
    );
    assert_eq!(
        replay(&store, &published[0].cursor).await,
        Ok(vec!["event-1".to_string(), "event-2".to_string()])
    );
    let earliest = store
        .earliest_cursor(&process, ProcessSequence::new(0))
        .await
        .expect("earliest cursor");
    assert_eq!(earliest, published[0].cursor);
}

/// A publication the store cannot hold is not dropped quietly: it fails,
/// takes no position, and retires the process's continuity, so a cursor
/// across it is a gap and its subscribers are told to resubscribe.
#[tokio::test]
async fn a_publication_the_store_cannot_hold_retires_the_process_continuity() {
    let bytes = one_event_bytes().await;
    let store = store(InMemoryProcessReplayStoreConfig {
        max_bytes_per_process: bytes * 2,
        ..InMemoryProcessReplayStoreConfig::standard()
    });
    let process = ProcessId::fixture("refused");
    let kept = store
        .publish(&process, vec![node(&process, "event-0")])
        .await
        .expect("publish");
    let ProcessReplaySubscribeOutcome::Subscribed(mut subscription) = store
        .subscribe_after_cursor(&kept[0].cursor)
        .await
        .expect("subscribe")
    else {
        panic!("the tail subscribes");
    };
    let oversized = (1..=3)
        .map(|index| node(&process, &format!("event-{index}")))
        .collect();
    assert!(matches!(
        store.publish(&process, oversized).await,
        Err(ProcessReplayStoreError::Store(_))
    ));
    assert_eq!(
        replay(&store, &kept[0].cursor).await,
        Err(ProcessReplayGapReason::Unavailable)
    );
    assert!(matches!(
        futures_util::StreamExt::next(&mut subscription).await,
        Some(Err(ProcessReplayStoreError::Closed))
    ));
}

/// The aggregate bound is the process store's own: under pressure the
/// idlest other process's window is evicted whole, its cursors gap, and the
/// publishing process keeps its history.
#[tokio::test]
async fn the_aggregate_bound_evicts_the_idlest_other_process() {
    let bytes = one_event_bytes().await;
    let window = memory_window_bytes();
    let store = store(InMemoryProcessReplayStoreConfig {
        max_retained_bytes: 2 * window + 3 * bytes,
        ..InMemoryProcessReplayStoreConfig::standard()
    });
    let (idle, busy) = (ProcessId::fixture("idle"), ProcessId::fixture("busy"));
    let idle_event = store
        .publish(&idle, vec![node(&idle, "event-0")])
        .await
        .expect("publish");
    let busy_start = store
        .earliest_cursor(&busy, ProcessSequence::new(0))
        .await
        .expect("cursor");
    for index in 0..3 {
        store
            .publish(&busy, vec![node(&busy, &format!("event-{index}"))])
            .await
            .expect("publish");
    }
    assert_eq!(
        replay(&store, &idle_event[0].cursor).await,
        Err(ProcessReplayGapReason::Unavailable)
    );
    assert_eq!(
        replay(&store, &busy_start).await.map(|events| events.len()),
        Ok(3)
    );
}

/// A clock the law advances by hand.
#[derive(Debug)]
struct HandClock(StdMutex<Instant>);

#[async_trait::async_trait]
impl crate::Clock for HandClock {
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

/// Idle expiry releases only windows nobody follows (FIG-5627): a process
/// that publishes nothing for longer than `max_age` keeps its window while
/// a subscriber holds it, so its follower's tail stays open and the next
/// event arrives without a gap; an unfollowed process idle as long is
/// released, and so is the followed one once its follower has left.
#[tokio::test]
async fn a_followed_window_idle_past_max_age_is_kept() {
    let clock = Arc::new(HandClock(StdMutex::new(Instant::now())));
    let config = InMemoryProcessReplayStoreConfig::standard();
    let past_max_age = config.max_age + Duration::from_secs(1);
    let store = InMemoryProcessReplayStore::with_clock(config, clock.clone());
    let (followed, unfollowed) = (ProcessId::fixture("followed"), ProcessId::fixture("alone"));
    let first = store
        .publish(&followed, vec![node(&followed, "event-0")])
        .await
        .expect("publish");
    let alone = store
        .publish(&unfollowed, vec![node(&unfollowed, "event-0")])
        .await
        .expect("publish");
    let ProcessReplaySubscribeOutcome::Subscribed(mut subscription) = store
        .subscribe_after_cursor(&first[0].cursor)
        .await
        .expect("subscribe")
    else {
        panic!("the tail subscribes");
    };

    *clock.0.lock_recover() += past_max_age;
    assert_eq!(
        store.expire_idle_processes(),
        1,
        "only the unfollowed window is released"
    );
    assert_eq!(
        replay(&store, &alone[0].cursor).await,
        Err(ProcessReplayGapReason::Unavailable)
    );
    assert_eq!(replay(&store, &first[0].cursor).await, Ok(Vec::new()));
    let second = store
        .publish(&followed, vec![node(&followed, "event-1")])
        .await
        .expect("publish");
    let next = futures_util::StreamExt::next(&mut subscription)
        .await
        .expect("the tail is open")
        .expect("the follower sees no gap");
    assert_eq!(next.cursor, second[0].cursor);

    drop(subscription);
    *clock.0.lock_recover() += past_max_age;
    assert_eq!(store.expire_idle_processes(), 1);
    assert_eq!(
        replay(&store, &second[0].cursor).await,
        Err(ProcessReplayGapReason::Unavailable)
    );
}
