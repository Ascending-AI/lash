//! The durability engine's PostgreSQL-only concurrency laws (L8, FIG-5178):
//! real connections racing on one database, where SQLite's single writer
//! would serialize the race away.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_ids::test_clock::TestClock;
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, Epoch, FormatSet, MailKind,
    MailTx, NodeId, NodeLease, NodeSpec, Release, StoreFailure, StoreFailureKind,
};

use super::PostgresDurableStore;
use crate::PostgresStorage;
use crate::testing::IsolatedDatabase;

const TTL: Duration = Duration::from_secs(15);
const LABEL: CommitLabel = CommitLabel::new("law.write");

fn formats() -> FormatSet {
    FormatSet::new("law-formats")
}

fn actor(name: &str) -> ActorKey {
    ActorKey::session(name).expect("a law's actor key")
}

/// A fresh isolated database and its storage, or `None` when no server is
/// configured.
async fn storage(law: &str) -> Option<(IsolatedDatabase, PostgresStorage)> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping {law}: database URL is not set");
        return None;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    Some((database, storage))
}

async fn node(store: &PostgresDurableStore, name: &str) -> NodeLease {
    store
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![formats()],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: i64::try_from(TTL.as_millis()).expect("ttl fits"),
        })
        .await
        .expect("register a node")
}

async fn create(store: &PostgresDurableStore, actors: &[ActorKey]) {
    let mut tx = MailTx::new();
    for actor in actors {
        tx.create_actor(actor.clone(), formats());
    }
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await
        .expect("create the actors");
}

async fn append(store: &PostgresDurableStore, targets: &[&ActorKey]) -> Result<(), DurableError> {
    let mut tx = MailTx::new();
    for target in targets {
        tx.append((*target).clone(), MailKind::new("law.note"), "note");
    }
    store.commit_mail(tx, CommitLabel::MAIL_SESSION).await?;
    Ok(())
}

/// Two producers append to the same two actors in opposite orders, again and
/// again. A mailbox commit takes its actors' row locks in key order, so the
/// two never deadlock: both always commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_in_opposite_orders_never_deadlocks() {
    let Some((_database, storage)) = storage("mail_in_opposite_orders_never_deadlocks").await
    else {
        return;
    };
    let store = storage.durable_store();
    let (first, second) = (actor("lock-a"), actor("lock-b"));
    create(&store, &[first.clone(), second.clone()]).await;
    let (forward_order, backward_order) = ([&first, &second], [&second, &first]);
    for round in 0..64 {
        let (forward, backward) = tokio::join!(
            append(&store, &forward_order),
            append(&store, &backward_order),
        );
        assert!(
            forward.is_ok() && backward.is_ok(),
            "round {round}: opposite-order mail answered {forward:?} and {backward:?}"
        );
    }
}

/// An owner releasing its actor to `waiting` races a producer's mail. The
/// mail is never lost: whichever commits second sees the other, so an actor
/// with unacknowledged mail is always left ready, never waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_to_waiting_racing_mail_never_loses_the_wakeup() {
    let Some((_database, storage)) =
        storage("a_release_to_waiting_racing_mail_never_loses_the_wakeup").await
    else {
        return;
    };
    let store = storage.durable_store();
    let actors: Vec<ActorKey> = (0..64)
        .map(|index| actor(&format!("wake-{index}")))
        .collect();
    create(&store, &actors).await;
    let owner = node(&store, "owner").await;
    let claimed = store.claim(&owner, actors.len()).await.expect("claim all");
    assert_eq!(claimed.len(), actors.len());
    for claimed in claimed {
        let mut tx = store
            .begin(&claimed.actor, claimed.epoch)
            .await
            .expect("open the owner's transaction");
        tx.give_up(Release::Waiting { next_due: None });
        let target = [&claimed.actor];
        let (released, mailed) = tokio::join!(store.commit(tx, LABEL), append(&store, &target));
        released.expect("the owner's release commits");
        mailed.expect("the producer's mail commits");
        let after = store
            .actor(&claimed.actor)
            .await
            .expect("read the actor")
            .expect("the actor exists");
        assert!(
            after.has_mail && after.state == ActorState::Ready,
            "{} lost its wakeup: {after:?}",
            claimed.actor
        );
    }
}

