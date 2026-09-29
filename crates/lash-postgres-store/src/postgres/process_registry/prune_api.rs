use super::*;
use lash_sansio::ProcessId;

fn watermark_change_seq(watermark: lash_core_execution::ProjectionWatermark) -> Option<i64> {
    match watermark {
        lash_core_execution::ProjectionWatermark::UpTo(cursor) => {
            Some(cursor.store_sequence() as i64)
        }
        lash_core_execution::ProjectionWatermark::NoProjector => None,
    }
}

async fn select_prunable<'c>(
    executor: impl sqlx::PgExecutor<'c>,
    sql: &str,
    cutoff: i64,
    max_change_seq: Option<i64>,
    filter: Option<&lash_core_execution::ProcessListFilter>,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let rows = sqlx::query(sql)
        .bind(cutoff)
        .bind(max_change_seq)
        .fetch_all(executor)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut prunable = Vec::new();
    for row in rows {
        let record_json: String = row.get(0);
        let record: ProcessRecord =
            serde_json::from_str(&record_json).map_err(process_decode_error)?;
        if filter.is_none_or(|filter| filter.matches_record(&record)) {
            prunable.push(record);
        }
    }
    Ok(prunable)
}

/// The survey half of the prune: the same predicate, read without locking or
/// deleting.
pub(super) async fn prunable_terminal_processes(
    registry: &PostgresProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<lash_core_execution::ProcessListFilter>,
    watermark: lash_core_execution::ProjectionWatermark,
) -> Result<Vec<ProcessId>, PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    Ok(select_prunable(
        &registry.pool,
        process_sql().process_postgres.list_prunable_terminal.sql(),
        cutoff,
        watermark_change_seq(watermark),
        filter.as_ref(),
    )
    .await?
    .into_iter()
    .map(|record| record.id)
    .collect())
}

/// Settled ledger rows the retention horizon has passed and no live child
/// still names.
///
/// The row has to outlive its scope — it is what refuses a late `Cancel`
/// child — so it is reclaimed by retention rather than by the sweep that
/// settles it. Past the same cutoff the process rows themselves are pruned
/// under, a settled scope with no live child can no longer be the parent of
/// anything lash will act on, so keeping the row would only grow the table by
/// one row per committed turn forever. A `caller_departed` child is not live
/// by construction: lash may never act on such a row, so it can never need a
/// parent-end cancel.
///
/// Reclaiming a row lifts the fence it was: once it is gone, a `Cancel` child
/// registering under that scope is admitted again rather than refused
/// `ParentEnded`. That is the deliberate trade — past the retention horizon the
/// scope is beyond anything lash reasons about, and a registration arriving
/// there is a new fact, not a late one.
async fn reclaim_settled_parent_end_plans_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cutoff: i64,
) -> Result<u64, PluginError> {
    sqlx::query(process_sql().plan_postgres.delete_reclaimable.sql())
        .bind(cutoff)
        .execute(&mut **tx)
        .await
        .map(|done| done.rows_affected())
        .map_err(plugin_sqlx_error)
}

pub(super) async fn prune_terminal_processes(
    registry: &PostgresProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<lash_core_execution::ProcessListFilter>,
    watermark: lash_core_execution::ProjectionWatermark,
) -> Result<ProcessPruneReport, PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let pruned_at_ms = crate::support::clamp_epoch_ms(registry.clock.timestamp_ms());
    let max_change_seq = watermark_change_seq(watermark);
    let mut tx = begin_guarded(&registry.pool, &registry.fence)
        .await
        .map_err(plugin_store_error)?;
    let prunable = select_prunable(
        &mut **tx,
        process_sql()
            .process_postgres
            .list_prunable_terminal_for_update
            .sql(),
        cutoff,
        max_change_seq,
        filter.as_ref(),
    )
    .await?;

    let reclaimed_plans = reclaim_settled_parent_end_plans_tx(&mut tx, cutoff).await?;
    if reclaimed_plans > 0 {
        tracing::debug!(
            reclaimed_plans,
            "retention reclaimed settled parent-end ledger rows"
        );
    }

    if prunable.is_empty() {
        tx.commit().await.map_err(plugin_sqlx_error)?;
        return Ok(ProcessPruneReport {
            pruned_processes: 0,
            pruned_events: 0,
            pruned_trigger_deliveries: 0,
        });
    }

    // The candidates stay locked from selection through the prune. The same
    // transaction fences each pruned process record and writes its cleanup.
    let process_ids = prunable
        .iter()
        .map(|record| record.id.clone())
        .collect::<Vec<_>>();
    let session_ids = process_ids
        .iter()
        .flat_map(facade_support::process_runtime_session_ids)
        .collect::<Vec<_>>();
    let blob_reclaim = delete_process_sessions_tx(&mut tx, &session_ids, registry.fence.fleet())
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
