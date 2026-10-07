//! S2's claim, fence, heartbeat/reap and wake measurements (FIG-5167),
//! repeated against the real `DurableStore` and `Signals` of the
//! PostgreSQL store instead of the spike's sketch tables.
//!
//! Each simulated node is a registered boot with its own pool. Claimed
//! actors are released as `waiting` with a due time already passed, so the
//! claimable set stays the same size throughout a run.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use futures_util::future::try_join_all;
use lash_core_execution::StoreSet;
use lash_durable::{
    ActorKey, CommitLabel, DurableStore, FormatSet, MailTx, NodeId, NodeLease, NodeSpec, Release,
    Signal, Signals, WakeBatch,
};
use lash_postgres_store::{
    PostgresEndpoints, PostgresHostConfig, PostgresStorage, PostgresStoreSet,
};
use serde::Serialize;

use crate::deploy::micros;
use crate::support::{Distribution, Report};

const TTL_MILLIS: i64 = 15_000;

struct BenchNode {
    store: Arc<dyn DurableStore>,
    signals: Option<Arc<dyn Signals>>,
    lease: NodeLease,
    storage: PostgresStorage,
}

async fn nodes(url: &str, count: usize, tag: &str, formats: &FormatSet) -> Result<Vec<BenchNode>> {
    let mut nodes = Vec::new();
    for index in 0..count {
        let mut config = PostgresHostConfig::default();
        config.roles.work.max_connections = 4;
        config.roles.max_store_operations = 4;
        let endpoints = PostgresEndpoints::from_url(url)
            .map_err(|error| anyhow::anyhow!("connect: {error}"))?;
        let storage = PostgresStorage::connect(&endpoints, &config, Default::default())
            .await
            .map_err(|error| anyhow::anyhow!("connect: {error}"))?;
        let set = PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
        );
        let store = set.durable_store();
        let lease = store
            .register_node(&NodeSpec {
                node: NodeId::new(format!("{tag}-n{index}")),
                decodes: vec![formats.clone()],
                ttl_millis: TTL_MILLIS,
            })
            .await
            .map_err(|error| anyhow::anyhow!("register: {error}"))?;
        nodes.push(BenchNode {
            store,
            signals: set.durable_signals(),
            lease,
            storage,
        });
    }
    Ok(nodes)
}

async fn retire(nodes: Vec<BenchNode>) -> Result<()> {
    for node in nodes {
        node.store
            .release_node(&node.lease)
            .await
            .map_err(|error| anyhow::anyhow!("release: {error}"))?;
        node.storage.pool().close().await;
    }
    Ok(())
}

async fn seed(
    store: &Arc<dyn DurableStore>,
    prefix: &str,
    count: usize,
    formats: &FormatSet,
) -> Result<Vec<ActorKey>> {
    let actors: Vec<ActorKey> = (0..count)
        .map(|index| ActorKey::session(&format!("{prefix}-{index}")))
        .collect::<Result<_, _>>()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    for chunk in actors.chunks(256) {
        let mut tx = MailTx::new();
        for actor in chunk {
            tx.create_actor(actor.clone(), formats.clone()).append(
                actor.clone(),
                bench_mail(),
                "{}".to_owned(),
            );
        }
        store
            .commit_mail(tx, CommitLabel::MAIL_SESSION)
            .await
            .map_err(|error| anyhow::anyhow!("seed: {error}"))?;
    }
    Ok(actors)
}

#[derive(Serialize)]
struct Receipt {
    operation: String,
    nodes: usize,
    elapsed_s: f64,
    operations_per_s: f64,
    actors_per_s: f64,
    empty_fraction: f64,
    latency: Distribution,
    release: Option<Distribution>,
}

