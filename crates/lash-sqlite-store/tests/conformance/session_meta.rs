use lash_core_execution::{
    SessionPolicy, SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, TurnBudget,
};
use lash_sansio::SessionId;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::unbound_session_meta_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.session_store_factory();
    for session_id in ["unbound-session-meta-a", "unbound-session-meta-b"] {
        factory
            .create_store(&SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from(session_id.to_string()),
                relation: SessionRelation::Root,
                policy: SessionPolicy::new(TurnBudget::Unbounded),
            })
            .await
            .unwrap_or_else(|error| panic!("admit `{session_id}`: {error}"));
    }
    let unbound = backend.store().await;
    (backend, "SQLite", async move {
        unbound.load_session_meta().await
    })
});
