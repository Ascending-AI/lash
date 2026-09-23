//! PostgreSQL registration of the queue-drain end laws (FIG-3419).
//!
//! One `PostgresStorage` is the world's whole durable substrate: the session
//! store the drain commits to, the process registry the runtime and the sweep
//! share, the session-store factory the sweep re-opens the session through
//! for `drain_end_exists`, and the journal `effect_host`/`group_host` both
//! connect to. The two hosts are distinct `PostgresEffectHost`s over the same
//! database, so L7's closing group holds a lease foreign to the draining
//! runtime and `resume_closing_groups` answers `Pending`.

use std::sync::Arc;
use std::time::Duration;

use lash_conformance::{DrainEndWorld, DrainEndWorldFactory};
use lash_core::store::RuntimePersistence;
use lash_core::{EffectHost, ProcessRegistry, SessionStoreFactory};
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage};
use lash_sansio::SessionId;

use super::{SharedDatabaseLock, database_url, reset};

fn postgres_drain_end_host(storage: &PostgresStorage) -> Arc<dyn EffectHost> {
    let ttl = Duration::from_secs(30);
    let host = PostgresEffectHost::with_options(
        storage,
        PostgresEffectReplayOptions {
            lease_timings: lash_core::facade_support::LeaseTimings::new(ttl, ttl / 3)
                .expect("a ttl three renew intervals wide"),
            drain_budget: Default::default(),
        },
    );
    lash_conformance::install_drain_end_executors(Arc::new(host))
}

async fn postgres_drain_end_world(storage: PostgresStorage) -> DrainEndWorld {
    let store_factory = storage.session_store_factory_with_shared_process_registry();
    let store = store_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("root"),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the drain-end session store");
    DrainEndWorld {
        store: store as Arc<dyn RuntimePersistence>,
        registry: Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>,
        session_factory: Arc::new(store_factory) as Arc<dyn SessionStoreFactory>,
        effect_host: postgres_drain_end_host(&storage),
        group_host: Some(postgres_drain_end_host(&storage)),
    }
}

lash_conformance::drain_end_tests!({
    let Some(url) = database_url() else {
        eprintln!("skipping Postgres drain-end conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    // A rerun must not inherit a previous run's journaled rows.
    reset(
        PostgresStorage::connect(&url)
            .await
            .expect("drain-end reset storage")
            .pool(),
    )
    .await;
    let make: DrainEndWorldFactory = Arc::new(move |_label| {
        let url = url.clone();
        Box::pin(async move {
            let storage = PostgresStorage::connect(&url)
                .await
                .expect("connect the drain-end world's storage");
            postgres_drain_end_world(storage).await
        })
    });
    (database_lock, "postgres-drain-end", make)
});
