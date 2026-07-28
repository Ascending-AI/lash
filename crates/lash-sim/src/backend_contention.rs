use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::{
    LeaseOwnerIdentity, LeaseOwnerLiveness, RuntimeCommit, RuntimePersistence, RuntimeSessionState,
    SessionExecutionLease, SessionExecutionLeaseClaimOutcome, SessionPolicy, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Barrier;

const LEASE_TTL_MS: u64 = 60_000;

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
    pub session_id: String,
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
    let artifact_root = artifact_root.as_ref();
    std::fs::create_dir_all(artifact_root).map_err(|err| err.to_string())?;
    let mut scenarios = Vec::new();

    let sqlite_root = artifact_root.join("sqlite-store");
    let sqlite_factory: Arc<dyn SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(&sqlite_root),
    );
    scenarios.push(
        run_factory_contention_scenario(
            "sqlite",
            "lash_sqlite_store::SqliteSessionStoreFactory",
            sqlite_factory,
        )
        .await?,
    );

    match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(database_url) => {
            let storage = Arc::new(
                lash_postgres_store::PostgresStorage::connect(&database_url)
                    .await
                    .map_err(|err| format!("connect postgres contention store: {err}"))?,
            );
            let postgres_factory: Arc<dyn SessionStoreFactory> =
                Arc::new(lash_postgres_store::PostgresSessionStoreFactory::new(
                    &storage,
                ));
            scenarios.push(
                run_factory_contention_scenario(
                    "postgres",
                    "lash_postgres_store::PostgresSessionStoreFactory",
                    postgres_factory,
                )
                .await?,
            );
        }
        Err(_) => scenarios.push(BackendContentionScenario {
            backend: "postgres".to_string(),
            status: "skipped".to_string(),
            store_factory: "lash_postgres_store::PostgresSessionStoreFactory".to_string(),
            session_id: "not-created".to_string(),
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
            production_api: "SessionExecutionLeaseStore claim/reclaim/renew/release and SessionCommitStore::commit_runtime_state through SessionStoreFactory handles",
            semantics: "Competing store handles must admit one lease owner, reject stale completion tokens, survive reopen handles, preserve idempotent retry, reject stale head revisions and stale write conflicts, and allow dead-owner reclaim without clearing the live successor.",
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
    factory: Arc<dyn SessionStoreFactory>,
) -> Result<BackendContentionScenario, String> {
    let session_id = format!("lash-sim-backend-contention-{backend}");
    factory.delete_session(&session_id).await?;
    let store = create_store(Arc::clone(&factory), &session_id).await?;
    let reopened = open_store(Arc::clone(&factory), &session_id).await?;
    let mut operations = Vec::new();
    operations.push(competing_first_claim(&session_id, Arc::clone(&store), reopened).await?);

    let store = open_store(Arc::clone(&factory), &session_id).await?;
    operations.push(stale_completion_is_fenced(&session_id, Arc::clone(&store)).await?);

    let store = open_store(Arc::clone(&factory), &session_id).await?;
    let reopened = open_store(Arc::clone(&factory), &session_id).await?;
    operations.push(reopen_handle_preserves_live_lease(&session_id, store, reopened).await?);

    let store = open_store(Arc::clone(&factory), &session_id).await?;
    operations.push(dead_owner_reclaim_preserves_live_successor(&session_id, store).await?);

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
    factory: Arc<dyn SessionStoreFactory>,
    session_id: &str,
) -> Result<Arc<dyn RuntimePersistence>, String> {
    factory
        .create_store(&store_request(session_id))
        .await
        .map_err(|err| format!("create store `{session_id}`: {err}"))
}

async fn open_store(
    factory: Arc<dyn SessionStoreFactory>,
    session_id: &str,
) -> Result<Arc<dyn RuntimePersistence>, String> {
    match factory
        .open_existing_store(&store_request(session_id))
        .await
        .map_err(|err| format!("open store `{session_id}`: {err}"))?
    {
        Some(store) => Ok(store),
        None => create_store(factory, session_id).await,
    }
}

fn store_request(session_id: &str) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: SessionRelation::Root,
        policy: SessionPolicy::default(),
    }
}

