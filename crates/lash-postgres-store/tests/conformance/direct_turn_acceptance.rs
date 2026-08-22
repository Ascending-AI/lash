//! ADR 0069 direct-turn ingress laws on PostgreSQL.
//!
//! A direct turn is one durable acceptance followed by a drive, so PostgreSQL
//! owes the same acceptance and recovery laws as every other backend.

use std::sync::Arc;

use lash_core::store::RuntimePersistence;

use super::{reset, storage};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_direct_turn_accepts_before_driving_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres direct-turn acceptance conformance: database is not configured"
        );
        return;
    };
    reset(&storage).await;
    Box::pin(
        lash_core::testing::conformance::direct_turn_accepts_before_driving(
            "postgres",
            Arc::new(storage.session_store("root")) as Arc<dyn RuntimePersistence>,
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_orphaned_direct_turn_input_is_drivable_by_another_worker_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres direct-turn recovery conformance: database is not configured");
        return;
    };
    reset(&storage).await;
    Box::pin(
        lash_core::testing::conformance::orphaned_direct_turn_input_is_drivable_by_another_worker(
            "postgres",
            Arc::new(storage.session_store("root")) as Arc<dyn RuntimePersistence>,
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_direct_turn_acceptance_mints_no_idempotency_key_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres direct-turn identity conformance: database is not configured");
        return;
    };
    reset(&storage).await;
    Box::pin(
        lash_core::testing::conformance::direct_turn_acceptance_mints_no_idempotency_key(
            "postgres",
            Arc::new(storage.session_store("root")) as Arc<dyn RuntimePersistence>,
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_unclaimed_turn_input_settlement_is_a_conditional_write_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres unclaimed-settlement conformance: database is not configured");
        return;
    };
    reset(&storage).await;
    Box::pin(
        lash_core::testing::conformance::unclaimed_turn_input_settlement_is_a_conditional_write(
            "postgres",
            Arc::new(storage.session_store("root")) as Arc<dyn RuntimePersistence>,
        ),
    )
    .await;
}
