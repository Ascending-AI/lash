//! SQLite proof that the parent-end ledger decodes its typed `parent_payload`
//! — and refuses anything else — rather than re-deriving a scope from its
//! `(parent_kind, parent_id)` projection.
//!
//! The projection is index material only: injective, comparable, never
//! parsed. Rows are injected straight into `parent_end_plans` so each refusal
//! reaches `get_parent_end_plan` exactly as a stored row would.

use std::sync::Arc;

use lash_execution_contracts::process::ProcessRegistry;
use lash_sqlite_store::SqliteProcessRegistry;

/// A turn scope keeps no ledger row: its close is its run's terminal. A
/// session operation's close is still its typed ledger row, so the
/// refusals are proved on one.
fn operation_scope(session: &str, operation: &str) -> lash_core_store::scope_identity::ScopeId {
    lash_core_store::scope_identity::ScopeId::session_operation(
        lash_kernel_value::identity::SessionId::fixture(session),
        operation,
    )
}

/// Insert a ledger row bypassing the registry so payloads the write path
/// would never produce reach the read path exactly as stored bytes.
fn inject(dir: &std::path::Path, kind: &str, id: &str, payload: &str) {
    let conn = rusqlite::Connection::open(dir.join("processes.db"))
        .expect("open the process registry database");
    conn.execute(
        "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
         VALUES (?1, ?2, ?3, 0)",
        rusqlite::params![kind, id, payload],
    )
    .expect("inject the ledger row");
}

/// ADR 0094's version-2 row keyed a parent scope (`Host` included) rather
/// than a lifetime scope. FIG-3607 re-keys the ledger by `ScopeId` in place,
/// under the same payload version; the old row's scope is not a `ScopeId`, so
/// it is refused as malformed, never reinterpreted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_scope_row_is_refused_as_malformed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open_standalone_for_testing(&dir.path().join("processes.db"))
            .await
            .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;

    let scope = operation_scope("v2-session", "v2-operation");
    let payload = serde_json::json!({
        "version": lash_core_store::scope_identity::SCOPE_STORAGE_PAYLOAD_VERSION,
        "scope": {
            "kind": "session_operation",
            "session_id": "v2-session",
            "operation_id": "v2-operation",
        },
    })
    .to_string();
    inject(
        dir.path(),
        scope.storage_kind(),
        &scope.storage_id(),
        &payload,
    );

    let error = registry
        .get_parent_end_plan(&scope)
        .await
        .expect_err("a parent-scope row must refuse");
    assert!(
        error.to_string().contains("malformed scope payload"),
        "the refusal names the payload shape: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_payload_version_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open_standalone_for_testing(&dir.path().join("processes.db"))
            .await
            .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;

    let scope = operation_scope("version-session", "version-operation");
    let payload = serde_json::json!({
        "version": lash_core_store::scope_identity::SCOPE_STORAGE_PAYLOAD_VERSION + 1,
        "scope": serde_json::to_value(&scope).expect("scope json"),
    })
    .to_string();
    inject(
        dir.path(),
        scope.storage_kind(),
        &scope.storage_id(),
        &payload,
    );

    let error = registry
        .get_parent_end_plan(&scope)
        .await
        .expect_err("a newer payload version must refuse");
    assert!(
        error
            .to_string()
            .contains("unsupported scope payload version"),
        "the refusal names the version boundary: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payload_that_disagrees_with_its_projection_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open_standalone_for_testing(&dir.path().join("processes.db"))
            .await
            .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;

    let scope = operation_scope("mismatch-session", "mismatch-operation");
    let payload = scope
        .storage_payload(lash_core_store::store::FleetFormat::current())
        .expect("encode the payload");
    // The payload names one operation; the projection names another. A reader that
    // trusted either side alone would resurrect the wrong scope.
    let other = operation_scope("mismatch-session", "other-operation");
    inject(
        dir.path(),
        other.storage_kind(),
        &other.storage_id(),
        &payload,
    );

    let error = registry
        .get_parent_end_plan(&other)
        .await
        .expect_err("a projection/payload disagreement must refuse");
    assert!(
        error
            .to_string()
            .contains("does not match its index projection"),
        "the refusal names the projection check: {error}"
    );
}
