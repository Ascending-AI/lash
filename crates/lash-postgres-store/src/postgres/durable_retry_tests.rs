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

/// Admit session `session` to the catalog, with no durable actor yet.
async fn admit(storage: &PostgresStorage, session: &lash_sansio::SessionId) {
    use lash_core_execution::{
        MaxToolCalls, SessionCatalogStore as _, SessionCreationHead, SessionPolicy,
        SessionRelation, SessionStoreCreateRequest, TurnBudget,
    };

    storage
        .session_store_factory()
        .admit_session(&SessionStoreCreateRequest {
            session_id: session.clone(),
            relation: SessionRelation::Root,
            config: SessionPolicy::new(TurnBudget::Unbounded, MaxToolCalls::new(16)).into(),
            head: SessionCreationHead::Config,
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
        })
        .await
        .expect("the catalog admits the session");
}

/// The advisory key of session `session`'s artifact referrer lock.
fn referrer_key(session: &lash_sansio::SessionId) -> String {
    let referrer = lash_core_execution::ArtifactReferrer::Session(session.clone());
    format!(
        "lash-artifact-referrer:{}:{}",
        referrer.kind().as_str(),
        referrer.canonical_id(),
    )
}

/// Wait until a backend of this database other than `holders` waits on a
/// lock.
async fn lock_wait_beside(storage: &PostgresStorage, holders: &[i32]) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                 WHERE datname = current_database() AND pid <> ALL($1)
                   AND wait_event_type = 'Lock')",
            )
            .bind(holders)
            .fetch_one(storage.pool())
            .await
            .expect("read the lock waits");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a producer waits on a held lock");
}

async fn backend_pid(tx: &mut sqlx::PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(tx)
        .await
        .expect("the backend pid")
}

/// A producer admission into session `name`, run on `storage` while an owner
/// transaction holds the session's actor row.
type Admission = Box<
    dyn FnOnce(
            PostgresStorage,
            lash_sansio::SessionId,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send,
>;

/// The session lock order (FIG-5423, FIG-5429): an admission that waits for
/// the session's owner holds no lock the owner's head commit takes after its
/// actor row. The owner holds the actor row; once the admission waits on a
/// lock, the owner takes the session's history lock and its referrer lock
/// without waiting. An admission that took history first and the actor row
/// last (in its wake) deadlocked with a producer that took them the other
/// way round, and one of them was refused `Contended`.
async fn a_waiting_admission_holds_no_history_or_referrer_lock(
    name: &'static str,
    admission: Admission,
) {
    let (_database, storage) = storage(name, &PostgresHostConfig::default())
        .await
        .expect("hermetic PostgreSQL is available");
    let session = lash_sansio::SessionId::from(name);
    admit(&storage, &session).await;
    let store = storage.durable_store();
    create(&store, &actor(session.as_str())).await;
    let mut owner = crate::begin_guarded(storage.pool(), &storage.fence)
        .await
        .expect("the owner's transaction opens");
    crate::PostgresDurableStore::lock_session_actor(&mut owner, &session)
        .await
        .expect("the owner holds its actor row");
    let owner_pid = backend_pid(&mut owner).await;

    let admitted = tokio::spawn(admission(storage.clone(), session.clone()));
    lock_wait_beside(&storage, &[owner_pid]).await;

    let history_free: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 1::bigint))")
            .bind(session.as_str())
            .fetch_one(&mut **owner)
            .await
            .expect("the owner probes its history lock");
    let referrer_free: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(referrer_key(&session))
            .fetch_one(&mut **owner)
            .await
            .expect("the owner probes its referrer lock");
    // Release the owner and join even on the red side: no task or lock is
    // left behind by the assertion.
    owner
        .rollback()
        .await
        .expect("the owner releases its locks");
    admitted.await.expect("the admission is admitted");
    assert!(
        history_free,
        "the waiting admission holds the owner's history lock"
    );
    assert!(
        referrer_free,
        "the waiting admission holds the owner's referrer lock"
    );
}

/// FIG-5423: a queued-work admission takes the actor row first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_admission_waiting_for_the_owner_holds_no_history_or_referrer_lock() {
    use lash_core_execution::DeliveryPolicy;
    use lash_core_execution::runtime::QueuedWorkBatchDraft;

    a_waiting_admission_holds_no_history_or_referrer_lock(
        "admission-lock-order",
        Box::new(|storage, session| {
            Box::pin(async move {
                let mut tx = crate::begin_guarded(storage.pool(), &storage.fence)
                    .await
                    .expect("admission opens");
                let batch = QueuedWorkBatchDraft::new(
                    session,
                    DeliveryPolicy::EarliestSafeBoundary,
                    lash_core_execution::facade_support::SessionCommand::RefreshToolCatalog {
                        reason: "concurrent owner commit".into(),
                    },
                );
                crate::runtime_persistence::enqueue_queued_work_with_outcome_tx(&mut tx, &batch, 1)
                    .await
                    .expect("the admission commits without contention");
                tx.commit().await.expect("the admission commits");
            })
        }),
    )
    .await;
}

