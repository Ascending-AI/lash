//! FIG-3531 cancelled-turn withheld-input laws on SQLite.
//!
//! A cancelled turn settles input it withheld from its terminal checkpoint
//! through the cancellation's undelivered disposition, so SQLite owes the same
//! law as every other backend.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::SessionStoreFactory as _;
use lash_core::store::RuntimePersistence;
use lash_sqlite_store::SqliteSessionStoreFactory;
use tempfile::TempDir;

async fn sqlite_withheld_input_store(dir: &TempDir) -> Arc<dyn RuntimePersistence> {
    let factory = SqliteSessionStoreFactory::new(dir.path().to_path_buf());
    factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("root"),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the SQLite cancelled-turn withheld-input store")
}

lash_conformance::cancelled_turn_withheld_input_tests!({
    let dir = tempfile::tempdir().expect("cancelled-turn withheld-input tempdir");
    let store = sqlite_withheld_input_store(&dir).await;
    (dir, "sqlite", store)
});
