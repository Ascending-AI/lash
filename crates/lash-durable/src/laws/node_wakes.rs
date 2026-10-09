//! The node-wake laws (L8, FIG-5178; FIG-5422): wake hints between nodes,
//! each boot's liveness lock and the reap of a boot whose lock is released,
//! written once and run by each dialect with several nodes over one
//! database: PostgreSQL, and a SQLite database file.
//!
//! A law runs production runners on the system clock against a
//! [`NodeWakeTier`], and returns the first rule it saw broken.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::{LawBroken, LawOutcome, ensure};
use crate::{
    ActorState, CommitLabel, DurableError, DurableStore, HeartbeatOutcome, LeaseSettings,
    NodeWakeEvent, NodeWakes, Owner, Release, WakeBatch,
};

mod harness;

use harness::{
    Claims, CountingStore, Evidence, WakeEvidence, actor, create, eventually, holding, listening,
    liveness, mail, node, observing, owner_of, quiet, under,
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
) -> (LawNode, Evidence) {
    let (inner, node_wakes) = tier.open().await;
    let store = CountingStore {
        inner,
        node: name.to_owned(),
        claims: claims.clone(),
    };
    let (node_wakes, evidence) = observing(node_wakes);
    (
        serve_node(Arc::new(store), Some(node_wakes), name, settings),
        evidence,
    )
}

/// A listener holds its boot's liveness lock. Only a reaper that itself
/// listens may reap through the lock, and only once the lock is free: then
/// the boot is reaped at once, long before its lease lapses, with its actors'
/// epochs bumped, so the dead boot's zombie commit is refused and leaves
/// nothing.
pub async fn a_released_liveness_lock_is_reaped_at_once_and_fences_the_zombie(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
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
    eventually(|| async { Ok(held(node_wakes, &dead.owner).await? == Some(false)) }).await?;
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
) -> LawOutcome {
    let (store, node_wakes) = tier.open().await;
    let lease = node(store.as_ref(), "blip").await?;
    let mut feed = node_wakes.listen(&lease).await?;
    tier.sever(&lease.owner).await;
    let event = feed.next().await;
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
    let event = feed.next().await;
    ensure!(
        event == NodeWakeEvent::Ready,
        "the new session heard {event:?}"
    );
    Ok(())
}

/// FIG-5549: an actor whose event log grew is named to every listening
/// node, whoever owns it: a node cannot know which nodes follow the log.
/// Two nodes that own nothing each hear one batch's actors from a third
/// handle's publish.
pub async fn an_appended_log_is_named_to_every_listening_node(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
    let mut feeds = Vec::new();
    for name in ["follower-one", "follower-two"] {
        let (store, node_wakes) = tier.open().await;
        let lease = node(store.as_ref(), name).await?;
        feeds.push((name, node_wakes.listen(&lease).await?));
    }
    let appended: BTreeSet<_> = ["grew-one", "grew-two"]
        .into_iter()
        .map(|id| crate::ActorKey::process(id).map_err(|error| LawBroken(error.to_string())))
        .collect::<Result<_, _>>()?;
    let (_store, publisher) = tier.open().await;
    publisher
        .publish(&WakeBatch {
            appended: appended.clone(),
            ..WakeBatch::default()
        })
        .await?;
    for (name, feed) in &mut feeds {
        let event = feed.next().await;
        let NodeWakeEvent::Appended(heard) = event else {
            return Err(LawBroken(format!("{name} heard {event:?}")));
        };
        ensure!(
            heard.iter().cloned().collect::<BTreeSet<_>>() == appended,
            "{name} heard {heard:?}"
        );
    }
    Ok(())
}

/// O1: mail written on node B to an actor hot on node A reaches A through
/// B's after-commit hint. The owner has finished its initial mailbox read,
/// and its fallback poll is effectively disabled, so only the hint can deliver.
pub async fn mail_from_another_node_reaches_a_hot_owner_through_its_hint(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
    let (store, node_wakes) = tier.open().await;
    let hot = actor("hot")?;
    create(store.as_ref(), &hot).await?;
    let slow_poll = quiet();
    let mut owner = start(tier, "a", under(slow_poll)).await;
    eventually(|| async { Ok(owner_of(store.as_ref(), &hot).await?.as_deref() == Some("a")) })
        .await?;
    let writer = start(tier, "b", under(slow_poll)).await;
    listening(node_wakes.as_ref(), "b").await?;

    owner.waiting().await?;
    let commit = store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await?;
    writer.hints.woke(&commit);
    owner.arrived().await?;
    Ok(())
}

