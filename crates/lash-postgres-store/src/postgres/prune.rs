use crate::*;
use lash_sansio::ProcessId;

pub(super) async fn prune_process_rows_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_ids: &[ProcessId],
    pruned_at_ms: i64,
) -> Result<ProcessPruneReport, PluginError> {
    // Candidate process rows remain locked from selection through this
    // statement. No status or child-row writer can race the batch, and any
    // tombstone conflict still rolls the entire transaction back as before.
    // Events are deleted explicitly for the report count; the final set-based
    // process delete cascades observers, leases, handovers, and terminal wake
    // deliveries through their existing foreign keys.
    let (pruned_events, pruned_processes) = sqlx::query_as::<_, (i64, i64)>(
        crate::process_sql::process_sql()
            .registry_postgres
            .prune_rows
            .sql(),
    )
    .bind(
        process_ids
            .iter()
            .map(ProcessId::as_str)
            .collect::<Vec<_>>(),
    )
    .bind(pruned_at_ms)
    .fetch_one(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;

    if pruned_processes != process_ids.len() as i64 {
        return Err(PluginError::Session(format!(
            "process prune candidate/tombstone divergence: expected {}, deleted {pruned_processes}",
            process_ids.len()
        )));
    }

    Ok(ProcessPruneReport {
        pruned_processes: pruned_processes as usize,
        pruned_events: pruned_events as usize,
        pruned_trigger_deliveries: 0,
        artifact_cleanup_acknowledgements: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_execution::{
        ProcessEventLogTestSupport as _, ProcessLifecycle as _, ProcessQuery as _,
        ProcessRegistrar as _,
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn candidate_tombstone_divergence_rolls_back_all_prune_mutations() {
        let Some(database_url) = crate::postgres_test_support::database_url() else {
            eprintln!("skipping Postgres prune rollback proof: database URL is not set");
            return;
        };
        let _database_lock =
            crate::postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect prune rollback storage");
        let ghost_id = lash_core_execution::mint_process_id();
        let registry = storage.process_registry();
        let process_id = registry
            .register_process(ProcessRegistration::new(
                lash_core_execution::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core_execution::RecoveryContract::ExternallyOwned,
                lash_core_execution::ProcessProvenance::host(),
                lash_core_execution::ProcessLifecyclePolicy::new(
                    lash_core_execution::ParentScope::Host,
                    lash_core_execution::OnParentEnd::Abandon,
                ),
            ))
            .await
            .expect("register rollback process")
            .id;
        registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::from_tool_output(lash_core_execution::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                lash_core_execution::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete rollback process");
        let events_before = serde_json::to_value(
            registry
                .full_event_window(&process_id, 0)
                .await
                .expect("read events before divergent prune"),
        )
        .expect("encode events before divergent prune");
        let clock_before: i64 = sqlx::query_scalar(
            "SELECT current_seq FROM lash_process_change_clock WHERE singleton = TRUE",
        )
        .fetch_one(storage.pool())
        .await
        .expect("read process clock before divergent prune");

        let mut tx = storage.pool().begin().await.expect("begin divergent prune");
        let error = prune_process_rows_tx(&mut tx, &[process_id.clone(), ghost_id], 123_456)
            .await
            .expect_err("candidate/tombstone divergence must abort the prune transaction");
        assert!(
            error.to_string().contains("candidate/tombstone divergence"),
            "unexpected divergence error: {error}"
        );
        tx.rollback().await.expect("roll back divergent prune");

        assert!(
            registry
                .get_process(&process_id)
                .await
                .expect("read process after divergent prune")
                .is_some(),
            "divergent prune must retain the process row"
        );
        assert_eq!(
            serde_json::to_value(
                registry
                    .full_event_window(&process_id, 0)
                    .await
                    .expect("read events after divergent prune"),
            )
            .expect("encode events after divergent prune"),
            events_before,
            "divergent prune must retain the event journal"
        );
        let tombstone_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM lash_process_tombstones WHERE process_id = $1",
        )
        .bind(process_id.as_str())
        .fetch_one(storage.pool())
        .await
        .expect("count tombstones after divergent prune");
        assert_eq!(
            tombstone_count, 0,
            "divergent prune must not leave a tombstone"
        );
        let clock_after: i64 = sqlx::query_scalar(
            "SELECT current_seq FROM lash_process_change_clock WHERE singleton = TRUE",
        )
        .fetch_one(storage.pool())
        .await
        .expect("read process clock after divergent prune");
        assert_eq!(
            clock_after, clock_before,
            "divergent prune must roll back the clock"
        );

        sqlx::query("DELETE FROM lash_processes WHERE process_id = $1")
            .bind(process_id.as_str())
            .execute(storage.pool())
            .await
            .expect("clean rollback process");
    }
}
