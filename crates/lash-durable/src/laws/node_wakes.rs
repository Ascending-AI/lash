//! The node-wake laws (L8, FIG-5178; FIG-5422): wake hints between nodes,
//! each boot's liveness lock and the reap of a boot whose lock is released,
//! written once and run by each dialect with several nodes over one
//! database: PostgreSQL, and a SQLite database file.
//!
//! A law runs production runners on the system clock against a
//! [`NodeWakeTier`], and returns the first rule it saw broken.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{LawBroken, LawResult, ensure};
use crate::{
    ActorState, CommitLabel, DurableError, DurableStore, HeartbeatOutcome, LeaseSettings,
    NodeWakeEvent, NodeWakes, Owner, Release, WakeBatch,
};

mod harness;

use harness::{
    Claims, CountingStore, actor, create, eventually, holding, listening, liveness, mail, node,
    owner_of, quiet, under,
};
pub use harness::{LawNode, serve_node};

/// One dialect's database, as the node-wake laws reach it.
#[async_trait::async_trait]
pub trait NodeWakeTier: Send + Sync {
    /// The durable store and node wakes one node runs on. A dialect may
    /// open each node its own handles over the one database, as a process
    /// of its own would.
    async fn open(&self) -> (Arc<dyn DurableStore>, Arc<dyn NodeWakes>);

    /// End `boot`'s listener session from outside the listener, as a server
    /// that drops its connection or an operator who deletes its lock would:
    /// the listener must take its lock and its subscription again.
    async fn sever(&self, boot: &Owner);
}

/// Whether `boot`'s liveness lock is held, as a probe sees it now.
async fn held(node_wakes: &dyn NodeWakes, boot: &Owner) -> Result<Option<bool>, LawBroken> {
    Ok(liveness(node_wakes)
        .await?
        .into_iter()
        .find(|liveness| liveness.boot == *boot)
        .map(|liveness| liveness.held))
}

/// Serve node `name` of `tier` under `settings`, with node wakes.
async fn start(tier: &dyn NodeWakeTier, name: &str, settings: crate::DurableSettings) -> LawNode {
    let (store, node_wakes) = tier.open().await;
    serve_node(store, Some(node_wakes), name, settings)
}

/// Serve node `name` of `tier` under `settings`, with node wakes, counting
/// its claim attempts in `claims`.
async fn start_counted(
    tier: &dyn NodeWakeTier,
    name: &str,
    settings: crate::DurableSettings,
    claims: &Claims,
) -> LawNode {
    let (inner, node_wakes) = tier.open().await;
    let store = CountingStore {
        inner,
        node: name.to_owned(),
        claims: claims.clone(),
    };
    serve_node(Arc::new(store), Some(node_wakes), name, settings)
}

/// A listener holds its boot's liveness lock. Only a reaper that itself
/// listens may reap through the lock, and only once the lock is free: then
/// the boot is reaped at once, long before its lease lapses, with its actors'
/// epochs bumped, so the dead boot's zombie commit is refused and leaves
/// nothing.
pub async fn a_released_liveness_lock_is_reaped_at_once_and_fences_the_zombie(
    tier: &dyn NodeWakeTier,
) -> LawResult {
    let (store, node_wakes) = tier.open().await;
    let (store, node_wakes) = (store.as_ref(), node_wakes.as_ref());
    let held_actor = actor("held")?;
    create(store, &held_actor).await?;
    let dead = node(store, "dead").await?;
    let watcher = node(store, "watcher").await?;
    let claimed = store.claim(&dead, 1).await?;
    ensure!(claimed.len() == 1, "the dead node claimed {claimed:?}");
    let feed = node_wakes.listen(&dead).await?;
    ensure!(
        held(node_wakes, &dead.owner).await? == Some(true),
        "a listening boot's lock reads free"
    );
    ensure!(
        held(node_wakes, &watcher.owner).await? == Some(false),
        "a boot that never listened reads held"
    );
    ensure!(
        node_wakes
            .reap_released(&watcher, &dead.owner)
            .await?
            .is_empty(),
        "a reaper that holds no lock of its own reaped"
    );
    let _watching = node_wakes.listen(&watcher).await?;
    ensure!(
        node_wakes
            .reap_released(&watcher, &dead.owner)
            .await?
            .is_empty(),
        "a boot whose lock is held was reaped"
    );
    let mut zombie = store.begin(&held_actor, claimed[0].epoch).await?;
    zombie.ack_seen().give_up(Release::Idle);

    drop(feed);
    eventually(Duration::from_secs(5), "the lock is released", || async {
        Ok(held(node_wakes, &dead.owner).await? == Some(false))
    })
    .await?;
    let reaped = node_wakes.reap_released(&watcher, &dead.owner).await?;
    ensure!(
        reaped.len() == 1
            && reaped[0].actor == held_actor
            && reaped[0].from == dead.owner
            && reaped[0].epoch > claimed[0].epoch,
        "the released boot's reap answered {reaped:?}"
    );
    ensure!(
        store.heartbeat(&dead).await? == HeartbeatOutcome::Reaped,
        "the reaped boot still renews"
    );
    let before = store.actor(&held_actor).await?;
    ensure!(
        matches!(
            store.commit(zombie, CommitLabel::new("law.write")).await,
            Err(DurableError::OwnershipLost(_))
        ),
        "the zombie's commit was not refused"
    );
    let after = store.actor(&held_actor).await?;
    ensure!(before == after, "the zombie's refused commit left a trace");
    ensure!(
        after.is_some_and(|after| after.state == ActorState::Ready),
        "the reaped actor is not ready"
    );
    Ok(())
}

