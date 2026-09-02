use super::*;

pub(super) async fn prune_terminal_processes(
    registry: &PostgresProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<lash_core::ProcessListFilter>,
    watermark: lash_core::ProjectionWatermark,
) -> Result<ProcessPruneReport, PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let pruned_at_ms = registry.clock.timestamp_ms() as i64;
    let max_change_seq = match watermark {
        lash_core::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence() as i64),
        lash_core::ProjectionWatermark::NoProjector => None,
    };
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    let rows = sqlx::query(
        "SELECT process_id, record_json FROM lash_processes
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
         ORDER BY process_id ASC
         FOR UPDATE",
    )
    .bind(cutoff)
    .bind(max_change_seq)
    .fetch_all(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let mut prunable = Vec::new();
    for row in rows {
        let process_id: String = row.get(0);
        let record_json: String = row.get(1);
        let record: ProcessRecord =
            serde_json::from_str(&record_json).map_err(process_decode_error)?;
        if filter
            .as_ref()
            .is_none_or(|filter| filter.matches_record(&record))
        {
            prunable.push(process_id);
        }
    }

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
        .flat_map(|process_id| facade_support::process_runtime_session_ids(process_id))
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
