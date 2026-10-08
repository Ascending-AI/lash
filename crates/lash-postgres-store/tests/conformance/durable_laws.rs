//! The engine-neutral laws that need an effect context, run on
//! the durable backend over PostgreSQL (ADR 0132 §14, FIG-5185).
//!
//! Each law's runtime runs in process over the production durable backend
//! assembled over an isolated database's store set, on an [`ActorContext`]
//! over that backend.

use std::sync::Arc;

use lash_conformance::ReopenableRuntimeStore;
use lash_core_execution::store::ConformanceDeployment;
use lash_core_execution::{ActorContext, SessionCatalogStore as _, StoreSet};

use super::*;

/// The effect context a law's turns run on: the durable backend over
/// `stores`, owning no actor.
fn durable_host(stores: &Arc<dyn StoreSet>) -> ActorContext {
    ActorContext::detached(lash_conformance::backend_over(Arc::clone(stores)))
}

#[tokio::test]
async fn ingress_plugin_callbacks_publish_state_that_survives_a_checkpoint() {
    let (_database_fixture, storage) = storage()
        .await
        .expect("PostgreSQL law requires its isolated database");
    reset(storage.pool()).await;
    let (_attachments, stores) = pg_law_stores(&storage);
    let store = Arc::new(storage.store());
    store
        .admit_session(&root_session_request("ingress-callback-state"))
        .await
        .expect("admit the callback's session");
    lash_conformance::ingress_plugin_callbacks_publish_state_that_survives_a_checkpoint(
        store,
        durable_host(&stores),
    )
    .await;
}

fn root_session_request(session_id: &str) -> lash_core_execution::SessionStoreCreateRequest {
    lash_core_execution::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(session_id.to_string()),
        relation: lash_core_execution::SessionRelation::Root,
        config: lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        )
        .into(),
        head: lash_core_execution::SessionCreationHead::Config,
    }
}

// The settlement laws run a facade runtime over a fresh durable backend per
// law, over this database's store set.
lash_conformance::session_config_settlement_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    let make = move || {
        let stores = Arc::clone(&stores);
        async move { lash_conformance::backend_over(stores) }
    };
    ((database_fixture, attachments), make)
});

/// The deployment-store fixture: one isolated database, reset for each store
/// a law asks for, and the durable host over it.
macro_rules! session_store_factory_fixture {
    () => {{
        let Some((database_fixture, storage)) = storage().await else {
            return;
        };
        let (host_attachments, host_stores) = pg_law_stores(&storage);
        let host = durable_host(&host_stores);
        let storage = Arc::new(storage);
        let make_storage = Arc::clone(&storage);
        let make = move || {
            let storage = Arc::clone(&make_storage);
            sync_await(async move {
                reset(storage.pool()).await;
                Arc::new(storage.session_store_factory()) as Arc<dyn ConformanceDeployment>
            })
        };
        let attachments = Arc::new(tempfile::tempdir().expect("attachment directory"));
        let attached_storage = Arc::clone(&storage);
        let attached_root = Arc::clone(&attachments);
        let make_attached = move || {
            let storage = Arc::clone(&attached_storage);
            let root = attached_root.path().join(uuid::Uuid::new_v4().to_string());
            sync_await(async move {
                reset(storage.pool()).await;
                (
                    Arc::new(storage.session_store_factory()) as Arc<dyn ConformanceDeployment>,
                    Arc::new(lash_core_execution::facade_support::FileAttachmentStore::new(root))
                        as Arc<dyn lash_core_execution::AttachmentStore>,
                )
            })
        };
        (
            (database_fixture, attachments, host_attachments),
            make,
            make_attached,
            host,
        )
    }};
}

lash_conformance::session_store_factory_tests!(@catalogue { session_store_factory_fixture!() }; [
    (session_store_factory, "session-store-factory"),
]);
lash_conformance::session_store_factory_tests!(@turn_cancel { session_store_factory_fixture!() }; [
    (session_meta_records_the_process_that_owns_it, "session-meta-owning-process"),
    (a_closing_session_lists_as_closing_never_as_live, "catalog-closing-entry"),
    (concurrent_session_admissions_preserve_one_relation, "concurrent-session-relation"),
    (a_session_that_never_ran_a_turn_forks_at_its_creation_revision, "fork-empty-session"),
]);

/// A fork inherits its source's history and none of its execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_inherits_history_without_execution_queues_waits_or_journals() {
    let (_database_fixture, storage) = storage()
        .await
        .expect("PostgreSQL law requires its isolated database");
    reset(storage.pool()).await;
    let (_attachments, stores) = pg_law_stores(&storage);
    lash_conformance::registration_macro_support::fork_inherits_history_without_execution_queues_waits_or_journals(
        Arc::new(storage.session_store_factory()) as Arc<dyn ConformanceDeployment>,
        durable_host(&stores),
    )
    .await;
}

lash_conformance::process_prune_session_store_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    let factory = Arc::new(storage.store()) as Arc<dyn DeploymentStore>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    (
        (database_fixture, attachments),
        factory,
        registry,
        durable_host(&stores),
    )
});

mod runtime_persistence {
    use super::*;

    lash_conformance::runtime_persistence_reopenable_tests!({
        let Some((database_fixture, storage)) = storage().await else {
            return;
        };
        // One reset per law: a law that opens several sessions (the factory
        // laws) admits each through `make`, and the catalog must keep them all.
        reset(storage.pool()).await;
        let (attachments, host_stores) = pg_law_stores(&storage);
        let effect_host = durable_host(&host_stores);
        drop(storage);
        let database_url = database_fixture.url().to_owned();
        let clock = Arc::new(lash_core_execution::testing::TestClock::new(10_000));
        let lease_clock = Arc::clone(&clock);
        (
            (database_fixture, attachments),
            move |session_id: &str| {
                let effect_host = effect_host.clone();
                let database_url = database_url.clone();
                let clock = Arc::clone(&clock);
                let request = root_session_request(session_id);
                sync_await(async move {
                    let open_storage = lash_postgres_store::testing::connect(&database_url)
                        .await
                        .expect("open first Postgres conformance pool");
                    let reopen_storage = lash_postgres_store::testing::connect(&database_url)
                        .await
                        .expect("open independent Postgres conformance pool");
                    let open_factory = open_storage
                        .session_store_factory()
                        .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>)
                        .with_lease_clock_for_testing(
                            Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>
                        );
                    let reopen_factory = reopen_storage
                        .session_store_factory()
                        .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>)
                        .with_lease_clock_for_testing(clock as Arc<dyn lash_core_execution::Clock>);
                    open_factory
                        .admit_session(&request)
                        .await
                        .expect("admit Postgres conformance session");
                    ReopenableRuntimeStore {
                        open: Arc::new(open_factory) as Arc<dyn RuntimeStore>,
                        reopen: Arc::new(reopen_factory) as Arc<dyn RuntimeStore>,
                        effect_host,
                    }
                })
            },
            lash_conformance::RuntimePersistenceLeaseTiming::controlled(move |ms| {
                lease_clock.advance(ms)
            }),
        )
    });
}
