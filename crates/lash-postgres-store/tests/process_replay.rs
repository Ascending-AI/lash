//! The PostgreSQL process replay store (FIG-5568,
//! `lash::postgres::PostgresProcessReplayStore`): the replay laws it shares
//! with every replay store, replicas sharing one database, and its
//! aggregate bounds.
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
    PostgresEndpoints, PostgresHostConfig, PostgresProcessReplayStore, ProcessReplayDataPolicy,
    ProcessReplayPolicy, ReplaySchemaMode,
};
use lash_core::testing::{process_language_observation, process_observation_label};
use lash_core::{
    ProcessId, ProcessObservationCursor, ProcessObservationEvent, ProcessReplayEventDraft,
    ProcessReplayGapReason, ProcessReplayOutcome, ProcessReplayStore, ProcessReplayStoreError,
    ProcessReplaySubscribeOutcome, ProcessReplaySubscription, ProcessSequence,
};
use lash_postgres_store::testing::{IsolatedDatabase, required_database_url};

#[path = "process_replay/schema.rs"]
mod schema;

/// A fresh schema's configuration: the store installs its tables, a short
/// tick so laws run quickly, and a small data pool so many stores fit the
/// server's connection limit.
fn config(schema: &str) -> PostgresHostConfig {
    with_policy(schema, |_| {})
}

/// [`config`] with `adjust` applied to its replay data policy.
fn with_data(
    schema: &str,
    adjust: impl FnOnce(&mut ProcessReplayDataPolicy),
) -> PostgresHostConfig {
    with_policy(schema, |policy| adjust(&mut policy.data))
}

fn with_policy(schema: &str, adjust: impl FnOnce(&mut ProcessReplayPolicy)) -> PostgresHostConfig {
    let mut policy = ProcessReplayPolicy::default();
    policy.data.schema = schema.to_string();
    policy.data.schema_mode = ReplaySchemaMode::Install;
    policy.data.publish_tick = Duration::from_millis(1);
    policy.data.publish_concurrency = 2;
    policy.pool.max_connections = 3;
    adjust(&mut policy);
    PostgresHostConfig {
        process_replay: Some(policy),
        ..PostgresHostConfig::default()
    }
}

fn endpoints(url: &str) -> PostgresEndpoints {
    PostgresEndpoints::from_url(url).expect("the database URL parses")
}

fn fresh_schema() -> String {
    format!("process_replay_{}", uuid::Uuid::new_v4().simple())
}

/// Connect a store from synchronous code, as the law factories are.
fn connect(url: &str, config: PostgresHostConfig) -> Arc<dyn ProcessReplayStore> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| {
        handle.block_on(async {
            Arc::new(
                PostgresProcessReplayStore::connect(&endpoints(url), &config)
                    .await
                    .expect("connect the PostgreSQL process replay store"),
            ) as Arc<dyn ProcessReplayStore>
        })
    })
}

lash_conformance::process_replay_tests!({
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
                with_data(&fresh_schema(), |data| data.max_events_per_process = 1),
            )
        },
        move || {
            connect(
                &ttl_url,
                with_data(&fresh_schema(), |data| {
                    data.max_events_per_process = 16;
                    data.max_age = Duration::from_millis(1);
                }),
            )
        },
        Duration::from_millis(20),
        (original, fresh, preserved),
    )
});

/// Publish one provisional observation labelled `label` under `event_key`.
async fn publish(
    store: &Arc<dyn ProcessReplayStore>,
    process: &ProcessId,
    event_key: &str,
    label: &str,
) -> Arc<ProcessObservationEvent> {
    store
        .publish(
            process,
            vec![ProcessReplayEventDraft::language_execution(
                ProcessSequence::new(1),
                process_language_observation(process, event_key, label),
            )],
        )
        .await
        .expect("publish")
        .into_iter()
        .next()
        .expect("a fresh observation publishes")
}

async fn current(
    store: &Arc<dyn ProcessReplayStore>,
    process: &ProcessId,
) -> ProcessObservationCursor {
    store
        .current_cursor(process, ProcessSequence::new(1))
        .await
        .expect("a current cursor")
}

