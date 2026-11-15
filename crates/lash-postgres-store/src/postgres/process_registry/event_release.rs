//! Host release of a retained process's event prefix (FIG-3482), the
//! PostgreSQL half.
//!
//! The release locks the process row, as every append does, so a release and
//! an append of the same process serialize: the horizon never passes an event
//! a concurrent append is still writing.

use super::*;

/// The highest sequence `process_id` released, `0` when it released none.
pub(crate) async fn released_through_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &ProcessId,
) -> Result<u64, PluginError> {
    crate::observed_sql::query_scalar::<_, i64>(
        process_sql().event_horizon.select_released_through.sql(),
    )
    .bind(process_id.as_str())
    .fetch_optional(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(plugin_sqlx_error)?
    .map(|released| plugin_u64_from_sql("ProcessEventHorizon", "released_through", released))
    .transpose()
    .map(Option::unwrap_or_default)
}

pub(super) async fn release_process_events(
    registry: &PostgresProcessRegistry,
    process_id: &ProcessId,
    through: u64,
) -> Result<lash_execution_contracts::process::ProcessEventRelease, PluginError> {
    let mut tx = begin_guarded(&registry.pool, &registry.fence)
        .await
        .map_err(plugin_store_error)?;
    let record = require_process_tx(&mut tx, process_id).await?;
    let previous = released_through_tx(&mut tx, process_id).await?;
    let target = through.min(record.last_event_sequence);
    if target <= previous {
        tx.rollback().await.map_err(plugin_sqlx_error)?;
        return Ok(lash_execution_contracts::process::ProcessEventRelease {
            released_through: previous,
            released_events: 0,
        });
    }
    let target_bound = clamp_sequence_bound(target);
    let mut after = previous;
    let mut released_events = 0_u64;
    loop {
        let row = crate::observed_sql::query(process_sql().event.page_release.sql())
            .bind(process_id.as_str())
            .bind(clamp_sequence_bound(after))
            .bind(target_bound)
            .bind(i64::from(
                registry.pools.maintenance.process_event_release_page_rows,
            ))
            .fetch_one(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(plugin_sqlx_error)?;
        let Some(end) = row.get::<Option<i64>, _>(0) else {
            break;
        };
        let count: i64 = row.get(1);
        for sql in [
            process_sql().event.release.sql(),
            process_sql().event.release_packed.sql(),
        ] {
            crate::observed_sql::query(sql)
                .bind(process_id.as_str())
                .bind(clamp_sequence_bound(after))
                .bind(end)
                .bind(lash_execution_contracts::process::ProcessEventKind::CancelRequested.as_str())
                .execute(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(plugin_sqlx_error)?;
        }
        released_events += plugin_u64_from_sql("ProcessEvent", "count", count)?;
        after = plugin_u64_from_sql("ProcessEvent", "sequence", end)?;
    }
    crate::observed_sql::query(process_sql().event_horizon.upsert.sql())
        .bind(process_id.as_str())
        .bind(target_bound)
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(plugin_sqlx_error)?;
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(lash_execution_contracts::process::ProcessEventRelease {
        released_through: target,
        released_events,
    })
}
