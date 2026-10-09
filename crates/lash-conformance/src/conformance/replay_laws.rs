//! The replay recovery and handoff laws, generic over a replay store.
//!
//! Session live replay ([`LiveReplayStore`]) and process replay
//! ([`ProcessReplayStore`]) are separate stores with separate events and
//! cursors, and one contract: one position sequence per subject, exact and
//! linearizable replay and subscription, typed gaps, incarnations, and
//! cursors that stay behind newer revisions. These laws state that contract
//! once. A [`ReplayLawKind`] says how a law publishes to, reads and names
//! things in one kind of store; `live_replay` and `process_replay` register
//! the laws for theirs and add what only their payloads can state.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt as _};
use pretty_assertions::assert_eq;

use super::helpers::assert_fresh_instances;

/// What a replay or a subscription answered.
#[derive(Debug)]
pub enum ReplayLawOutcome<T> {
    Continued(T),
    Gap(ReplayLawGap),
}

/// The two gap classes every replay store answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayLawGap {
    Trimmed,
    Unavailable,
}

/// What a law reads of one published event.
#[derive(Clone, Debug)]
pub struct ReplayLawEvent<C> {
    pub cursor: C,
    /// The label the event was published with.
    pub label: String,
    pub position: u64,
    pub revision: u64,
    pub incarnation: String,
}

/// One kind of replay store, as the laws drive it.
#[async_trait::async_trait]
pub trait ReplayLawKind: Send + Sync + 'static {
    type Store: ?Sized + Send + Sync + 'static;
    /// The identity a store keeps one window for.
    type Subject: Clone + Send + Sync + 'static;
    type Cursor: Clone + PartialEq + fmt::Debug + Send + Sync + 'static;
    type Event: Clone + Send + Sync + 'static;
    type Error: fmt::Display + Send + 'static;
    type Subscription: Stream<Item = Result<Self::Event, Self::Error>> + Unpin + Send + 'static;

    /// The subject `label` names; equal labels name one subject.
    fn subject(label: &str) -> Self::Subject;

    /// Publish one batch of provisional events at `revision`, one per
    /// label. Every call's events are new observations, never redeliveries.
    async fn publish(
        store: &Self::Store,
        subject: &Self::Subject,
        revision: u64,
        labels: Vec<String>,
    ) -> Result<Vec<Self::Event>, Self::Error>;

    async fn current_cursor(
        store: &Self::Store,
        subject: &Self::Subject,
        revision: u64,
    ) -> Self::Cursor;

    async fn replay(
        store: &Self::Store,
        cursor: &Self::Cursor,
    ) -> Result<ReplayLawOutcome<Vec<Self::Event>>, Self::Error>;

    async fn subscribe(
        store: &Self::Store,
        cursor: &Self::Cursor,
    ) -> Result<ReplayLawOutcome<Self::Subscription>, Self::Error>;

    /// Invalidate one subject's continuity.
    async fn invalidate(store: &Self::Store, subject: &Self::Subject);

    /// Apply retention to one subject's window.
    async fn trim(store: &Self::Store, subject: &Self::Subject);

    fn describe(event: &Self::Event) -> ReplayLawEvent<Self::Cursor>;

    /// A well-formed cursor of `incarnation` at `position`.
    fn cursor_at(
        incarnation: &str,
        subject: &Self::Subject,
        revision: u64,
        position: u64,
    ) -> Self::Cursor;

    /// A cursor token no store wrote.
    fn malformed_cursor() -> Self::Cursor;

    fn is_malformed_cursor_error(error: &Self::Error) -> bool;

    /// Whether `error` ends a subscription whose continuity was
    /// invalidated.
    fn is_closed(error: &Self::Error) -> bool;
}

/// A replay store that can invalidate every subject at once.
#[async_trait::async_trait]
pub trait ReplayLawInvalidateAll: ReplayLawKind {
    async fn invalidate_all(store: &Self::Store);
}

async fn publish_one<K: ReplayLawKind>(
    store: &K::Store,
    subject: &K::Subject,
    revision: u64,
    label: &str,
) -> ReplayLawEvent<K::Cursor> {
    let events = K::publish(store, subject, revision, vec![label.to_string()])
        .await
        .unwrap_or_else(|error| panic!("publish `{label}`: {error}"));
    assert_eq!(events.len(), 1, "a single fresh event publishes whole");
    K::describe(&events[0])
}

