//! Process-retention laws: the live-status contract and the SQL literals
//! that spell it.

use super::*;

/// Seed one process into `waiting` and assert the retention contract: live rows
/// are listed as non-terminal and are never prune candidates.
async fn assert_waiting_process_is_live_not_prunable(
    registry: &dyn lash_execution_contracts::process::ConformanceProcessRegistry,
) {
    let process_id = &registry
        .register_process(lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::process::ProcessInput::Engine {
                kind: "retention-test".to_string(),
                payload: serde_json::Value::Null,
                launch: lash_execution_contracts::process::EngineLaunch::new(
                    lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                    None,
                ),
            },
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .expect("register waiting retention process")
        .id;
    let wait = lash_execution_contracts::process::WaitState {
        since_ms: 1,
        kind: lash_execution_contracts::process::WaitKind::Call {
            resume_at_ms: None,
            call_id: lash_protocol_types::ToolCallId::fixture("retention-call"),
            tool_id: lash_vocabulary::tool_contract::ToolId::new("retention"),
        },
        site: None,
    };
    registry
        .append_event_for_testing(
            process_id,
            lash_execution_contracts::process::ProcessEventAppendRequest::wait_entered(
                process_id, &wait,
            ),
        )
        .await
        .expect("enter wait");
    let waiting = registry
        .get_process(process_id)
        .await
        .expect("read the waiting process")
        .expect("the waiting process is registered");
    assert_eq!(
        waiting.status().label(),
        "waiting",
        "the wait must land in the persisted status label the retention SQL reads"
    );
    assert!(!waiting.is_terminal(), "a waiting process is not terminal");

    let live = registry
        .list_non_terminal_processes_page(
            std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
            None,
        )
        .await
        .expect("list non-terminal processes")
        .records;
    assert!(
        live.iter().any(|record| record.id == process_id),
        "a waiting process must be listed as live"
    );

    let report = registry
        .prune_terminal_processes(
            u64::MAX,
            None,
            lash_execution_contracts::process::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune terminal processes");
    assert_eq!(
        report.pruned_processes, 0,
        "a waiting process must never be a prune candidate, whatever the cutoff"
    );
    assert!(
        registry
            .get_process(process_id)
            .await
            .expect("read waiting retention process")
            .is_some(),
        "the waiting process row must survive the prune"
    );
}

/// A waiting process is live, not prunable. The PostgreSQL half of
/// `sqlite_waiting_processes_are_live_not_prunable`: both backends generate the
/// live predicate from `ProcessStatus`, so both need the behavioural referee
/// that the generated fragment lands where the query means it to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_waiting_processes_are_live_not_prunable_when_configured() {
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("skipping PostgreSQL waiting-retention regression: database URL is not set");
        return;
    };
    reset(storage.pool()).await;
    lash_execution_contracts::testing::process_execution_env_fixture(&storage.process_env_store())
        .await;
    let registry = storage.process_registry();
    assert_waiting_process_is_live_not_prunable(&registry).await;
}