async fn competing_first_claim(
    session_id: &str,
    open: Arc<dyn RuntimePersistence>,
    reopened: Arc<dyn RuntimePersistence>,
) -> Result<BackendContentionOperation, String> {
    let owner_a = LeaseOwnerIdentity::opaque("contention-owner-a", "contention-owner-a:001");
    let owner_b = LeaseOwnerIdentity::opaque("contention-owner-b", "contention-owner-b:001");
    let barrier = Arc::new(Barrier::new(3));
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left_release_store = Arc::clone(&open);
    let right_release_store = Arc::clone(&reopened);
    let left_session = session_id.to_string();
    let right_session = session_id.to_string();
    let left_owner = owner_a.clone();
    let right_owner = owner_b.clone();
    let left = tokio::spawn(async move {
        left_barrier.wait().await;
        open.try_claim_session_execution_lease(&left_session, &left_owner, LEASE_TTL_MS)
            .await
    });
    let right = tokio::spawn(async move {
        right_barrier.wait().await;
        reopened
            .try_claim_session_execution_lease(&right_session, &right_owner, LEASE_TTL_MS)
            .await
    });
    barrier.wait().await;
    let left = left
        .await
        .map_err(|err| format!("join left first-claim race: {err}"))?
        .map_err(|err| format!("left first-claim race: {err}"))?;
    let right = right
        .await
        .map_err(|err| format!("join right first-claim race: {err}"))?
        .map_err(|err| format!("right first-claim race: {err}"))?;
    let winner = single_acquired(&left, &right)?;
    let winner_summary = lease_summary(&winner);
    let release_target = match &left {
        SessionExecutionLeaseClaimOutcome::Acquired(_) => "left",
        SessionExecutionLeaseClaimOutcome::Busy { .. } => "right",
    };
    match (&left, &right) {
        (SessionExecutionLeaseClaimOutcome::Acquired(lease), _) => {
            left_release_store
                .release_session_execution_lease(&lease.completion())
                .await
                .map_err(|err| format!("release left first-claim winner: {err}"))?;
        }
        (_, SessionExecutionLeaseClaimOutcome::Acquired(lease)) => {
            right_release_store
                .release_session_execution_lease(&lease.completion())
                .await
                .map_err(|err| format!("release right first-claim winner: {err}"))?;
        }
        _ => return Err("first-claim race had no winner".to_string()),
    }
    Ok(BackendContentionOperation {
        operation_id: "runtime-persistence.competing-first-claim",
        status: "passed",
        production_api: "SessionExecutionLeaseStore::try_claim_session_execution_lease",
        assertion: "two concurrently opened backend handles cannot both acquire the first session execution lease",
        evidence: json!({
            "left": claim_outcome_summary(&left),
            "right": claim_outcome_summary(&right),
            "winner": winner_summary,
            "release_target": release_target,
            "acquired_count": 1,
        }),
    })
}