async fn replayed<K: ReplayLawKind>(
    store: &K::Store,
    cursor: &K::Cursor,
    context: &str,
) -> Vec<ReplayLawEvent<K::Cursor>> {
    match K::replay(store, cursor).await {
        Ok(ReplayLawOutcome::Continued(events)) => events.iter().map(K::describe).collect(),
        Ok(ReplayLawOutcome::Gap(gap)) => panic!("{context}: expected a replay, got gap {gap:?}"),
        Err(error) => panic!("{context}: {error}"),
    }
}

async fn subscribed<K: ReplayLawKind>(
    store: &K::Store,
    cursor: &K::Cursor,
    context: &str,
) -> K::Subscription {
    match K::subscribe(store, cursor).await {
        Ok(ReplayLawOutcome::Continued(subscription)) => subscription,
        Ok(ReplayLawOutcome::Gap(gap)) => {
            panic!("{context}: expected a subscription, got gap {gap:?}")
        }
        Err(error) => panic!("{context}: {error}"),
    }
}

/// Both readers answer `expected` from `cursor`: a replay and a
/// subscription never disagree about a gap.
async fn expect_gap<K: ReplayLawKind>(
    store: &K::Store,
    cursor: &K::Cursor,
    expected: ReplayLawGap,
    context: &str,
) {
    match K::replay(store, cursor).await {
        Ok(ReplayLawOutcome::Gap(gap)) => assert_eq!(gap, expected, "{context}: replay"),
        Ok(ReplayLawOutcome::Continued(events)) => panic!(
            "{context}: expected replay gap {expected:?}, got {} events",
            events.len()
        ),
        Err(error) => panic!("{context}: replay: {error}"),
    }
    match K::subscribe(store, cursor).await {
        Ok(ReplayLawOutcome::Gap(gap)) => assert_eq!(gap, expected, "{context}: subscribe"),
        Ok(ReplayLawOutcome::Continued(_)) => {
            panic!("{context}: expected subscribe gap {expected:?}, got a subscription")
        }
        Err(error) => panic!("{context}: subscribe: {error}"),
    }
}

/// The subscription's next event, however long the store takes to deliver
/// it. A store's delivery is not bounded by how loaded its host is
/// (FIG-5148): an event that never arrives leaves the law to the test
/// target's timeout.
async fn next_event<K: ReplayLawKind>(
    subscription: &mut K::Subscription,
    context: &str,
) -> ReplayLawEvent<K::Cursor> {
    match subscription.next().await {
        Some(Ok(event)) => K::describe(&event),
        Some(Err(error)) => panic!("{context}: replay subscriber failed: {error}"),
        None => panic!("{context}: replay subscriber closed"),
    }
}

/// The subscription has nothing to deliver now.
async fn expect_waiting<K: ReplayLawKind>(
    subscription: &mut K::Subscription,
    wait: Duration,
    context: &str,
) {
    assert!(
        tokio::time::timeout(wait, subscription.next())
            .await
            .is_err(),
        "{context}"
    );
}

fn labels<C>(events: &[ReplayLawEvent<C>]) -> Vec<String> {
    events.iter().map(|event| event.label.clone()).collect()
}

fn cursors<C: Clone>(events: &[ReplayLawEvent<C>]) -> Vec<C> {
    events.iter().map(|event| event.cursor.clone()).collect()
}

/// `make` must return a fresh, empty store on each call.
///
/// The non-durable observation contract a reconnecting host relies on:
/// cursors track per-subject positions, replay returns only events after the
/// cursor, subscriptions deliver buffered events before live ones, malformed
/// cursors fail before replay, cursors ahead of the tail report a
/// recoverable unavailable gap, and writers racing on one subject share one
/// gap-free order.
pub async fn replay_store_laws<K, F>(make: F)
where
    K: ReplayLawKind,
    F: Fn() -> Arc<K::Store>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "replay_store_laws");
    drop((first, second));
    exclusive_after_valid_cursor::<K>(&make()).await;
    cursor_preserves_newer_revisions::<K>(&make()).await;
    subscribe_replays_then_yields_live_events::<K>(&make()).await;
    rejects_malformed_cursors::<K>(&make()).await;
    empty_is_proven_continuity_not_missing_history::<K>(&make()).await;
    replay_cut_and_live_registration_are_linearizable::<K, F>(&make).await;
    concurrent_writers_share_one_gap_free_order::<K>(make()).await;
}