/// A reap of an expired node races its owner's commits. For every actor
/// exactly one wins: an owner commit that lands first is visible and the reap
/// releases the actor after it; a reap that lands first fences the owner,
/// whose commit is refused and leaves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_reap_racing_owner_commits_has_one_winner_per_actor() {
    let Some((_database, storage)) =
        storage("a_reap_racing_owner_commits_has_one_winner_per_actor").await
    else {
        return;
    };
    let clock = Arc::new(TestClock::new(1_000_000));
    let store = storage
        .durable_store()
        .with_clock_for_testing(clock.clone());
    let actors: Vec<ActorKey> = (0..48)
        .map(|index| actor(&format!("reap-{index}")))
        .collect();
    create(&store, &actors).await;
    let zombie = node(&store, "zombie").await;
    let claimed = store.claim(&zombie, actors.len()).await.expect("claim all");
    assert_eq!(claimed.len(), actors.len());
    let mut opened = Vec::new();
    for claimed in &claimed {
        append(&store, &[&claimed.actor]).await.expect("mail");
        let mut tx = store
            .begin(&claimed.actor, claimed.epoch)
            .await
            .expect("open the owner's transaction");
        tx.ack_seen();
        opened.push((claimed.clone(), tx));
    }
    clock.advance(u64::try_from(TTL.as_millis()).expect("ttl fits") + 1);
    let reaper = node(&store, "reaper").await;

    let mut racing = tokio::task::JoinSet::new();
    for (claimed, tx) in opened {
        let store = store.clone();
        racing.spawn(async move { (claimed, store.commit(tx, LABEL).await) });
    }
    let reaped = store.reap(&reaper).await.expect("the reap commits");
    let commits = racing.join_all().await;
    assert_eq!(reaped.len(), actors.len(), "the reap released {reaped:?}");
    let mut won = 0;
    for (claimed, commit) in commits {
        let after = store
            .actor(&claimed.actor)
            .await
            .expect("read the actor")
            .expect("the actor exists");
        assert!(
            after.state == ActorState::Ready && after.epoch == Epoch(claimed.epoch.0 + 1),
            "the reap did not release {} with one epoch bump: {after:?}",
            claimed.actor
        );
        match commit {
            Ok(_) => {
                won += 1;
                assert!(
                    !after.has_mail && after.pending_mail == 0,
                    "an owner commit that won is not visible: {after:?}"
                );
            }
            Err(DurableError::OwnershipLost(fenced)) => assert!(
                fenced.held == claimed.epoch && after.has_mail && after.pending_mail == 1,
                "a fenced owner commit left a trace or misnamed its epochs: {fenced}, {after:?}"
            ),
            Err(other) => panic!("an owner commit answered {other}"),
        }
    }
    eprintln!("reap race: {won} of {} owner commits won", actors.len());
}

