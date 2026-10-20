//! Unfenced domain read connections and snapshot transactions.
use super::*;

impl PostgresDurableStore {
    /// A pooled connection for one unfenced domain read.
    pub(super) async fn reader(
        &self,
    ) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>, DurableError> {
        crate::observed_sql::checkout(&self.pools.work)
            .await
            .map_err(sqlx_failure)
    }
}

/// A `REPEATABLE READ READ ONLY` transaction on `connection`, for an
/// unfenced read of several statements: one snapshot, so a root a peel
/// retires during the read is read whole.
pub(super) async fn snapshot(
    connection: &mut PgConnection,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, DurableError> {
    let mut snapshot = crate::observed_sql::control("BEGIN", sqlx::Connection::begin(connection))
        .await
        .map_err(sqlx_failure)?;
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .begin_repeatable_read_read_only
            .sql(),
    )
    .execute(crate::observed_sql::executor(&mut *snapshot))
    .await
    .map_err(sqlx_failure)?;
    Ok(snapshot)
}