/// Claim `batch` at a time from `set` claimable actors on `count` nodes for
/// `seconds`, releasing each claimed actor due at once.
async fn claim(
    report: &Report,
    url: &str,
    count: usize,
    set: usize,
    batch: usize,
    seconds: u64,
) -> Result<()> {
    let tag = format!("claim-{set}-{batch}-{count}");
    let formats = FormatSet::new(format!("bench/{tag}"));
    let nodes = nodes(url, count, &tag, &formats).await?;
    seed(&nodes[0].store, &tag, set, &formats).await?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let results = try_join_all(nodes.iter().map(|node| async move {
        let mut claims = Vec::new();
        let mut releases = Vec::new();
        let mut actors = 0usize;
        let mut empty = 0usize;
        while Instant::now() < deadline {
            let at = Instant::now();
            let claimed = node
                .store
                .claim(&node.lease, batch)
                .await
                .map_err(|error| anyhow::anyhow!("claim: {error}"))?;
            claims.push(micros(at, Instant::now()));
            if claimed.is_empty() {
                empty += 1;
                continue;
            }
            actors += claimed.len();
            let now = node
                .store
                .now()
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            for claimed in claimed {
                let at = Instant::now();
                let mut tx = node
                    .store
                    .begin(&claimed.actor, claimed.epoch)
                    .await
                    .map_err(|error| anyhow::anyhow!("begin: {error}"))?;
                tx.give_up(Release::Waiting {
                    next_due: Some(now),
                });
                node.store
                    .commit(tx, CommitLabel::SESSION_RELEASE)
                    .await
                    .map_err(|error| anyhow::anyhow!("release: {error}"))?;
                releases.push(micros(at, Instant::now()));
            }
        }
        anyhow::Ok((claims, releases, actors, empty))
    }))
    .await?;
    let elapsed = started.elapsed().as_secs_f64();
    let mut claims = Vec::new();
    let mut releases = Vec::new();
    let mut actors = 0;
    let mut empty = 0;
    for (node_claims, node_releases, node_actors, node_empty) in results {
        claims.extend(node_claims);
        releases.extend(node_releases);
        actors += node_actors;
        empty += node_empty;
    }
    report.write(
        "store",
        &Receipt {
            operation: format!("claim_{set}_batch_{batch}"),
            nodes: count,
            elapsed_s: elapsed,
            operations_per_s: claims.len() as f64 / elapsed,
            actors_per_s: actors as f64 / elapsed,
            empty_fraction: empty as f64 / claims.len().max(1) as f64,
            latency: Distribution::of(&claims)?,
            release: Distribution::of(&releases).ok(),
        },
    )?;
    retire(nodes).await
}

/// Each node owns one actor and commits empty fenced owner transactions to
/// it (begin, then commit) for `seconds`.
async fn fence(report: &Report, url: &str, count: usize, seconds: u64) -> Result<()> {
    let tag = format!("fence-{count}");
    let formats = FormatSet::new(format!("bench/{tag}"));
    let nodes = nodes(url, count, &tag, &formats).await?;
    seed(&nodes[0].store, &tag, count, &formats).await?;
    let mut owned = Vec::new();
    for node in &nodes {
        let claimed = node
            .store
            .claim(&node.lease, 1)
            .await
            .map_err(|error| anyhow::anyhow!("claim: {error}"))?;
        ensure!(claimed.len() == 1, "a fence node claimed {}", claimed.len());
        owned.push(claimed.into_iter().next().context("one claim")?);
    }
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let results = try_join_all(nodes.iter().zip(&owned).map(|(node, claimed)| async move {
        let mut latencies = Vec::new();
        while Instant::now() < deadline {
            let at = Instant::now();
            let mut tx = node
                .store
                .begin(&claimed.actor, claimed.epoch)
                .await
                .map_err(|error| anyhow::anyhow!("begin: {error}"))?;
            tx.ack_seen();
            node.store
                .commit(tx, CommitLabel::TURN_PREPARE)
                .await
                .map_err(|error| anyhow::anyhow!("commit: {error}"))?;
            latencies.push(micros(at, Instant::now()));
        }
        anyhow::Ok(latencies)
    }))
    .await?;
    let elapsed = started.elapsed().as_secs_f64();
    let latencies: Vec<u64> = results.into_iter().flatten().collect();
    report.write(
        "store",
        &Receipt {
            operation: "fence_distinct".to_owned(),
            nodes: count,
            elapsed_s: elapsed,
            operations_per_s: latencies.len() as f64 / elapsed,
            actors_per_s: 0.0,
            empty_fraction: 0.0,
            latency: Distribution::of(&latencies)?,
            release: None,
        },
    )?;
    retire(nodes).await
}

/// Each node renews its lease and sweeps for dead nodes, back to back.
async fn heartbeat(report: &Report, url: &str, count: usize, seconds: u64) -> Result<()> {
    let tag = format!("heartbeat-{count}");
    let formats = FormatSet::new(format!("bench/{tag}"));
    let nodes = nodes(url, count, &tag, &formats).await?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let results = try_join_all(nodes.iter().map(|node| async move {
        let mut latencies = Vec::new();
        while Instant::now() < deadline {
            let at = Instant::now();
            node.store
                .heartbeat(&node.lease)
                .await
                .map_err(|error| anyhow::anyhow!("heartbeat: {error}"))?;
            node.store
                .reap(&node.lease)
                .await
                .map_err(|error| anyhow::anyhow!("reap: {error}"))?;
            latencies.push(micros(at, Instant::now()));
        }
        anyhow::Ok(latencies)
    }))
    .await?;
    let elapsed = started.elapsed().as_secs_f64();
    let latencies: Vec<u64> = results.into_iter().flatten().collect();
    report.write(
        "store",
        &Receipt {
            operation: "heartbeat_reap_empty".to_owned(),
            nodes: count,
            elapsed_s: elapsed,
            operations_per_s: latencies.len() as f64 / elapsed,
            actors_per_s: 0.0,
            empty_fraction: 0.0,
            latency: Distribution::of(&latencies)?,
            release: None,
        },
    )?;
    retire(nodes).await
}