/// Whether the reap waits on a lock at its delete of expired boots before it
/// ends.
async fn until_reap_waits<T>(pool: &sqlx::PgPool, reap: &tokio::task::JoinHandle<T>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'
               AND query = $1)",
        )
        .bind(super::SQL.node.delete_expired.sql())
        .fetch_one(pool)
        .await
        .expect("look for the reap's lock wait");
        if waiting {
            return true;
        }
        if reap.is_finished() {
            return false;
        }
        assert!(
            Instant::now() < deadline,
            "the reap neither waited on a lock nor finished"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A claim that found its boot's lease races the reap that deletes that
/// boot (FIG-6131). The claim locks the node row `FOR SHARE` at `node_live`,
/// so the reap's delete waits for the claim and then releases what it took:
/// no actor stays owned by a deleted boot, a successor claims every one and
/// the deleted boot's epoch is fenced. Were the row not locked, the reap
/// would delete the boot first and the claim would then hand its actors to
/// a boot nothing reaps again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_racing_the_reap_of_its_boot_leaves_no_actor_owned_by_it() {
    use crate::testing::{StorageGates, StoragePoint};

    let Some((_database, storage)) =
        storage("a_claim_racing_the_reap_of_its_boot_leaves_no_actor_owned_by_it").await
    else {
        return;
    };
    let clock = Arc::new(TestClock::new(1_000_000));
    let gates = StorageGates::new();
    let store = storage
        .durable_store()
        .with_clock_for_testing(clock.clone())
        .with_storage_gates_for_testing(gates.clone());
    let actors: Vec<ActorKey> = (0..4)
        .map(|index| actor(&format!("orphan-{index}")))
        .collect();
    create(&store, &actors).await;
    let doomed = node(&store, "doomed").await;
    clock.advance(u64::try_from(TTL.as_millis()).expect("ttl fits") + 1);
    let reaper = node(&store, "reaper").await;

    let held = gates.pause_next(StoragePoint::ClaimAfterNodeLive);
    let claim = tokio::spawn({
        let (store, doomed, limit) = (store.clone(), doomed.clone(), actors.len());
        async move { store.claim(&doomed, limit).await }
    });
    held.reached().await;
    let reap = tokio::spawn({
        let store = store.clone();
        async move { store.reap(&reaper).await }
    });
    let waited = until_reap_waits(storage.pool(), &reap).await;
    held.release();
    let claimed = claim
        .await
        .expect("the claim task")
        .expect("the claim commits");
    let reaped = reap
        .await
        .expect("the reap task")
        .expect("the reap commits");

    let left = store
        .owned(&doomed)
        .await
        .expect("read the deleted boot's actors");
    assert!(
        left.is_empty(),
        "actors stayed owned by the reaped boot: {left:?}; the reap released {reaped:?}"
    );
    assert!(
        waited,
        "the reap did not wait for the claim's node-row lock"
    );
    assert_eq!(claimed.len(), actors.len(), "the claim took {claimed:?}");
    let mut released: Vec<_> = reaped
        .iter()
        .map(|reaped| (reaped.actor.clone(), reaped.epoch))
        .collect();
    released.sort();
    assert_eq!(
        released,
        claimed
            .iter()
            .map(|claimed| (claimed.actor.clone(), Epoch(claimed.epoch.0 + 1)))
            .collect::<Vec<_>>(),
        "the reap did not release every claimed actor with one epoch bump"
    );
    let successor = node(&store, "successor").await;
    let taken = store
        .claim(&successor, actors.len())
        .await
        .expect("the successor's claim commits");
    assert_eq!(
        taken.iter().map(|taken| &taken.actor).collect::<Vec<_>>(),
        actors.iter().collect::<Vec<_>>(),
        "the successor could not claim every released actor"
    );
    for claimed in &claimed {
        assert!(
            matches!(
                store.begin(&claimed.actor, claimed.epoch).await,
                Err(DurableError::OwnershipLost(_))
            ),
            "the reaped boot still opens {} at its claimed epoch",
            claimed.actor
        );
    }
}

