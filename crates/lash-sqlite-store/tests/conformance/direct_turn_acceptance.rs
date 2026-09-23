//! ADR 0069 direct-turn ingress laws on SQLite.
//!
//! A direct turn is one durable acceptance followed by a drive, so SQLite owes
//! the same acceptance and recovery laws as every other backend.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core_execution::SessionStoreFactory as _;
use lash_core_execution::store::RuntimePersistence;

use super::SUBSTRATE;
use crate::deployment_fixture::TestDeployment;

async fn sqlite_direct_turn_store(deployment: &TestDeployment) -> Arc<dyn RuntimePersistence> {
    deployment
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
        .expect("create the SQLite direct-turn acceptance store")
}

lash_conformance::direct_turn_acceptance_tests!({
    let deployment = TestDeployment::open(SUBSTRATE).await;
    let store = sqlite_direct_turn_store(&deployment).await;
    (deployment, "sqlite", store)
});