async fn replay(
    store: &Arc<dyn ProcessReplayStore>,
    cursor: &ProcessObservationCursor,
) -> Result<Vec<Arc<ProcessObservationEvent>>, ProcessReplayGapReason> {
    match store.replay_after_cursor(cursor).await.expect("replay") {
        ProcessReplayOutcome::Replayed(events) => Ok(events),
        ProcessReplayOutcome::Gap(reason) => Err(reason),
    }
}

async fn subscribed(
    store: &Arc<dyn ProcessReplayStore>,
    cursor: &ProcessObservationCursor,
) -> ProcessReplaySubscription {
    match store
        .subscribe_after_cursor(cursor)
        .await
        .expect("subscribe")
    {
        ProcessReplaySubscribeOutcome::Subscribed(subscription) => subscription,
        ProcessReplaySubscribeOutcome::Gap(reason) => {
            panic!("expected a subscription, got {reason:?}")
        }
    }
}

/// The next item of a process replay subscription.
async fn next_item(
    subscription: &mut ProcessReplaySubscription,
) -> Option<Result<Arc<ProcessObservationEvent>, ProcessReplayStoreError>> {
    use lash::observe::Stream as _;
    tokio::time::timeout(
        Duration::from_secs(10),
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut *subscription).poll_next(cx)),
    )
    .await
    .expect("the subscription yields or ends")
}

async fn next(subscription: &mut ProcessReplaySubscription) -> Arc<ProcessObservationEvent> {
    next_item(subscription)
        .await
        .expect("the subscription is open")
        .expect("the subscription yields an event")
}

fn cursors(events: &[Arc<ProcessObservationEvent>]) -> Vec<ProcessObservationCursor> {
    events.iter().map(|event| event.cursor.clone()).collect()
}

/// Two replicas: one schema, two stores.
struct Replicas {
    database: IsolatedDatabase,
    schema: String,
    a: Arc<dyn ProcessReplayStore>,
    b: Arc<dyn ProcessReplayStore>,
}

async fn replicas(adjust: impl Fn(&mut ProcessReplayPolicy)) -> Replicas {
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let schema = fresh_schema();
    let a = connect(database.url(), with_policy(&schema, &adjust));
    let b = connect(database.url(), with_policy(&schema, &adjust));
    Replicas {
        database,
        schema,
        a,
        b,
    }
}

/// Run `statement` on a connection of the test's own.
async fn execute(url: &str, statement: &str) {
    let mut connection = <sqlx::PgConnection as sqlx::Connection>::connect(url)
        .await
        .expect("connect");
    sqlx::raw_sql(statement)
        .execute(&mut connection)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"));
}