#[derive(Serialize)]
struct WakeReceipt {
    operation: &'static str,
    nodes: usize,
    deliveries: usize,
    /// Mail commit start to the owner's listener hearing it.
    end_to_end: Distribution,
    /// The mail commit alone.
    commit: Distribution,
    /// Publish start to the owner's listener hearing it.
    publish_to_delivery: Distribution,
}

/// Each of `count` owner nodes listens and owns one actor; a producer node
/// appends mail to each in turn, then publishes the post-commit hint the
/// commit's receipt names, as `Backend::commit_mail` does.
async fn wake(report: &Report, url: &str, count: usize, events: usize) -> Result<()> {
    let tag = format!("wake-{count}");
    let formats = FormatSet::new(format!("bench/{tag}"));
    let mut nodes = nodes(url, count + 1, &tag, &formats).await?;
    let producer = nodes.pop().context("a producer node")?;
    seed(&producer.store, &tag, count, &formats).await?;
    let mut feeds = Vec::new();
    let mut owned = Vec::new();
    for node in &nodes {
        let signals = node.signals.as_ref().context("PostgreSQL has signals")?;
        let feed = signals
            .listen(&node.lease)
            .await
            .map_err(|error| anyhow::anyhow!("listen: {error}"))?;
        let claimed = node
            .store
            .claim(&node.lease, 1)
            .await
            .map_err(|error| anyhow::anyhow!("claim: {error}"))?;
        ensure!(claimed.len() == 1, "a wake node claimed {}", claimed.len());
        feeds.push(feed);
        owned.push(claimed.into_iter().next().context("one claim")?.actor);
    }
    let signals = producer
        .signals
        .as_ref()
        .context("PostgreSQL has signals")?;
    let mut end_to_end = Vec::new();
    let mut commits = Vec::new();
    let mut deliveries = Vec::new();
    for event in 0..events * count {
        let target = event % count;
        let actor = &owned[target];
        let started = Instant::now();
        let mut tx = MailTx::new();
        tx.append(actor.clone(), bench_mail(), "{}".to_owned());
        let commit = producer
            .store
            .commit_mail(tx, CommitLabel::MAIL_SESSION)
            .await
            .map_err(|error| anyhow::anyhow!("mail: {error}"))?;
        let committed = Instant::now();
        let mut owned_batch: BTreeMap<NodeId, BTreeSet<ActorKey>> = BTreeMap::new();
        for woken in commit.woken {
            if let Some(owner) = woken.owner {
                owned_batch
                    .entry(owner.node)
                    .or_default()
                    .insert(woken.actor);
            }
        }
        ensure!(!owned_batch.is_empty(), "the mail commit woke no owner");
        let publishing = Instant::now();
        signals
            .publish(&WakeBatch {
                ready: BTreeSet::new(),
                owned: owned_batch,
            })
            .await
            .map_err(|error| anyhow::anyhow!("publish: {error}"))?;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), feeds[target].next())
                .await
                .context("no delivery within 10 s")?
            {
                Signal::Owned(actors) if actors.contains(actor) => break,
                _ => {}
            }
        }
        let heard = Instant::now();
        end_to_end.push(micros(started, heard));
        commits.push(micros(started, committed));
        deliveries.push(micros(publishing, heard));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    report.write(
        "store",
        &WakeReceipt {
            operation: "wake_notify_after_commit",
            nodes: count,
            deliveries: end_to_end.len(),
            end_to_end: Distribution::of(&end_to_end)?,
            commit: Distribution::of(&commits)?,
            publish_to_delivery: Distribution::of(&deliveries)?,
        },
    )?;
    drop(feeds);
    nodes.push(producer);
    retire(nodes).await
}

/// Every store measurement at each node count.
pub async fn run(
    report: &Report,
    url: &str,
    counts: &[usize],
    seconds: u64,
    events: usize,
) -> Result<()> {
    for &count in counts {
        for (set, batch) in [(4096, 1), (4096, 16), (4096, 64), (16, 16)] {
            claim(report, url, count, set, batch, seconds).await?;
        }
        fence(report, url, count, seconds).await?;
        heartbeat(report, url, count, seconds).await?;
        wake(report, url, count, events).await?;
        eprintln!("store measurements at {count} nodes done");
    }
    Ok(())
}

/// The kind of the bench's mail: the store carries it, no actor runs it.
fn bench_mail() -> lash_durable::MailKind {
    lash_durable::MailKind::new("bench.mail")
}
