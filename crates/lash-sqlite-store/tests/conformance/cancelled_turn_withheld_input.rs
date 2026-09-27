//! FIG-3531 and FIG-3543 cancelled-turn withheld-work laws on SQLite.
//!
//! A cancelled turn never completes work it withheld from its terminal
//! checkpoint: input settles through the cancellation's undelivered
//! disposition, and a process wake is always deferred. SQLite owes the same
//! laws as every other backend.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::SessionStoreFactory as _;
use lash_core::store::RuntimePersistence;

use super::SUBSTRATE;
use crate::backend_fixture::TestEngineBackend;

async fn sqlite_withheld_work_store(backend: &TestEngineBackend) -> Arc<dyn RuntimePersistence> {
    backend
        .session_store_factory()
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(lash_conformance::CANCELLED_TURN_WITHHELD_INPUT_SESSION_ID),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the SQLite withheld-work session store")
}

lash_conformance::cancelled_turn_withheld_input_tests!({
    let backend = TestEngineBackend::open(SUBSTRATE).await;
    let store = sqlite_withheld_work_store(&backend).await;
    let law_backend = backend.as_backend();
    (backend, "sqlite", law_backend, store)
});