async fn stale_completion_is_fenced(
    session_id: &str,
    store: Arc<dyn RuntimePersistence>,
) -> Result<BackendContentionOperation, String> {
    let owner_a = LeaseOwnerIdentity::opaque("stale-release-owner-a", "stale-release-owner-a:001");
    let owner_b = LeaseOwnerIdentity::opaque("stale-release-owner-b", "stale-release-owner-b:001");
    let lease = acquired(
        store
            .try_claim_session_execution_lease(session_id, &owner_a, LEASE_TTL_MS)
            .await
            .map_err(|err| format!("claim stale-release setup: {err}"))?,
    )?;
    let mut stale_completion = lease.completion();
    stale_completion.lease_token.push_str(":stale");
    store
        .release_session_execution_lease(&stale_completion)
        .await
        .map_err(|err| format!("release stale completion: {err}"))?;
    let after_stale_release = store
        .try_claim_session_execution_lease(session_id, &owner_b, LEASE_TTL_MS)
        .await
        .map_err(|err| format!("claim after stale release: {err}"))?;
    if !matches!(
        after_stale_release,
        SessionExecutionLeaseClaimOutcome::Busy { .. }
    ) {
        return Err("stale release cleared the live session execution lease".to_string());
    }
    store
        .release_session_execution_lease(&lease.completion())
        .await
        .map_err(|err| format!("release live lease after stale-release proof: {err}"))?;
    Ok(BackendContentionOperation {
        operation_id: "runtime-persistence.stale-completion-fenced",
        status: "passed",
        production_api: "SessionExecutionLeaseStore::release_session_execution_lease",
        assertion: "a stale completion token is idempotent and cannot clear a newer or live lease",
        evidence: json!({
            "live_lease": lease_summary(&lease),
            "stale_completion": {
                "session_id": stale_completion.session_id,
                "fencing_token": stale_completion.fencing_token,
                "lease_token_was_mutated": true,
            },
            "claim_after_stale_release": claim_outcome_summary(&after_stale_release),
        }),
    })
}

async fn reopen_handle_preserves_live_lease(
    session_id: &str,
    open: Arc<dyn RuntimePersistence>,
    reopened: Arc<dyn RuntimePersistence>,
) -> Result<BackendContentionOperation, String> {
    let owner_a = LeaseOwnerIdentity::opaque("reopen-owner-a", "reopen-owner-a:001");
    let owner_b = LeaseOwnerIdentity::opaque("reopen-owner-b", "reopen-owner-b:001");
    let lease = acquired(
        open.try_claim_session_execution_lease(session_id, &owner_a, LEASE_TTL_MS)
            .await
            .map_err(|err| format!("claim reopen setup: {err}"))?,
    )?;
    let reopened_claim = reopened
        .try_claim_session_execution_lease(session_id, &owner_b, LEASE_TTL_MS)
        .await
        .map_err(|err| format!("claim from reopened handle: {err}"))?;
    if !matches!(
        reopened_claim,
        SessionExecutionLeaseClaimOutcome::Busy { .. }
    ) {
        return Err("reopened handle did not observe the live lease".to_string());
    }
    reopened
        .release_session_execution_lease(&lease.completion())
        .await
        .map_err(|err| format!("release lease from reopened handle: {err}"))?;
    let after_release = acquired(
        reopened
            .try_claim_session_execution_lease(session_id, &owner_b, LEASE_TTL_MS)
            .await
            .map_err(|err| format!("claim after reopened release: {err}"))?,
    )?;
    reopened
        .release_session_execution_lease(&after_release.completion())
        .await
        .map_err(|err| format!("release reopened successor lease: {err}"))?;
    Ok(BackendContentionOperation {
        operation_id: "runtime-persistence.reopen-handle-preserves-lease",
        status: "passed",
        production_api: "SessionStoreFactory::open_existing_store + SessionExecutionLeaseStore lease methods",
        assertion: "a reopened backend handle observes live lease state and can release by fenced completion",
        evidence: json!({
            "initial_lease": lease_summary(&lease),
            "reopened_claim_while_live": claim_outcome_summary(&reopened_claim),
            "successor_lease": lease_summary(&after_release),
            "successor_fencing_token_advanced": after_release.fencing_token > lease.fencing_token,
        }),
    })
}

