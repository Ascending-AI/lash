//! [`SessionFaultStore`] for [`PostgresStore`]: a session's standing fault
//! (ADR 0109 §9), on its `session_meta` row.

use super::*;
use lash_core_execution::store::{SessionFault, SessionFaultRecord, SessionFaultStore};

/// The session's standing fault (ADR 0109 §9).
async fn session_fault_conn(
    connection: &mut sqlx::PgConnection,
    session_id: &SessionId,
) -> Result<Option<SessionFault>, StoreError> {
    sqlx::query(session_sql().meta.select_fault.sql())
        .bind(session_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?
        .map(|row| {
            let json: String = row.try_get(0).map_err(store_sqlx_error)?;
            let at_ms: i64 = row.try_get(1).map_err(store_sqlx_error)?;
            SessionFault::from_stored(session_id.clone(), &json, at_ms)
        })
        .transpose()
}

#[async_trait::async_trait]
impl SessionFaultStore for PostgresStore {
    async fn record_session_fault(
        &self,
        session_id: &SessionId,
        record: &SessionFaultRecord,
        at_ms: u64,
    ) -> Result<Option<SessionFault>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let changed = sqlx::query(session_sql().meta.record_fault.sql())
            .bind(session_id.as_str())
            .bind(record.to_stored()?)
            .bind(sql_counter_value("fault_at_ms", at_ms)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if changed.rows_affected() == 1 {
            tx.stage_turn_change(crate::session_factory::session_terminal(
                session_id,
                Some(record.to_stored()?),
                sql_counter_value("fault_at_ms", at_ms)?,
            ))
            .await
            .map_err(store_sqlx_error)?;
        }
        let stored = session_fault_conn(&mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(stored)
    }

    async fn session_fault(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionFault>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        session_fault_conn(&mut connection, session_id).await
    }

    async fn list_session_faults(
        &self,
        after: Option<&SessionId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<SessionFault>, StoreError> {
        let rows = sqlx::query(session_sql().meta.list_faults.sql())
            .bind(after.map_or("", SessionId::as_str))
            .bind(i64::try_from(limit.get()).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        rows.iter()
            .map(|row| {
                let session_id: String = row.try_get(0).map_err(store_sqlx_error)?;
                let json: String = row.try_get(1).map_err(store_sqlx_error)?;
                let at_ms: i64 = row.try_get(2).map_err(store_sqlx_error)?;
                SessionFault::from_stored(SessionId::parse(session_id)?, &json, at_ms)
            })
            .collect()
    }

    async fn clear_session_fault(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let changed = sqlx::query(session_sql().meta.clear_fault.sql())
            .bind(session_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(changed == 1)
    }
}
