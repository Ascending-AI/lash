use super::*;
use lash_sansio::ProcessId;

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
        process_sql().process_postgres.list_prunable_terminal.sql(),
        cutoff,
        watermark_change_seq(watermark),
        filter.as_ref(),
    )
    .await
}

pub(super) async fn complete_process_artifact_cleanup(
    registry: &PostgresProcessRegistry,
    process_id: &ProcessId,
    incarnation: lash_core::ProcessIncarnation,
) -> Result<lash_core::ProcessArtifactCleanupAck, PluginError> {
    let (removed, current_incarnation): (bool, Option<i64>) = sqlx::query_as(
        process_sql()
            .cleanup_postgres
            .delete_for_incarnation_reporting_incarnation
            .sql(),
    )
    .bind(process_id.as_str())
    .bind(incarnation.registration_sequence() as i64)
    .fetch_one(&registry.pool)
    .await
    .map_err(plugin_sqlx_error)?;
    let process_ref = lash_core::ProcessRef::new(process_id.clone(), incarnation);
    Ok(match current_incarnation {
        Some(found) => {
            let found = lash_core::ProcessIncarnation::from_registration_sequence(
                plugin_u64_from_sql("ProcessRecord", "incarnation", found)?,
            );
            if found != incarnation {
                lash_core::ProcessArtifactCleanupAck::StaleIncarnation {
                    expected: process_ref,
                    found: lash_core::ProcessRef::new(process_id.clone(), found),
                }
            } else if removed {
                lash_core::ProcessArtifactCleanupAck::Acknowledged { process_ref }
            } else {
                lash_core::ProcessArtifactCleanupAck::Unknown { process_ref }
            }
        }
        None if removed => lash_core::ProcessArtifactCleanupAck::Acknowledged { process_ref },
        None => lash_core::ProcessArtifactCleanupAck::Unknown { process_ref },
    })
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
    filter: Option<lash_core::ProcessListFilter>,
    watermark: lash_core::ProjectionWatermark,
) -> Result<ProcessPruneReport, PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let pruned_at_ms = registry.clock.timestamp_ms() as i64;
    let max_change_seq = watermark_change_seq(watermark);
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    let prunable = select_prunable(
        &mut *tx,
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
            artifact_cleanup_acknowledgements: Vec::new(),
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