async fn dead_owner_reclaim_preserves_live_successor(
    session_id: &str,
    store: Arc<dyn RuntimePersistence>,
) -> Result<BackendContentionOperation, String> {
    let stale_owner = LeaseOwnerIdentity {
        owner_id: "dead-worker-owner".to_string(),
        incarnation_id: "dead-worker-owner:001".to_string(),
        liveness: LeaseOwnerLiveness::local_process_for_test(
            "lash-sim-contention-host",
            "lash-sim-contention-boot",
            u32::MAX,
            "dead-process",
        ),
    };
    let live_owner = LeaseOwnerIdentity {
        owner_id: "live-worker-owner".to_string(),
        incarnation_id: "live-worker-owner:001".to_string(),
        liveness: LeaseOwnerLiveness::local_process_for_test(
            "lash-sim-contention-host",
            "lash-sim-contention-boot",
            std::process::id(),
            "live-process",
        ),
    };
    let stale_lease = acquired(
        store
            .try_claim_session_execution_lease(session_id, &stale_owner, LEASE_TTL_MS)
            .await
            .map_err(|err| format!("claim dead-owner setup: {err}"))?,
    )?;
    let live_lease = acquired(
        store
            .reclaim_session_execution_lease(
                session_id,
                &live_owner,
                &stale_lease.fence(),
                LEASE_TTL_MS,
            )
            .await
            .map_err(|err| format!("reclaim dead-owner lease: {err}"))?,
    )?;
    store
        .release_session_execution_lease(&stale_lease.completion())
        .await
        .map_err(|err| format!("release stale completion after reclaim: {err}"))?;
    let renewed_live = store
        .renew_session_execution_lease(&live_lease.fence(), LEASE_TTL_MS)
        .await
        .map_err(|err| format!("renew live successor after stale completion: {err}"))?;
    store
        .release_session_execution_lease(&renewed_live.completion())
        .await
        .map_err(|err| format!("release renewed live successor: {err}"))?;
    Ok(BackendContentionOperation {
        operation_id: "runtime-persistence.dead-owner-reclaim",
        status: "passed",
        production_api: "SessionExecutionLeaseStore::reclaim_session_execution_lease + renew_session_execution_lease",
        assertion: "dead-owner reclaim advances the lease and stale predecessor completion cannot clear the live successor",
        evidence: json!({
            "stale_lease": lease_summary(&stale_lease),
            "live_lease": lease_summary(&live_lease),
            "renewed_live_lease": lease_summary(&renewed_live),
            "stale_completion_left_live_lease_renewable": true,
            "fencing_token_advanced": live_lease.fencing_token > stale_lease.fencing_token,
            "scope": "session_execution_lease_only",
            "worker_process_terminal_oracle": "evaluated by RuntimeBoundaryHarness against ProcessRegistry::complete_process_with_lease",
        }),
    })
}