/// Writers on two replicas racing on one process share one gap-free
/// position order, a subscriber on B sees A's events live in that order
/// without contacting A, and B's cursor for a stale snapshot sits before
/// what A published at a newer sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writers_on_two_replicas_share_one_gap_free_order_that_both_observe() {
    let Replicas {
        database: _database,
        a,
        b,
        ..
    } = replicas(|_| {}).await;
    let process = ProcessId::fixture("replicated");
    let start = b
        .current_cursor(&process, ProcessSequence::new(0))
        .await
        .expect("a start cursor on B");
    let mut on_b = subscribed(&b, &start).await;

    const BATCHES: usize = 150;
    let writers = [("a", Arc::clone(&a)), ("b", Arc::clone(&b))].map(|(name, store)| {
        let process = process.clone();
        tokio::spawn(async move {
            let mut positions = Vec::new();
            for batch in 0..BATCHES {
                let key = format!("{name}-{batch}");
                positions.push(publish(&store, &process, &key, &key).await.live_position());
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

    let replayed = replay(&a, &start).await.expect("replay on A");
    let mut live = Vec::new();
    while live.len() < replayed.len() {
        live.push(next(&mut on_b).await);
    }
    assert_eq!(
        cursors(&live),
        cursors(&replayed),
        "B's subscriber sees both replicas' events once, in A's replay order"
    );
    assert!(
        live.iter()
            .any(|event| process_observation_label(event).starts_with('a')),
        "B's subscriber sees events A published"
    );
    assert_eq!(
        current(&b, &process)
            .await
            .parse()
            .expect("parse")
            .live_position,
        first + 2 * BATCHES as u64 - 1,
        "B's current cursor is the shared tail"
    );
    assert_eq!(
        b.current_cursor(&process, ProcessSequence::new(0))
            .await
            .expect("a stale cursor on B"),
        start,
        "a snapshot older than every event replays them all"
    );
}

/// Notifications sent while a replica's listener is away are lost; the rows
/// are not. A subscriber on B receives what A published during the outage
/// once B's listener is back, in order and once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_receives_what_was_published_while_its_listener_was_away() {
    let Replicas { database, a, b, .. } = replicas(|policy| {
        // Long enough that A's publications land while B is not listening.
        policy.reconnect.initial_delay = Duration::from_millis(500);
        policy.reconnect.max_delay = Duration::from_millis(500);
        policy.reconnect.jitter = false;
    })
    .await;
    let process = ProcessId::fixture("listener-away");
    let before = publish(&a, &process, "k0", "before").await;
    let mut on_b = subscribed(&b, &before.cursor).await;

    let pool = sqlx::PgPool::connect(database.url())
        .await
        .expect("connect to end the listeners");
    let ended: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = current_database() \
           AND application_name LIKE '%process-replay-listener'",
    )
    .fetch_all(&pool)
    .await
    .expect("end the listeners' sessions");
    pool.close().await;
    assert_eq!(
        ended,
        [true, true],
        "both replicas' listeners lost their sessions"
    );
    let during = [
        publish(&a, &process, "k1", "during one").await,
        publish(&a, &process, "k2", "during two").await,
    ];
    let received = [next(&mut on_b).await, next(&mut on_b).await];
    assert_eq!(cursors(&received), cursors(&during));

    // The reconnected listener still delivers: a later publication arrives
    // by its own doorbell.
    let after = publish(&a, &process, "k3", "after").await;
    assert_eq!(next(&mut on_b).await.cursor, after.cursor);
}

/// Crash recovery or failover truncates the unlogged tables, the sentinel
/// among them; the next writer rotates the incarnation, every older cursor
/// gaps on both replicas, and a live subscription on the other replica
/// ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_sentinel_rotates_the_incarnation_for_every_replica() {
    let Replicas {
        database,
        schema,
        a,
        b,
    } = replicas(|_| {}).await;
    let process = ProcessId::fixture("truncated");
    let before = publish(&a, &process, "k0", "before").await;
    let mut on_b = subscribed(&b, &before.cursor).await;

    execute(
        database.url(),
        &format!(
            "TRUNCATE \"{schema}\".process_replay_sentinel, \"{schema}\".process_replay_head, \
             \"{schema}\".process_replay_log, \"{schema}\".process_replay_dedupe"
        ),
    )
    .await;

    let after = publish(&a, &process, "k1", "after").await;
    assert_ne!(
        after.replay_incarnation_id(),
        before.replay_incarnation_id(),
        "a writer after the truncation publishes under a new incarnation"
    );
    for (name, store) in [("A", &a), ("B", &b)] {
        assert_eq!(
            replay(store, &before.cursor).await.err(),
            Some(ProcessReplayGapReason::Unavailable),
            "{name}: a cursor from the lost history gaps"
        );
    }
    let ended = next_item(&mut on_b).await;
    assert!(
        matches!(ended, None | Some(Err(ProcessReplayStoreError::Closed))),
        "B's subscription into the lost history closes, got {ended:?}"
    );
    assert_eq!(
        current(&b, &process)
            .await
            .parse()
            .expect("parse")
            .replay_incarnation_id,
        after.replay_incarnation_id(),
        "B's cursors name the new incarnation"
    );
    let rotations: i64 = sqlx::query_scalar(&format!(
        "SELECT rotations FROM \"{schema}\".process_replay_incarnation"
    ))
    .fetch_one(
        &sqlx::PgPool::connect(database.url())
            .await
            .expect("connect to read the incarnation"),
    )
    .await
    .expect("read the incarnation");
    assert_eq!(
        rotations, 1,
        "the lost sentinel rotated the incarnation once"
    );
}

/// What the sentinel accounts and what the heads hold.
async fn budget(url: &str, schema: &str) -> ((i64, i64), (i64, i64, i64)) {
    let pool = sqlx::PgPool::connect(url)
        .await
        .expect("connect to read the budget");
    let sentinel = sqlx::query_as(&format!(
        "SELECT resident_processes, reserved_bytes FROM \"{schema}\".process_replay_sentinel"
    ))
    .fetch_one(&pool)
    .await
    .expect("read the sentinel");
    let heads = sqlx::query_as(&format!(
        "SELECT count(*), COALESCE(sum(reserved_bytes), 0)::bigint, \
                COALESCE(max(retained_bytes - reserved_bytes), 0)::bigint \
         FROM \"{schema}\".process_replay_head"
    ))
    .fetch_one(&pool)
    .await
    .expect("read the heads");
    pool.close().await;
    (sentinel, heads)
}

/// The aggregate bounds hold across replicas: the store keeps at most
/// `max_processes` windows and reserves at most `max_retained_bytes` for
/// them, evicting the idlest window to admit another. An evicted process's
/// cursors gap, its positions never repeat, and the sentinel's budget is
/// exactly what the heads hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_idlest_window_is_evicted_to_keep_the_aggregate_bounds() {
    const STEP: usize = 4096;
    // By count: two windows.
    let Replicas {
        database,
        schema,
        a,
        b,
    } = replicas(|policy| policy.data.max_processes = 2).await;
    let [one, two, three] = ["one", "two", "three"].map(ProcessId::fixture);
    let first = publish(&a, &one, "k", "one").await;
    publish(&a, &two, "k", "two").await;
    let mut on_b = subscribed(&b, &first.cursor).await;
    let third = publish(&b, &three, "k", "three").await;
    assert_eq!(
        replay(&a, &first.cursor).await.err(),
        Some(ProcessReplayGapReason::Unavailable),
        "the idlest process was evicted for the third"
    );
    let ended = next_item(&mut on_b).await;
    assert!(
        matches!(ended, None | Some(Err(ProcessReplayStoreError::Closed))),
        "a subscription to an evicted process closes, got {ended:?}"
    );
    let start = b
        .current_cursor(&three, ProcessSequence::new(0))
        .await
        .expect("a start cursor");
    assert_eq!(
        cursors(&replay(&a, &start).await.expect("replay")),
        std::slice::from_ref(&third.cursor)
    );
    let again = publish(&a, &one, "k", "one again").await;
    assert!(
        again.live_position() > first.live_position(),
        "an evicted process returns above every position it had"
    );
    let ((resident, reserved), (heads, held, over)) = budget(database.url(), &schema).await;
    assert_eq!(
        (resident, reserved),
        (heads, held),
        "the sentinel's budget is the heads'"
    );
    assert_eq!(resident, 2);
    assert!(over <= 0, "no window holds more than it reserved");

    // By bytes: one reservation step in all, so two processes cannot both
    // hold a window.
    let Replicas {
        database,
        schema,
        a,
        ..
    } = replicas(|policy| {
        policy.data.max_bytes_per_process = STEP;
        policy.data.reservation_bytes = STEP;
        policy.data.max_retained_bytes = STEP as u64;
    })
    .await;
    let first = publish(&a, &one, "k", "one").await;
    publish(&a, &two, "k", "two").await;
    assert_eq!(
        replay(&a, &first.cursor).await.err(),
        Some(ProcessReplayGapReason::Unavailable),
        "the idlest process's reservation was taken for the second"
    );
    let ((resident, reserved), (heads, held, over)) = budget(database.url(), &schema).await;
    assert_eq!(
        (resident, reserved),
        (heads, held),
        "the sentinel's budget is the heads'"
    );
    assert_eq!((resident, reserved), (1, STEP as i64));
    assert!(over <= 0, "no window holds more than it reserved");
}
