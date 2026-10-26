//! PostgreSQL proof that the parent-end ledger decodes its typed
//! `parent_payload` — and refuses anything else — rather than re-deriving a
//! scope from its `(parent_kind, parent_id)` projection.
//!
//! The projection is index material only: injective, comparable, never
//! parsed. Rows are injected straight into `lash_parent_end_plans` so each
//! refusal reaches `get_parent_end_plan` exactly as a stored row would.

use std::sync::Arc;

use lash_execution_contracts::process::ProcessRegistry;
use lash_postgres_store::PostgresStorage;
use sqlx::PgPool;

use crate::support::{SharedDatabaseLock, database_url};

/// A turn scope keeps no ledger row: its close is its run's terminal. A
/// session operation's close is still its typed ledger row, so the
/// refusals are proved on one.
fn operation_scope(session: &str, operation: &str) -> lash_core_store::scope_identity::ScopeId {
    lash_core_store::scope_identity::ScopeId::session_operation(
        lash_kernel_value::identity::SessionId::fixture(session),
        operation,
    )
}

/// Drain one injected row by the exact `(parent_kind, parent_id)` key the
/// test passed to [`inject`]; a malformed row left behind would fail every
/// later ledger read on the shared database. Matching the key itself — not a
/// guessed id prefix — keeps the drain in step with however the id was
/// minted (a literal, or a scope's rendered `storage_id`), and never touches
/// another test's rows.
async fn clean_injected(pool: &PgPool, kind: &str, id: &str) {
    sqlx::query("DELETE FROM lash_parent_end_plans WHERE parent_kind = $1 AND parent_id = $2")
        .bind(kind)
        .bind(id)
        .execute(pool)
        .await
        .expect("drain the injected ledger row");
}

/// Insert a ledger row bypassing the registry so payloads the write path
/// would never produce reach the read path exactly as stored bytes.
async fn inject(pool: &PgPool, kind: &str, id: &str, payload: &str) {
    sqlx::query(
        "INSERT INTO lash_parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
         VALUES ($1, $2, $3, 0)",
    )
    .bind(kind)
    .bind(id)
    .bind(payload)
    .execute(pool)
    .await
    .expect("inject the ledger row");
}

async fn storage() -> Option<(SharedDatabaseLock, PostgresStorage, PgPool)> {
    let url = database_url()?;
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = lash_postgres_store::testing::connect(&url)
        .await
        .expect("connect postgres");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .expect("connect injection pool");
    Some((database_lock, storage, pool))
}

/// ADR 0094's version-2 row keyed a parent scope (`Host` included) rather
/// than a lifetime scope. FIG-3607 re-keys the ledger by `ScopeId` in place,
/// under the same payload version; the old row's scope is not a `ScopeId`, so
/// it is refused as malformed, never reinterpreted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_scope_row_is_refused_as_malformed() {
    let Some((_database_lock, storage, pool)) = storage().await else {
        eprintln!("skipping PostgreSQL parent-end payload test: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;

    let scope = operation_scope("pg-v2-session", "pg-v2-operation");
    let payload = serde_json::json!({
        "version": lash_core_store::scope_identity::SCOPE_STORAGE_PAYLOAD_VERSION,
        "scope": {
            "kind": "session_operation",
            "session_id": "pg-v2-session",
            "operation_id": "pg-v2-operation",
        },
    })
    .to_string();
    let kind = scope.storage_kind();
    let id = scope.storage_id();
    clean_injected(&pool, kind, &id).await;
    inject(&pool, kind, &id, &payload).await;

    let error = registry
        .get_parent_end_plan(&scope)
        .await
        .expect_err("a parent-scope row must refuse");
    clean_injected(&pool, kind, &id).await;
    assert!(
        error.to_string().contains("malformed scope payload"),
        "the refusal names the payload shape: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_payload_version_is_refused() {
    let Some((_database_lock, storage, pool)) = storage().await else {
        eprintln!("skipping PostgreSQL parent-end payload test: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;

    let scope = operation_scope("pg-version-session", "pg-version-operation");
    let payload = serde_json::json!({
        "version": lash_core_store::scope_identity::SCOPE_STORAGE_PAYLOAD_VERSION + 1,
        "scope": serde_json::to_value(&scope).expect("scope json"),
    })
    .to_string();
    let kind = scope.storage_kind();
    let id = scope.storage_id();
    clean_injected(&pool, kind, &id).await;
    inject(&pool, kind, &id, &payload).await;

    let error = registry
        .get_parent_end_plan(&scope)
        .await
        .expect_err("a newer payload version must refuse");
    clean_injected(&pool, kind, &id).await;
    assert!(
        error
            .to_string()
            .contains("unsupported scope payload version"),
        "the refusal names the version boundary: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payload_that_disagrees_with_its_projection_is_refused() {
    let Some((_database_lock, storage, pool)) = storage().await else {
        eprintln!("skipping PostgreSQL parent-end payload test: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;

    let scope = operation_scope("pg-mismatch-session", "pg-mismatch-operation");
    let payload = scope
        .storage_payload(lash_core_store::store::FleetFormat::current())
        .expect("encode the payload");
    // The payload names one operation; the projection names another. A reader that
    // trusted either side alone would resurrect the wrong scope.
    let other = operation_scope("pg-mismatch-session", "other-operation");
    let (kind, id) = (other.storage_kind(), other.storage_id());
    clean_injected(&pool, kind, &id).await;
    inject(&pool, kind, &id, &payload).await;

    let error = registry
        .get_parent_end_plan(&other)
        .await
        .expect_err("a projection/payload disagreement must refuse");
    clean_injected(&pool, kind, &id).await;
    assert!(
        error
            .to_string()
            .contains("does not match its index projection"),
        "the refusal names the projection check: {error}"
    );
}
