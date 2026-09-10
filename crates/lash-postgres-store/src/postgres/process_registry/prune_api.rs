use super::*;
use lash_sansio::ProcessId;

/// The prune eligibility predicate. The prune appends `FOR UPDATE`; the
/// survey reads it as is.
const PRUNABLE_TERMINAL_SELECT: &str = "SELECT process_id, record_json FROM lash_processes
         WHERE status NOT IN ('running', 'waiting')
           AND updated_at_ms < $1
           AND ($2::BIGINT IS NULL OR change_seq <= $2)
           AND NOT EXISTS (
               SELECT 1 FROM lash_process_wake_deliveries AS delivery
               WHERE delivery.process_id = lash_processes.process_id
                 AND delivery.state IN ('pending', 'enqueuing')
           )
           AND NOT EXISTS (
               SELECT 1 FROM lash_process_parent_end_plans AS plan
               WHERE plan.process_id = lash_processes.process_id
           )
         ORDER BY process_id ASC";

fn prune_terminal_sql() -> String {
    format!("{PRUNABLE_TERMINAL_SELECT}\n         FOR UPDATE")
}

fn watermark_change_seq(watermark: lash_core::ProjectionWatermark) -> Option<i64> {
    match watermark {
        lash_core::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence() as i64),
        lash_core::ProjectionWatermark::NoProjector => None,
    }
}

async fn select_prunable<'c>(
    executor: impl sqlx::PgExecutor<'c>,
    sql: &str,
    cutoff: i64,
    max_change_seq: Option<i64>,
    filter: Option<&lash_core::ProcessListFilter>,
) -> Result<Vec<ProcessId>, PluginError> {
    let rows = sqlx::query(sql)
        .bind(cutoff)
        .bind(max_change_seq)
        .fetch_all(executor)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut prunable = Vec::new();
    for row in rows {
        let process_id: ProcessId = ProcessId::from(row.get::<String, _>(0));
        let record_json: String = row.get(1);
        let record: ProcessRecord =
            serde_json::from_str(&record_json).map_err(process_decode_error)?;
        if filter.is_none_or(|filter| filter.matches_record(&record)) {
            prunable.push(process_id);
        }
    }
    Ok(prunable)
}

/// The survey half of the prune: the same predicate, read without locking or
/// deleting.
pub(super) async fn prunable_terminal_processes(
    registry: &PostgresProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<lash_core::ProcessListFilter>,
    watermark: lash_core::ProjectionWatermark,
) -> Result<Vec<ProcessId>, PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    select_prunable(
        &registry.pool,
        PRUNABLE_TERMINAL_SELECT,
        cutoff,
        watermark_change_seq(watermark),
        filter.as_ref(),
    )
    .await
}

pub(super) async fn prune_terminal_processes(
    registry: &PostgresProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<lash_core::ProcessListFilter>,
    watermark: lash_core::ProjectionWatermark,
) -> Result<ProcessPruneReport, PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let pruned_at_ms = registry.clock.timestamp_ms() as i64;
    let max_change_seq = watermark_change_seq(watermark);
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    let prunable = select_prunable(
        &mut *tx,
        &prune_terminal_sql(),
        cutoff,
        max_change_seq,
        filter.as_ref(),
    )
    .await?;

    if prunable.is_empty() {
        tx.commit().await.map_err(plugin_sqlx_error)?;
        return Ok(ProcessPruneReport {
            pruned_processes: 0,
            pruned_events: 0,
            pruned_trigger_deliveries: 0,
        });
    }

    let process_ids = prunable;
    let session_ids = process_ids
        .iter()
        .flat_map(facade_support::process_runtime_session_ids)
        .collect::<Vec<_>>();
    let blob_reclaim = delete_process_sessions_tx(&mut tx, &session_ids)
        .await
        .map_err(|failure| {
            PluginError::Session(format!(
                "process session blob reclaim {}; partial report: {:?}",
                failure.stop, failure.partial
            ))
        })?;

    let report = prune_process_rows_tx(&mut tx, &process_ids, pruned_at_ms).await?;
    tx.commit().await.map_err(plugin_sqlx_error)?;
    tracing::debug!(
        enumerated_blob_count = blob_reclaim.enumerated_blob_count,
        retained_blob_count = blob_reclaim.retained_blob_count,
        deleted_blob_count = blob_reclaim.deleted_blob_count,
        "process prune reclaimed process-session checkpoint blobs"
    );
    Ok(report)
}

#[cfg(test)]
mod planner_tests {
    use super::*;

    #[tokio::test]
    async fn prune_parent_plan_anti_join_uses_index_order_without_sort() {
        let Some(url) = crate::postgres_test_support::database_url() else {
            return;
        };
        let _lock = crate::postgres_test_support::SharedDatabaseLock::acquire(&url).await;
        let storage = crate::PostgresStorage::connect(&url)
            .await
            .expect("connect planner witness");
        let mut tx = storage.pool().begin().await.expect("begin planner witness");
        // As in the worklist planner witness, remove small-table cost preference.
        // Disable alternative joins to prove the existing btrees can supply merge
        // order directly; a collation mismatch still requires an explicit sort.
        for setting in [
            "SET LOCAL enable_seqscan = off",
            "SET LOCAL enable_bitmapscan = off",
            "SET LOCAL enable_hashjoin = off",
            "SET LOCAL enable_nestloop = off",
        ] {
            sqlx::query(setting)
                .execute(&mut *tx)
                .await
                .expect("set planner witness preference");
        }
        let plan = sqlx::query_scalar::<_, String>(&format!(
            "EXPLAIN (COSTS OFF) {}",
            prune_terminal_sql()
        ))
        .bind(i64::MAX)
        .bind(None::<i64>)
        .fetch_all(&mut *tx)
        .await
        .expect("explain process prune")
        .join(" | ");
        eprintln!("prune anti-join plan: {plan}");
        assert!(
            plan.contains("Merge Anti Join")
                && plan.contains("lash_process_parent_end_plans_pkey")
                && !plan.contains("Sort Key: plan.process_id"),
            "prune parent-plan anti-join must inherit btree order without sorting: {plan}"
        );
        tx.rollback().await.expect("rollback planner witness");
    }
}
