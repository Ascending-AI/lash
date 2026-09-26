//! FIG-3552 restored-claim cede laws on SQLite.
//!
//! A redrive whose journal-restored checkpoint claim another driver answered
//! cedes instead of committing the same words again, so SQLite owes the same
//! law as every other backend.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::SessionStoreFactory as _;
use lash_core::store::RuntimePersistence;

use super::SUBSTRATE;
use crate::backend_fixture::TestEngineBackend;

async fn sqlite_restored_claim_cede_store(
    backend: &TestEngineBackend,
) -> Arc<dyn RuntimePersistence> {
    backend
        .session_store_factory()
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(lash_conformance::RESTORED_CLAIM_CEDE_SESSION_ID),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the SQLite restored-claim cede store")
}

lash_conformance::restored_claim_cede_tests!({
    let backend = TestEngineBackend::open(SUBSTRATE).await;
    let store = sqlite_restored_claim_cede_store(&backend).await;
    let law_backend = backend.as_backend();
    (backend, "sqlite", law_backend, store)
});
