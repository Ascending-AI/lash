//! Terminal-session receipt sweep and dependent-root reconciliation (FIG-2502).
use crate::session_sql::session_sql;
use crate::*;

/// The sweep's outcome, boxed on the failure side: `MaintenanceFailure`
/// carries the partial report beside the stop, so the `Err` arm is several
/// times the size of the report alone (`clippy::result_large_err`); the
/// factory's trait method, whose signature the trait fixes, unboxes it.
pub(crate) type ReclaimResult = Result<
    lash_core_execution::store::RetentionReport,
    Box<lash_core_execution::MaintenanceFailure<lash_core_execution::store::RetentionReport>>,
>;

pub(crate) async fn reclaim(
    factory: &PostgresStore,
    bound: lash_core_execution::store::RetentionBound,
) -> ReclaimResult {
    async {
        let mut tx = begin_guarded(&factory.pool, &factory.fence).await?;
        // One cross-worker fence for this host-invoked, atomic multi-phase sweep.
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_evidence_retention
                .sql(),
        )
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        crate::usage_accounting::lock_retention(&mut tx).await?;
        // deleted_sessions permanently protects identity reuse (FIG-754 / FIG-748).
        let removed_receipt_count =
            sqlx::query(session_sql().turn_commits_postgres.delete_retained.sql())
                .bind(clamp_epoch_ms(bound.committed_before_epoch_ms))
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected() as usize;
        // Only terminal usage becomes eligible; live ledgers reconstruct
        // resumed accounting. Anti-join after the receipt-root sweep.
        let removed_usage_delta_count = sqlx::query(session_sql().usage.delete_reclaimable.sql())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected() as usize;
        let (facts, runs, owners) = crate::usage_accounting::retention_sql();
        let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);
        let removed_usage_fact_count = sqlx::query(facts)
            .bind(cutoff)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected() as usize;
        let removed_usage_run_count = sqlx::query(runs)
            .bind(cutoff)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected() as usize;
        let removed_usage_owner_retirement_count = sqlx::query(owners)
            .bind(cutoff)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected() as usize;
        // Terminal markers replace the positive receipt oracle for deleted
        // owners; graph retention independently protects committed attachments.
        let removed_attachment_root_count = sqlx::query(
            crate::attachments::attachment_sql()
                .manifest_postgres
                .delete_deleted_session_roots
                .sql(),
        )
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core_execution::store::RetentionReport {
            removed_receipt_count,
            removed_usage_fact_count,
            removed_usage_run_count,
            removed_usage_owner_retirement_count,
            removed_usage_delta_count,
            removed_attachment_root_count,
            // Effect scopes are the engine's to retire; this catalog holds
            // no effect journal.
            retired_effect_scope_count: 0,
        })
    }
    .await
    .map_err(|error| {
        Box::new(lash_core_execution::MaintenanceFailure::failed_before_any_work(error))
    })
}
