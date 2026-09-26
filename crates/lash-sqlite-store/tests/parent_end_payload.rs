//! SQLite proof that the parent-end ledger decodes its typed `parent_payload`
//! — and refuses anything else — rather than re-deriving a scope from its
//! `(parent_kind, parent_id)` projection.
//!
//! The projection is index material only: injective, comparable, never
//! parsed. Rows are injected straight into `parent_end_plans` so each refusal
//! reaches `list_pending_parent_end_plans`/`get_parent_end_plan` exactly as a
//! stored row would.

use std::sync::Arc;

use lash_core_execution::ProcessRegistry;
use lash_sqlite_store::SqliteProcessRegistry;

fn turn_scope(session: &str, turn: &str) -> lash_core_execution::ScopeId {
    lash_core_execution::ScopeId::turn(
        lash_sansio::SessionId::from(session),
        lash_core_execution::TurnId::from(turn),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ledger_row_decodes_its_typed_payload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            &dir.path().join("sessions"),
        )
        .await
        .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;

    let process_parent = registry
        .register_process(lash_core_execution::ProcessRegistration::new(
            lash_core_execution::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core_execution::RecoveryContract::Rerunnable,
            lash_core_execution::ProcessProvenance::session(
                lash_core_execution::SessionScope::new("payload-session"),
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
        "the ledger decodes the typed parent — incarnation included — from its payload"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_cutover_ledger_row_is_refused_not_migrated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            &dir.path().join("sessions"),
        )
        .await
        .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;

    // The pre-FIG-3418 row: a `ParentScope` serialized without the version
    // wrapper, keyed by the rendered `{session}/{turn}` id it used to parse.
    let old_payload = serde_json::json!({
        "kind": "turn",
        "session_id": "old-session",
        "turn_id": "old-turn",
    })
    .to_string();
    inject(dir.path(), "turn", "old-session/old-turn", &old_payload);

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect_err("an old-format row must fail closed, not decode");
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
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            &dir.path().join("sessions"),
        )
        .await
        .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;

    let scope = turn_scope("v2-session", "v2-turn");
    let payload = serde_json::json!({
        "version": 2,
        "scope": {"kind": "turn", "session_id": "v2-session", "turn_id": "v2-turn"},
    })
    .to_string();
    inject(
        dir.path(),
        scope.storage_kind(),
        &scope.storage_id(),
        &payload,
    );

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
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
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            &dir.path().join("sessions"),
        )
        .await
        .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;

    let scope = turn_scope("version-session", "version-turn");
    let payload = serde_json::json!({
        "version": lash_core_execution::SCOPE_STORAGE_PAYLOAD_VERSION + 1,
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
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
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
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            &dir.path().join("sessions"),
        )
        .await
        .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;

    let scope = turn_scope("mismatch-session", "mismatch-turn");
    let payload = scope
        .storage_payload(lash_core_execution::FleetFormat::current())
        .expect("encode the payload");
    // The payload names one turn; the projection names another. A reader that
    // trusted either side alone would resurrect the wrong scope.
    inject(dir.path(), "turn", "mismatch-session/other-turn", &payload);

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect_err("a projection/payload disagreement must refuse");
    assert!(
        error
            .to_string()
            .contains("does not match its index projection"),
        "the refusal names the projection check: {error}"
    );
}
