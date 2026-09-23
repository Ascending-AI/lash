use lash_core_execution::{
    SessionPolicy, SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, TurnBudget,
};
use lash_sansio::SessionId;

use super::SUBSTRATE;
use crate::deployment_fixture::TestDeployment;

lash_conformance::unbound_session_meta_tests!({
    let deployment = TestDeployment::open(SUBSTRATE).await;
    let factory = deployment.session_store_factory();
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
    let unbound = deployment.store().await;
    (deployment, "SQLite", async move {
        unbound.load_session_meta().await
    })
});
