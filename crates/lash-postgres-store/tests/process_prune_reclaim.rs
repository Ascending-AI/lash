//! Postgres proof that the process-prune delete path obeys the tombstone-reclaim
//! law: the batch delete drains rows orphaned under sessions outside the batch,
//! pruned process-session ids join the deleted set, and a later delete drains
//! rows orphaned under them.
//!
//! This lives in its own test target rather than the conformance suite, which is
//! at its line budget.

use std::sync::Arc;

use lash_core_execution::{
    ProcessLifecycle as _, ProcessRegistrar as _, ProcessRegistry, ProcessRetention as _,
    SessionStoreFactory,
};
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
           AND tablename NOT IN ('lash_schema_versions', 'lash_catalog_identity')
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_cleanup_evidence_survives_reopen_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process cleanup recovery: database URL is not set");
        return;
    };
    reset(&storage).await;
    let registry = storage.process_registry();
    let registered = registry
        .register_process(
            lash_core_execution::ProcessRegistration::new(
                lash_core_execution::ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: serde_json::json!({"module_ref": "module-postgres"}),
                },
                lash_core_execution::RecoveryContract::Rerunnable,
                lash_core_execution::ProcessProvenance::host(),
                lash_core_execution::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(
                lash_core_execution::ProcessExecutionEnvRef::new("process-env:postgres-cleanup"),
            )),
        )
        .await
        .expect("register cleanup process");
    registry
        .complete_process(
            &registered.id,
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_core_execution::ProcessCompletionAuthority::workflow_key("postgres-prune-cleanup"),
        )
        .await
        .expect("complete cleanup process");
    registry
        .prune_terminal_processes(
            u64::MAX,
            None,
            lash_core_execution::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune with cleanup evidence");
    drop(registry);

    let reopened = storage.process_registry();
    let pending = reopened
        .pending_process_artifact_cleanup()
        .await
        .expect("read cleanup evidence after reopen");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].process_id, registered.id);
    assert_eq!(pending[0].env_ref, registered.env_ref);
    assert_eq!(pending[0].input, registered.input);
    let acknowledgement = reopened
        .complete_process_artifact_cleanup(&registered.id)
        .await
        .expect("ack cleanup evidence");
    assert_eq!(
        acknowledgement,
        lash_core_execution::ProcessArtifactCleanupAck::Acknowledged {
            process_id: registered.id.clone(),
        }
    );
    assert!(
        reopened
            .pending_process_artifact_cleanup()
            .await
            .expect("read acknowledged cleanup")
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_removes_queued_run_admission_and_members() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process-prune queued-run law: database URL is not set");
        return;
    };
    reset(&storage).await;
    let registry = storage.process_registry();
    let process = registry
        .register_process(lash_core_execution::ProcessRegistration::new(
            lash_core_execution::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core_execution::RecoveryContract::ExternallyOwned,
            lash_core_execution::ProcessProvenance::host(),
            lash_core_execution::Lifetime::Detached,
        ))
        .await
        .expect("register process");
    let session_id =
        lash_core_execution::facade_support::process_runtime_session_ids(&process.id)[0].clone();
    let factory = storage.session_store_factory_with_shared_process_registry();
    let store = factory
        .create_store(&lash_core_execution::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core_execution::SessionRelation::default(),
            policy: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
            ),
        })
        .await
        .expect("create process-owned session");
    let input = store
        .enqueue_pending_turn_input(lash_core_execution::PendingTurnInputDraft::new(
            &session_id,
            lash_core_execution::TurnInputIngress::NextTurn,
            lash_core_execution::TurnInput::text("queued before prune"),
        ))
        .await
        .expect("enqueue pending input");
    let owner =
        lash_core_execution::LeaseOwnerIdentity::opaque("prune-run-owner", "prune-run-incarnation");
    let lease = store
        .try_claim_session_execution_lease(&session_id, &owner, "prune-run-executor", 60_000)
        .await
        .expect("claim lane")
        .acquired()
        .expect("lane is free");
    let admission = store
        .begin_or_resume_queued_run(
            &lease.authority(),
            lash_core_execution::store::BeginQueuedRun {
                session_id: session_id.clone(),
                identity: None,
                request: lash_core_execution::store::QueuedRunRequest::Automatic,
                configuration: lash_core_execution::PersistedSessionConfig::new(
                    lash_core_execution::TurnBudget::Unbounded,
                ),
                expected_head_revision: 0,
                initial_turn_index: 1,
                generation: None,
            },
        )
        .await
        .expect("admit queued run");
    let selected = store
        .select_queued_run(
            &lease.authority(),
            &admission.scope,
            &owner,
            64,
            &admission.configuration,
            lash_core_execution::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("freeze queued input");
    assert_eq!(
        selected.admission.members,
        Some(vec![lash_core_execution::store::QueuedRunMember::Input(
            input.input_id
        )])
    );
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .expect("release lane before prune");
    for table in ["lash_queued_runs", "lash_queued_run_members"] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE session_id = $1"
        ))
        .bind(session_id.as_str())
        .fetch_one(storage.pool())
        .await
        .expect("count admission rows before prune");
        assert!(count > 0, "{table} contains the process-owned admission");
    }
    let terminal = registry
        .complete_process(
            &process.id,
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_core_execution::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete process");
    registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            lash_core_execution::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune process-owned session");
    for table in ["lash_queued_runs", "lash_queued_run_members"] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE session_id = $1"
        ))
        .bind(session_id.as_str())
        .fetch_one(storage.pool())
        .await
        .expect("count admission rows after prune");
        assert_eq!(
            count, 0,
            "{table} must not retain a deleted process session"
        );
    }
}
