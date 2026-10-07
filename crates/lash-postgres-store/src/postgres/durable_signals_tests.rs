//! Laws of the durability engine's cross-node signals over PostgreSQL (L8,
//! FIG-5178), on real connections and the database clock.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_durable::runner::{Activation, Exit, Owned, Runner, RunnerConfig, Stopped};
use lash_durable::{
    ActorState, CommitLabel, DurableError, DurableSettings, DurableStore, FormatSet,
    HeartbeatOutcome, LeaseSettings, MailKind, MailTx, NodeSpec, Release,
};
use tokio::sync::mpsc;

use super::*;
use crate::PostgresStorage;
use crate::testing::IsolatedDatabase;

const TTL: Duration = Duration::from_secs(15);

fn formats() -> FormatSet {
    FormatSet::new("law-formats")
}

fn actor(name: &str) -> ActorKey {
    ActorKey::session(name).expect("a law's actor key")
}

async fn database(law: &str) -> Option<IsolatedDatabase> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping {law}: database URL is not set");
        return None;
    };
    Some(IsolatedDatabase::create(&database_url).await)
}

async fn storage(database: &IsolatedDatabase) -> PostgresStorage {
    crate::testing::connect(database.url())
        .await
        .expect("open the isolated store")
}

async fn node(store: &dyn DurableStore, name: &str) -> NodeLease {
    store
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![formats()],
            ttl_millis: i64::try_from(TTL.as_millis()).expect("ttl fits"),
        })
        .await
        .expect("register a node")
}

async fn create(store: &dyn DurableStore, actor: &ActorKey) {
    let mut tx = MailTx::new();
    tx.create_actor(actor.clone(), formats());
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await
        .expect("create the actor");
}

fn mail(actor: &ActorKey) -> MailTx {
    let mut tx = MailTx::new();
    tx.append(actor.clone(), MailKind::new("law.note"), "note");
    tx
}

/// Whether `boot`'s liveness lock is held, as a probe sees it now.
async fn held(signals: &PostgresSignals, boot: &Owner) -> Option<bool> {
    signals
        .liveness()
        .await
        .expect("probe liveness")
        .into_iter()
        .find(|liveness| liveness.boot == *boot)
        .map(|liveness| liveness.held)
}