/// A listener whose session is ended from outside opens another, takes its
/// lock and subscribes again, and only then reports `Resubscribed`; hints
/// sent after that reach it.
pub async fn a_lost_listener_session_resubscribes_holding_its_lock(
    tier: &dyn NodeWakeTier,
) -> LawResult {
    let (store, node_wakes) = tier.open().await;
    let lease = node(store.as_ref(), "blip").await?;
    let mut feed = node_wakes.listen(&lease).await?;
    tier.sever(&lease.owner).await;
    let event = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .map_err(|_| LawBroken("the listener never reported its new session".to_owned()))?;
    ensure!(
        event == NodeWakeEvent::Resubscribed,
        "the listener reported {event:?}"
    );
    ensure!(
        feed.session() == 1,
        "the listener counts {} lost sessions",
        feed.session()
    );
    ensure!(
        held(node_wakes.as_ref(), &lease.owner).await? == Some(true),
        "the new session does not hold the boot's lock"
    );
    node_wakes
        .publish(&WakeBatch {
            ready: BTreeSet::from([lease.owner.node.clone()]),
            ..WakeBatch::default()
        })
        .await?;
    let event = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .map_err(|_| LawBroken("a hint never reached the new session".to_owned()))?;
    ensure!(
        event == NodeWakeEvent::Ready,
        "the new session heard {event:?}"
    );
    Ok(())
}

/// O1: mail written on node B to an actor hot on node A reaches A through
/// B's after-commit hint, far inside A's ten-second mail poll.
pub async fn mail_from_another_node_reaches_a_hot_owner_through_its_hint(
    tier: &dyn NodeWakeTier,
) -> LawResult {
    let (store, node_wakes) = tier.open().await;
    let hot = actor("hot")?;
    create(store.as_ref(), &hot).await?;
    let slow_poll = LeaseSettings {
        claim_poll: Duration::from_secs(10),
        ..LeaseSettings::default()
    };
    let mut owner = start(tier, "a", under(slow_poll)).await;
    eventually(Duration::from_secs(5), "node a owns the actor", || async {
        Ok(owner_of(store.as_ref(), &hot).await?.as_deref() == Some("a"))
    })
    .await?;
    let writer = start(tier, "b", under(slow_poll)).await;
    listening(node_wakes.as_ref(), "b").await?;

    let sent = Instant::now();
    let commit = store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await?;
    writer.hints.woke(&commit);
    let arrived = owner.arrived(Duration::from_secs(5)).await?;
    let latency = arrived.saturating_duration_since(sent);
    eprintln!("cross-node hint: mail seen after {latency:?}");
    ensure!(
        latency < Duration::from_secs(1),
        "the hint took {latency:?}, as long as a poll"
    );
    Ok(())
}