async fn exclusive_after_valid_cursor<K: ReplayLawKind>(store: &K::Store) {
    let (a, b) = (K::subject("subject-a"), K::subject("subject-b"));
    let start_a = K::current_cursor(store, &a, 7).await;
    let start_b = K::current_cursor(store, &b, 7).await;
    assert!(
        replayed::<K>(store, &start_a, "empty replay from initial cursor")
            .await
            .is_empty(),
        "initial cursor must replay no events"
    );

    let first_a = publish_one::<K>(store, &a, 7, "alpha one").await;
    let first_b = publish_one::<K>(store, &b, 7, "beta one").await;
    let second_a = publish_one::<K>(store, &a, 8, "alpha two").await;

    assert_eq!(first_a.revision, 7);
    assert_eq!(second_a.revision, 8);
    assert_eq!(
        first_a.incarnation, second_a.incarnation,
        "one store construction must stamp one stable replay incarnation"
    );
    assert_eq!(
        first_a.incarnation, first_b.incarnation,
        "the replay incarnation is store-scoped rather than subject-scoped"
    );
    assert_ne!(
        first_a.cursor, second_a.cursor,
        "each appended event must receive a distinct cursor"
    );

    assert_eq!(
        labels(&replayed::<K>(store, &start_a, "subject-a replay").await),
        ["alpha one", "alpha two"]
    );
    assert_eq!(
        labels(&replayed::<K>(store, &first_a.cursor, "subject-a replay after first").await),
        ["alpha two"]
    );
    assert_eq!(
        labels(&replayed::<K>(store, &start_b, "subject-b replay").await),
        ["beta one"],
        "a replay never holds another subject's events"
    );

    let tail_a = K::current_cursor(store, &a, 9).await;
    assert!(
        replayed::<K>(store, &tail_a, "subject-a replay from tail cursor")
            .await
            .is_empty(),
        "current tail cursor must not replay old events"
    );

    let mut from_start = subscribed::<K>(store, &start_a, "subscribe from initial cursor").await;
    let first = next_event::<K>(&mut from_start, "first exclusive event").await;
    let second = next_event::<K>(&mut from_start, "second exclusive event").await;
    assert_eq!(labels(&[first, second]), ["alpha one", "alpha two"]);

    let mut after_first = subscribed::<K>(store, &first_a.cursor, "subscribe after first").await;
    let suffix = next_event::<K>(&mut after_first, "exclusive suffix event").await;
    assert_eq!(suffix.label, "alpha two");

    let mut at_tail = subscribed::<K>(store, &second_a.cursor, "subscribe at tail").await;
    expect_waiting::<K>(
        &mut at_tail,
        Duration::from_millis(10),
        "a valid tail subscription must wait rather than gap or replay",
    )
    .await;
}

async fn cursor_preserves_newer_revisions<K: ReplayLawKind>(store: &K::Store) {
    let subject = K::subject("stale-snapshot");
    publish_one::<K>(store, &subject, 2, "newer worker commit").await;
    let stale = K::current_cursor(store, &subject, 1).await;
    assert_eq!(
        labels(&replayed::<K>(store, &stale, "newer revision after stale snapshot").await),
        ["newer worker commit"]
    );
}

async fn subscribe_replays_then_yields_live_events<K: ReplayLawKind>(store: &K::Store) {
    let subject = K::subject("subscribe");
    let start = K::current_cursor(store, &subject, 3).await;
    publish_one::<K>(store, &subject, 3, "buffered one").await;
    publish_one::<K>(store, &subject, 3, "buffered two").await;

    let mut subscription = subscribed::<K>(store, &start, "subscribe after initial cursor").await;
    let first = next_event::<K>(&mut subscription, "first buffered event").await;
    let second = next_event::<K>(&mut subscription, "second buffered event").await;
    assert_eq!(labels(&[first, second]), ["buffered one", "buffered two"]);

    publish_one::<K>(store, &subject, 3, "live three").await;
    let live = next_event::<K>(&mut subscription, "live event after replay").await;
    assert_eq!(live.label, "live three");
}

