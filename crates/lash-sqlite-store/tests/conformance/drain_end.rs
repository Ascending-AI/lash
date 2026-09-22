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

use lash_conformance::{
    DrainEndWorld, DrainEndWorldFactory, EffectHost, ProcessRegistry, SessionStoreFactory,
};
use lash_core::RuntimeEffectEnvelope;
use lash_core::store::RuntimePersistence;
use lash_sansio::SessionId;
use lash_sqlite_store::{SqliteEffectHost, SqliteProcessRegistry, SqliteSessionStoreFactory};
fn sqlite_drain_end_host(effects_db: &std::path::Path) -> Arc<dyn EffectHost> {
    let host = SqliteEffectHost::open(effects_db.to_path_buf()).expect("open the effect host");
    host.register_group_executors(Arc::new(SettlingExecutors))
        .expect("a fresh host has no resolver yet");
    Arc::new(host)
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
        registry: SqliteProcessRegistry::open(dir.join("processes.db"), dir.join("sessions"))
            .expect("open the drain-end process registry")
            as Arc<dyn ProcessRegistry>,
        session_factory: Arc::new(store_factory) as Arc<dyn SessionStoreFactory>,
        effect_host: sqlite_drain_end_host(&dir.join("effects.db")),
        group_host: Some(sqlite_drain_end_host(&dir.join("effects.db"))),
    }
}

/// The drain-end laws never drive grouped children through a drain; the
/// resolver exists only so the hosts support groups at all, and every child a
/// law opens is served from the suite's staged-executor table first.
struct SettlingExecutors;

impl lash_conformance::GroupExecutors for SettlingExecutors {
    fn executor_for(
        &self,
        _envelope: &RuntimeEffectEnvelope,
    ) -> Option<lash_conformance::RuntimeEffectLocalExecutor<'static>> {
        Some(lash_conformance::RuntimeEffectLocalExecutor::testing(
            |_| async move {
                Ok(
                    lash_conformance::RuntimeEffectOutcome::LanguageRuntimeValue {
                        value: serde_json::json!({"settled": true}),
                    },
                )
            },
        ))
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
