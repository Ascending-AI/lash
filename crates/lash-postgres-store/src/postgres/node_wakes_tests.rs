//! Laws of the durability engine's node wakes over PostgreSQL (L8,
//! FIG-5178), on real connections and the database clock: the node-wake laws
//! every dialect keeps (`lash_durable::laws::node_wakes`), and the ones only
//! PostgreSQL's pools and payloads have.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_durable::laws::node_wakes::{NodeWakeTier, serve_node};
use lash_durable::{
    DurableSettings, DurableStore, FormatSet, HeartbeatOutcome, LeaseSettings, NodeSpec,
};

use super::*;
use crate::PostgresStorage;
use crate::testing::IsolatedDatabase;

fn formats() -> FormatSet {
    FormatSet::new("law-formats")
}

fn actor(name: &str) -> ActorKey {
    ActorKey::session(name).expect("a law's actor key")
}

/// An isolated database and the storage every node of a law shares.
struct PostgresTier {
    storage: PostgresStorage,
    _database: IsolatedDatabase,
}

impl PostgresTier {
    /// The tier of `law` under `config`, or `None` when the run was handed
    /// no server.
    async fn open(law: &str, config: crate::PostgresHostConfig) -> Option<Self> {
        let Some(database_url) = crate::postgres_test_support::database_url() else {
            eprintln!("skipping {law}: database URL is not set");
            return None;
        };
        let database = IsolatedDatabase::create(&database_url).await;
        let storage = crate::testing::connect_with(database.url(), &config)
            .await
            .expect("open the isolated store");
        Some(Self {
            storage,
            _database: database,
        })
    }
}

#[async_trait::async_trait]
impl NodeWakeTier for PostgresTier {
    async fn open(&self) -> (Arc<dyn DurableStore>, Arc<dyn NodeWakes>) {
        (
            Arc::new(self.storage.durable_store()),
            Arc::new(self.storage.node_wakes()),
        )
    }

    async fn sever(&self, boot: &Owner) {
        let ended: Vec<bool> = sqlx::query_scalar(
            "SELECT pg_terminate_backend(pid) FROM pg_locks
             WHERE locktype = 'advisory' AND granted
               AND classid = 1818325864 AND objid = hashtext($1)::oid",
        )
        .bind(boot.boot.as_str())
        .fetch_all(self.storage.pool())
        .await
        .expect("end the listener's session");
        assert_eq!(ended, vec![true], "one session held the boot's lock");
    }
}

/// Room for sixteen listeners on one storage, and a scheduler pool small
/// enough that every claim waits its turn.
fn sixteen_nodes() -> crate::PostgresHostConfig {
    let mut config = crate::testing::fixture_config();
    config.roles.served_nodes = 16;
    config.roles.scheduler.max_connections = 4;
    config
}

macro_rules! node_wake_laws {
    ($($name:ident => $config:expr),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let Some(tier) = PostgresTier::open(stringify!($name), $config).await else {
                return;
            };
            lash_durable::laws::node_wakes::$name(&tier)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    )*};
}

node_wake_laws!(
    a_released_liveness_lock_is_reaped_at_once_and_fences_the_zombie =>
        crate::testing::fixture_config(),
    a_lost_listener_session_resubscribes_holding_its_lock => crate::testing::fixture_config(),
    mail_from_another_node_reaches_a_hot_owner_through_its_hint =>
        crate::testing::fixture_config(),
    mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll =>
        crate::testing::fixture_config(),
    a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses =>
        crate::testing::fixture_config(),
    a_readied_actor_is_claimed_by_one_attempt_on_one_node => sixteen_nodes(),
    a_hint_to_a_dead_node_is_backed_by_the_claim_poll => crate::testing::fixture_config(),
);

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
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!(
            "skipping a_saturated_shared_pool_cannot_starve_the_heartbeat: database URL is not set"
        );
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
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
    let node = serve_node(
        Arc::new(store.clone()),
        None,
        "busy",
        DurableSettings {
            lease,
            ..DurableSettings::default()
        },
    );
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
            !node.is_finished(),
            "the busy node stopped after {:?}",
            started.elapsed(),
        );
    }
    // A reap in the last tick ends the runner at its next heartbeat.
    tokio::time::sleep(2 * lease.heartbeat_every).await;
    assert!(!node.is_finished(), "the busy node's lease was reaped");
    node.kill().await;
}

#[test]
fn a_batch_rings_each_channel_with_bounded_payloads() {
    let mut batch = WakeBatch {
        ready: std::collections::BTreeSet::from([NodeId::new("b")]),
        ..WakeBatch::default()
    };
    let crowd: std::collections::BTreeSet<ActorKey> = (0..1_000)
        .map(|index| actor(&format!("crowded-{index:04}")))
        .collect();
    batch.owned.insert(NodeId::new("a"), crowd.clone());
    let (channels, payloads) = notifications(&batch);
    assert_eq!(
        (channels[0].as_str(), payloads[0].as_str()),
        ("lash_node_b", ""),
        "a ready hint rings its one node with an empty payload"
    );
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
