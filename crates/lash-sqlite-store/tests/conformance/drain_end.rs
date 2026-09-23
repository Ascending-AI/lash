//! SQLite registration of the queue-drain end laws.
//!
//! The world carries the session store the drain commits to, the process
//! registry the runtime and the sweep share, and the factory the sweep
//! re-opens the session through to read `drain_end_exists`. The group seam is
//! real on this tier: `effect_host` is the host the drain's scope is minted
//! from and `group_host` a second `SqliteEffectHost` over the same journal,
//! so L7's closing group holds a lease foreign to the draining runtime and
//! `resume_closing_groups` answers `Pending`.

use std::sync::Arc;

use lash_conformance::{DrainEndWorld, DrainEndWorldFactory};
use lash_core_execution::store::RuntimePersistence;
use lash_core_execution::{EffectHost, ProcessRegistry, SessionStoreFactory};
use lash_sansio::SessionId;

use super::{Retained, SUBSTRATE};
use crate::deployment_fixture::TestDeployment;

async fn sqlite_drain_end_world(retained: Retained) -> DrainEndWorld {
    let deployment = TestDeployment::open(SUBSTRATE).await;
    let store = deployment
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
    let group_host = deployment.reopen().await.effect_host();
    let world = DrainEndWorld {
        store: store as Arc<dyn RuntimePersistence>,
        registry: deployment.process_registry() as Arc<dyn ProcessRegistry>,
        session_factory: deployment.session_store_factory() as Arc<dyn SessionStoreFactory>,
        effect_host: lash_conformance::install_drain_end_executors(
            deployment.effect_host() as Arc<dyn EffectHost>
        ),
        group_host: Some(lash_conformance::install_drain_end_executors(
            group_host as Arc<dyn EffectHost>,
        )),
    };
    // The store connections the world returns read and write the deployment
    // for the whole law, not just for the factory call.
    retained.keep(&deployment);
    world
}

lash_conformance::drain_end_tests!({
    let retained = Retained::default();
    let worlds = retained.clone();
    (
        retained,
        "sqlite-drain-end",
        Arc::new(move |_label| {
            Box::pin(sqlite_drain_end_world(worlds.clone()))
                as std::pin::Pin<Box<dyn std::future::Future<Output = DrainEndWorld> + Send>>
        }) as DrainEndWorldFactory,
    )
});