/// Poll `reached` every 25 ms until it holds or `within` passes; answers how
/// long it took.
async fn eventually<F, Fut>(within: Duration, what: &str, mut reached: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = Instant::now();
    while !reached().await {
        assert!(started.elapsed() < within, "{what} not within {within:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    started.elapsed()
}

/// Holds every actor it claims hot, acknowledging its mail and reporting
/// when each arrived.
struct Hold {
    arrived: mpsc::UnboundedSender<Instant>,
}

#[async_trait::async_trait]
impl Activation for Hold {
    async fn activate(&self, owned: Owned) -> Exit {
        loop {
            let Ok(mut tx) = owned.begin().await else {
                return Exit::Released;
            };
            if !tx.mail().is_empty() {
                let arrived = Instant::now();
                tx.ack_seen();
                if owned.commit(tx, CommitLabel::new("law.ack")).await.is_err() {
                    return Exit::Released;
                }
                let _ = self.arrived.send(arrived);
            }
            owned.wait_for_mail().await;
        }
    }
}

/// A runner over `storage` with signals, holding what it claims.
struct Node {
    hints: lash_durable::runner::Hints,
    arrived: mpsc::UnboundedReceiver<Instant>,
    task: tokio::task::JoinHandle<Result<Stopped, DurableError>>,
}

fn start(storage: &PostgresStorage, name: &str, lease: LeaseSettings, signals: bool) -> Node {
    let config = DurableSettings {
        lease,
        ..DurableSettings::default()
    }
    .validate()
    .expect("the law's settings validate");
    let (send, arrived) = mpsc::unbounded_channel();
    let mut runner = Runner::new(
        Arc::new(storage.durable_store()),
        Arc::new(lash_core_execution::runtime::SystemClock),
        RunnerConfig::new(NodeId::new(name), vec![formats()], &config),
        Arc::new(Hold { arrived: send }),
    );
    if signals {
        runner = runner.with_signals(Arc::new(storage.durable_signals()));
    }
    let hints = runner.hints();
    let task = tokio::spawn(runner.run(std::future::pending()));
    Node {
        hints,
        arrived,
        task,
    }
}

async fn owner_of(store: &dyn DurableStore, actor: &ActorKey) -> Option<String> {
    store
        .actor(actor)
        .await
        .expect("read the actor")
        .and_then(|snapshot| snapshot.owner)
        .map(|owner| owner.node.as_str().to_owned())
}

/// A listener holds its boot's liveness lock. Only a reaper that itself
/// listens may reap through the lock, and only once the lock is free: then
/// the boot is reaped at once, long before its lease lapses, with its actors'
/// epochs bumped, so the dead boot's zombie commit is refused and leaves
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_liveness_lock_is_reaped_at_once_and_fences_the_zombie() {
    let Some(database) =
        database("a_released_liveness_lock_is_reaped_at_once_and_fences_the_zombie").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let held_actor = actor("held");
    create(&store, &held_actor).await;
    let dead = node(&store, "dead").await;
    let watcher = node(&store, "watcher").await;
    let claimed = store.claim(&dead, 1).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    let feed = signals.listen(&dead).await.expect("listen");
    assert_eq!(held(&signals, &dead.owner).await, Some(true));
    assert_eq!(held(&signals, &watcher.owner).await, Some(false));
    assert!(
        signals
            .reap_released(&watcher, &dead.owner)
            .await
            .expect("reap")
            .is_empty(),
        "a reaper that holds no lock of its own reaped"
    );
    let _watching = signals.listen(&watcher).await.expect("listen");
    assert!(
        signals
            .reap_released(&watcher, &dead.owner)
            .await
            .expect("reap")
            .is_empty(),
        "a boot whose lock is held was reaped"
    );
    let mut zombie = store
        .begin(&held_actor, claimed[0].epoch)
        .await
        .expect("the owner opens");
    zombie.ack_seen().give_up(Release::Idle);

    drop(feed);
    eventually(Duration::from_secs(5), "the lock is released", || async {
        held(&signals, &dead.owner).await == Some(false)
    })
    .await;
    let reaped = signals
        .reap_released(&watcher, &dead.owner)
        .await
        .expect("reap");
    assert!(
        reaped.len() == 1
            && reaped[0].actor == held_actor
            && reaped[0].from == dead.owner
            && reaped[0].epoch > claimed[0].epoch,
        "the released boot's reap answered {reaped:?}"
    );
    assert_eq!(
        store.heartbeat(&dead).await.expect("heartbeat"),
        HeartbeatOutcome::Reaped
    );
    let before = store.actor(&held_actor).await.expect("read");
    assert!(matches!(
        store.commit(zombie, CommitLabel::new("law.write")).await,
        Err(DurableError::OwnershipLost(_))
    ));
    let after = store.actor(&held_actor).await.expect("read");
    assert_eq!(before, after, "the zombie's refused commit left a trace");
    assert!(after.is_some_and(|after| after.state == ActorState::Ready));
}

/// A listener whose session the server ends opens another, subscribes and
/// takes its lock again, and only then reports `Resubscribed`; hints sent
/// after that reach it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_listener_session_resubscribes_holding_its_lock() {
    let Some(database) = database("a_lost_listener_session_resubscribes_holding_its_lock").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let lease = node(&store, "blip").await;
    let mut feed = signals.listen(&lease).await.expect("listen");
    let ended: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks
         WHERE locktype = 'advisory' AND granted
           AND classid = 1818325864 AND objid = hashtext($1)::oid",
    )
    .bind(lease.owner.boot.as_str())
    .fetch_all(storage.pool())
    .await
    .expect("end the listener's session");
    assert_eq!(ended, vec![true], "one session held the boot's lock");
    let signal = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .expect("the listener reports its new session");
    assert_eq!(signal, Signal::Resubscribed);
    assert_eq!(feed.session(), 1);
    assert_eq!(held(&signals, &lease.owner).await, Some(true));
    signals
        .publish(&WakeBatch {
            ready: true,
            ..WakeBatch::default()
        })
        .await
        .expect("publish");
    let signal = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .expect("a hint reaches the new session");
    assert_eq!(signal, Signal::Ready);
}

