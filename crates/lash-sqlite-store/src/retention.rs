//! Terminal-session receipt sweep and dependent-root reconciliation.
use crate::session_sql::session_sql;
use crate::*;

pub(crate) type ReclaimResult = Result<
    lash_core_execution::store::RetentionReport,
    Box<lash_core_execution::MaintenanceFailure<lash_core_execution::store::RetentionReport>>,
>;

pub(crate) async fn reclaim(
    store: &SqliteStore,
    bound: lash_core_execution::store::RetentionBound,
) -> ReclaimResult {
    let failed_before_any_work = |error: lash_core_execution::StoreError| {
        Box::new(lash_core_execution::MaintenanceFailure::failed_before_any_work(error))
    };
    let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);
    store
        .conn
        .write_flow(move |tx| {
            Ok(
                match (|| {
                    let sql = &session_sql().turn_commits;
                    let current: i64 = tx
                        .query_row(sql.change_clock.sql(), [], |row| row.get(0))
                        .map_err(sqlite_error)?;
                    let current =
                        u64::try_from(current).map_err(|_| StoreError::StoredDataCorrupt {
                            record_kind: "TurnChangeClock",
                            message: "negative current sequence".to_owned(),
                        })?;
                    let watermark = bound.turn_watermark.acknowledged_sequence(current)?;
                    let horizon: Option<i64> = tx
                        .query_row(
                            sql.removed_horizon.sql(),
                            params![cutoff, watermark],
                            |row| row.get(0),
                        )
                        .map_err(sqlite_error)?;
                    if let Some(horizon) = horizon {
                        crate::conn::cached_execute(
                            tx,
                            sql.advance_horizon.sql(),
                            params![horizon],
                        )
                        .map_err(sqlite_error)?;
                    }
                    let removed_session_terminal_count = crate::conn::cached_execute(
                        tx,
                        sql.delete_session_terminals.sql(),
                        params![cutoff, watermark],
                    )
                    .map_err(sqlite_error)?;
                    let removed_receipt_count = crate::conn::cached_execute(
                        tx,
                        session_sql().turn_commits_sqlite.delete_retained.sql(),
                        params![cutoff, watermark],
                    )
                    .map_err(sqlite_error)?;
                    let (facts, runs, owners) = crate::usage_accounting::retention_sql();
                    let removed_usage_fact_count =
                        crate::conn::cached_execute(tx, facts, params![cutoff])
                            .map_err(sqlite_error)?;
                    let removed_usage_meter_count =
                        crate::conn::cached_execute(tx, runs, params![cutoff])
                            .map_err(sqlite_error)?;
                    let removed_usage_owner_retirement_count =
                        crate::conn::cached_execute(tx, owners, params![cutoff])
                            .map_err(sqlite_error)?;
                    Ok(lash_core_execution::store::RetentionReport {
                        removed_receipt_count,
                        removed_session_terminal_count,
                        removed_usage_fact_count,
                        removed_usage_meter_count,
                        removed_usage_owner_retirement_count,
                        removed_attachment_root_count: 0,
                        retired_effect_scope_count: 0,
                    })
                })() {
                    Ok(report) => TxOutcome::Commit(Ok(report)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                },
            )
        })
        .await
        .map_err(|error| failed_before_any_work(sqlite_error(error)))?
        .map_err(failed_before_any_work)
}
