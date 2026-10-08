//! The laws of a durable commit's one retry owner (FIG-5242): a commit the
//! database aborted for contention runs again, the same write under the
//! same epoch, and commits once; a connection lost at `COMMIT` is
//! reconciled from the transaction's recorded outcome, never re-run blind;
//! and the retries end within the operation's deadline.

// Test code: the server comes from the environment the target's runner
// hands it.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use lash_durable::{
    ActorKey, CommitLabel, DurableError, DurableStore, FormatSet, MailKind, MailTx, NodeId,
    NodeLease, NodeSpec, Release, StateRevision, StoreFailure, StoreFailureKind,
};

use super::PostgresDurableStore;
use crate::PostgresStorage;
use crate::host::{PostgresHostConfig, RetryPolicy, ServerTimeout};
use crate::testing::{CommitFault, IsolatedDatabase, LostCommit};

const LABEL: CommitLabel = CommitLabel::new("law.write");

fn formats() -> FormatSet {
    FormatSet::new("law-formats")
}

fn actor(name: &str) -> ActorKey {
    ActorKey::session(name).expect("a law's actor key")
}

async fn storage(
    law: &str,
    config: &PostgresHostConfig,
) -> Option<(IsolatedDatabase, PostgresStorage)> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping {law}: database URL is not set");
        return None;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect_with(database.url(), config)
        .await
        .expect("open the isolated store");
    Some((database, storage))
}

async fn create(store: &PostgresDurableStore, actor: &ActorKey) {
    let mut tx = MailTx::new();
    tx.create_actor(actor.clone(), formats());
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await
        .expect("create the actor");
}

async fn node(store: &PostgresDurableStore) -> NodeLease {
    store
        .register_node(&NodeSpec {
            node: NodeId::new("retry-law"),
            decodes: vec![formats()],
            ttl_millis: 15_000,
        })
        .await
        .expect("register a node")
}

async fn append(store: &PostgresDurableStore, target: &ActorKey) -> Result<(), DurableError> {
    let mut tx = MailTx::new();
    tx.append(target.clone(), MailKind::new("law.note"), "note");
    store.commit_mail(tx, CommitLabel::MAIL_SESSION).await?;
    Ok(())
}

async fn pending_mail(store: &PostgresDurableStore, target: &ActorKey) -> u64 {
    store
        .actor(target)
        .await
        .expect("read the actor")
        .expect("the actor exists")
        .pending_mail
}

/// A trigger that fails the first `failures` rows `event` names on `table`
/// with `sqlstate`, counting every row it saw in the sequence `counter`.
async fn inject(
    storage: &PostgresStorage,
    counter: &str,
    event: &str,
    table: &str,
    sqlstate: &str,
    failures: i64,
) {
    sqlx::raw_sql(&format!(
        r#"
        CREATE SEQUENCE {counter};
        CREATE FUNCTION {counter}_fault() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF nextval('{counter}') <= {failures} THEN
                RAISE EXCEPTION 'injected contention' USING ERRCODE = '{sqlstate}';
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER {counter}_fault BEFORE {event} ON {table}
            FOR EACH ROW EXECUTE FUNCTION {counter}_fault();
        "#
    ))
    .execute(storage.pool())
    .await
    .expect("install the injected contention");
}

async fn seen(storage: &PostgresStorage, counter: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT last_value FROM {counter}"))
        .fetch_one(storage.pool())
        .await
        .expect("read the attempts")
}

/// An owner commit that meets a deadlock (`40P01`) and a mailbox commit
/// that meets a serialization failure (`40001`) each run again, the same
/// write under the same epoch, and commit once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contended_durable_commit_retries_and_commits_once() {
    let Some((_database, storage)) = storage(
        "a_contended_durable_commit_retries_and_commits_once",
        &crate::testing::fixture_config(),
    )
    .await
    else {
        return;
    };
    let store = storage.durable_store();
    let owned = actor("contended-owner");
    create(&store, &owned).await;
    let lease = node(&store).await;
    let claimed = store.claim(&lease, 1).await.expect("claim the actor");
    assert_eq!(claimed.len(), 1, "the owner claims its actor");
    let mailed = actor("contended-mail");
    create(&store, &mailed).await;
    let before = store
        .actor(&owned)
        .await
        .expect("read the owner's actor")
        .expect("the actor exists")
        .revision;

    inject(
        &storage,
        "owner_attempts",
        "UPDATE OF state_revision",
        "lash_actors",
        "40P01",
        1,
    )
    .await;
    inject(
        &storage,
        "mail_attempts",
        "INSERT",
        "lash_actor_mail",
        "40001",
        1,
    )
    .await;

    let mut tx = store
        .begin(&claimed[0].actor, claimed[0].epoch)
        .await
        .expect("open the owner's transaction");
    tx.give_up(Release::Idle);
    store
        .commit(tx, LABEL)
        .await
        .expect("the deadlocked owner commit runs again and commits");
    let after = store
        .actor(&owned)
        .await
        .expect("read the owner's actor")
        .expect("the actor exists")
        .revision;
    assert_eq!(
        after,
        StateRevision(before.0 + 1),
        "the owner commit landed once"
    );
    assert_eq!(
        seen(&storage, "owner_attempts").await,
        2,
        "one aborted attempt, one commit"
    );

    append(&store, &mailed)
        .await
        .expect("the serialization-failed mailbox commit runs again and commits");
    assert_eq!(
        pending_mail(&store, &mailed).await,
        1,
        "the mail landed once"
    );
    assert_eq!(
        seen(&storage, "mail_attempts").await,
        2,
        "one aborted attempt, one commit"
    );
}

