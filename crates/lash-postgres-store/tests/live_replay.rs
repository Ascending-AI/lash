//! The PostgreSQL live replay store (FIG-5101,
//! `lash::postgres::PostgresLiveReplayStore`): the store-generic live-replay
//! laws, and two replicas sharing one database.
//!
//! Every store runs in its own schema of one isolated database; two stores
//! over one schema are two replicas of one host.

#![expect(
    clippy::expect_used,
    reason = "test target: the fixtures around the laws are test code too"
)]

use std::sync::Arc;
use std::time::Duration;

use lash::postgres::{
    PostgresEndpoints, PostgresHostConfig, PostgresLiveReplayStore, ReplayDataPolicy,
    ReplaySchemaMode,
};
use lash_core::{
    LiveReplayEventDraft, LiveReplayGapReason, LiveReplayOutcome, LiveReplayStore,
    LiveReplayStoreError, LiveReplaySubscribeOutcome, LiveReplaySubscription, SessionCursor,
    SessionObservationEvent, SessionObservationEventPayload, SessionRevision, TurnActivity,
    TurnActivityId, TurnEvent,
};
use lash_postgres_store::testing::{IsolatedDatabase, required_database_url};
use lash_sansio::SessionId;

#[path = "live_replay/bench.rs"]
mod bench;
#[path = "live_replay/schema.rs"]
mod schema;

/// A fresh schema's configuration: the store installs its tables, a short
/// tick so laws run quickly, and a small data pool so many stores fit the
/// server's connection limit.
fn config(schema: &str) -> PostgresHostConfig {
    with_data(schema, |_| {})
}

/// [`config`] with `adjust` applied to its replay data policy.
fn with_data(schema: &str, adjust: impl FnOnce(&mut ReplayDataPolicy)) -> PostgresHostConfig {
    let mut policy = lash::postgres::LiveReplayPolicy::default();
    policy.data.schema = schema.to_string();
    policy.data.schema_mode = ReplaySchemaMode::Install;
    policy.data.publish_tick = Duration::from_millis(1);
    policy.data.publish_concurrency = 2;
    policy.pool.max_connections = 3;
    adjust(&mut policy.data);
    PostgresHostConfig {
        live_replay: Some(policy),
        ..PostgresHostConfig::default()
    }
}

fn endpoints(url: &str) -> PostgresEndpoints {
    PostgresEndpoints::from_url(url).expect("the database URL parses")
}

fn fresh_schema() -> String {
    format!("live_replay_{}", uuid::Uuid::new_v4().simple())
}

/// Connect a store from synchronous code, as the law factories are.
fn connect(url: &str, config: PostgresHostConfig) -> Arc<dyn LiveReplayStore> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| {
        handle.block_on(async {
            Arc::new(
                PostgresLiveReplayStore::connect(&endpoints(url), &config)
                    .await
                    .expect("connect the PostgreSQL live replay store"),
            ) as Arc<dyn LiveReplayStore>
        })
    })
}

lash_conformance::live_replay_tests!({
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let url = database.url().to_string();
    let preserved_schema = fresh_schema();
    let original = connect(&url, config(&preserved_schema));
    let preserved = connect(&url, config(&preserved_schema));
    let fresh = connect(&url, config(&fresh_schema()));
    let plain_url = url.clone();
    let capacity_url = url.clone();
    let ttl_url = url.clone();
    (
        database,
        move || connect(&plain_url, config(&fresh_schema())),
        move || {
            connect(
                &capacity_url,
                with_data(&fresh_schema(), |data| data.max_events_per_session = 1),
            )
        },
        move || {
            connect(
                &ttl_url,
                with_data(&fresh_schema(), |data| {
                    data.max_events_per_session = 16;
                    data.max_age = Duration::from_millis(1);
                }),
            )
        },
        Duration::from_millis(20),
        (original, fresh, preserved),
    )
});

fn text(id: &str, text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(TurnActivity {
        id: TurnActivityId::new(id),
        correlation_id: TurnActivityId::new("text:0"),
        event: TurnEvent::AssistantProseDelta {
            text: text.into(),
            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
        },
    })
}

fn label(event: &SessionObservationEvent) -> String {
    match &event.payload {
        SessionObservationEventPayload::TurnActivity(TurnActivity {
            event: TurnEvent::AssistantProseDelta { text, .. },
            ..
        }) => text.to_string(),
        other => format!("{other:?}"),
    }
}

/// The next item of a live replay subscription.
async fn next_item(
    subscription: &mut LiveReplaySubscription,
) -> Option<Result<Arc<SessionObservationEvent>, LiveReplayStoreError>> {
    use lash::observe::Stream as _;
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *subscription).poll_next(cx)).await
}

fn live_position(cursor: &SessionCursor) -> u64 {
    cursor.parse().expect("a store cursor parses").live_position
}

