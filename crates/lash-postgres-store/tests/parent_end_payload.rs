//! PostgreSQL proof that the parent-end ledger decodes its typed
//! `parent_payload` — and refuses anything else — rather than re-deriving a
//! scope from its `(parent_kind, parent_id)` projection.
//!
//! The projection is index material only: injective, comparable, never
//! parsed. Rows are injected straight into `lash_parent_end_plans` so each
//! refusal reaches `list_pending_parent_end_plans`/`get_parent_end_plan`
//! exactly as a stored row would.

use std::sync::Arc;

use lash_core_execution::ProcessRegistry;
use lash_postgres_store::PostgresStorage;
use sqlx::PgPool;

use crate::support::{SharedDatabaseLock, database_url};

fn turn_scope(session: &str, turn: &str) -> lash_core_execution::ScopeId {
    lash_core_execution::ScopeId::turn(
        lash_sansio::SessionId::from(session),
        lash_core_execution::TurnId::from(turn),
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
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect postgres");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .expect("connect injection pool");
    Some((database_lock, storage, pool))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ledger_row_decodes_its_typed_payload() {
    let Some((_database_lock, storage, _pool)) = storage().await else {
        eprintln!("skipping PostgreSQL parent-end payload test: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;

    let process_parent = registry
        .register_process(lash_core_execution::ProcessRegistration::new(
            lash_core_execution::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core_execution::RecoveryContract::Rerunnable,
            lash_core_execution::ProcessProvenance::session(
                lash_core_execution::SessionScope::new("pg-payload-session"),
            ),
            lash_core_execution::Lifetime::Detached,
        ))
        .await
        .expect("register the process parent");
    let scope = lash_core_execution::ScopeId::process(process_parent.id.clone());
    registry
        .record_parent_end(&scope)
        .await
        .expect("end the process scope");

    let plan = registry
        .get_parent_end_plan(&scope)
        .await
        .expect("read the ledger row")
        .expect("the row exists");
    assert_eq!(
        plan.parent, scope,
        "the ledger decodes the typed parent from its payload"
    );

    // Leave no pending row behind on the shared database: a later ledger
    // listing on it must not meet a scope it did not end.
    registry
        .settle_parent_end_plan(&scope)
        .await
        .expect("settle the row the test ended");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_cutover_ledger_row_is_refused_not_migrated() {
    let Some((_database_lock, storage, pool)) = storage().await else {
        eprintln!("skipping PostgreSQL parent-end payload test: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;

    // The pre-FIG-3418 row: a `ParentScope` serialized without the version
    // wrapper, keyed by the rendered `{session}/{turn}` id it used to parse.
    let (kind, id) = ("turn", "pg-old-session/pg-old-turn");
    clean_injected(&pool, kind, id).await;
    let old_payload = serde_json::json!({
        "kind": "turn",
        "session_id": "pg-old-session",
        "turn_id": "pg-old-turn",
    })
    .to_string();
    inject(&pool, kind, id, &old_payload).await;

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect_err("an old-format row must fail closed, not decode");
    clean_injected(&pool, kind, id).await;
    assert!(
        error.to_string().contains("malformed scope payload"),
        "the refusal names the payload shape: {error}"
    );
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

    let scope = turn_scope("pg-v2-session", "pg-v2-turn");
    let payload = serde_json::json!({
        "version": 2,
        "scope": {"kind": "turn", "session_id": "pg-v2-session", "turn_id": "pg-v2-turn"},
    })
    .to_string();
    let kind = scope.storage_kind();
    let id = scope.storage_id();
    clean_injected(&pool, kind, &id).await;
    inject(&pool, kind, &id, &payload).await;

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
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

    let scope = turn_scope("pg-version-session", "pg-version-turn");
    let payload = serde_json::json!({
        "version": lash_core_execution::SCOPE_STORAGE_PAYLOAD_VERSION + 1,
        "scope": serde_json::to_value(&scope).expect("scope json"),
    })
    .to_string();
    let kind = scope.storage_kind();
    let id = scope.storage_id();
    clean_injected(&pool, kind, &id).await;
    inject(&pool, kind, &id, &payload).await;

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
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

    let scope = turn_scope("pg-mismatch-session", "pg-mismatch-turn");
    let payload = scope
        .storage_payload(lash_core_execution::FleetFormat::current())
        .expect("encode the payload");
    // The payload names one turn; the projection names another. A reader that
    // trusted either side alone would resurrect the wrong scope.
    let (kind, id) = ("turn", "pg-mismatch-session/other-turn");
    clean_injected(&pool, kind, id).await;
    inject(&pool, kind, id, &payload).await;

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect_err("a projection/payload disagreement must refuse");
    clean_injected(&pool, kind, id).await;
    assert!(
        error
            .to_string()
            .contains("does not match its index projection"),
        "the refusal names the projection check: {error}"
    );
}