/// A claim skips an eligible actor another transaction holds locked and
/// takes the next oldest (FIG-6131): with limit one it answers the second
/// actor while the oldest stays locked, neither nothing nor a wait, and the
/// oldest is claimable again once its holder ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_takes_the_next_actor_past_a_locked_oldest() {
    let Some((_database, storage)) =
        storage("a_claim_takes_the_next_actor_past_a_locked_oldest").await
    else {
        return;
    };
    let clock = Arc::new(TestClock::new(1_000_000));
    let store = storage
        .durable_store()
        .with_clock_for_testing(clock.clone());
    // Key order opposes age, so only age makes the locked actor the oldest.
    let (oldest, next) = (actor("z-oldest"), actor("a-next"));
    create(&store, std::slice::from_ref(&oldest)).await;
    clock.advance(1);
    create(&store, std::slice::from_ref(&next)).await;
    let claimer = node(&store, "claimer").await;
    let claim_one = || async {
        tokio::time::timeout(Duration::from_secs(10), store.claim(&claimer, 1))
            .await
            .map_err(|_| "the claim waited".to_owned())?
            .map(|claimed| {
                claimed
                    .into_iter()
                    .map(|claimed| claimed.actor)
                    .collect::<Vec<_>>()
            })
            .map_err(|error| error.to_string())
    };

    let mut holder = storage.pool().begin().await.expect("open the holder");
    sqlx::query(super::HOLD_ACTOR_SQL)
        .bind(oldest.as_str())
        .fetch_one(&mut *holder)
        .await
        .expect("lock the oldest actor");
    let past = claim_one().await;
    holder.rollback().await.expect("end the holder");
    assert_eq!(
        past,
        Ok(vec![next]),
        "a claim did not take the next actor past the locked oldest"
    );
    assert_eq!(
        claim_one().await,
        Ok(vec![oldest]),
        "the oldest was not claimable once its holder ended"
    );
}

/// The claim latencies and outcomes of a contention run.
#[derive(Default)]
struct Contention {
    latencies: Vec<Duration>,
    claims: usize,
    empty: usize,
    taken: Vec<(ActorKey, Epoch)>,
}

fn claimer_config() -> crate::PostgresHostConfig {
    let mut config = crate::testing::fixture_config();
    // This law asserts disjoint claims, not the production 500 ms checkout
    // SLA. A loaded executor can delay SQLx's spawned connection-return task
    // past that deadline with no transaction or lock holding the connection.
    // Keep one scheduler connection and the SQL lock/statement guards, but
    // allow scheduling and checkout to complete within the test's bound.
    config.roles.scheduler.acquire_timeout = Duration::from_secs(30);
    config.guards.scheduler.operation_deadline = Some(Duration::from_secs(60));
    config
}

/// A delayed executor must not turn the disjoint-claim oracle into a test
/// of the production checkout deadline. SQLx returns a dropped connection
/// through a spawned task, even when its backend is idle with no locks.
#[tokio::test]
async fn a_hot_set_claimer_survives_executor_delay_while_its_connection_returns() {
    use std::future::Future;
    use std::task::Poll;

    let Some((database, storage)) =
        storage("a_hot_set_claimer_survives_executor_delay_while_its_connection_returns").await
    else {
        return;
    };
    let store = crate::testing::connect_with(database.url(), &claimer_config())
        .await
        .expect("open a claimer's store")
        .durable_store();
    let target = actor("delayed-claimer");
    create(&store, std::slice::from_ref(&target)).await;
    let lease = node(&store, "delayed-claimer").await;
    assert_eq!(store.pools.scheduler.options().get_max_connections(), 1);
    let mut connection = store
        .pools
        .scheduler
        .acquire()
        .await
        .expect("hold the sole connection");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .expect("identify the held backend");
    let idle: bool = sqlx::query_scalar(
        "SELECT state = 'idle' AND xact_start IS NULL AND wait_event_type IS DISTINCT FROM 'Lock'
         FROM pg_stat_activity WHERE pid = $1",
    )
    .bind(pid)
    .fetch_one(storage.pool())
    .await
    .expect("inspect the backend from a separate connection");
    assert!(idle, "the held connection has no transaction or lock wait");

    let mut claim = Box::pin(store.claim(&lease, 1));
    std::future::poll_fn(|cx| {
        assert!(
            claim.as_mut().poll(cx).is_pending(),
            "claim awaits the pool permit"
        );
        Poll::Ready(())
    })
    .await;
    drop(connection);
    // This current-thread executor cannot run SQLx's return task during the
    // pause. No competing SQL transaction or lock can cause this wait.
    std::thread::sleep(Duration::from_millis(750));
    let claimed = claim
        .await
        .expect("a delayed claimer still takes its actor");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].actor, target);
}

