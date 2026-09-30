use crate::*;
use lash_core_execution::store::worker_recovery::*;
use lash_store_sql::{Dialect, worker_recovery::WorkerRecoveryStatements};
use sqlx::Row;
use std::sync::LazyLock;
static SQL: LazyLock<WorkerRecoveryStatements> =
    LazyLock::new(|| WorkerRecoveryStatements::render(Dialect::postgres()));
fn store(error: impl ToString) -> WorkerRecoveryError {
    WorkerRecoveryError::Store(StoreError::Backend(error.to_string()))
}
#[async_trait::async_trait]
impl WorkerRecoveryStore for PostgresLashlangArtifactStore {
    async fn reserve(
        &self,
        scope: &str,
        limits: WorkerRecoveryLimits,
    ) -> Result<WorkerRecoveryClaim, WorkerRecoveryError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(WorkerRecoveryError::Store)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(format!("lash-worker-recovery:{scope}"))
        .execute(&mut **tx)
        .await
        .map_err(store)?;
        let row = sqlx::query(SQL.select.sql())
            .bind(scope)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store)?;
        let current = match row {
            Some(row) => WorkerRecoveryRow {
                revision: u64::try_from(row.try_get::<i64, _>(0).map_err(store)?).map_err(store)?,
                totals: WorkerRecoveryTotals {
                    attempts: u32::try_from(row.try_get::<i32, _>(1).map_err(store)?)
                        .map_err(store)?,
                    cpu_nanos: u64::try_from(row.try_get::<i64, _>(2).map_err(store)?)
                        .map_err(store)?,
                    replacement: row.try_get::<i32, _>(3).map_err(store)? != 0,
                    unknown_cpu_attempts: u32::try_from(row.try_get::<i32, _>(4).map_err(store)?)
                        .map_err(store)?,
                },
                in_flight: row.try_get::<i32, _>(5).map_err(store)? != 0,
            },
            None => WorkerRecoveryRow::default(),
        };
        let (claim, row) = reserve_worker_recovery(scope.to_owned(), current, limits)?;
        sqlx::query(SQL.reserve.sql())
            .bind(scope)
            .bind(i64::try_from(row.revision).map_err(store)?)
            .bind(i32::try_from(row.totals.attempts).map_err(store)?)
            .bind(i64::try_from(row.totals.cpu_nanos).map_err(store)?)
            .bind(i32::from(row.totals.replacement))
            .bind(i32::try_from(row.totals.unknown_cpu_attempts).map_err(store)?)
            .execute(&mut **tx)
            .await
            .map_err(store)?;
        tx.commit().await.map_err(store)?;
        Ok(claim)
    }
    async fn mark_running(&self, claim: &WorkerRecoveryClaim) -> Result<(), WorkerRecoveryError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(WorkerRecoveryError::Store)?;
        let changed = sqlx::query(SQL.running.sql())
            .bind(&claim.scope)
            .bind(i64::try_from(claim.revision).map_err(store)?)
            .execute(&mut **tx)
            .await
            .map_err(store)?
            .rows_affected();
        tx.commit().await.map_err(store)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(WorkerRecoveryError::Fenced)
        }
    }
    async fn settle(
        &self,
        claim: &WorkerRecoveryClaim,
        totals: WorkerRecoveryTotals,
    ) -> Result<(), WorkerRecoveryError> {
        validate_worker_recovery_settlement(claim, totals)?;
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(WorkerRecoveryError::Store)?;
        let changed = sqlx::query(SQL.settle.sql())
            .bind(&claim.scope)
            .bind(i64::try_from(claim.revision).map_err(store)?)
            .bind(i32::try_from(totals.attempts).map_err(store)?)
            .bind(i64::try_from(totals.cpu_nanos).map_err(store)?)
            .bind(i32::from(totals.replacement))
            .bind(i32::try_from(totals.unknown_cpu_attempts).map_err(store)?)
            .execute(&mut **tx)
            .await
            .map_err(store)?
            .rows_affected();
        tx.commit().await.map_err(store)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(WorkerRecoveryError::Fenced)
        }
    }
}
