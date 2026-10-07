//! [`SessionFaultStore`] for [`SqliteStore`]: a session's standing fault
//! (ADR 0109 §9), on its `session_meta` row.

use super::*;
use lash_core_execution::store::{SessionFault, SessionFaultRecord, SessionFaultStore};

/// The session's standing fault (ADR 0109 §9), read inside the caller's
/// transaction.
fn session_fault_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<SessionFault>, StoreError> {
    conn.query_row(
        session_sql().meta.select_fault.sql(),
        params![session_id.as_str()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )
    .optional()
    .map_err(sqlite_error)?
    .map(|(json, at_ms)| SessionFault::from_stored(session_id.clone(), &json, at_ms))
    .transpose()
}

fn commit<T>(outcome: Result<T, StoreError>) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match outcome {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}

#[async_trait::async_trait]
impl SessionFaultStore for SqliteStore {
    async fn record_session_fault(
        &self,
        session_id: &SessionId,
        record: &SessionFaultRecord,
        at_ms: u64,
    ) -> Result<Option<SessionFault>, StoreError> {
        let session_id = session_id.clone();
        let fault_json = record.to_stored()?;
        let at_ms = sql_counter_value("fault_at_ms", at_ms)?;
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    let changed = tx
                        .execute(
                            session_sql().meta.record_fault.sql(),
                            params![session_id.as_str(), fault_json, at_ms],
                        )
                        .map_err(sqlite_error)?;
                    if changed == 1 {
                        crate::catalog::catalog_reads::record_session_terminal(
                            tx,
                            &session_id,
                            Some(&fault_json),
                            at_ms,
                        )?;
                    }
                    session_fault_conn(tx, &session_id)
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn session_fault(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionFault>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| Ok(session_fault_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn list_session_faults(
        &self,
        after: Option<&SessionId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<SessionFault>, StoreError> {
        let after = after.map_or_else(String::new, |id| id.as_str().to_owned());
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let rows: Vec<(String, String, i64)> = self
            .conn
            .call(move |conn| {
                let mut select = conn.prepare_cached(session_sql().meta.list_faults.sql())?;
                select
                    .query_map(params![after, limit], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(|(session_id, json, at_ms)| {
                SessionFault::from_stored(SessionId::parse(session_id)?, &json, at_ms)
            })
            .collect()
    }

    async fn clear_session_fault(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .write_flow(move |tx| {
                commit(
                    tx.execute(
                        session_sql().meta.clear_fault.sql(),
                        params![session_id.as_str()],
                    )
                    .map(|changed| changed == 1)
                    .map_err(sqlite_error),
                )
            })
            .await
            .map_err(sqlite_error)?
    }
}