async fn publish(
    store: &Arc<dyn LiveReplayStore>,
    session: &SessionId,
    id: &str,
    body: &str,
) -> Arc<SessionObservationEvent> {
    store
        .publish(
            session,
            SessionRevision::new(1),
            vec![LiveReplayEventDraft::new(
                None::<lash_core::TurnId>,
                text(id, body),
            )],
        )
        .await
        .expect("publish")
        .into_iter()
        .next()
        .expect("a fresh activity publishes")
}

fn subscribed(
    outcome: Result<LiveReplaySubscribeOutcome, LiveReplayStoreError>,
) -> LiveReplaySubscription {
    match outcome.expect("subscribe") {
        LiveReplaySubscribeOutcome::Subscribed(subscription) => subscription,
        LiveReplaySubscribeOutcome::Gap(reason) => {
            panic!("expected a subscription, got {reason:?}")
        }
    }
}

async fn next(subscription: &mut LiveReplaySubscription) -> Arc<SessionObservationEvent> {
    tokio::time::timeout(Duration::from_secs(5), next_item(subscription))
        .await
        .expect("a live event arrives")
        .expect("the subscription is open")
        .expect("the subscription yields an event")
}

/// Two replicas: one schema, two stores.
struct Replicas {
    database: IsolatedDatabase,
    schema: String,
    a: Arc<dyn LiveReplayStore>,
    b: Arc<dyn LiveReplayStore>,
}

async fn replicas() -> Replicas {
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let schema = fresh_schema();
    let a = connect(database.url(), config(&schema));
    let b = connect(database.url(), config(&schema));
    Replicas {
        database,
        schema,
        a,
        b,
    }
}

/// P1/P2 across replicas: writers on two replicas racing on one session
/// share one gap-free position order, a subscriber on B sees A's events
/// live in that order, and B's cursor for a stale snapshot sits before
/// A's events (P12).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writers_on_two_replicas_share_one_gap_free_order_that_both_observe() {
    let Replicas {
        database: _database,
        a,
        b,
        ..
    } = replicas().await;
    let session = SessionId::from("replicated");
    let start = b.current_cursor(&session, SessionRevision::new(0));
    let mut on_b = subscribed(b.subscribe_after_cursor(&start).await);

    const BATCHES: usize = 150;
    let writers = [("a", Arc::clone(&a)), ("b", Arc::clone(&b))].map(|(name, store)| {
        let session = session.clone();
        tokio::spawn(async move {
            let mut positions = Vec::new();
            for batch in 0..BATCHES {
                let event = publish(
                    &store,
                    &session,
                    &format!("{name}-{batch}"),
                    &format!("{name}{batch}"),
                )
                .await;
                positions.push(live_position(&event.cursor));
            }
            positions
        })
    });
    let mut published = Vec::new();
    for writer in writers {
        let positions = writer.await.expect("join a writer");
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "a writer's events keep its order"
        );
        published.extend(positions);
    }
    published.sort_unstable();
    let first = published[0];
    assert_eq!(
        published,
        (first..first + 2 * BATCHES as u64).collect::<Vec<_>>(),
        "two replicas' writers share one contiguous position sequence"
    );

    let replayed = match a.replay_after_cursor(&start).await.expect("replay on A") {
        LiveReplayOutcome::Replayed(events) => events,
        LiveReplayOutcome::Gap(reason) => panic!("replay on A gapped: {reason:?}"),
    };
    let mut live = Vec::new();
    while live.len() < replayed.len() {
        live.push(next(&mut on_b).await);
    }
    assert_eq!(
        live.iter()
            .map(|event| event.cursor.clone())
            .collect::<Vec<_>>(),
        replayed
            .iter()
            .map(|event| event.cursor.clone())
            .collect::<Vec<_>>(),
        "B's subscriber sees both replicas' events once, in A's replay order"
    );
    assert!(
        live.iter().any(|event| label(event).starts_with('a')),
        "B's subscriber sees events A published"
    );

    // Each replica's mirror learned the other's events from its doorbells.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let tail = live_position(&b.current_cursor(&session, SessionRevision::new(1)));
            if tail == first + 2 * BATCHES as u64 - 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("B's current cursor reaches the shared tail");
    assert_eq!(
        b.current_cursor(&session, SessionRevision::new(0)),
        start,
        "a snapshot older than every event replays them all"
    );
}

