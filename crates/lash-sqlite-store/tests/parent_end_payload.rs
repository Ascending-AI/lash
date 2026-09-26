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

fn turn_scope(session: &str, turn: &str) -> lash_core_execution::ParentScope {
    lash_core_execution::ParentScope::turn(
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
            lash_core_execution::ProcessLifecyclePolicy::new(
                lash_core_execution::ParentScope::Host,
                lash_core_execution::OnParentEnd::Abandon,
            ),
        ))
        .await
        .expect("register the process parent");
    let scope = lash_core_execution::ParentScope::process(process_parent.id.clone());
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
        error.to_string().contains("malformed parent-scope payload"),
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
        "version": lash_core_execution::PARENT_SCOPE_STORAGE_PAYLOAD_VERSION + 1,
        "scope": serde_json::to_value(&scope).expect("scope json"),
    })
    .to_string();
    inject(
        dir.path(),
        scope.storage_kind(),
        &scope.storage_id().expect("turn scopes carry an id"),
        &payload,
    );

    let error = registry
        .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
        .await
        .expect_err("a newer payload version must refuse");
    assert!(
        error
            .to_string()
            .contains("unsupported parent-scope payload version"),
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
    let payload = scope.storage_payload().expect("encode the payload");
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
