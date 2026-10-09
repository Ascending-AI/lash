//! Fleet scans capture their change fence before reading the first keyset.
use super::*;
use lash_core_execution::{ProcessChangeBounds, ProcessRosterCursor, ProcessRosterRecords};

pub(super) fn bounds_conn(
    conn: &Connection,
) -> Result<ProcessChangeBounds, lash_core_execution::PluginError> {
    let current = conn
        .query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(process_sqlite_error)?;
    let horizon = conn
        .query_row(
            process_sql().clock_sqlite.select_compaction_horizon.sql(),
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(process_sqlite_error)?;
    Ok(ProcessChangeBounds {
        current: ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChangeClock",
            "current_seq",
            current,
        )?),
        retained_after: ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChangeClock",
            "tombstone_compaction_horizon",
            horizon,
        )?),
    })
}

pub(super) async fn bounds(
    registry: &SqliteProcessRegistry,
) -> Result<ProcessChangeBounds, lash_core_execution::PluginError> {
    registry
        .conn
        .call(|conn| {
            Ok((|| {
                let tx = conn.unchecked_transaction().map_err(process_sqlite_error)?;
                let bounds = bounds_conn(&tx)?;
                tx.commit().map_err(process_sqlite_error)?;
                Ok(bounds)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn page(
    registry: &SqliteProcessRegistry,
    filter: &ProcessListFilter,
    limit: std::num::NonZeroUsize,
    cursor: Option<ProcessRosterCursor>,
) -> Result<ProcessRosterRecords, lash_core_execution::PluginError> {
    let store = format!("sqlite:{}", registry.location.target().canonical_name());
    let filter = filter.clone();
    let limit = limit
        .get()
        .min(lash_core_execution::MAX_PROCESS_ROSTER_PAGE_SIZE);
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let tx = conn.unchecked_transaction().map_err(process_sqlite_error)?;
                let bounds = bounds_conn(&tx)?;
                if let Some(cursor) = &cursor {
                    cursor.validate(&store, &filter, bounds)?;
                }
                let through = match &cursor {
                    Some(cursor) => Some(cursor.through().clone()),
                    None => tx
                        .query_row(
                            process_sql().process.select_max_process_id.sql(),
                            [],
                            |row| row.get::<_, Option<String>>(0),
                        )
                        .map_err(process_sqlite_error)?
                        .map(|id| crate::stored_process_id(&id))
                        .transpose()?,
                };
                let mut candidates = Vec::new();
                if let Some(through) = &through {
                    let (sql, values) = match &cursor {
                        Some(cursor) => (
                            process_sql().process.list_next_roster_candidates.sql(),
                            vec![
                                rusqlite::types::Value::Text(through.as_str().to_owned()),
                                rusqlite::types::Value::Text(cursor.after().as_str().to_owned()),
                                rusqlite::types::Value::Integer((limit + 1) as i64),
                            ],
                        ),
                        None => (
                            process_sql().process.list_first_roster_candidates.sql(),
                            vec![
                                rusqlite::types::Value::Text(through.as_str().to_owned()),
                                rusqlite::types::Value::Integer((limit + 1) as i64),
                            ],
                        ),
                    };
                    let mut stmt = tx.prepare_cached(sql).map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values), |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(process_sqlite_error)?;
                    for row in rows {
                        candidates.push(
                            serde_json::from_str(&row.map_err(process_sqlite_error)?)
                                .map_err(process_decode_error)?,
                        );
                    }
                }
                tx.commit().map_err(process_sqlite_error)?;
                Ok(ProcessRosterRecords::from_candidates(
                    store,
                    &filter,
                    limit,
                    cursor.as_ref(),
                    through,
                    bounds,
                    candidates,
                ))
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}
