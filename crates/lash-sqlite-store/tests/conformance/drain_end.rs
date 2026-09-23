//! SQLite registration of the queue-drain end laws.
//!
//! The world carries the session store the drain commits to, the process
//! registry the runtime and the sweep share, and the factory the sweep
//! re-opens the session through to read `drain_end_exists`. The group seam is
//! real on this tier: `effect_host` is the host the drain's scope is minted
//! from and `group_host` a second `SqliteEffectHost` over the same journal,
//! so L7's closing group holds a lease foreign to the draining runtime and
//! `resume_closing_groups` answers `Pending`.

use std::path::PathBuf;
use std::sync::Arc;

use lash_conformance::{DrainEndWorld, DrainEndWorldFactory};
use lash_core::store::RuntimePersistence;
use lash_core::{EffectHost, ProcessRegistry, SessionStoreFactory};
use lash_sansio::SessionId;
use lash_sqlite_store::{SqliteEffectHost, SqliteProcessRegistry, SqliteSessionStoreFactory};
async fn sqlite_drain_end_host(effects_db: &std::path::Path) -> Arc<dyn EffectHost> {
    let host = SqliteEffectHost::open(effects_db)
        .await
        .expect("open the effect host");
    lash_conformance::install_drain_end_executors(Arc::new(host))
}

async fn sqlite_drain_end_world(dir: &std::path::Path) -> DrainEndWorld {
    let store_factory = SqliteSessionStoreFactory::new_with_process_registry(
        dir.to_path_buf(),
        dir.join("processes.db"),
    );
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
        registry: Arc::new(
            SqliteProcessRegistry::open(&dir.join("processes.db"), dir.join("sessions"))
                .await
                .expect("open the drain-end process registry"),
        ) as Arc<dyn ProcessRegistry>,
        session_factory: Arc::new(store_factory) as Arc<dyn SessionStoreFactory>,
        effect_host: sqlite_drain_end_host(&dir.join("effects.db")).await,
        group_host: Some(sqlite_drain_end_host(&dir.join("effects.db")).await),
    }
}

lash_conformance::drain_end_tests!({
    (
        (),
        "sqlite-drain-end",
        Arc::new(|_label| {
            Box::pin(async move {
                // `keep` leaves the durable files behind: the store
                // connections the world returns read and write them for the
                // whole law, not just for the factory call.
                let dir: PathBuf = tempfile::tempdir().expect("drain-end tempdir").keep();
                sqlite_drain_end_world(&dir).await
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = DrainEndWorld> + Send>>
        }) as DrainEndWorldFactory,
    )
});
