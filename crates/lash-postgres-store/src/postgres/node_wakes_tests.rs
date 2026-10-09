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
    ActorKey, DurableSettings, DurableStore, FormatSet, HeartbeatOutcome, LeaseSettings, NodeSpec,
};

use super::*;
use crate::PostgresStorage;
use crate::testing::IsolatedDatabase;

fn formats() -> FormatSet {
    FormatSet::new("law-formats")
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
    an_appended_log_is_named_to_every_listening_node => crate::testing::fixture_config(),
    mail_for_an_oversized_key_reaches_a_hot_owner_through_a_store_scan_hint =>
        crate::testing::fixture_config(),
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

/// FIG-5555: wake hints preserve delimiter-bearing identities and the
/// largest plain key that fits the transport envelope, without oversized payloads.
#[test]
fn wake_payloads_round_trip_delimiters_and_a_maximal_fitting_key() {
    for actors in [
        std::collections::BTreeSet::from([
            ActorKey::process("b\ns/a").expect("newlines are valid"),
            ActorKey::session("quotes\"\\\0雪").expect("escaped text is valid"),
        ]),
        std::collections::BTreeSet::from([ActorKey::session(
            &"x".repeat(node_wake_payload::MAX_BYTES - 6),
        )
        .expect("actor ids have no length bound")]),
        std::collections::BTreeSet::from([ActorKey::session(
            &"\n".repeat((node_wake_payload::MAX_BYTES - 6) / 2),
        )
        .expect("escaping counts toward the bound")]),
    ] {
        let batch = WakeBatch {
            owned: std::collections::BTreeMap::from([(NodeId::new("round-trip"), actors.clone())]),
            ..WakeBatch::default()
        };
        let (_, payloads) = notifications(&batch);
        assert!(
            payloads
                .iter()
                .all(|payload| payload.len() <= node_wake_payload::MAX_BYTES)
        );
        let carried: std::collections::BTreeSet<ActorKey> = payloads
            .iter()
            .flat_map(|payload| {
                match node_wake_payload::decode(payload).expect("a valid wake payload") {
                    NodeWakeEvent::Owned(actors) => actors,
                    event => panic!("expected owned actors, got {event:?}"),
                }
            })
            .collect();
        assert_eq!(
            carried, actors,
            "wake hints preserve every complete actor identity"
        );
    }
}

/// FIG-5555: both transports split encoded batches at the byte bound and
/// replace individually oversized keys with one typed store-scan hint.
/// FIG-5549: an appended log rides the one channel every node listens to,
/// in an envelope that decodes to the same actors.
#[test]
fn an_appended_log_is_named_on_every_nodes_channel() {
    let grew = std::collections::BTreeSet::from([
        ActorKey::process("grew\n\"one\"").expect("a valid escaped actor key"),
        ActorKey::process("grew-two").expect("a valid actor key"),
    ]);
    let (channels, payloads) = notifications(&WakeBatch {
        appended: grew.clone(),
        ..WakeBatch::default()
    });
    assert_eq!(channels, [APPENDED_CHANNEL]);
    let [payload] = payloads.as_slice() else {
        panic!("one envelope names both logs: {payloads:?}");
    };
    match node_wake_payload::decode(payload).expect("a valid wake payload") {
        NodeWakeEvent::Appended(actors) => assert_eq!(
            actors
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            grew
        ),
        event => panic!("expected appended logs, got {event:?}"),
    }
}

#[test]
fn wake_payloads_split_encoded_batches_and_poll_for_oversized_keys() {
    let actors: std::collections::BTreeSet<ActorKey> = (0..1_000)
        .map(|index| {
            ActorKey::session(&format!("crowded-{index:04}\n\0雪"))
                .expect("a valid escaped actor key")
        })
        .collect();
    let mut oversized = actors.clone();
    oversized.extend([
        ActorKey::session(&"x".repeat(node_wake_payload::MAX_BYTES)).expect("unbounded ids"),
        ActorKey::process(&"\n".repeat(node_wake_payload::MAX_BYTES / 2)).expect("unbounded ids"),
    ]);
    let batch = WakeBatch {
        ready: std::collections::BTreeSet::from([NodeId::new("ready")]),
        owned: std::collections::BTreeMap::from([(
            NodeId::new("a node name that cannot be a channel identifier as it stands"),
            oversized,
        )]),
        ..WakeBatch::default()
    };
    let (channels, payloads) = notifications(&batch);
    assert_eq!(channels[0], node_channel(&NodeId::new("ready")));
    let owned_channel = node_channel(batch.owned.keys().next().expect("one owner node"));
    assert!(owned_channel.len() <= 63);
    assert!(
        channels[1..]
            .iter()
            .all(|channel| channel == &owned_channel)
    );
    assert!(payloads.len() > 3, "the encoded crowd must split");
    let mut carried = std::collections::BTreeSet::new();
    let mut ready = 0;
    let mut poll_store = 0;
    for payload in payloads {
        assert!(payload.len() <= node_wake_payload::MAX_BYTES);
        assert!(
            !payload.contains('\0'),
            "NOTIFY payloads contain no raw NUL"
        );
        match node_wake_payload::decode(&payload).expect("a bounded wake envelope") {
            NodeWakeEvent::Ready => ready += 1,
            NodeWakeEvent::Owned(actors) => carried.extend(actors),
            NodeWakeEvent::PollStore => poll_store += 1,
            event => panic!("unexpected wake event: {event:?}"),
        }
    }
    assert_eq!(ready, 1);
    assert_eq!(poll_store, 1, "oversized keys coalesce into one store scan");
    assert_eq!(
        carried, actors,
        "every fitting key rides a complete envelope"
    );
}
