//! Catalog-wide reads: the turn change feed's sequence and session terminals.

use super::*;

pub(crate) fn next_turn_change_sequence(conn: &Connection) -> Result<i64, StoreError> {
    conn.query_row(
        crate::session_sql::session_sql()
            .turn_commits
            .next_change_seq
            .sql(),
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(sqlite_error)?
    .ok_or(StoreError::MonotonicCounterOverflow {
        counter: "turn_change_sequence",
        current: i64::MAX as u64,
    })
}

pub(crate) fn record_session_terminal(
    conn: &Connection,
    session_id: &SessionId,
    fault_json: Option<&str>,
    at_ms: i64,
) -> Result<(), StoreError> {
    let sequence = next_turn_change_sequence(conn)?;
    conn.execute(
        crate::session_sql::session_sql()
            .turn_commits
            .insert_session_terminal
            .sql(),
        params![sequence, session_id.as_str(), fault_json, at_ms],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

impl SqliteStore {
    pub(crate) async fn read_turn_changes(
        &self,
        after: lash_core_execution::store::TurnChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<lash_core_execution::store::TurnChangePage, StoreError> {
        let fleet = self.conn.fleet();
        self.read_connection()
            .read(move |conn| {
                // The clock and rows share one read snapshot, including compaction.
                Ok((|| {
                    let sql = &crate::session_sql::session_sql().turn_commits;
                    let (current, horizon): (i64, i64) = conn
                        .query_row(sql.change_clock.sql(), [], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })
                        .map_err(sqlite_error)?;
                    let current =
                        u64::try_from(current).map_err(|_| StoreError::StoredDataCorrupt {
                            record_kind: "TurnChangeClock",
                            message: "negative current sequence".to_owned(),
                        })?;
                    let horizon =
                        u64::try_from(horizon).map_err(|_| StoreError::StoredDataCorrupt {
                            record_kind: "TurnChangeClock",
                            message: "negative retention horizon".to_owned(),
                        })?;
                    after.check(current, horizon)?;
                    let mut stmt = conn
                        .prepare_cached(sql.changes_after.sql())
                        .map_err(sqlite_error)?;
                    let rows = stmt
                        .query_map(
                            params![
                                after.store_sequence() as i64,
                                i64::try_from(limit.get()).unwrap_or(i64::MAX)
                            ],
                            |row| {
                                Ok((
                                    row.get::<_, i64>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, Option<String>>(2)?,
                                    row.get::<_, Option<String>>(3)?,
                                    row.get::<_, Option<String>>(4)?,
                                    row.get::<_, i64>(5)?,
                                ))
                            },
                        )
                        .map_err(sqlite_error)?;
                    let mut changes = Vec::new();
                    for row in rows {
                        let (seq, session, operation, payload, code, at_ms) =
                            row.map_err(sqlite_error)?;
                        changes.push(lash_core_execution::store::TurnChange::from_stored(
                            seq, session, operation, payload, code, at_ms, fleet,
                        )?);
                    }
                    let next = changes.last().map_or(
                        lash_core_execution::store::TurnChangeCursor::from_store_sequence(current),
                        |change| change.cursor,
                    );
                    Ok(lash_core_execution::store::TurnChangePage {
                        changes,
                        next,
                        retained_after:
                            lash_core_execution::store::TurnChangeCursor::from_store_sequence(
                                horizon,
                            ),
                    })
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}
