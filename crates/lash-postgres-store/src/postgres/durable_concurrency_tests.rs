//! The durability engine's PostgreSQL-only concurrency laws (L8, FIG-5178):
//! real connections racing on one database, where SQLite's single writer
//! would serialize the race away.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_execution::testing::TestClock;
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, Epoch, FormatSet, MailKind,
    MailTx, NodeId, NodeLease, NodeSpec, Release,
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

/// The claim latencies and outcomes of a contention run.
#[derive(Default)]
struct Contention {
    latencies: Vec<Duration>,
    claims: usize,
    empty: usize,
    taken: Vec<(ActorKey, Epoch)>,
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
    let contention = Arc::new(Mutex::new(Contention::default()));
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
    for lease in leases {
        // Each claimer is a node with a pool of its own, as in a deployment.
        let store = crate::testing::connect(database.url())
            .await
            .expect("open a claimer's store")
            .durable_store();
        let contention = Arc::clone(&contention);
        claimers.spawn(async move {
            while Instant::now() < deadline {
                let started = Instant::now();
                let claimed = store.claim(&lease, 16).await.expect("a claim commits");
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
                    let mut tx = store
                        .begin(&claimed.actor, claimed.epoch)
                        .await
                        .expect("a claimer owns what it claimed");
                    tx.ack_seen().give_up(Release::Idle);
                    store
                        .commit(tx, LABEL)
                        .await
                        .expect("no overlapping claim fences an owner");
                    let mut wake = MailTx::new();
                    wake.wake(claimed.actor.clone());
                    store
                        .commit_mail(wake, CommitLabel::MAIL_SESSION)
                        .await
                        .expect("wake the released actor");
                }
            }
        });
    }
    claimers.join_all().await;
    running.store(false, std::sync::atomic::Ordering::Release);
    let (ticks, waits) = observer.await.expect("the lock-wait observer");

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
    contention.latencies.sort();
    let at = |percent: usize| {
        let index = (contention.latencies.len() - 1) * percent / 100;
        contention.latencies[index].as_secs_f64() * 1000.0
    };
    eprintln!(
        "contention: {} claims ({} empty), {} actors taken, claim p50 {:.3} ms p99 {:.3} ms, \
         {waits} lock-wait observations in {ticks} ticks",
        contention.claims,
        contention.empty,
        contention.taken.len(),
        at(50),
        at(99),
    );
}