/// O1: mail written on node B to an actor hot on node A reaches A through
/// B's after-commit hint, far inside A's ten-second mail poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_from_another_node_reaches_a_hot_owner_through_its_hint() {
    let Some(database) =
        database("mail_from_another_node_reaches_a_hot_owner_through_its_hint").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let hot = actor("hot");
    create(&store, &hot).await;
    let slow_poll = LeaseSettings {
        claim_poll: Duration::from_secs(10),
        ..LeaseSettings::default()
    };
    let mut owner = start(&storage, "a", slow_poll, true);
    eventually(Duration::from_secs(5), "node a owns the actor", || async {
        owner_of(&store, &hot).await.as_deref() == Some("a")
    })
    .await;
    let writer = start(&storage, "b", slow_poll, true);
    eventually(Duration::from_secs(5), "node b listens", || async {
        signals
            .liveness()
            .await
            .expect("probe")
            .iter()
            .any(|liveness| liveness.boot.node.as_str() == "b" && liveness.held)
    })
    .await;

    let sent = Instant::now();
    let commit = store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await
        .expect("mail");
    writer.hints.woke(&commit);
    let arrived = tokio::time::timeout(Duration::from_secs(5), owner.arrived.recv())
        .await
        .expect("the mail arrives")
        .expect("the owner runs");
    let latency = arrived.saturating_duration_since(sent);
    eprintln!("cross-node hint: mail seen after {latency:?}");
    assert!(
        latency < Duration::from_secs(1),
        "the hint took {latency:?}, as long as a poll"
    );
    owner.task.abort();
    writer.task.abort();
}

/// O1: mail whose hint is lost (a writer with no signals, standing in for a
/// dropped NOTIFY) still reaches a hot owner within its mail poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll() {
    let Some(database) =
        database("mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let hot = actor("hot");
    create(&store, &hot).await;
    let poll = Duration::from_millis(500);
    let lease = LeaseSettings {
        claim_poll: poll,
        ..LeaseSettings::default()
    };
    let mut owner = start(&storage, "a", lease, true);
    eventually(Duration::from_secs(5), "node a owns the actor", || async {
        owner_of(&store, &hot).await.as_deref() == Some("a")
    })
    .await;
    let sent = Instant::now();
    store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await
        .expect("mail");
    let arrived = tokio::time::timeout(Duration::from_secs(5), owner.arrived.recv())
        .await
        .expect("the mail arrives")
        .expect("the owner runs");
    let latency = arrived.saturating_duration_since(sent);
    eprintln!("lost hint: mail seen after {latency:?} on a {poll:?} poll");
    assert!(
        latency < poll + Duration::from_millis(500),
        "the poll took {latency:?}"
    );
    owner.task.abort();
}

/// A node that dies is reaped through its liveness lock, and its actor is
/// claimed by a surviving node, in a small fraction of the fifteen-second
/// lease a lease reap would wait for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses() {
    let Some(database) =
        database("a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let hot = actor("hot");
    create(&store, &hot).await;
    let dying = start(&storage, "dying", LeaseSettings::default(), true);
    eventually(
        Duration::from_secs(5),
        "the dying node owns the actor",
        || async { owner_of(&store, &hot).await.as_deref() == Some("dying") },
    )
    .await;
    let survivor = start(&storage, "survivor", LeaseSettings::default(), true);
    eventually(Duration::from_secs(5), "both nodes listen", || async {
        let live = signals.liveness().await.expect("probe");
        live.len() == 2 && live.iter().all(|liveness| liveness.held)
    })
    .await;
    // Let the survivor's watch see the dying node's lock held.
    tokio::time::sleep(Duration::from_millis(600)).await;

    dying.task.abort();
    let failover = eventually(
        Duration::from_secs(10),
        "the survivor takes over",
        || async { owner_of(&store, &hot).await.as_deref() == Some("survivor") },
    )
    .await;
    eprintln!("lock failover: the survivor owns the actor after {failover:?}");
    assert!(
        failover < Duration::from_secs(3),
        "the takeover took {failover:?}, near the lease"
    );
    survivor.task.abort();
}