async fn stale_head_transaction_is_rejected(
    session_id: &str,
    store: Arc<dyn RuntimePersistence>,
) -> Result<BackendContentionOperation, String> {
    let incarnation_id = store
        .load_session_meta()
        .await
        .map_err(|err| format!("load session metadata: {err}"))?
        .ok_or_else(|| format!("session `{session_id}` has no metadata"))?
        .incarnation_id;
    let expected_head_revision = store
        .load_session()
        .await
        .map_err(|err| format!("load current session head: {err}"))?
        .map_or(0, |read| read.head_revision);
    let current = RuntimeSessionState {
        session_id: session_id.to_string(),
        session_lifetime: lash_core::SessionLifetime::durable(incarnation_id.clone()),
        head_revision: expected_head_revision,
        ..RuntimeSessionState::default()
    };
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&current, &[]))
        .await
        .map_err(|err| format!("establish current session head: {err}"))?;
    let stale = RuntimeSessionState {
        session_id: session_id.to_string(),
        session_lifetime: lash_core::SessionLifetime::durable(incarnation_id),
        head_revision: expected_head_revision,
        ..RuntimeSessionState::default()
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

async fn final_commit_retry_and_conflict_are_fenced(
    session_id: &str,
    store: Arc<dyn RuntimePersistence>,
) -> Result<BackendContentionOperation, String> {
    let incarnation_id = store
        .load_session_meta()
        .await
        .map_err(|err| format!("load session metadata: {err}"))?
        .ok_or_else(|| format!("session `{session_id}` has no metadata"))?
        .incarnation_id;
    let state = RuntimeSessionState {
        session_id: session_id.to_string(),
        session_lifetime: lash_core::SessionLifetime::durable(incarnation_id.clone()),
        ..RuntimeSessionState::default()
    };
    let operation =
        lash_core::OperationId::turn(session_id, "backend-contention-final-turn", "final");
    let (stamped_commit, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation.clone())
        .map_err(|err| format!("stamp final commit: {err}"))?;
    let owner =
        LeaseOwnerIdentity::opaque("final-commit-owner", "final-commit-owner:incarnation-001");
    let lease = acquired(
        store
            .try_claim_session_execution_lease(session_id, &owner, LEASE_TTL_MS)
            .await
            .map_err(|err| format!("claim final commit lease: {err}"))?,
    )?;
    let first = store
        .commit_runtime_state(
            stamped_commit
                .clone()
                .releasing_session_execution_lease(lease.completion()),
        )
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
        session_id: session_id.to_string(),
        session_lifetime: lash_core::SessionLifetime::durable(incarnation_id),
        turn_index: 1,
        ..RuntimeSessionState::default()
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

fn single_acquired(
    left: &SessionExecutionLeaseClaimOutcome,
    right: &SessionExecutionLeaseClaimOutcome,
) -> Result<SessionExecutionLease, String> {
    let mut leases = Vec::new();
    if let SessionExecutionLeaseClaimOutcome::Acquired(lease) = left {
        leases.push(lease.clone());
    }
    if let SessionExecutionLeaseClaimOutcome::Acquired(lease) = right {
        leases.push(lease.clone());
    }
    match leases.len() {
        1 => Ok(leases.remove(0)),
        count => Err(format!(
            "expected exactly one first-claim race winner, observed {count}"
        )),
    }
}

fn acquired(outcome: SessionExecutionLeaseClaimOutcome) -> Result<SessionExecutionLease, String> {
    match outcome {
        SessionExecutionLeaseClaimOutcome::Acquired(lease) => Ok(lease),
        SessionExecutionLeaseClaimOutcome::Busy { holder } => Err(format!(
            "expected acquired lease, observed busy holder {} fencing {}",
            holder.owner.owner_id, holder.fencing_token
        )),
    }
}

fn claim_outcome_summary(outcome: &SessionExecutionLeaseClaimOutcome) -> Value {
    match outcome {
        SessionExecutionLeaseClaimOutcome::Acquired(lease) => json!({
            "outcome": "acquired",
            "lease": lease_summary(lease),
        }),
        SessionExecutionLeaseClaimOutcome::Busy { holder } => json!({
            "outcome": "busy",
            "holder": lease_summary(holder),
        }),
    }
}

fn lease_summary(lease: &SessionExecutionLease) -> Value {
    json!({
        "session_id": lease.session_id,
        "owner_id": lease.owner.owner_id,
        "incarnation_id": lease.owner.incarnation_id,
        "fencing_token": lease.fencing_token,
        "lease_token_present": !lease.lease_token.is_empty(),
        "expires_at_epoch_ms": lease.expires_at_epoch_ms,
    })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn backend_contention_report_runs_sqlite_and_records_artifact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = super::run_backend_contention_report(tmp.path())
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
                    && scenario.operations.len() >= 6)
        );
        assert!(report.report_path.exists());
        let body = std::fs::read_to_string(report.report_path).expect("report body");
        assert!(body.contains("runtime-persistence.competing-first-claim"));
        assert!(body.contains("runtime-persistence.dead-owner-reclaim"));
        assert!(body.contains("runtime-persistence.stale-head-transaction-rejected"));
        assert!(body.contains("runtime-persistence.idempotent-retry-and-stale-write-conflict"));
    }
}