/// FIG-5555: an individually oversized key requests a typed store scan,
/// waking its hot owner before the durable polling fallback is due.
pub async fn mail_for_an_oversized_key_reaches_a_hot_owner_through_a_store_scan_hint(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
    let (store, node_wakes) = tier.open().await;
    let hot = actor(&"x".repeat(crate::node_wake_payload::MAX_BYTES))?;
    create(store.as_ref(), &hot).await?;
    let slow_poll = quiet();
    let mut owner = start(tier, "a", under(slow_poll)).await;
    eventually(|| async { Ok(owner_of(store.as_ref(), &hot).await?.as_deref() == Some("a")) })
        .await?;
    let writer = start(tier, "b", under(slow_poll)).await;
    listening(node_wakes.as_ref(), "b").await?;
    // Synchronize with the real activation's mailbox read: the mail below
    // must wake an already-waiting owner, rather than ride its initial read.
    owner.waiting().await?;
    let commit = store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await?;
    writer.hints.woke(&commit);
    // With the initial read finished and fallback polls disabled, only the
    // typed store-scan hint can cause this delivery.
    owner.arrived().await?;
    Ok(())
}

/// O1: mail whose hint is lost (a writer with no node wakes, standing in for
/// a dropped hint) still reaches a hot owner within its mail poll.
pub async fn mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
    let (store, _) = tier.open().await;
    let hot = actor("hot")?;
    create(store.as_ref(), &hot).await?;
    let poll = Duration::from_millis(500);
    let lease = LeaseSettings {
        claim_poll: poll,
        ..LeaseSettings::default()
    };
    let mut owner = start(tier, "a", under(lease)).await;
    eventually(|| async { Ok(owner_of(store.as_ref(), &hot).await?.as_deref() == Some("a")) })
        .await?;
    owner.waiting().await?;
    store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await?;
    owner.arrived().await?;
    Ok(())
}

/// A node that dies is reaped through its liveness lock, and its actor is
/// claimed by a surviving node. Its lease and the expiry reap are effectively
/// disabled: the survivor must observe the held lock, then reap its release.
pub async fn a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
    let (store, _) = tier.open().await;
    let hot = actor("hot")?;
    create(store.as_ref(), &hot).await?;
    let lease = LeaseSettings {
        ttl: quiet().claim_poll,
        self_stop_after: quiet().claim_poll / 2,
        reap_every: quiet().claim_poll,
        ..LeaseSettings::default()
    };
    let mut dying = start(tier, "dying", under(lease)).await;
    dying.waiting().await?;
    let claimed = store
        .actor(&hot)
        .await?
        .ok_or_else(|| LawBroken("the dying node's actor is absent".to_owned()))?;
    let dead_boot = claimed
        .owner
        .ok_or_else(|| LawBroken("the dying node does not own its actor".to_owned()))?;
    let (survivor_store, survivor_wakes) = tier.open().await;
    let (survivor_wakes, mut evidence) = observing(survivor_wakes);
    let mut survivor = serve_node(
        survivor_store,
        Some(survivor_wakes),
        "survivor",
        under(lease),
    );
    evidence
        .until(|event| {
            matches!(event, WakeEvidence::Watched(boots)
            if boots.iter().any(|boot| boot.boot == dead_boot && boot.held)
                && boots.iter().any(|boot| boot.boot.node.as_str() == "survivor" && boot.held))
        })
        .await?;

    dying.kill().await;
    let WakeEvidence::Reaped(reaped) = evidence
        .until(|event| matches!(event, WakeEvidence::Reaped(_)))
        .await?
    else {
        unreachable!("the predicate selects lock reaping evidence");
    };
    ensure!(
        reaped.len() == 1
            && reaped[0].actor == hot
            && reaped[0].from == dead_boot
            && reaped[0].epoch > claimed.epoch,
        "the survivor's lock reap answered {reaped:?}"
    );
    survivor.waiting().await?;
    ensure!(
        owner_of(store.as_ref(), &hot).await?.as_deref() == Some("survivor"),
        "the survivor did not take over the reaped actor"
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
) -> LawOutcome {
    const NODES: usize = 16;
    const READIED: usize = 8;
    let (store, node_wakes) = tier.open().await;
    let claims = Claims::default();
    let mut nodes = Vec::new();
    for index in 1..NODES {
        let name = format!("peer-{index:02}");
        let (peer, _) = start_counted(tier, &name, holding(quiet(), 256), &claims).await;
        nodes.push(peer);
        listening(node_wakes.as_ref(), &name).await?;
    }
    claims.wait_for(NODES - 1).await;
    ensure!(
        claims.len() == NODES - 1,
        "the peers claimed more than once"
    );
    let held_actor = actor("held")?;
    create(store.as_ref(), &held_actor).await?;
    let (mut producer, mut evidence) =
        start_counted(tier, "producer", holding(quiet(), 1), &claims).await;
    producer.waiting().await?;
    claims.take();

    for index in 0..READIED {
        let readied = actor(&format!("readied-{index}"))?;
        producer
            .hints
            .woke(&create(store.as_ref(), &readied).await?);
        let WakeEvidence::Published(batch) = evidence
            .until(|event| matches!(event, WakeEvidence::Published(_)))
            .await?
        else {
            unreachable!("the predicate selects publication evidence");
        };
        ensure!(
            batch.ready.len() == 1 && batch.owned.is_empty() && batch.appended.is_empty(),
            "a readied actor rings exactly one peer: {batch:?}"
        );
        claims.wait_for(index + 1).await;
        let owner = owner_of(store.as_ref(), &readied).await?;
        ensure!(
            owner
                .as_ref()
                .is_some_and(|owner| owner.starts_with("peer-")
                    && batch.ready.contains(&crate::NodeId::new(owner))),
            "the hinted peer did not take the actor: {owner:?}, {batch:?}"
        );
    }
    // Stop and join every claimant before inspecting the final count. No
    // arbitrary settling sleep, and no later claim can race the assertion.
    producer.kill().await;
    for peer in nodes {
        peer.kill().await;
    }
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
    Ok(())
}

