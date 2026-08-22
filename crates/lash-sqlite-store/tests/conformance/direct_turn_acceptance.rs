//! ADR 0069 direct-turn ingress laws on SQLite.
//!
//! A direct turn is one durable acceptance followed by a drive, so SQLite owes
//! the same acceptance and recovery laws as every other backend.

use std::sync::Arc;

use lash_core::SessionStoreFactory as _;
use lash_core::store::RuntimePersistence;
use lash_sqlite_store::SqliteSessionStoreFactory;
use tempfile::TempDir;

async fn sqlite_direct_turn_store(dir: &TempDir) -> Arc<dyn RuntimePersistence> {
    let factory = SqliteSessionStoreFactory::new(dir.path().to_path_buf());
    factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "root".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the SQLite direct-turn acceptance store")
}

#[tokio::test]
async fn sqlite_direct_turn_accepts_before_driving() {
    let dir = tempfile::tempdir().expect("direct-turn acceptance tempdir");
    Box::pin(
        lash_core::testing::conformance::direct_turn_accepts_before_driving(
            "sqlite",
            sqlite_direct_turn_store(&dir).await,
        ),
    )
    .await;
}

#[tokio::test]
async fn sqlite_orphaned_direct_turn_input_is_drivable_by_another_worker() {
    let dir = tempfile::tempdir().expect("direct-turn recovery tempdir");
    Box::pin(
        lash_core::testing::conformance::orphaned_direct_turn_input_is_drivable_by_another_worker(
            "sqlite",
            sqlite_direct_turn_store(&dir).await,
        ),
    )
    .await;
}

#[tokio::test]
async fn sqlite_direct_turn_acceptance_mints_no_idempotency_key() {
    let dir = tempfile::tempdir().expect("direct-turn identity tempdir");
    Box::pin(
        lash_core::testing::conformance::direct_turn_acceptance_mints_no_idempotency_key(
            "sqlite",
            sqlite_direct_turn_store(&dir).await,
        ),
    )
    .await;
}

#[tokio::test]
async fn sqlite_unclaimed_turn_input_settlement_is_a_conditional_write() {
    let dir = tempfile::tempdir().expect("unclaimed settlement tempdir");
    Box::pin(
        lash_core::testing::conformance::unclaimed_turn_input_settlement_is_a_conditional_write(
            "sqlite",
            sqlite_direct_turn_store(&dir).await,
        ),
    )
    .await;
}
