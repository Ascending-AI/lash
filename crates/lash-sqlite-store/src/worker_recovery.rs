use crate::{SqliteStore, schema_layout::Schema, sqlite_error};
use lash_core_execution::store::worker_recovery::*;
use lash_store_sql::worker_recovery::WorkerRecoveryStatements;
use rusqlite::OptionalExtension;
use std::sync::LazyLock;

static SQL: LazyLock<WorkerRecoveryStatements> =
    LazyLock::new(|| WorkerRecoveryStatements::render(Schema::Main.dialect()));
#[async_trait::async_trait]
impl WorkerRecoveryStore for SqliteStore {
    async fn reserve(
        &self,
        scope: &str,
        limits: WorkerRecoveryLimits,
    ) -> Result<WorkerRecoveryClaim, WorkerRecoveryError> {
        let scope = scope.to_owned();
        self.conn
            .write(move |tx| {
                let current = tx
                    .query_row(SQL.select.sql(), [&scope], |row| {
                        Ok(WorkerRecoveryRow {
                            revision: row.get(0)?,
                            totals: WorkerRecoveryTotals {
                                attempts: row.get(1)?,
                                cpu_nanos: row.get(2)?,
                                replacement: row.get::<_, i64>(3)? != 0,
                                unknown_cpu_attempts: row.get(4)?,
                            },
                            in_flight: row.get::<_, i64>(5)? != 0,
                        })
                    })
                    .optional()?
                    .unwrap_or_default();
                let (claim, row) = match reserve_worker_recovery(scope, current, limits) {
                    Ok(plan) => plan,
                    Err(error) => return Ok(Err(error)),
                };
                tx.execute(
                    SQL.reserve.sql(),
                    rusqlite::params![
                        claim.scope,
                        row.revision,
                        row.totals.attempts,
                        row.totals.cpu_nanos,
                        i64::from(row.totals.replacement),
                        row.totals.unknown_cpu_attempts
                    ],
                )?;
                Ok(Ok(claim))
            })
            .await
            .map_err(|error| WorkerRecoveryError::Store(sqlite_error(error)))?
    }
    async fn mark_running(&self, claim: &WorkerRecoveryClaim) -> Result<(), WorkerRecoveryError> {
        let claim = claim.clone();
        let changed = self
            .conn
            .write(move |tx| {
                tx.execute(
                    SQL.running.sql(),
                    rusqlite::params![claim.scope, claim.revision],
                )
            })
            .await
            .map_err(|error| WorkerRecoveryError::Store(sqlite_error(error)))?;
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
        let claim = claim.clone();
        let changed = self
            .conn
            .write(move |tx| {
                tx.execute(
                    SQL.settle.sql(),
                    rusqlite::params![
                        claim.scope,
                        claim.revision,
                        totals.attempts,
                        totals.cpu_nanos,
                        i64::from(totals.replacement),
                        totals.unknown_cpu_attempts
                    ],
                )
            })
            .await
            .map_err(|error| WorkerRecoveryError::Store(sqlite_error(error)))?;
        if changed == 1 {
            Ok(())
        } else {
            Err(WorkerRecoveryError::Fenced)
        }
    }
}
