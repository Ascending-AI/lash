//! The engine-neutral laws that need an effect context, run on
//! the durable backend over this substrate (ADR 0132 §14, FIG-5185).
//!
//! Each law's runtime runs in process over the production durable backend
//! assembled over this substrate's store set, on an [`ActorContext`] over
//! that backend.

use std::sync::Arc;

use lash_conformance::ReopenableRuntimeStore;
use lash_core_execution::store::{ConformanceDeployment, RuntimeStore};
use lash_core_execution::{
    ActorContext, DeploymentStore, ProcessRegistry, SessionCatalogStore as _,
};

use super::{Retained, SUBSTRATE, root_session_request};
use crate::backend_fixture::{TestBackend, sync_await};

/// The effect context a law's turns run on: the durable backend over
/// `backend`'s store set, owning no actor.
fn durable_host(backend: &TestBackend) -> ActorContext {
    ActorContext::detached(backend.as_backend())
}

/// The deployment-store fixture: a fresh substrate per store, and the durable
/// host over one more.
macro_rules! session_store_factory_fixture {
    () => {{
        let retained: Retained<TestBackend> = Retained::default();
        let make_retained = retained.clone();
        let make = move || {
            make_retained.open_blocking().blocking_store() as Arc<dyn ConformanceDeployment>
        };
        let attached_retained = retained.clone();
        let make_attached = move || {
            let backend = attached_retained.open_blocking();
            (
                backend.blocking_store() as Arc<dyn ConformanceDeployment>,
                backend.attachment_store() as Arc<dyn lash_core_execution::AttachmentStore>,
            )
        };
        let backend = TestBackend::open(SUBSTRATE).await;
        let host = durable_host(&backend);
        ((retained, backend), make, make_attached, host)
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
#[ignore = "blocked on L4 (FIG-5174): the source turn's tool completion key reaches await_event_legacy::port_pending, todo!() until fig-5174-pending ports it"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_inherits_history_without_execution_queues_waits_or_journals() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let host = durable_host(&backend);
    lash_conformance::registration_macro_support::fork_inherits_history_without_execution_queues_waits_or_journals(
        backend.store().await as Arc<dyn ConformanceDeployment>,
        host,
    )
    .await;
}

lash_conformance::process_prune_session_store_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let factory = backend.store().await as Arc<dyn DeploymentStore>;
    let host = durable_host(&backend);
    (backend, factory, registry, host)
});

mod runtime_persistence {
    use super::*;

    lash_conformance::runtime_persistence_reopenable_tests!({
        let retained: Retained<TestBackend> = Retained::default();
        let clock = Arc::new(lash_core_execution::testing::TestClock::new(10_000));
        let store_clock = Arc::clone(&clock);
        (
            retained.clone(),
            move |session_id: &str| {
                let request = root_session_request(session_id);
                let clock = store_clock.clone() as Arc<dyn lash_core_execution::Clock>;
                let (backend, open, reopen) = sync_await(async move {
                    let backend = TestBackend::open_with_clock(SUBSTRATE, clock).await;
                    let open = backend.store().await;
                    open.admit_session(&request)
                        .await
                        .expect("admit SQLite conformance session");
                    let reopen = backend.reopen().await.store().await;
                    (backend, open, reopen)
                });
                let effect_host = durable_host(&backend);
                retained.keep(&backend);
                ReopenableRuntimeStore {
                    open: open as Arc<dyn RuntimeStore>,
                    reopen: reopen as Arc<dyn RuntimeStore>,
                    effect_host,
                }
            },
            lash_conformance::RuntimePersistenceLeaseTiming::controlled({
                let clock = Arc::clone(&clock);
                move |duration_ms| clock.advance(duration_ms)
            }),
        )
    });
}