/// Contention wrote nothing. Retry the same request or immutable commit;
/// every other refusal still fails the law, including an ownership fence.
async fn retry_contended<T, F>(
    mut attempt: impl FnMut() -> F,
    retries: &AtomicUsize,
) -> Result<T, DurableError>
where
    F: std::future::Future<Output = Result<T, DurableError>>,
{
    loop {
        match attempt().await {
            Err(DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Contended,
                ..
            })) => {
                retries.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
            answer => return answer,
        }
    }
}

/// Sixteen nodes claim from a hot set of sixteen actors, each releasing what
/// it took and waking it again. Claims stay disjoint: no actor is taken twice
/// at one epoch, and no owner commit is ever fenced, which it would be if two
/// claims overlapped. The run reports claim latency and lock waits against
/// S2's (FIG-5167) numbers: claim p99 1.120 ms at sixteen nodes, and no
/// lock-wait observation on a hot set without held locks.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn sixteen_claimers_on_a_hot_set_take_disjoint_actors() {
    let Some((database, storage)) =
        storage("sixteen_claimers_on_a_hot_set_take_disjoint_actors").await
    else {
        return;
    };
    let store = storage.durable_store();
    let actors: Vec<ActorKey> = (0..16)
        .map(|index| actor(&format!("hot-{index}")))
        .collect();
    create(&store, &actors).await;
    let mut leases = Vec::new();
    for index in 0..16 {
        leases.push(node(&store, &format!("claimer-{index}")).await);
    }
    // Sequences survive rollback: one claim is deterministically contended,
    // then every retry can proceed through the real PostgreSQL claim path.
    sqlx::raw_sql(super::CONTEND_FIRST_CLAIM_SQL)
        .execute(storage.pool())
        .await
        .expect("make the first claim contended");
    let mut workers = Vec::new();
    for lease in leases {
        // Each claimer is a node with a pool of its own, as in a deployment.
        // Finish fallible setup before starting the lock-wait sampler.
        let store = crate::testing::connect_with(database.url(), &claimer_config())
            .await
            .expect("open a claimer's store")
            .durable_store();
        workers.push((lease, store));
    }
    let contention = Arc::new(Mutex::new(Contention::default()));
    let retries = Arc::new(AtomicUsize::new(0));
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let observer = {
        let pool = storage.pool().clone();
        let running = Arc::clone(&running);
        tokio::spawn(async move {
            let (mut ticks, mut waits) = (0_u64, 0_i64);
            while running.load(std::sync::atomic::Ordering::Acquire) {
                let waiting: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock'",
                )
                .fetch_one(&pool)
                .await
                .expect("sample lock waits");
                ticks += 1;
                waits += waiting;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            (ticks, waits)
        })
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut claimers = tokio::task::JoinSet::new();
    for (lease, store) in workers {
        let contention = Arc::clone(&contention);
        let retries = Arc::clone(&retries);
        claimers.spawn(async move {
            while Instant::now() < deadline {
                let started = Instant::now();
                let claimed = retry_contended(|| store.claim(&lease, 16), &retries)
                    .await
                    .expect("a claim commits");
                let took = started.elapsed();
                {
                    let mut contention = contention.lock().expect("contention record");
                    contention.latencies.push(took);
                    contention.claims += 1;
                    if claimed.is_empty() {
                        contention.empty += 1;
                    }
                    contention
                        .taken
                        .extend(claimed.iter().map(|c| (c.actor.clone(), c.epoch)));
                }
                for claimed in claimed {
                    let mut tx =
                        retry_contended(|| store.begin(&claimed.actor, claimed.epoch), &retries)
                            .await
                            .expect("a claimer owns what it claimed");
                    tx.ack_seen().give_up(Release::Idle);
                    retry_contended(|| store.commit(tx.clone(), LABEL), &retries)
                        .await
                        .expect("no overlapping claim fences an owner");
                    let mut wake = MailTx::new();
                    wake.wake(claimed.actor.clone());
                    retry_contended(
                        || store.commit_mail(wake.clone(), CommitLabel::MAIL_SESSION),
                        &retries,
                    )
                    .await
                    .expect("wake the released actor");
                }
            }
        });
    }
    // join_all propagates a worker panic immediately. Collect failures instead
    // so the sampler always stops and joins before a panic drops the database.
    let mut failures = Vec::new();
    while let Some(result) = claimers.join_next().await {
        if let Err(error) = result {
            failures.push(error);
        }
    }
    running.store(false, std::sync::atomic::Ordering::Release);
    let (ticks, waits) = observer.await.expect("the lock-wait observer");
    assert!(failures.is_empty(), "claimers failed: {failures:?}");
    let retries = retries.load(Ordering::Relaxed);
    assert!(retries > 0, "the law must retry the contended first claim");

    let mut contention = contention.lock().expect("contention record");
    let mut seen = std::collections::BTreeSet::new();
    for taken in &contention.taken {
        assert!(seen.insert(taken.clone()), "{taken:?} was claimed twice");
    }
    assert!(
        contention.taken.len() >= actors.len(),
        "only {} claims took an actor",
        contention.taken.len()
    );
    assert_eq!(
        contention
            .taken
            .iter()
            .map(|(actor, _)| actor.clone())
            .collect::<std::collections::BTreeSet<_>>(),
        actors.into_iter().collect(),
        "every actor in the hot set was claimed"
    );
    contention.latencies.sort();
    let at = |percent: usize| {
        let index = (contention.latencies.len() - 1) * percent / 100;
        contention.latencies[index].as_secs_f64() * 1000.0
    };
    eprintln!(
        "contention: {} claims ({} empty), {} actors taken, claim p50 {:.3} ms p99 {:.3} ms, \
         {retries} contention retries, {waits} lock-wait observations in {ticks} ticks",
        contention.claims,
        contention.empty,
        contention.taken.len(),
        at(50),
        at(99),
    );
}