/// P5: crash recovery or failover truncates the unlogged log; the next
/// writer rotates the incarnation, every older cursor gaps on both
/// replicas, and a live subscription on the other replica ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_truncated_log_rotates_the_incarnation_for_every_replica() {
    let Replicas {
        database,
        schema,
        a,
        b,
    } = replicas().await;
    let session = SessionId::from("truncated");
    let before = publish(&a, &session, "k#0", "before").await;
    let mut on_b = subscribed(b.subscribe_after_cursor(&before.cursor).await);

    let mut connection = <sqlx::PgConnection as sqlx::Connection>::connect(database.url())
        .await
        .expect("connect to truncate");
    sqlx::query(&format!(
        "TRUNCATE \"{schema}\".live_replay_log, \"{schema}\".live_replay_head"
    ))
    .execute(&mut connection)
    .await
    .expect("truncate the unlogged tables");

    let after = publish(&a, &session, "k#1", "after").await;
    assert_ne!(
        after.replay_incarnation_id(),
        before.replay_incarnation_id(),
        "a writer after the truncation publishes under a new incarnation"
    );
    for (name, store) in [("A", &a), ("B", &b)] {
        assert!(
            matches!(
                store
                    .replay_after_cursor(&before.cursor)
                    .await
                    .expect("replay"),
                LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable)
            ),
            "{name}: a cursor from the lost history gaps"
        );
    }
    let ended = tokio::time::timeout(Duration::from_secs(5), next_item(&mut on_b))
        .await
        .expect("B's subscription ends");
    assert!(
        matches!(ended, None | Some(Err(LiveReplayStoreError::Closed))),
        "B's subscription into the lost history closes, got {ended:?}"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while b
            .current_cursor(&session, SessionRevision::new(1))
            .parse()
            .expect("parse")
            .replay_incarnation_id
            != after.replay_incarnation_id()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("B's cursors name the new incarnation");
}

/// P11: invalidating a session on one replica gaps every cursor into it and
/// closes the other replica's live subscriptions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalidation_closes_subscriptions_on_every_replica() {
    let Replicas {
        database: _database,
        a,
        b,
        ..
    } = replicas().await;
    let session = SessionId::from("invalidated");
    let first = publish(&a, &session, "k#0", "first").await;
    let mut on_b = subscribed(b.subscribe_after_cursor(&first.cursor).await);
    a.invalidate_session(&session)
        .await
        .expect("invalidate on A");
    let ended = tokio::time::timeout(Duration::from_secs(5), next_item(&mut on_b))
        .await
        .expect("B's subscription ends");
    assert!(
        matches!(ended, None | Some(Err(LiveReplayStoreError::Closed))),
        "B's subscription closes, got {ended:?}"
    );
    assert!(
        matches!(
            b.subscribe_after_cursor(&first.cursor)
                .await
                .expect("resubscribe on B"),
            LiveReplaySubscribeOutcome::Gap(LiveReplayGapReason::Unavailable)
        ),
        "a cursor from before the invalidation gaps on B"
    );
    let fresh = b.current_cursor(&session, SessionRevision::new(1));
    let mut again = subscribed(b.subscribe_after_cursor(&fresh).await);
    let next_event = publish(&a, &session, "k#1", "second").await;
    assert_eq!(
        next(&mut again).await.cursor,
        next_event.cursor,
        "fresh continuity follows"
    );
}

/// A takeover across replicas (FIG-5366): replica B streams a session and
/// dies with a batch in flight; replica A, which took the turn over, reads
/// back everything B published after the cursor B pinned with the call
/// before streaming (FIG-5399), so a re-sent call can retract B's abandoned
/// attempt, and its own publication continues the
/// sequence: a subscriber on A follows B's events and A's in one order,
/// without a gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_replica_reads_back_a_dead_replicas_stream_and_continues_it() {
    let Replicas {
        database: _database,
        a,
        b,
        ..
    } = replicas().await;
    let session = SessionId::from("taken-over");
    let start = a.current_cursor(&session, SessionRevision::new(0));
    let mut on_a = subscribed(a.subscribe_after_cursor(&start).await);
    let pinned = b.current_cursor(&session, SessionRevision::new(1));
    publish(&b, &session, "attempt-1#0", "abandoned one").await;
    publish(&b, &session, "attempt-1#1", "abandoned two").await;
    let in_flight = {
        let b = Arc::clone(&b);
        let session = session.clone();
        tokio::spawn(async move {
            b.publish(
                &session,
                SessionRevision::new(1),
                vec![LiveReplayEventDraft::new(
                    None::<lash_core::TurnId>,
                    text("attempt-1#2", "abandoned three"),
                )],
            )
            .await
        })
    };
    in_flight.abort();
    drop(b);

    let window = match a.replay_after_cursor(&pinned).await.expect("replay on A") {
        LiveReplayOutcome::Replayed(events) => events,
        LiveReplayOutcome::Gap(reason) => panic!("B's pinned cursor gapped on A: {reason:?}"),
    };
    let labels: Vec<String> = window.iter().map(|event| label(event)).collect();
    assert_eq!(
        labels[..2],
        ["abandoned one", "abandoned two"],
        "A reads back what B published"
    );
    let resumed = publish(&a, &session, "attempt-2#0", "resumed").await;
    assert_eq!(
        live_position(&resumed.cursor),
        live_position(&window.last().expect("B published").cursor) + 1,
        "A's publication continues B's sequence"
    );
    let mut followed = Vec::new();
    while followed.len() < window.len() + 1 {
        followed.push(next(&mut on_a).await);
    }
    assert_eq!(
        followed
            .iter()
            .map(|event| event.cursor.clone())
            .collect::<Vec<_>>(),
        window
            .iter()
            .chain(std::iter::once(&resumed))
            .map(|event| event.cursor.clone())
            .collect::<Vec<_>>(),
        "A's subscriber follows B's events and A's in one order"
    );
}
