//! Fleet discovery retains the feed's committed sequencing-before-read contract.
use super::*;
use lash_core_execution::{ProcessChangeBounds, ProcessRosterCursor, ProcessRosterRecords};

async fn bounds_tx(tx: &mut sqlx::PgConnection) -> Result<ProcessChangeBounds, PluginError> {
    let row = sqlx::query(process_sql().clock_postgres.select_bounds_for_share.sql())
        .fetch_one(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(ProcessChangeBounds {
        current: ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChangeClock",
            "current_seq",
            row.get(0),
        )?),
        retained_after: ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChangeClock",
            "tombstone_compaction_horizon",
            row.get(1),
        )?),
    })
}

async fn sequence(registry: &PostgresProcessRegistry) -> Result<(), PluginError> {
    crate::change_feed::sequence_before_read(
        &registry.pool,
        &registry.fence,
        crate::change_feed::Feed::Processes,
    )
    .await
    .map_err(plugin_store_error)
}

pub(super) async fn bounds(
    registry: &PostgresProcessRegistry,
) -> Result<ProcessChangeBounds, PluginError> {
    sequence(registry).await?;
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    let bounds = bounds_tx(&mut tx).await?;
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(bounds)
}

pub(super) async fn page(
    registry: &PostgresProcessRegistry,
    filter: &lash_core_execution::ProcessListFilter,
    limit: std::num::NonZeroUsize,
    cursor: Option<ProcessRosterCursor>,
) -> Result<ProcessRosterRecords, PluginError> {
    sequence(registry).await?;
    let limit = limit
        .get()
        .min(lash_core_execution::MAX_PROCESS_ROSTER_PAGE_SIZE);
    let store = format!("postgres:{}", registry.catalog_id);
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    // This share lock keeps compaction and sequencing behind the page. Saves
    // that race it remain unsequenced and therefore belong after the scan fence.
    let bounds = bounds_tx(&mut tx).await?;
    if let Some(cursor) = &cursor {
        cursor.validate(&store, filter, bounds)?;
    }
    let through = match &cursor {
        Some(cursor) => Some(cursor.through().clone()),
        None => sqlx::query_scalar::<_, Option<String>>(
            process_sql().process.select_max_process_id.sql(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?
        .map(|id| crate::stored_process_id(&id))
        .transpose()?,
    };
    let mut candidates = Vec::new();
    if let Some(through) = &through {
        let query = match &cursor {
            Some(cursor) => sqlx::query_scalar::<_, String>(
                process_sql().process.list_next_roster_candidates.sql(),
            )
            .bind(through.as_str())
            .bind(cursor.after().as_str())
            .bind((limit + 1) as i64),
            None => sqlx::query_scalar::<_, String>(
                process_sql().process.list_first_roster_candidates.sql(),
            )
            .bind(through.as_str())
            .bind((limit + 1) as i64),
        };
        let rows = query.fetch_all(&mut *tx).await.map_err(plugin_sqlx_error)?;
        for json in rows {
            candidates.push(serde_json::from_str(&json).map_err(process_decode_error)?);
        }
    }
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(ProcessRosterRecords::from_candidates(
        store,
        filter,
        limit,
        cursor.as_ref(),
        through,
        bounds,
        candidates,
    ))
}