/// A connection lost at `COMMIT` is reconciled from the transaction's
/// recorded outcome: a commit that landed answers once without running
/// again, and one that rolled back runs again and lands. Either way the
/// mail is appended exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_lost_at_commit_is_reconciled_to_one_effect() {
    let Some((_database, storage)) = storage(
        "a_connection_lost_at_commit_is_reconciled_to_one_effect",
        &crate::testing::fixture_config(),
    )
    .await
    else {
        return;
    };
    let plain = storage.durable_store();
    for lost in [LostCommit::BeforeCommit, LostCommit::AfterCommit] {
        let target = actor(&format!("lost-{lost:?}").to_lowercase());
        create(&plain, &target).await;
        let fault = CommitFault::new(lost);
        let store = storage
            .durable_store()
            .with_commit_fault_for_testing(fault.clone());
        let answered = append(&store, &target).await;
        assert!(fault.taken(), "{lost:?}: the commit's connection was lost");
        if let Err(error) = answered {
            panic!("{lost:?}: the lost commit was not reconciled: {error}");
        }
        assert_eq!(
            pending_mail(&plain, &target).await,
            1,
            "{lost:?}: the mail landed exactly once"
        );
    }
}

/// A commit that stays contended stops retrying at its operation's
/// deadline, with the contention, however many attempts its policy allows.
/// Freeze the operation's monotonic timer during off-clock PostgreSQL work.
/// Advancing only after a failed attempt returned its connection drives the
/// actual retry pauses, without racing SQL against machine scheduling.
#[tokio::test]
async fn durable_retries_end_within_the_operation_deadline() {
    let deadline = Duration::from_millis(800);
    let mut config = crate::testing::fixture_config();
    config.guards.durable.lock = ServerTimeout::Limit(Duration::from_millis(200));
    config.guards.durable.statement = ServerTimeout::Limit(Duration::from_millis(500));
    config.guards.durable.idle_in_transaction = ServerTimeout::Limit(Duration::from_millis(500));
    config.guards.durable.operation_deadline = Some(deadline);
    config.retry.durable = RetryPolicy {
        attempts: 1_000,
        initial_delay: Duration::from_millis(50),
        max_delay: Duration::from_millis(50),
        jitter: false,
    };
    let Some(url) = crate::postgres_test_support::database_url() else {
        eprintln!(
            "skipping durable_retries_end_within_the_operation_deadline: database URL is not set"
        );
        return;
    };
    let _database = IsolatedDatabase::create(&url).await;
    let releases = Arc::new(AtomicUsize::new(0));
    let released = Arc::clone(&releases);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .after_release(move |_, _| {
            released.fetch_add(1, Ordering::Release);
            Box::pin(async { Ok(true) })
        })
        .connect(_database.url())
        .await
        .expect("open the observed work pool");
    let storage = crate::testing::from_pool(pool.clone(), &config)
        .await
        .expect("open the isolated store");
    let store = storage.durable_store();
    let target = actor("stays-contended");
    create(&store, &target).await;
    inject(
        &storage,
        "stuck_attempts",
        "INSERT",
        "lash_actor_mail",
        "40P01",
        i64::MAX,
    )
    .await;

    // Setup also releases connections. Wait until its releases are complete
    // before counting the failed commits, and keep this task runnable during
    // SQL so Tokio cannot auto-advance the frozen clock while I/O is pending.
    while pool.num_idle() as u32 != pool.size() {
        tokio::task::yield_now().await;
    }
    let mut observed = releases.load(Ordering::Acquire);
    tokio::time::pause();
    let started = Instant::now();
    let mut operation = Box::pin(append(&store, &target));
    let answered = loop {
        tokio::select! {
            biased;
            answered = &mut operation => break answered,
            () = tokio::task::yield_now() => {
                let ended = releases.load(Ordering::Acquire);
                if ended > observed && pool.num_idle() as u32 == pool.size() {
                    // RetryPolicy never holds an attempt's connection across
                    // its backoff. Once it is idle the pending work is the
                    // retry pause, so the law, not wall time, ends that pause.
                    observed = ended;
                    // Tokio rounds a sleep to the next millisecond tick.
                    // Include that tick so a fractional start cannot strand
                    // the timer after advancing exactly the policy's 50 ms.
                    tokio::time::advance(Duration::from_millis(51)).await;
                }
            }
        }
    };
    let took = started.elapsed();
    tokio::time::resume();
    assert!(
        matches!(
            answered,
            Err(DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Contended,
                ..
            }))
        ),
        "a commit that stays contended answers the contention: {answered:?}"
    );
    assert_eq!(
        took,
        Duration::from_millis(765),
        "no retry pause reaches the 800 ms deadline"
    );
    let attempts = seen(&storage, "stuck_attempts").await;
    assert_eq!(attempts, 16, "the actual commits share one retry deadline");
    assert_eq!(
        pending_mail(&store, &target).await,
        0,
        "no aborted attempt left a trace"
    );
}