/// FIG-5555: an individually oversized key requests a typed store scan,
/// waking its hot owner before the durable polling fallback is due.
pub async fn mail_for_an_oversized_key_reaches_a_hot_owner_through_a_store_scan_hint(
    tier: &dyn NodeWakeTier,
) -> LawResult {
    let (store, node_wakes) = tier.open().await;
    let hot = actor(&"x".repeat(crate::node_wake_payload::MAX_BYTES))?;
    create(store.as_ref(), &hot).await?;
    let slow_poll = LeaseSettings {
        claim_poll: Duration::from_secs(10),
        ..LeaseSettings::default()
    };
    let mut owner = start(tier, "a", under(slow_poll)).await;
    eventually(
        Duration::from_secs(5),
        "node a owns the oversized key",
        || async { Ok(owner_of(store.as_ref(), &hot).await?.as_deref() == Some("a")) },
    )
    .await?;
    let writer = start(tier, "b", under(slow_poll)).await;
    listening(node_wakes.as_ref(), "b").await?;
    // Synchronize with the real activation's mailbox read: the mail below
    // must wake an already-waiting owner, rather than ride its initial read.
    owner.waiting(Duration::from_secs(5)).await?;
    let commit = store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await?;
    writer.hints.woke(&commit);
    // The only periodic mail wake is ten seconds away. This is a hang
    // guard, not a subsecond performance requirement on the store commit.
    owner.arrived(Duration::from_secs(5)).await?;
    Ok(())
}

/// O1: mail whose hint is lost (a writer with no node wakes, standing in for
/// a dropped hint) still reaches a hot owner within its mail poll.
pub async fn mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll(
    tier: &dyn NodeWakeTier,
) -> LawResult {
    let (store, _) = tier.open().await;
    let hot = actor("hot")?;
    create(store.as_ref(), &hot).await?;
    let poll = Duration::from_millis(500);
    let lease = LeaseSettings {
        claim_poll: poll,
        ..LeaseSettings::default()
    };
    let mut owner = start(tier, "a", under(lease)).await;
    eventually(Duration::from_secs(5), "node a owns the actor", || async {
        Ok(owner_of(store.as_ref(), &hot).await?.as_deref() == Some("a"))
    })
    .await?;
    let sent = Instant::now();
    store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await?;
    let arrived = owner.arrived(Duration::from_secs(5)).await?;
    let latency = arrived.saturating_duration_since(sent);
    eprintln!("lost hint: mail seen after {latency:?} on a {poll:?} poll");
    ensure!(
        latency < poll + Duration::from_millis(500),
        "the poll took {latency:?}"
    );
    Ok(())
}

/// A node that dies is reaped through its liveness lock, and its actor is
/// claimed by a surviving node, in a small fraction of the fifteen-second
/// lease a lease reap would wait for.
pub async fn a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses(
    tier: &dyn NodeWakeTier,
) -> LawResult {
    let (store, node_wakes) = tier.open().await;
    let hot = actor("hot")?;
    create(store.as_ref(), &hot).await?;
    let dying = start(tier, "dying", under(LeaseSettings::default())).await;
    eventually(
        Duration::from_secs(5),
        "the dying node owns the actor",
        || async { Ok(owner_of(store.as_ref(), &hot).await?.as_deref() == Some("dying")) },
    )
    .await?;
    let _survivor = start(tier, "survivor", under(LeaseSettings::default())).await;
    eventually(Duration::from_secs(5), "both nodes listen", || async {
        let live = liveness(node_wakes.as_ref()).await?;
        Ok(live.len() == 2 && live.iter().all(|liveness| liveness.held))
    })
    .await?;
    // Let the survivor's watch see the dying node's lock held.
    tokio::time::sleep(Duration::from_millis(600)).await;

    dying.kill().await;
    let failover = eventually(
        Duration::from_secs(10),
        "the survivor takes over",
        || async { Ok(owner_of(store.as_ref(), &hot).await?.as_deref() == Some("survivor")) },
    )
    .await?;
    eprintln!("lock failover: the survivor owns the actor after {failover:?}");
    ensure!(
        failover < Duration::from_secs(3),
        "the takeover took {failover:?}, near the lease"
    );
    Ok(())
}

