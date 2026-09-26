//! FIG-3531 cancelled-turn withheld-input laws on SQLite.
//!
//! A cancelled turn settles input it withheld from its terminal checkpoint
//! through the cancellation's undelivered disposition, so SQLite owes the same
//! law as every other backend.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::SessionStoreFactory as _;
use lash_core::store::RuntimePersistence;

use super::SUBSTRATE;
use crate::backend_fixture::TestEngineBackend;

async fn sqlite_withheld_input_store(backend: &TestEngineBackend) -> Arc<dyn RuntimePersistence> {
    backend
        .session_store_factory()
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(lash_conformance::CANCELLED_TURN_WITHHELD_INPUT_SESSION_ID),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the SQLite cancelled-turn withheld-input store")
}

lash_conformance::cancelled_turn_withheld_input_tests!({
    let backend = TestEngineBackend::open(SUBSTRATE).await;
    let store = sqlite_withheld_input_store(&backend).await;
    let law_backend = backend.as_backend();
    (backend, "sqlite", law_backend, store)
});
