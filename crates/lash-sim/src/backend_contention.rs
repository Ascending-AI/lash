use lash_sansio::SessionId;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::{
    DeploymentStore, RuntimeCommit, RuntimeSessionState, RuntimeStore, SessionCreationHead,
    SessionRelation, SessionStoreCreateRequest, StoreError,
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
pub struct BackendContentionReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub scenarios: Vec<BackendContentionScenario>,
    pub summary: BackendContentionTotals,
    #[serde(skip)]
    pub report_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct BackendContentionTotals {
    pub passed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub production_api: &'static str,
    pub semantics: &'static str,
}

#[derive(Debug, Serialize)]
pub struct BackendContentionScenario {
    pub backend: String,
    pub status: String,
    pub store_factory: String,
    pub session_id: SessionId,
    pub operations: Vec<BackendContentionOperation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BackendContentionOperation {
    pub operation_id: &'static str,
    pub status: &'static str,
    pub production_api: &'static str,
    pub assertion: &'static str,
    pub evidence: Value,
}

pub async fn run_backend_contention_report(
    artifact_root: impl AsRef<Path>,
) -> Result<BackendContentionReport, String> {
    let postgres_database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|database_url| !database_url.is_empty());
    run_backend_contention_report_against(artifact_root, postgres_database_url).await
}

/// Runs the contention report against an explicitly chosen Postgres database,
/// or SQLite only when `postgres_database_url` is `None`.
///
/// The explicit form exists so a test can point the Postgres lane at its own
/// isolated database instead of the shared one every other suite writes to.
pub async fn run_backend_contention_report_against(
    artifact_root: impl AsRef<Path>,
    postgres_database_url: Option<String>,
) -> Result<BackendContentionReport, String> {
    let artifact_root = artifact_root.as_ref();
    std::fs::create_dir_all(artifact_root).map_err(|err| err.to_string())?;
    let mut scenarios = Vec::new();

    let sqlite_root = artifact_root.join("sqlite-store");
    let sqlite_factory: Arc<dyn DeploymentStore> = lash_sqlite_store::SqliteStoreSet::open(
        sqlite_root.join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .map_err(|error| format!("open SQLite contention store: {error}"))?
    .session_store_factory();
    scenarios.push(
        run_factory_contention_scenario("sqlite", "lash_sqlite_store::SqliteStore", sqlite_factory)
            .await?,
    );

    match postgres_database_url {
        Some(database_url) => {
            let storage = Arc::new(
                lash_postgres_store::testing::connect(&database_url)
                    .await
                    .map_err(|err| format!("connect postgres contention store: {err}"))?,
            );
            let postgres_factory: Arc<dyn DeploymentStore> =
                Arc::new(lash_postgres_store::PostgresStore::new(
                    &storage,
                ));
            scenarios.push(
                run_factory_contention_scenario(
                    "postgres",
                    "lash_postgres_store::PostgresStore",
                    postgres_factory,
                )
                .await?,
            );
        }
        None => scenarios.push(BackendContentionScenario {
            backend: "postgres".to_string(),
            status: "skipped".to_string(),
            store_factory: "lash_postgres_store::PostgresStore".to_string(),
            session_id: SessionId::from("not-created"),
            operations: Vec::new(),
            skip_reason: Some(
                "LASH_POSTGRES_DATABASE_URL is not set; broad/full gate Docker bootstrap reruns this lane with Postgres enabled"
                    .to_string(),
            ),
        }),
    }

    let passed = scenarios
        .iter()
        .filter(|scenario| scenario.status == "passed")
        .count();
    let skipped = scenarios
        .iter()
        .filter(|scenario| scenario.status == "skipped")
        .count();
    let failed = scenarios
        .iter()
        .filter(|scenario| scenario.status == "failed")
        .count();
    let status = if failed == 0 { "passed" } else { "failed" };
    let report_path = artifact_root.join("backend-contention.json");
    let report = BackendContentionReport {
        schema: "lash.sim.backend-contention.v1",
        status,
        scenarios,
        summary: BackendContentionTotals {
            passed,
            skipped,
            failed,
            production_api: "SessionCommitStore::commit_runtime_state through DeploymentStore handles",
            semantics: "Session commits preserve idempotent retry and reject stale head revisions and changed retries.",
        },
        report_path: report_path.clone(),
    };
    std::fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;
    Ok(report)
}

async fn run_factory_contention_scenario(
    backend: &str,
    store_factory: &str,
    factory: Arc<dyn DeploymentStore>,
) -> Result<BackendContentionScenario, String> {
    let session_id = SessionId::fixture(format!("lash-sim-backend-contention-{backend}"));
    factory
        .delete_session(&session_id)
        .await
        .map_err(|error| error.to_string())?;
    let store = create_store(Arc::clone(&factory), &session_id).await?;
    let mut operations = Vec::new();
    operations.push(operation_commit_retry_and_conflict_are_fenced(&session_id, store).await?);

    let store = open_store(Arc::clone(&factory), &session_id).await?;
    operations.push(stale_head_transaction_is_rejected(&session_id, store).await?);

    Ok(BackendContentionScenario {
        backend: backend.to_string(),
        status: "passed".to_string(),
        store_factory: store_factory.to_string(),
        session_id,
        operations,
        skip_reason: None,
    })
}

async fn create_store(
    factory: Arc<dyn DeploymentStore>,
    session_id: &SessionId,
) -> Result<Arc<dyn RuntimeStore>, String> {
    factory
        .admit_session(&store_request(session_id))
        .await
        .map_err(|err| format!("create store `{session_id}`: {err}"))?;
    let store: Arc<dyn RuntimeStore> = factory;
    Ok(store)
}

async fn open_store(
    factory: Arc<dyn DeploymentStore>,
    session_id: &SessionId,
) -> Result<Arc<dyn RuntimeStore>, String> {
    match factory
        .lookup_session(session_id)
        .await
        .map_err(|err| format!("open store `{session_id}`: {err}"))?
    {
        lash_core::SessionLookup::Live(_) => {
            let store: Arc<dyn RuntimeStore> = factory;
            Ok(store)
        }
        lash_core::SessionLookup::Absent => create_store(factory, session_id).await,
        lash_core::SessionLookup::Deleted => Err(format!("store `{session_id}` was deleted")),
    }
}

fn store_request(session_id: &SessionId) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: SessionRelation::Root,
        config: lash_core::PersistedSessionConfig::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
            lash_core::SessionToolAccess::ambient(),
        ),
        head: SessionCreationHead::Config,
        retention: lash_core::Retention::UntilGc,
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness establishes this value"
)]
async fn stale_head_transaction_is_rejected(
    session_id: &SessionId,
    store: Arc<dyn RuntimeStore>,
) -> Result<BackendContentionOperation, String> {
    let expected_head_revision = store
        .load_session_head_meta(session_id)
        .await
        .map_err(|err| format!("load current session head: {err}"))?
        .map_or(0, |read| read.head_revision);
    let current = RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        head_revision: expected_head_revision,
        ..RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&current))
        .await
        .map_err(|err| format!("establish current session head: {err}"))?;
    let stale = RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        head_revision: expected_head_revision,
        ..RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    let err = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&stale))
        .await
        .expect_err("stale-head transaction must fail");
    if !matches!(err, StoreError::HeadRevisionConflict { .. }) {
        return Err(format!(
            "stale-head commit returned {err:?}, expected HeadRevisionConflict"
        ));
    }
    Ok(BackendContentionOperation {
        operation_id: "runtime-persistence.stale-head-transaction-rejected",
        status: "passed",
        production_api: "SessionCommitStore::commit_runtime_state",
        assertion: "transaction loss or reconnect with a stale head revision cannot publish session state",
        evidence: json!({
            "session_id": session_id,
            "error": "HeadRevisionConflict",
            "retryable_class": "reload_current_head_and_retry",
        }),
    })
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn operation_commit_retry_and_conflict_are_fenced(
    session_id: &SessionId,
    store: Arc<dyn RuntimeStore>,
) -> Result<BackendContentionOperation, String> {
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    let operation = lash_core::OperationId::new(
        lash_core::ExecutionScope::runtime_operation(format!("{session_id}:backend-contention")),
        "commit",
    );
    let (stamped_commit, _) = RuntimeCommit::persisted_state_for_test(&state)
        .with_operation(operation.clone())
        .map_err(|err| format!("stamp operation commit: {err}"))?;
    let first = store
        .commit_runtime_state(stamped_commit.clone())
        .await
        .map_err(|err| format!("first operation commit failed: {err}"))?;
    let retry = store
        .commit_runtime_state(stamped_commit)
        .await
        .map_err(|err| format!("idempotent retry failed: {err}"))?;
    if retry.head_revision != first.head_revision || retry.checkpoint_ref != first.checkpoint_ref {
        return Err("idempotent retry returned a different persisted result".to_string());
    }

    let changed_state = RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        turn_index: 1,
        ..RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    let (changed_commit, _) = RuntimeCommit::persisted_state_for_test(&changed_state)
        .with_operation(operation)
        .map_err(|err| format!("stamp changed operation commit: {err}"))?;
    let err = store
        .commit_runtime_state(changed_commit)
        .await
        .expect_err("changed retry with same operation id must conflict");
    if !matches!(err, StoreError::RuntimeTurnCommitConflict { .. }) {
        return Err(format!(
            "changed duplicate operation commit returned {err:?}, expected RuntimeTurnCommitConflict"
        ));
    }

    Ok(BackendContentionOperation {
        operation_id: "runtime-persistence.idempotent-retry-and-stale-write-conflict",
        status: "passed",
        production_api: "SessionCommitStore::commit_runtime_state + OperationId",
        assertion: "duplicate delivery of the same operation commit is idempotent, while a stale changed commit with the same operation id is rejected",
        evidence: json!({
            "session_id": session_id,
            "first_head_revision": first.head_revision,
            "retry_head_revision": retry.head_revision,
            "same_checkpoint_ref": retry.checkpoint_ref == first.checkpoint_ref,
            "duplicate_retry_idempotent": true,
            "changed_retry_error": "RuntimeTurnCommitConflict",
            "operation_scope": format!("{session_id}:backend-contention"),
        }),
    })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn backend_contention_report_runs_sqlite_and_records_artifact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = super::run_backend_contention_report_against(tmp.path(), None)
            .await
            .expect("backend contention report");
        assert_eq!(report.status, "passed");
        assert!(report.summary.passed >= 1);
        assert!(
            report
                .scenarios
                .iter()
                .any(|scenario| scenario.backend == "sqlite"
                    && scenario.status == "passed"
                    && scenario.operations.len() >= 2)
        );
        assert!(report.report_path.exists());
        let body = std::fs::read_to_string(report.report_path).expect("report body");
        assert!(body.contains("runtime-persistence.stale-head-transaction-rejected"));
        assert!(body.contains("runtime-persistence.idempotent-retry-and-stale-write-conflict"));
    }
    #[tokio::test]
    #[ignore = "requires PostgreSQL; select inside a with-service.sh pg gate"]
    async fn backend_contention_report_runs_postgres_and_records_artifact() {
        let database = crate::postgres_test_isolation::isolated_database().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = super::run_backend_contention_report_against(
            tmp.path(),
            Some(database.url().to_string()),
        )
        .await
        .expect("backend contention report");
        assert_eq!(report.status, "passed");
        assert!(
            report
                .scenarios
                .iter()
                .any(|scenario| scenario.backend == "postgres"
                    && scenario.status == "passed"
                    && scenario.operations.len() >= 2)
        );
        assert!(report.report_path.exists());
    }

    #[test]
    fn postgres_variants_never_pass_without_a_database_url() {
        crate::postgres_test_isolation::assert_requires_database_url(
            "backend_contention::tests::backend_contention_report_runs_postgres_and_records_artifact",
        );
    }
}