async fn rejects_malformed_cursors<K: ReplayLawKind>(store: &K::Store) {
    let malformed = K::malformed_cursor();
    assert!(
        matches!(K::replay(store, &malformed).await, Err(error) if K::is_malformed_cursor_error(&error)),
        "replay must reject malformed cursors before reading replay state"
    );
    assert!(
        matches!(K::subscribe(store, &malformed).await, Err(error) if K::is_malformed_cursor_error(&error)),
        "subscribe must reject malformed cursors before reading replay state"
    );
}

async fn empty_is_proven_continuity_not_missing_history<K: ReplayLawKind>(store: &K::Store) {
    let subject = K::subject("ahead");
    let existing = publish_one::<K>(store, &subject, 4, "existing").await;
    assert!(
        replayed::<K>(store, &existing.cursor, "replay from proven tail")
            .await
            .is_empty(),
        "only a valid tail may replay empty"
    );
    let mut tail = subscribed::<K>(store, &existing.cursor, "subscribe from proven tail").await;
    expect_waiting::<K>(
        &mut tail,
        Duration::from_millis(10),
        "a proven tail subscription must remain live and empty",
    )
    .await;

    let ahead = K::cursor_at(&existing.incarnation, &subject, 4, existing.position + 98);
    expect_gap::<K>(
        store,
        &ahead,
        ReplayLawGap::Unavailable,
        "cursor ahead of tail",
    )
    .await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn replay_cut_and_live_registration_are_linearizable<K, F>(make: &F)
where
    K: ReplayLawKind,
    F: Fn() -> Arc<K::Store>,
{
    const RACES: usize = 64;
    for race in 0..RACES {
        let store = make();
        let subject = K::subject(&format!("subscribe-race-{race}"));
        let start = K::current_cursor(&store, &subject, 5).await;
        let prior = publish_one::<K>(&store, &subject, 5, "prior").await;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let subscribe = tokio::spawn({
            let (store, start, barrier) = (Arc::clone(&store), start.clone(), Arc::clone(&barrier));
            async move {
                barrier.wait().await;
                subscribed::<K>(&store, &start, "racing subscription").await
            }
        });
        let append = tokio::spawn({
            let (store, subject, barrier) =
                (Arc::clone(&store), subject.clone(), Arc::clone(&barrier));
            async move {
                barrier.wait().await;
                publish_one::<K>(&store, &subject, 5, "raced").await
            }
        });
        barrier.wait().await;

        let mut subscription = subscribe.await.expect("join racing subscription");
        let raced = append.await.expect("join racing append");
        let first = next_event::<K>(&mut subscription, "prior racing event").await;
        let second = next_event::<K>(&mut subscription, "concurrent racing event").await;
        assert_eq!(first.cursor, prior.cursor, "prior event must remain first");
        assert_eq!(
            second.cursor, raced.cursor,
            "raced event must appear exactly once"
        );
        expect_waiting::<K>(
            &mut subscription,
            Duration::from_millis(2),
            "the append racing subscription creation must not be duplicated",
        )
        .await;
    }
}

/// Writers [`concurrent_writers_share_one_gap_free_order`] races on one
/// subject, as a run's runtime and another publisher do.
const CONCURRENT_WRITERS: usize = 2;
/// Batches each concurrent writer publishes; every third holds two events.
const CONCURRENT_BATCHES: usize = 120;

/// The store is each subject's sequencer: writers racing on one subject
/// receive contiguous positions in one total order, each writer's events
/// keep its own order, and a subscriber from before the race and a replay
/// after it see that same order, with nothing missing or repeated (FIG-5099).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn concurrent_writers_share_one_gap_free_order<K: ReplayLawKind>(store: Arc<K::Store>) {
    let subject = K::subject("concurrent-writers");
    let start = K::current_cursor(&store, &subject, 3).await;
    let mut subscription =
        subscribed::<K>(&store, &start, "subscribe before the writers race").await;
    let barrier = Arc::new(tokio::sync::Barrier::new(CONCURRENT_WRITERS));
    let writers = (0..CONCURRENT_WRITERS)
        .map(|writer| {
            let (store, subject, barrier) =
                (Arc::clone(&store), subject.clone(), Arc::clone(&barrier));
            tokio::spawn(async move {
                barrier.wait().await;
                let mut published = Vec::new();
                for batch in 0..CONCURRENT_BATCHES {
                    let size = if batch % 3 == 0 { 2 } else { 1 };
                    let batch_labels = (0..size)
                        .map(|part| format!("writer {writer}:{batch}.{part}"))
                        .collect();
                    let events = match K::publish(&store, &subject, 3, batch_labels).await {
                        Ok(events) => events,
                        Err(error) => panic!("a racing writer publishes: {error}"),
                    };
                    assert_eq!(events.len(), size, "a batch publishes whole");
                    published.extend(events.iter().map(K::describe));
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

    for events in &published {
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].position < pair[1].position),
            "a writer's events keep the order it published them in"
        );
    }
    let replay = replayed::<K>(&store, &start, "replay after the race").await;
    assert_eq!(replay.len(), total, "a replay holds every raced event once");
    let first = replay[0].position;
    assert!(
        replay
            .iter()
            .enumerate()
            .all(|(offset, event)| event.position == first + offset as u64),
        "raced writers share one contiguous position sequence"
    );
    let mut published_positions = published
        .iter()
        .flatten()
        .map(|event| event.position)
        .collect::<Vec<_>>();
    published_positions.sort_unstable();
    assert_eq!(
        published_positions,
        replay
            .iter()
            .map(|event| event.position)
            .collect::<Vec<_>>(),
        "the replay holds exactly the events the writers published"
    );
    let mut live = Vec::with_capacity(total);
    while live.len() < total {
        live.push(next_event::<K>(&mut subscription, "raced event").await);
    }
    assert_eq!(
        cursors(&live),
        cursors(&replay),
        "a subscriber from before the race sees the replay's order"
    );
}

/// Subjects [`replay_store_burst`] publishes to at once.
const BURST_SUBJECTS: usize = 6;
/// Events each burst subject publishes: within the default per-subject
/// retention, so every subscriber can hold the whole burst.
const BURST_EVENTS: usize = 600;

/// A burst of events across several subjects reaches every subscriber with
/// no loss, no reordering, no duplicate and no other subject's event: one
/// subscribed before the burst, one subscribed from the same cursor while it
/// runs, and a replay after it ends (FIG-5090).
///
/// `make` must return a fresh store whose per-subject retention and
/// subscriber buffer hold [`BURST_EVENTS`] events.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn replay_store_burst<K, F>(make: F)
where
    K: ReplayLawKind,
    F: Fn() -> Arc<K::Store>,
{
    let store = make();
    let subjects = (0..BURST_SUBJECTS)
        .map(|index| K::subject(&format!("burst-subject-{index}")))
        .collect::<Vec<_>>();
    let expected = |index: usize| {
        (0..BURST_EVENTS)
            .map(|event| format!("burst {index}:{event}"))
            .collect::<Vec<_>>()
    };
    let mut starts = Vec::with_capacity(subjects.len());
    for subject in &subjects {
        starts.push(K::current_cursor(&store, subject, 4).await);
    }
    let drain = |subscription: K::Subscription| {
        tokio::spawn(async move {
            let mut subscription = subscription;
            let mut seen = Vec::with_capacity(BURST_EVENTS);
            while seen.len() < BURST_EVENTS {
                seen.push(
                    next_event::<K>(&mut subscription, "burst event")
                        .await
                        .label,
                );
            }
            expect_waiting::<K>(
                &mut subscription,
                Duration::from_millis(20),
                "a burst subscriber receives no event past the burst",
            )
            .await;
            seen
        })
    };
    let mut early = Vec::with_capacity(starts.len());
    for start in &starts {
        early.push(drain(
            subscribed::<K>(&store, start, "subscribe before the burst").await,
        ));
    }

    let publishers = subjects
        .iter()
        .enumerate()
        .map(|(index, subject)| {
            let (store, subject) = (Arc::clone(&store), subject.clone());
            tokio::spawn(async move {
                for event in 0..BURST_EVENTS {
                    publish_one::<K>(&store, &subject, 4, &format!("burst {index}:{event}")).await;
                    if event % 64 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    tokio::task::yield_now().await;
    let mut late = Vec::with_capacity(starts.len());
    for start in &starts {
        late.push(drain(
            subscribed::<K>(&store, start, "subscribe during the burst").await,
        ));
    }
    for publisher in publishers {
        publisher.await.expect("join a burst publisher");
    }

    for (index, (early, late)) in early.into_iter().zip(late).enumerate() {
        assert_eq!(
            early.await.expect("join an early burst subscriber"),
            expected(index),
            "a subscriber from before the burst sees every event once, in order"
        );
        assert_eq!(
            late.await.expect("join a late burst subscriber"),
            expected(index),
            "a subscriber joining mid-burst sees every event once, in order"
        );
        assert_eq!(
            labels(&replayed::<K>(&store, &starts[index], "replay after the burst").await),
            expected(index),
            "a replay after the burst holds every event once, in order"
        );
    }
}

/// Capacity retention forces a snapshot: a cursor behind what the window
/// dropped answers `Trimmed`, and the retained boundary still continues.
///
/// `make` must return a fresh store configured to retain exactly one event
/// per subject. Stores with a fixed larger capacity should expose a test
/// configuration rather than weakening this contract.
pub async fn replay_store_capacity_trim<K, F>(make: F)
where
    K: ReplayLawKind,
    F: Fn() -> Arc<K::Store>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "replay_store_capacity_trim");
    drop((first, second));
    let store = make();
    let subject = K::subject("capacity");
    let start = K::current_cursor(&store, &subject, 1).await;
    let first = publish_one::<K>(&store, &subject, 1, "capacity one").await;
    publish_one::<K>(&store, &subject, 1, "capacity two").await;

    expect_gap::<K>(
        &store,
        &start,
        ReplayLawGap::Trimmed,
        "capacity trim from dropped cursor",
    )
    .await;
    assert_eq!(
        labels(&replayed::<K>(&store, &first.cursor, "after first cursor").await),
        ["capacity two"]
    );
    let mut after_first =
        subscribed::<K>(&store, &first.cursor, "subscribe from retained boundary").await;
    let retained = next_event::<K>(&mut after_first, "capacity-trim retained suffix").await;
    assert_eq!(retained.label, "capacity two");

    expect_clean_tail::<K>(&store, &subject, 1, "capacity").await;
}

/// Age retention forces a snapshot, as capacity does.
///
/// `make` must return a fresh store whose event TTL expires within
/// `expiration_wait`. The law calls the store's trim after waiting so
/// implementations can keep trimming lazy and local.
pub async fn replay_store_ttl_trim<K, F>(make: F, expiration_wait: Duration)
where
    K: ReplayLawKind,
    F: Fn() -> Arc<K::Store>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "replay_store_ttl_trim");
    drop((first, second));
    let store = make();
    let subject = K::subject("ttl");
    let start = K::current_cursor(&store, &subject, 1).await;
    publish_one::<K>(&store, &subject, 1, "ttl expired").await;
    tokio::time::sleep(expiration_wait).await;
    K::trim(&store, &subject).await;

    expect_gap::<K>(
        &store,
        &start,
        ReplayLawGap::Trimmed,
        "ttl trim from expired cursor",
    )
    .await;
    expect_clean_tail::<K>(&store, &subject, 1, "ttl").await;
}

/// The subject's current cursor replays nothing and its subscription waits.
async fn expect_clean_tail<K: ReplayLawKind>(
    store: &K::Store,
    subject: &K::Subject,
    revision: u64,
    context: &str,
) {
    let tail = K::current_cursor(store, subject, revision).await;
    assert!(
        replayed::<K>(store, &tail, context).await.is_empty(),
        "{context}: the tail cursor must replay no events"
    );
    let mut subscription = subscribed::<K>(store, &tail, context).await;
    expect_waiting::<K>(
        &mut subscription,
        Duration::from_millis(10),
        "a tail subscription must wait for a future event",
    )
    .await;
}

/// A fresh replay incarnation invalidates an old cursor, while a store
/// which genuinely preserves both history and incarnation may continue it.
///
/// `original` and `preserved` must be distinct handles over the same replay
/// substrate. `fresh` must start empty with a new incarnation. The vector
/// deliberately creates the same numeric position in `fresh`, so a backend
/// which compares offsets but not incarnation returns a forbidden clean
/// empty replay and fails deterministically.
pub async fn replay_incarnation_change_invalidates_cursor<K: ReplayLawKind>(
    original: Arc<K::Store>,
    fresh: Arc<K::Store>,
    preserved: Arc<K::Store>,
) {
    assert!(
        !Arc::ptr_eq(&original, &preserved),
        "preserved-history conformance requires a distinct reopened handle"
    );
    let subject = K::subject("incarnation-change");
    let old = publish_one::<K>(&original, &subject, 9, "old incarnation").await;
    let collision = publish_one::<K>(&fresh, &subject, 9, "fresh incarnation collision").await;
    assert_ne!(
        old.cursor, collision.cursor,
        "equal numeric positions in different incarnations must produce distinct cursors"
    );
    expect_gap::<K>(
        &fresh,
        &old.cursor,
        ReplayLawGap::Unavailable,
        "fresh incarnation from old cursor",
    )
    .await;

    assert!(
        replayed::<K>(
            &preserved,
            &old.cursor,
            "preserved incarnation from old tail"
        )
        .await
        .is_empty(),
        "a preserved incarnation may prove its old tail is clean empty"
    );
    publish_one::<K>(&preserved, &subject, 9, "preserved continuation").await;
    assert_eq!(
        labels(&replayed::<K>(&preserved, &old.cursor, "preserved continuation").await),
        ["preserved continuation"]
    );
}

/// The subscription ends closed, with nothing delivered first.
async fn expect_closed<K: ReplayLawKind>(subscription: &mut K::Subscription, context: &str) {
    match subscription.next().await {
        Some(Err(error)) if K::is_closed(&error) => {}
        Some(Err(error)) => panic!("{context}: expected a closed subscription, got {error}"),
        Some(Ok(event)) => panic!(
            "{context}: expected a closed subscription, got event `{}`",
            K::describe(&event).label
        ),
        None => {}
    }
}

/// Invalidating one subject gaps that subject alone: its earlier cursors
/// answer `Unavailable`, its open subscription ends closed, a cursor taken
/// afterwards continues, and another subject's window is untouched.
pub async fn invalidation_gaps_the_subject_alone<K: ReplayLawKind>(store: Arc<K::Store>) {
    let (lost, kept) = (K::subject("invalidated"), K::subject("kept"));
    let lost_start = K::current_cursor(&store, &lost, 1).await;
    let kept_start = K::current_cursor(&store, &kept, 1).await;
    let before = publish_one::<K>(&store, &lost, 1, "before invalidation").await;
    publish_one::<K>(&store, &kept, 1, "kept one").await;
    let mut open = subscribed::<K>(&store, &before.cursor, "subscribe before invalidation").await;

    K::invalidate(&store, &lost).await;

    expect_closed::<K>(&mut open, "a subscription across an invalidation").await;
    for cursor in [&lost_start, &before.cursor] {
        expect_gap::<K>(
            &store,
            cursor,
            ReplayLawGap::Unavailable,
            "a cursor from before an invalidation",
        )
        .await;
    }
    let fresh = K::current_cursor(&store, &lost, 1).await;
    let after = publish_one::<K>(&store, &lost, 1, "after invalidation").await;
    assert!(
        after.position > before.position,
        "positions never repeat across an invalidation"
    );
    assert_eq!(
        labels(&replayed::<K>(&store, &fresh, "a cursor taken after an invalidation").await),
        ["after invalidation"]
    );
    assert_eq!(
        labels(&replayed::<K>(&store, &kept_start, "another subject's replay").await),
        ["kept one"],
        "an invalidation never touches another subject"
    );
}

/// After a store-wide invalidation every earlier cursor of every subject
/// answers `Unavailable`, every open subscription ends closed, and a cursor
/// taken afterwards continues cleanly.
pub async fn store_wide_invalidation_gaps_every_subscriber<K: ReplayLawInvalidateAll>(
    store: Arc<K::Store>,
) {
    let subjects = [K::subject("wide-a"), K::subject("wide-b")];
    let mut before = Vec::new();
    let mut open = Vec::new();
    for subject in &subjects {
        let event = publish_one::<K>(&store, subject, 1, "before invalidation").await;
        open.push(subscribed::<K>(&store, &event.cursor, "subscribe before invalidation").await);
        before.push(event);
    }

    K::invalidate_all(&store).await;

    for ((subject, before), open) in subjects.iter().zip(&before).zip(&mut open) {
        expect_closed::<K>(open, "a subscription across a store-wide invalidation").await;
        expect_gap::<K>(
            &store,
            &before.cursor,
            ReplayLawGap::Unavailable,
            "a cursor from before a store-wide invalidation",
        )
        .await;
        let fresh = K::current_cursor(&store, subject, 1).await;
        let after = publish_one::<K>(&store, subject, 1, "after invalidation").await;
        assert_ne!(
            after.cursor, before.cursor,
            "an old cursor never names a new event"
        );
        assert_eq!(
            labels(&replayed::<K>(&store, &fresh, "a cursor taken afterwards").await),
            ["after invalidation"]
        );
    }
}
