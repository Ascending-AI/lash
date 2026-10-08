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
    sqlx::query_scalar::<_, i64>(process_sql().event_horizon.select_released_through.sql())
        .bind(process_id.as_str())
        .fetch_optional(&mut **tx)
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
) -> Result<lash_core_execution::ProcessEventRelease, PluginError> {
    let mut tx = begin_guarded(&registry.pool, &registry.fence)
        .await
        .map_err(plugin_store_error)?;
    let record = require_process_tx(&mut tx, process_id).await?;
    let previous = released_through_tx(&mut tx, process_id).await?;
    let target = through.min(record.last_event_sequence);
    if target <= previous {
        tx.rollback().await.map_err(plugin_sqlx_error)?;
        return Ok(lash_core_execution::ProcessEventRelease {
            released_through: previous,
            released_events: 0,
        });
    }
    let target_bound = clamp_sequence_bound(target);
    let mut after = previous;
    let mut released_events = 0_u64;
    loop {
        let rows = sqlx::query(process_sql().event.page_release.sql())
            .bind(process_id.as_str())
            .bind(clamp_sequence_bound(after))
            .bind(target_bound)
            .bind(i64::from(
                registry.pools.maintenance.process_event_release_page_rows,
            ))
            .fetch_all(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let sequence: i64 = row.get(0);
            let event: ProcessEvent =
                serde_json::from_str(&row.get::<String, _>(1)).map_err(process_decode_error)?;
            if let Some(digest) =
                lash_core_execution::runtime::release_process_event_payload(&event)
            {
                let released = lash_core_execution::ReleasedProcessEvent {
                    process_id: event.process_id.clone(),
                    sequence: event.sequence,
                    event_type: event.fact.event_type().to_owned(),
                    invocation: event.invocation,
                    trace_cause: event.trace_cause,
                    occurred_at: event.occurred_at,
                };
                sqlx::query(process_sql().event.release.sql())
                    .bind(process_id.as_str())
                    .bind(sequence)
                    .bind(serde_json::to_string(&released).map_err(process_decode_error)?)
                    .bind(digest)
                    .execute(&mut **tx)
                    .await
                    .map_err(plugin_sqlx_error)?;
            }
            released_events += 1;
            after = plugin_u64_from_sql("ProcessEvent", "sequence", sequence)?;
        }
    }
    sqlx::query(process_sql().event_horizon.upsert.sql())
        .bind(process_id.as_str())
        .bind(target_bound)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(lash_core_execution::ProcessEventRelease {
        released_through: target,
        released_events,
    })
}