/// The hinted node dies before it claims: its hint is lost, and a live
/// node's claim poll takes the actor within one poll interval. The full
/// producer last probed liveness while the dead node was its only live
/// peer, so the hint goes to the dead node.
pub async fn a_hint_to_a_dead_node_is_backed_by_the_claim_poll(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
    let (store, node_wakes) = tier.open().await;
    let claims = Claims::default();
    let (doomed, _) = start_counted(tier, "doomed", holding(quiet(), 256), &claims).await;
    listening(node_wakes.as_ref(), "doomed").await?;
    claims.wait_for(1).await;
    let held_actor = actor("held")?;
    create(store.as_ref(), &held_actor).await?;
    let (mut producer, mut evidence) =
        start_counted(tier, "producer", holding(quiet(), 1), &claims).await;
    producer.waiting().await?;
    let poll = Duration::from_millis(500);
    let polling = LeaseSettings {
        claim_poll: poll,
        claim_backoff: poll,
        ..LeaseSettings::default()
    };
    let before_survivor = claims.len();
    let (mut survivor, _) = start_counted(tier, "survivor", holding(polling, 256), &claims).await;
    listening(node_wakes.as_ref(), "survivor").await?;
    claims.wait_for(before_survivor + 1).await;
    doomed.kill().await;
    eventually(|| async {
        Ok(liveness(node_wakes.as_ref())
            .await?
            .iter()
            .all(|liveness| liveness.boot.node.as_str() != "doomed" || !liveness.held))
    })
    .await?;

    let readied = actor("readied")?;
    producer
        .hints
        .woke(&create(store.as_ref(), &readied).await?);
    let WakeEvidence::Published(batch) = evidence
        .until(|event| matches!(event, WakeEvidence::Published(_)))
        .await?
    else {
        unreachable!("the predicate selects publication evidence");
    };
    ensure!(
        batch.ready == BTreeSet::from([crate::NodeId::new("doomed")])
            && batch.owned.is_empty()
            && batch.appended.is_empty(),
        "the hint did not go only to the dead node: {batch:?}"
    );
    survivor.waiting().await?;
    ensure!(
        owner_of(store.as_ref(), &readied).await?.as_deref() == Some("survivor"),
        "the survivor's claim poll did not take the actor"
    );
    Ok(())
}