/// A readied unowned actor rings one node, not every node (FIG-5277). The
/// producing node is full, so its hint goes to one of fifteen peers; each
/// readied actor then costs exactly one claim attempt, and that attempt
/// takes it. Before, every peer claimed on a shared ready channel and all
/// but one came back empty.
pub async fn a_readied_actor_is_claimed_by_one_attempt_on_one_node(
    tier: &dyn NodeWakeTier,
) -> LawResult {
    const NODES: usize = 16;
    const READIED: usize = 8;
    let (store, node_wakes) = tier.open().await;
    let claims = Claims::default();
    let mut nodes = Vec::new();
    for index in 1..NODES {
        let name = format!("peer-{index:02}");
        nodes.push(start_counted(tier, &name, holding(quiet(), 256), &claims).await);
        listening(node_wakes.as_ref(), &name).await?;
    }
    eventually(Duration::from_secs(5), "each peer claimed once", || async {
        Ok(claims.len() == NODES - 1)
    })
    .await?;
    let held_actor = actor("held")?;
    create(store.as_ref(), &held_actor).await?;
    let producer = start_counted(tier, "producer", holding(quiet(), 1), &claims).await;
    eventually(Duration::from_secs(5), "the producer is full", || async {
        Ok(owner_of(store.as_ref(), &held_actor).await?.as_deref() == Some("producer"))
    })
    .await?;
    claims.take();

    for index in 0..READIED {
        let readied = actor(&format!("readied-{index}"))?;
        producer
            .hints
            .woke(&create(store.as_ref(), &readied).await?);
        eventually(Duration::from_secs(5), "a peer runs the actor", || async {
            Ok(owner_of(store.as_ref(), &readied)
                .await?
                .is_some_and(|owner| owner.starts_with("peer-")))
        })
        .await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let attempts = claims.take();
    let empty = attempts.iter().filter(|(_, taken)| *taken == 0).count();
    eprintln!(
        "ready wake on {NODES} nodes: {} claim attempts for {READIED} readied actors, \
         {empty} empty",
        attempts.len()
    );
    ensure!(
        (attempts.len(), empty) == (READIED, 0),
        "each readied actor costs one claim attempt that takes it: {attempts:?}"
    );
    drop(nodes);
    Ok(())
}

/// The hinted node dies before it claims: its hint is lost, and a live
/// node's claim poll takes the actor within one poll interval. The full
/// producer last probed liveness while the dead node was its only live
/// peer, so the hint goes to the dead node.
pub async fn a_hint_to_a_dead_node_is_backed_by_the_claim_poll(
    tier: &dyn NodeWakeTier,
) -> LawResult {
    let (store, node_wakes) = tier.open().await;
    let claims = Claims::default();
    let doomed = start_counted(tier, "doomed", holding(quiet(), 256), &claims).await;
    listening(node_wakes.as_ref(), "doomed").await?;
    eventually(
        Duration::from_secs(5),
        "the doomed node claimed",
        || async { Ok(claims.len() == 1) },
    )
    .await?;
    let held_actor = actor("held")?;
    create(store.as_ref(), &held_actor).await?;
    let producer = start_counted(tier, "producer", holding(quiet(), 1), &claims).await;
    eventually(Duration::from_secs(5), "the producer is full", || async {
        Ok(owner_of(store.as_ref(), &held_actor).await?.as_deref() == Some("producer"))
    })
    .await?;
    let poll = Duration::from_millis(500);
    let polling = LeaseSettings {
        claim_poll: poll,
        claim_backoff: poll,
        ..LeaseSettings::default()
    };
    let _survivor = start_counted(tier, "survivor", holding(polling, 256), &claims).await;
    listening(node_wakes.as_ref(), "survivor").await?;
    doomed.kill().await;
    eventually(
        Duration::from_secs(5),
        "the doomed node's lock is free",
        || async {
            Ok(liveness(node_wakes.as_ref())
                .await?
                .iter()
                .all(|liveness| liveness.boot.node.as_str() != "doomed" || !liveness.held))
        },
    )
    .await?;

    let readied = actor("readied")?;
    let sent = Instant::now();
    producer
        .hints
        .woke(&create(store.as_ref(), &readied).await?);
    eventually(
        Duration::from_secs(5),
        "the survivor runs the actor",
        || async { Ok(owner_of(store.as_ref(), &readied).await?.as_deref() == Some("survivor")) },
    )
    .await?;
    let latency = sent.elapsed();
    eprintln!("lost ready hint: the poll claimed the actor after {latency:?}");
    ensure!(
        latency < poll + Duration::from_millis(500),
        "the poll took {latency:?} on a {poll:?} poll"
    );
    Ok(())
}
