use crate::*;

pub(super) async fn prune_process_rows_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_ids: &[String],
    pruned_at_ms: i64,
) -> Result<ProcessPruneReport, PluginError> {
    // Candidate process rows remain locked from selection through this
    // statement. No status or child-row writer can race the batch, and any
    // tombstone conflict still rolls the entire transaction back as before.
    // Events are deleted explicitly for the report count; the final set-based
    // process delete cascades observers, leases, handovers, and terminal wake
    // deliveries through their existing foreign keys.
    let (pruned_events, pruned_processes) = sqlx::query_as::<_, (i64, i64)>(
        "WITH candidates AS (
             SELECT process_id, ordinality
             FROM unnest($1::TEXT[]) WITH ORDINALITY
                  AS candidate(process_id, ordinality)
         ),
         deleted_events AS (
             DELETE FROM lash_process_events AS event
             USING candidates AS candidate
             WHERE event.process_id = candidate.process_id
             RETURNING event.process_id
         ),
         event_count AS MATERIALIZED (
             SELECT count(*) AS value FROM deleted_events
         ),
         advanced_clock AS (
             UPDATE lash_process_change_clock
             SET current_seq = current_seq + (SELECT count(*) FROM candidates)
             WHERE singleton = TRUE
             RETURNING current_seq
         ),
         inserted_tombstones AS (
             INSERT INTO lash_process_tombstones (
                 process_id, terminal_label, pruned_at_ms, pruned_change_seq
             )
             SELECT candidate.process_id,
                    process.status,
                    $2,
                    clock.current_seq - (SELECT count(*) FROM candidates)
                        + candidate.ordinality
             FROM candidates AS candidate
             JOIN lash_processes AS process USING (process_id)
             CROSS JOIN advanced_clock AS clock
             CROSS JOIN event_count
             WHERE event_count.value >= 0
             ORDER BY candidate.ordinality
             RETURNING process_id
         ),
         deleted_processes AS (
             DELETE FROM lash_processes AS process
             USING candidates AS candidate,
                   (SELECT count(*) FROM inserted_tombstones) AS tombstones
             WHERE process.process_id = candidate.process_id
               AND tombstones.count = (SELECT count(*) FROM candidates)
             RETURNING process.process_id
         )
         SELECT (SELECT value FROM event_count),
                (SELECT count(*) FROM deleted_processes)",
    )
    .bind(process_ids)
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let process_id = format!("prune-rollback:{}", uuid::Uuid::new_v4());
        let ghost_id = format!("prune-rollback-ghost:{}", uuid::Uuid::new_v4());
        let registry = storage.process_registry();
        registry
            .register_process(ProcessRegistration::new(
                &process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            ))
            .await
            .expect("register rollback process");
        registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::Success {
                    value: serde_json::Value::Null,
                    control: None,
                },
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete rollback process");
        let events_before = serde_json::to_value(
            registry
                .events_after(&process_id, 0)
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
                    .events_after(&process_id, 0)
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
        .bind(&process_id)
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
            .bind(&process_id)
            .execute(storage.pool())
            .await
            .expect("clean rollback process");
    }
}
