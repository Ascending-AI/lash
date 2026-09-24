//! PostgreSQL registration of the queue-drain end laws (FIG-3419).
//!
//! One `PostgresStorage` is the world's whole durable substrate: the session
//! store the drain commits to, the store set over it (the process registry
//! the runtime and the sweep share, and the session-store factory the sweep
//! re-opens the session through for `drain_end_exists`), and the journal
//! `effect_host`/`group_host` both connect to. The two hosts are distinct `PostgresEffectHost`s over the same
//! database, so L7's closing group holds a lease foreign to the draining
//! runtime and `resume_closing_groups` answers `Pending`.

use std::sync::Arc;
use std::time::Duration;

use lash_conformance::{DrainEndWorld, DrainEndWorldFactory};
use lash_core_execution::store::RuntimePersistence;
use lash_core_execution::{EffectHost, SessionStoreFactory as _, StoreSet};
use lash_postgres_store::{
    PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage, PostgresStoreSet,
};
use lash_sansio::SessionId;

use super::{SharedDatabaseLock, database_url, reset};

fn postgres_drain_end_host(
    storage: &PostgresStorage,
    stores: &PostgresStoreSet,
) -> Arc<dyn EffectHost> {
    let ttl = Duration::from_secs(30);
    let host = PostgresEffectHost::with_options(
        storage,
        PostgresEffectReplayOptions {
            lease_timings: lash_core_execution::facade_support::LeaseTimings::new(ttl, ttl / 3)
                .expect("a ttl three renew intervals wide"),
            drain_budget: Default::default(),
        },
    );
    lash_conformance::install_drain_end_executors(Arc::new(host), stores.process_env_store())
}

async fn postgres_drain_end_world(
    storage: PostgresStorage,
    attachment_root: std::path::PathBuf,
) -> DrainEndWorld {
    let stores = PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core_execution::facade_support::FileAttachmentStore::new(attachment_root)),
    );
    let store = stores
        .session_store_factory()
        .create_store(&lash_core_execution::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("root"),
            relation: lash_core_execution::SessionRelation::Root,
            policy: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
            ),
        })
        .await
        .expect("create the drain-end session store");
    DrainEndWorld {
        store: store as Arc<dyn RuntimePersistence>,
        effect_host: postgres_drain_end_host(&storage, &stores),
        group_host: Some(postgres_drain_end_host(&storage, &stores)),
        stores: Arc::new(stores) as Arc<dyn StoreSet>,
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
    let attachments = tempfile::tempdir().expect("attachment root");
    let attachment_root = attachments.path().to_path_buf();
    let make: DrainEndWorldFactory = Arc::new(move |label| {
        let url = url.clone();
        let attachment_root = attachment_root.join(label);
        Box::pin(async move {
            let storage = PostgresStorage::connect(&url)
                .await
                .expect("connect the drain-end world's storage");
            postgres_drain_end_world(storage, attachment_root).await
        })
    });
    ((database_lock, attachments), "postgres-drain-end", make)
});