/// FIG-5429: a turn-input batch takes the actor row before the history lock
/// its ingress allocation needs, so it never holds history while its wake
/// waits for the actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_input_admission_waiting_for_the_owner_holds_no_history_or_referrer_lock() {
    use lash_core_execution::{PendingTurnInputBatch, TurnInputStore as _};

    a_waiting_admission_holds_no_history_or_referrer_lock(
        "turn-input-lock-order",
        Box::new(|storage, session| {
            Box::pin(async move {
                let draft = lash_core_execution::PendingTurnInputDraft::new(
                    &session,
                    lash_core_execution::TurnInputIngress::NextTurn,
                    lash_core_execution::TurnInput::text("racing"),
                )
                .with_source_key("racing-input");
                storage
                    .store()
                    .enqueue_pending_turn_inputs(PendingTurnInputBatch::one(draft))
                    .await
                    .expect("the turn input is admitted without contention");
            })
        }),
    )
    .await;
}

/// FIG-5429: a session close takes the actor row before the history lock,
/// so it never holds history while its wake waits for the actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_close_waiting_for_the_owner_holds_no_history_or_referrer_lock() {
    use lash_core_execution::store::ControlIntentStore as _;

    a_waiting_admission_holds_no_history_or_referrer_lock(
        "session-close-lock-order",
        Box::new(|storage, session| {
            Box::pin(async move {
                storage
                    .store()
                    .begin_session_close(&session, 1)
                    .await
                    .expect("the close begins without contention")
                    .expect("the session exists");
            })
        }),
    )
    .await;
}

/// FIG-5429: a producer that waited for history while the session's actor
/// was created takes the new actor row before history after all. Holding
/// history while it waits for the row, it deadlocks with a writer that
/// locked the row once it appeared and then waits for history: the racing
/// producers of the turn-input batch law met exactly that and were refused
/// `Contended`. Here the owner holds the new row as the producer leaves
/// its history wait; once the producer waits for the row, the owner takes
/// history without waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_producer_queued_behind_the_actors_creation_takes_the_actor_row_before_history() {
    use lash_core_execution::{PendingTurnInputBatch, TurnInputStore as _};

    let name = "actor-creation-lock-order";
    let (_database, storage) = storage(name, &PostgresHostConfig::default())
        .await
        .expect("hermetic PostgreSQL is available");
    let session = lash_sansio::SessionId::from(name);
    admit(&storage, &session).await;
    let mut history = crate::begin_guarded(storage.pool(), &storage.fence)
        .await
        .expect("the history holder opens");
    crate::runtime_persistence::lock_session_history_mutation_tx(&mut history, &session)
        .await
        .expect("the holder takes history while the session has no actor");
    let history_pid = backend_pid(&mut history).await;

    let produced = tokio::spawn({
        let store = storage.store();
        let session = session.clone();
        async move {
            let draft = lash_core_execution::PendingTurnInputDraft::new(
                &session,
                lash_core_execution::TurnInputIngress::NextTurn,
                lash_core_execution::TurnInput::text("queued"),
            )
            .with_source_key("queued-input");
            store
                .enqueue_pending_turn_inputs(PendingTurnInputBatch::one(draft))
                .await
                .expect("the producer is admitted without contention");
        }
    });
    lock_wait_beside(&storage, &[history_pid]).await;
    create(&storage.durable_store(), &actor(session.as_str())).await;
    let mut owner = crate::begin_guarded(storage.pool(), &storage.fence)
        .await
        .expect("the owner's transaction opens");
    crate::PostgresDurableStore::lock_session_actor(&mut owner, &session)
        .await
        .expect("the owner holds the new actor row");
    let owner_pid = backend_pid(&mut owner).await;
    history.commit().await.expect("the holder releases history");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks
                 WHERE NOT granted AND locktype IN ('transactionid', 'tuple')
                   AND pid <> $1)",
            )
            .bind(owner_pid)
            .fetch_one(storage.pool())
            .await
            .expect("read the actor row's waiters");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the producer waits for the actor row");
    let history_free: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 1::bigint))")
            .bind(session.as_str())
            .fetch_one(&mut **owner)
            .await
            .expect("the owner probes its history lock");
    // Release the producer and join even on the red side.
    owner
        .rollback()
        .await
        .expect("the owner releases its locks");
    produced.await.expect("the producer is admitted");
    assert!(
        history_free,
        "the producer holds history while it waits for the actor row"
    );
}

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
