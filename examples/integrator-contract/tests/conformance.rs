//! Third-party certification: complete imported catalogues, without copying
//! their laws or reaching into Lash's private implementation modules.
#![expect(clippy::expect_used, reason = "conformance fixture setup")]

#[path = "conformance/fixture.rs"]
mod fixture;

use std::sync::Arc;

use fixture::{Fixture, Retained, sync_await};
use lash::StoreSet;
use lash::persistence::{AttachmentStorePersistence, RuntimeStore};
use lash::runtime::Clock;
use lash::testing::TestClock;
use lash_conformance::{
    ReopenableAttachmentStore, ReopenableProcessRegistry, ReopenableRuntimeStore,
    RuntimePersistenceLeaseTiming,
};

mod persistence {
    use super::*;

    lash_conformance::runtime_persistence_reopenable_tests!({
        let retained = Retained::default();
        let clock = Arc::new(TestClock::new(10_000));
        let store_clock = clock.clone();
        (
            retained.clone(),
            move |session_id: &str| {
                let request = lash::testing::store_fixtures::session_store_request(
                    &lash::SessionId::fixture(session_id),
                    "conformance-model",
                    lash::persistence::SessionRelation::Root,
                );
                let clock = store_clock.clone() as Arc<dyn Clock>;
                let (fixture, reopened) = sync_await(async move {
                    let fixture = Fixture::open(clock).await;
                    fixture
                        .stores
                        .session_store_factory()
                        .admit_session(&request)
                        .await
                        .expect("admit certification session");
                    let reopened = fixture.reopen().await;
                    (fixture, reopened)
                });
                retained.keep(&fixture);
                retained.keep(&reopened);
                ReopenableRuntimeStore {
                    open: fixture.stores.session_store_factory() as Arc<dyn RuntimeStore>,
                    reopen: reopened.stores.session_store_factory() as Arc<dyn RuntimeStore>,
                    effect_host: fixture.host(),
                }
            },
            RuntimePersistenceLeaseTiming::controlled(move |ms| clock.advance(ms)),
        )
    });
}

mod processes {
    use super::*;

    lash_conformance::process_registry_reopenable_tests!({
        let retained = Retained::default();
        (retained.clone(), move |_label: &str| {
            let fixture = retained.fresh();
            let source = fixture.clone();
            let reopened = sync_await(async move {
                lash_conformance::publish_process_registry_fixture_environments(
                    source.stores.process_env_store().as_ref(),
                )
                .await;
                source.reopen().await
            });
            retained.keep(&reopened);
            ReopenableProcessRegistry {
                open: fixture.registry,
                reopen: reopened.registry,
            }
        })
    });
}

mod catalog {
    use super::*;

    lash_conformance::session_store_factory_tests!({
        let retained = Retained::default();
        let make_retained = retained.clone();
        let make_attached_retained = retained.clone();
        let fixture = retained.fresh();
        (
            retained,
            move || make_retained.fresh().deployment,
            move || {
                let fixture = make_attached_retained.fresh();
                (fixture.deployment, fixture.stores.attachment_store())
            },
            fixture.host(),
        )
    });

    lash_conformance::session_read_view_tests!({
        let fixture = Retained::default().fresh();
        let factory = fixture.stores.session_store_factory();
        (fixture, factory)
    });

    lash_conformance::session_graph_append_tests!({
        let fixture = Retained::default().fresh();
        let factory = fixture.stores.session_store_factory();
        (fixture, factory)
    });

    lash_conformance::retention_tests!({
        let fixture = Retained::default().fresh();
        let factory = fixture.stores.session_store_factory();
        (fixture, factory)
    });
}

mod attachments {
    use super::*;

    lash_conformance::attachment_store_reopenable_tests!({
        let retained = Retained::default();
        (
            retained.clone(),
            move || {
                let fixture = retained.fresh();
                let source = fixture.clone();
                let reopened = sync_await(async move { source.reopen().await });
                retained.keep(&reopened);
                ReopenableAttachmentStore {
                    open: fixture.stores.attachment_store(),
                    reopen: reopened.stores.attachment_store(),
                }
            },
            AttachmentStorePersistence::Ephemeral,
        )
    });
}
