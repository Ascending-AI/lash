//! ADR 0069 direct-turn ingress laws on SQLite.
//!
//! A direct turn is one durable acceptance followed by a drive, so SQLite owes
//! the same acceptance and recovery laws as every other backend.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::SessionStoreFactory as _;
use lash_core::store::RuntimePersistence;
use lash_sqlite_store::SqliteSessionStoreFactory;
use tempfile::TempDir;

async fn sqlite_direct_turn_store(dir: &TempDir) -> Arc<dyn RuntimePersistence> {
    let factory = SqliteSessionStoreFactory::new(dir.path().to_path_buf());
    factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("root"),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the SQLite direct-turn acceptance store")
}

lash_conformance::direct_turn_acceptance_tests!({
    let dir = tempfile::tempdir().expect("direct-turn acceptance tempdir");
    let store = sqlite_direct_turn_store(&dir).await;
    (dir, "sqlite", store)
});