/// A node's lease renews on a task of its own over its renewal connection:
/// with every work, scheduler and critical connection held, so the runner's
/// claim waits on the scheduler pool, the node keeps serving across three
/// self-stop windows, so it renewed at least three times. Its stored lease
/// lives two seconds and a watcher on a second storage reaps expired leases
/// every 250 ms, so those renewals reached the database: a reaped node's
/// next heartbeat answers `Reaped` and its runner stops (FIG-5241,
/// FIG-5240).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturated_shared_pool_cannot_starve_the_heartbeat() {
    let Some(database) = database("a_saturated_shared_pool_cannot_starve_the_heartbeat").await
    else {
        return;
    };
    let storage = crate::testing::connect_with(database.url(), &crate::testing::work_pool_of(2))
        .await
        .expect("open the isolated store");
    let store = storage.durable_store();
    let watching = crate::testing::connect(database.url())
        .await
        .expect("open the watcher's store");
    let watching = watching.durable_store();
    let lease = LeaseSettings {
        ttl: Duration::from_secs(2),
        heartbeat_every: Duration::from_millis(300),
        self_stop_after: Duration::from_millis(1_500),
        ..LeaseSettings::default()
    };
    let watcher = watching
        .register_node(&NodeSpec {
            node: NodeId::new("watcher"),
            decodes: vec![formats()],
            ttl_millis: i64::try_from(lease.ttl.as_millis()).expect("ttl fits"),
        })
        .await
        .expect("register the watcher");
    let pools = &store.pools;
    let mut held = Vec::new();
    for pool in [&pools.work, &pools.scheduler, &pools.critical] {
        for _ in 0..pool.options().get_max_connections() {
            held.push(pool.acquire().await.expect("hold a connection"));
        }
    }
    let node = start(&storage, "busy", lease, false);
    let started = Instant::now();
    while started.elapsed() < 3 * lease.self_stop_after {
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            matches!(
                watching.heartbeat(&watcher).await,
                Ok(HeartbeatOutcome::Renewed { .. })
            ),
            "the watcher renews"
        );
        watching.reap(&watcher).await.expect("reap expired leases");
        assert!(
            !node.task.is_finished(),
            "the busy node stopped {:?} in: {:?}",
            started.elapsed(),
            node.task.await
        );
    }
    // A reap in the last tick ends the runner at its next heartbeat.
    tokio::time::sleep(2 * lease.heartbeat_every).await;
    assert!(
        !node.task.is_finished(),
        "the busy node's lease was reaped: {:?}",
        node.task.await
    );
    node.task.abort();
}

#[test]
fn a_batch_rings_each_channel_with_bounded_payloads() {
    let mut batch = WakeBatch {
        ready: true,
        ..WakeBatch::default()
    };
    let crowd: std::collections::BTreeSet<ActorKey> = (0..1_000)
        .map(|index| actor(&format!("crowded-{index:04}")))
        .collect();
    batch.owned.insert(NodeId::new("a"), crowd.clone());
    let (channels, payloads) = notifications(&batch);
    assert_eq!(channels[0], READY_CHANNEL);
    assert!(channels[1..].iter().all(|channel| channel == "lash_node_a"));
    assert!(
        payloads
            .iter()
            .all(|payload| payload.len() <= PAYLOAD_LIMIT)
    );
    let carried: std::collections::BTreeSet<ActorKey> = payloads[1..]
        .iter()
        .flat_map(|payload| payload.split('\n'))
        .map(|key| ActorKey::parse(key).expect("a carried key"))
        .collect();
    assert_eq!(carried, crowd, "every woken actor rides some payload");
    let long = NodeId::new("a node name that cannot be a channel identifier as it stands");
    assert!(node_channel(&long).len() <= 63);
}
