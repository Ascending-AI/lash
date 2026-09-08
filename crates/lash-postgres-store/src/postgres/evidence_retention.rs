//! Terminal-session receipt sweep and dependent-root reconciliation (FIG-2502).
use crate::*;

pub(crate) async fn reclaim(
    factory: &PostgresSessionStoreFactory,
    bound: lash_core::store::RetentionBound,
) -> lash_core::MaintenanceResult<lash_core::store::RetentionReport> {
    async {
        let mut tx = factory.pool.begin().await.map_err(store_sqlx_error)?;
        // One cross-worker fence for this host-invoked, atomic two-phase sweep.
        sqlx::query("SELECT pg_advisory_xact_lock(715423, 0)")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        // deleted_sessions permanently protects identity reuse (FIG-754 / FIG-748).
        let removed_receipt_count = sqlx::query(
            "DELETE FROM lash_runtime_turn_commits AS receipt
             WHERE receipt.committed_at_ms < $1
               AND EXISTS (SELECT 1 FROM lash_deleted_sessions AS deleted
                           WHERE deleted.session_id = receipt.session_id)",
        )
        .bind(clamp_epoch_ms(bound.committed_before_epoch_ms))
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        // Only terminal usage becomes eligible; live ledgers reconstruct
        // resumed accounting. Anti-join after the receipt-root sweep.
        let removed_usage_delta_count = sqlx::query(
            "DELETE FROM lash_usage_deltas AS usage
             WHERE EXISTS (SELECT 1 FROM lash_deleted_sessions AS deleted
                           WHERE deleted.session_id = usage.session_id)
               AND NOT EXISTS (SELECT 1 FROM lash_runtime_turn_commits AS receipt
                               WHERE receipt.session_id = usage.session_id
                                 AND receipt.turn_id = usage.operation_storage_key)",
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        // Terminal markers replace the positive receipt oracle for deleted
        // owners; graph retention independently protects committed attachments.
        let removed_attachment_root_count =
            sqlx::query(crate::attachments::RECLAIM_DELETED_ATTACHMENT_ROOTS)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected() as usize;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::store::RetentionReport {
            removed_receipt_count,
            removed_usage_delta_count,
            removed_attachment_root_count,
        })
    }
    .await
    .map_err(lash_core::MaintenanceFailure::failed_before_any_work)
}
