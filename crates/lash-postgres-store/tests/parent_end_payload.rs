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

fn turn_scope(session: &str, turn: &str) -> lash_core_execution::ParentScope {
    lash_core_execution::ParentScope::turn(
        lash_sansio::SessionId::from(session),
        lash_core_execution::TurnId::from(turn),
    )
}

/// Every injected row's `parent_id` carries this prefix so a test can drain
/// its own experiments; a malformed row left behind would fail every later
/// ledger read on the shared database.
const INJECTED_ID_PREFIX: &str = "pg-%";

async fn clean_injected(pool: &PgPool) {
    sqlx::query("DELETE FROM lash_parent_end_plans WHERE parent_id LIKE $1")
        .bind(INJECTED_ID_PREFIX)
        .execute(pool)
        .await
        .expect("drain injected ledger rows");
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
            "pg-payload-parent",
            lash_core_execution::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core_execution::RecoveryContract::Rerunnable,
            lash_core_execution::ProcessProvenance::session(
                lash_core_execution::SessionScope::new("pg-payload-session"),
            ),
            lash_core_execution::ProcessLifecyclePolicy::new(
                lash_core_execution::ParentScope::Host,
                lash_core_execution::OnParentEnd::Abandon,
            ),
        ))
        .await
        .expect("register the process parent");
    let scope = lash_core_execution::ParentScope::process(
        lash_core_execution::ProcessRef::from_record(&process_parent),
    );
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
        "the ledger decodes the typed parent — incarnation included — from its payload"
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
    clean_injected(&pool).await;
    let old_payload = serde_json::json!({
        "kind": "turn",
        "session_id": "pg-old-session",
        "turn_id": "pg-old-turn",
    })
    .to_string();
    inject(&pool, "turn", "pg-old-session/pg-old-turn", &old_payload).await;

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect_err("an old-format row must fail closed, not decode");
    clean_injected(&pool).await;
    assert!(
        error.to_string().contains("malformed parent-scope payload"),
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
        "version": lash_core_execution::PARENT_SCOPE_STORAGE_PAYLOAD_VERSION + 1,
        "scope": serde_json::to_value(&scope).expect("scope json"),
    })
    .to_string();
    clean_injected(&pool).await;
    inject(
        &pool,
        scope.storage_kind(),
        &scope.storage_id().expect("turn scopes carry an id"),
        &payload,
    )
    .await;

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect_err("a newer payload version must refuse");
    clean_injected(&pool).await;
    assert!(
        error
            .to_string()
            .contains("unsupported parent-scope payload version"),
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
    let payload = scope.storage_payload().expect("encode the payload");
    // The payload names one turn; the projection names another. A reader that
    // trusted either side alone would resurrect the wrong scope.
    clean_injected(&pool).await;
    inject(&pool, "turn", "pg-mismatch-session/other-turn", &payload).await;

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect_err("a projection/payload disagreement must refuse");
    clean_injected(&pool).await;
    assert!(
        error
            .to_string()
            .contains("does not match its index projection"),
        "the refusal names the projection check: {error}"
    );
}