/// Whether lock waiters in this database reach `waiters` before one of
/// `racing` ends.
async fn until_lock_waiters<T>(
    pool: &sqlx::PgPool,
    waiters: i64,
    racing: &[&tokio::task::JoinHandle<T>],
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .expect("count lock waiters");
        if waiting >= waiters {
            return true;
        }
        if racing.iter().any(|task| task.is_finished()) {
            return false;
        }
        assert!(
            Instant::now() < deadline,
            "lock waiters stayed at {waiting}, short of {waiters}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A record and a release that share a text, interleaved in both orders,
/// round after round (S6, FIG-5272). In one order the record has rooted the
/// text and is still open when the release drops the text's last other
/// edge; in the other the release has reclaimed the text and is still open
/// when the record names it. Each side is held open by an edge waiting
/// behind a held row, so the interleaving is fixed, not timed. Retirement
/// skips a text held by a publisher and keeps its durable cleanup frontier.
/// Both always
/// commit, the recorded call keeps its text, and once both sessions release
/// nothing of theirs is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_record_racing_a_release_of_a_shared_text_keeps_it() {
    use lash_durable::domain::{ModelCallId, PromptCallKey, PromptText, PromptWrite};
    use lash_durable::{DomainWrite, DurableReads as _};
    use lash_kernel_value::identity::SessionId;
    use lash_vocabulary::identity::TurnId;

    let Some((_database, storage)) =
        storage("a_record_racing_a_release_of_a_shared_text_keeps_it").await
    else {
        return;
    };
    let store = storage
        .durable_store()
        .with_clock_for_testing(Arc::new(TestClock::new(1_000_000)));
    let pool = storage.pool().clone();
    let names = ["releasing", "recording", "anchor"];
    let actors: Vec<ActorKey> = names.iter().map(|name| actor(name)).collect();
    create(&store, &actors).await;
    let owner = node(&store, "prompt-owner").await;
    let claimed = store.claim(&owner, actors.len()).await.expect("claim all");
    assert_eq!(claimed.len(), actors.len());
    let epoch_of = |name: &str| {
        claimed
            .iter()
            .find(|claimed| claimed.actor == actor(name))
            .expect("every actor is claimed")
            .epoch
    };
    let call = |session: &str, run: &str, call: u32| PromptCallKey {
        session: SessionId::try_from(session.to_owned()).expect("a law's session id"),
        call: ModelCallId::Turn {
            run: TurnId::try_from(run.to_owned()).expect("a law's turn id"),
            ordinal: call,
        },
    };
    let text = |hash: &str| PromptText {
        hash: lash_core_store::store::BlobRef::for_content(hash.as_bytes()).0,
        text: format!("text of {hash}").into(),
        children: Vec::new(),
    };
    let record = |call: PromptCallKey, hashes: &[&str]| {
        DomainWrite::Prompt(PromptWrite::Record {
            snapshot: Some(format!("snapshot of call {}", call.call)),
            request: format!("request of call {}", call.call),
            call,
            snapshot_texts: hashes.iter().map(|hash| text(hash)).collect(),
            request_texts: Default::default(),
            prefix: None,
        })
    };
    let release = |session: &str, run: &str| {
        DomainWrite::Prompt(PromptWrite::Release {
            session: SessionId::try_from(session.to_owned()).expect("a law's session id"),
            run: Some(TurnId::try_from(run.to_owned()).expect("a law's turn id")),
        })
    };
    let commit = |name: &'static str, writes: Vec<DomainWrite>| {
        let store = store.clone();
        let epoch = epoch_of(name);
        async move {
            let mut tx = store.begin(&actor(name), epoch).await?;
            for write in writes {
                tx.write(write);
            }
            store.commit(tx, LABEL).await
        }
    };
    let hold = |hash: &'static str| {
        let pool = pool.clone();
        async move {
            let mut held = pool.begin().await.expect("open the holding transaction");
            sqlx::query(super::prompts::LAW_SQL.hold_text.sql())
                .bind(crate::keys::key(
                    lash_core_store::store::BlobRef::for_content(hash.as_bytes()).as_str(),
                ))
                .fetch_one(&mut *held)
                .await
                .expect("hold the blocking text");
            held
        }
    };
    // The anchor roots the blocking text for good; holding its row stalls
    // any commit that roots it too, after that commit's earlier writes.
    commit(
        "anchor",
        vec![record(call("anchor", "anchor", 1), &["blocker"])],
    )
    .await
    .expect("root the blocking text");

    for round in 0..20 {
        let (seed, shared) = (format!("seed-{round}"), format!("shared-{round}"));
        let shared = shared.as_str();
        commit(
            "releasing",
            vec![record(call("releasing", &seed, 1), &[shared])],
        )
        .await
        .unwrap_or_else(|error| panic!("round {round}: seed the shared text: {error}"));

        // The record roots the text and stays open; the release then drops
        // the text's only other edge.
        let held = hold("blocker").await;
        let recorded = tokio::spawn(commit(
            "recording",
            vec![
                record(call("recording", &format!("rooted-{round}"), 1), &[shared]),
                record(
                    call("recording", &format!("rooted-{round}"), 2),
                    &["blocker"],
                ),
            ],
        ));
        let stalled = until_lock_waiters(&pool, 1, &[&recorded]).await;
        let released = tokio::spawn(commit("releasing", vec![release("releasing", &seed)]));
        // Retirement defers a publisher's locked candidates instead of
        // waiting. The uncommitted recorder still roots the shared text.
        let released = tokio::time::timeout(Duration::from_secs(30), released)
            .await
            .expect("retirement must complete while the publisher is blocked");
        assert!(
            stalled,
            "round {round}: the publisher did not reach its held edge"
        );
        assert!(
            !recorded.is_finished(),
            "round {round}: the publisher must still hold its shared-text lock",
        );
        held.rollback().await.expect("let the record finish");
        let recorded = recorded.await;
        assert!(
            matches!((&recorded, &released), (Ok(Ok(_)), Ok(Ok(_)))),
            "round {round}: a record open across a release answered {recorded:?} and {released:?}"
        );
        let kept = store
            .prompt_snapshot(&call("recording", &format!("rooted-{round}"), 1))
            .await
            .expect("read the recorded root");
        assert!(
            kept.is_some_and(|root| root.snapshot_texts == [text(shared).hash]),
            "round {round}: the recorded call lost its root"
        );
        assert_eq!(
            store
                .prompt_texts(&[text(shared).hash])
                .await
                .expect("read"),
            [text(shared)],
            "round {round}: the release reclaimed a text a committed record roots"
        );

        // The release reclaims the text and stays open; the record then
        // names it.
        let (seed, shared) = (format!("reseed-{round}"), format!("reshared-{round}"));
        let shared = shared.as_str();
        commit(
            "releasing",
            vec![record(call("releasing", &seed, 1), &[shared])],
        )
        .await
        .unwrap_or_else(|error| panic!("round {round}: seed the shared text: {error}"));
        let held = hold("blocker").await;
        let released = tokio::spawn(commit(
            "releasing",
            vec![
                release("releasing", &seed),
                record(
                    call("releasing", &format!("after-{round}"), 1),
                    &["blocker"],
                ),
            ],
        ));
        let stalled = until_lock_waiters(&pool, 1, &[&released]).await;
        let recorded = tokio::spawn(commit(
            "recording",
            vec![record(
                call("recording", &format!("named-{round}"), 1),
                &[shared],
            )],
        ));
        let waited = stalled && until_lock_waiters(&pool, 2, &[&released, &recorded]).await;
        held.rollback().await.expect("let the release finish");
        let (released, recorded) = (released.await, recorded.await);
        assert!(
            waited,
            "round {round}: the record did not wait for the open release: \
             {recorded:?} and {released:?}"
        );
        assert!(
            matches!((&recorded, &released), (Ok(Ok(_)), Ok(Ok(_)))),
            "round {round}: a record behind an open release answered {recorded:?} and {released:?}"
        );
        assert_eq!(
            store
                .prompt_texts(&[text(shared).hash])
                .await
                .expect("read"),
            [text(shared)],
            "round {round}: a record behind a reclaim lost its text"
        );
    }

    // Releasing both sessions leaves no text of theirs: nothing orphaned.
    for name in ["releasing", "recording"] {
        commit(
            name,
            vec![DomainWrite::Prompt(PromptWrite::Release {
                session: SessionId::try_from(name.to_owned()).expect("a law's session id"),
                run: None,
            })],
        )
        .await
        .unwrap_or_else(|error| panic!("release {name}: {error}"));
    }
    let used: Vec<String> = (0..20)
        .flat_map(|round| {
            [
                text(&format!("shared-{round}")).hash,
                text(&format!("reshared-{round}")).hash,
            ]
        })
        .collect();
    let left = store.prompt_texts(&used).await.expect("read");
    assert!(left.is_empty(), "released texts were left behind: {left:?}");
    assert_eq!(
        store
            .prompt_texts(&[text("blocker").hash])
            .await
            .expect("read"),
        [text("blocker")],
        "a text the anchor still roots was reclaimed"
    );
}
