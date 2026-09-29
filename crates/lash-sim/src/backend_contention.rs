use lash_sansio::SessionId;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::{
    DeploymentStore, RuntimeCommit, RuntimeSessionState, RuntimeStore, SessionCreationHead,
    SessionPolicy, SessionRelation, SessionStoreCreateRequest, StoreError,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Barrier;

#[derive(Debug, Serialize)]
pub struct BackendContentionReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub scenarios: Vec<BackendContentionScenario>,
    pub summary: BackendContentionSummary,
    #[serde(skip)]
    pub report_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct BackendContentionSummary {
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
    let sqlite_factory: Arc<dyn DeploymentStore> =
        lash_sqlite_store::SqliteStoreSet::open(&sqlite_root)
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
                lash_postgres_store::PostgresStorage::connect(&database_url)
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
        summary: BackendContentionSummary {
            passed,
            skipped,
            failed,
            production_api: "DriveEpochStore::seal_drive_epoch and SessionCommitStore::commit_runtime_state through DeploymentStore handles",
            semantics: "Competing drive seals from the same epoch admit one winner; session commits preserve idempotent retry and reject stale head revisions and changed retries.",
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
    let session_id = SessionId::from(format!("lash-sim-backend-contention-{backend}"));
    factory
        .delete_session(&session_id)
        .await
        .map_err(|error| error.to_string())?;
    let store = create_store(Arc::clone(&factory), &session_id).await?;
    let reopened = open_store(Arc::clone(&factory), &session_id).await?;
    let mut operations = Vec::new();
    operations.push(competing_drive_seals(&session_id, Arc::clone(&store), reopened).await?);

    let store = open_store(Arc::clone(&factory), &session_id).await?;
    operations.push(final_commit_retry_and_conflict_are_fenced(&session_id, store).await?);

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
        session_id: SessionId::from(session_id.to_string()),
        relation: SessionRelation::Root,
        config: SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
        head: SessionCreationHead::CommittedByCreator,
    }
}

async fn competing_drive_seals(
    session_id: &SessionId,
    left_store: Arc<dyn RuntimeStore>,
    right_store: Arc<dyn RuntimeStore>,
) -> Result<BackendContentionOperation, String> {
    use lash_core::store::{AdmissionId, DriveEpochSeal, RootStartNonce};
    let observed = left_store
        .drive_epoch(session_id)
        .await
        .map_err(|error| error.to_string())?;
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for (store, name) in [(left_store, "left"), (right_store, "right")] {
        let barrier = Arc::clone(&barrier);
        let session = session_id.clone();
        handles.push(tokio::spawn(async move {
            let admission = AdmissionId::new(format!("backend-contention-{name}"));
            barrier.wait().await;
            store
                .seal_drive_epoch(
                    &session,
                    &admission,
                    observed.epoch,
                    &RootStartNonce::new(admission.as_str()),
                )
                .await
        }));
    }
    barrier.wait().await;
    let mut sealed = 0;
    let mut superseded = 0;
    for handle in handles {
        match handle
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?
        {
            DriveEpochSeal::Sealed(_) => sealed += 1,
            DriveEpochSeal::Superseded { .. } => superseded += 1,
            DriveEpochSeal::ExecutionLost => {
                return Err("drive seal unexpectedly lost execution".to_string());
            }
        }
    }
    if (sealed, superseded) != (1, 1) {
        return Err(format!(
            "expected one seal and one supersession, got {sealed}/{superseded}"
        ));
    }
    Ok(BackendContentionOperation {
        operation_id: "runtime-persistence.competing-drive-seals",
        status: "passed",
        production_api: "DriveEpochStore::seal_drive_epoch",
        assertion: "two handles sealing different admissions from one observed epoch produce exactly one winner",
        evidence: json!({"sealed": sealed, "superseded": superseded}),
    })
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
        session_id: SessionId::from(session_id.to_string()),
        head_revision: expected_head_revision,
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&current, &[]))
        .await
        .map_err(|err| format!("establish current session head: {err}"))?;
    let stale = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        head_revision: expected_head_revision,
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let err = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&stale, &[]))
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
async fn final_commit_retry_and_conflict_are_fenced(
    session_id: &SessionId,
    store: Arc<dyn RuntimeStore>,
) -> Result<BackendContentionOperation, String> {
    let state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let operation =
        lash_core::OperationId::turn(session_id, "backend-contention-final-turn", "final");
    let (stamped_commit, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation.clone())
        .map_err(|err| format!("stamp final commit: {err}"))?;
    let first = store
        .commit_runtime_state(stamped_commit.clone())
        .await
        .map_err(|err| format!("first final commit failed: {err}"))?;
    let retry = store
        .commit_runtime_state(stamped_commit)
        .await
        .map_err(|err| format!("idempotent retry failed: {err}"))?;
    if retry.head_revision != first.head_revision || retry.checkpoint_ref != first.checkpoint_ref {
        return Err("idempotent retry returned a different persisted result".to_string());
    }

    let changed_state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        turn_index: 1,
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let (changed_commit, _) = RuntimeCommit::persisted_state_for_test(&changed_state, &[])
        .with_operation(operation)
        .map_err(|err| format!("stamp changed final commit: {err}"))?;
    let err = store
        .commit_runtime_state(changed_commit)
        .await
        .expect_err("changed retry with same turn id must conflict");
    if !matches!(err, StoreError::RuntimeTurnCommitConflict { .. }) {
        return Err(format!(
            "changed duplicate final commit returned {err:?}, expected RuntimeTurnCommitConflict"
        ));
    }

    Ok(BackendContentionOperation {
        operation_id: "runtime-persistence.idempotent-retry-and-stale-write-conflict",
        status: "passed",
        production_api: "SessionCommitStore::commit_runtime_state + RuntimeTurnCommitStamp",
        assertion: "duplicate delivery of the same final commit is idempotent, while a stale changed commit with the same turn id is rejected",
        evidence: json!({
            "session_id": session_id,
            "first_head_revision": first.head_revision,
            "retry_head_revision": retry.head_revision,
            "same_checkpoint_ref": retry.checkpoint_ref == first.checkpoint_ref,
            "duplicate_retry_idempotent": true,
            "changed_retry_error": "RuntimeTurnCommitConflict",
            "turn_id": "backend-contention-final-turn",
        }),
    })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn backend_contention_report_runs_sqlite_and_records_artifact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // The Postgres lane runs against a database created for this test
        // alone: the shared database is truncated out from under it by the
        // conformance suites running in sibling processes.
        let database = crate::postgres_test_isolation::isolated_database().await;
        let report = super::run_backend_contention_report_against(
            tmp.path(),
            database.as_ref().map(|database| database.url().to_string()),
        )
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
                    && scenario.operations.len() >= 3)
        );
        assert!(report.report_path.exists());
        let body = std::fs::read_to_string(report.report_path).expect("report body");
        assert!(body.contains("runtime-persistence.competing-drive-seals"));
        assert!(body.contains("runtime-persistence.stale-head-transaction-rejected"));
        assert!(body.contains("runtime-persistence.idempotent-retry-and-stale-write-conflict"));
    }
}
