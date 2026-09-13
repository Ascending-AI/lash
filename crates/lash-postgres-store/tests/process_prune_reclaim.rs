//! Postgres proof that the process-prune delete path obeys the tombstone-reclaim
//! law: the batch delete drains rows orphaned under sessions outside the batch,
//! pruned process-session ids join the deleted set, and a later delete drains
//! rows orphaned under them.
//!
//! This lives in its own test target rather than the conformance suite, which is
//! at its line budget.

use std::sync::Arc;

use lash_core::{ProcessRegistry, SessionStoreFactory};
use lash_postgres_store::PostgresStorage;

use crate::support::{SharedDatabaseLock, database_url};

#[path = "blob_probe.rs"]
mod blob_probe;

async fn storage() -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let url = database_url()?;
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect postgres");
    Some((database_lock, storage))
}

/// Truncate every `lash_*` fixture table, derived from the live catalog so a new
/// table cannot silently bleed state in. `lash_schema_versions` holds the
/// component version gate, not fixture rows.
async fn reset(storage: &PostgresStorage) {
    let pool = storage.pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("list lash_* tables");
    assert!(!tables.is_empty(), "lash_* schema tables must exist");
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(pool)
    .await
    .expect("reset postgres tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(pool)
    .await
    .expect("reset postgres process change clock");
}

lash_conformance::process_prune_reclaim_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process-prune blob reclaim law: database URL is not set");
        return;
    };
    reset(&storage).await;
    let storage = Arc::new(storage);
    let factory = Arc::new(storage.session_store_factory_with_shared_process_registry())
        as Arc<dyn SessionStoreFactory>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let probe = Arc::new(blob_probe::PostgresBlobProbe::new(
        storage,
        "fail_process_prune_blob_delete",
    ));
    (database_lock, "postgres", factory, registry, probe)
});
