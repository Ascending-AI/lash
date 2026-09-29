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
        .write(move |tx| {
            let removed_receipt_count = crate::conn::cached_execute(
                tx,
                session_sql().turn_commits_sqlite.delete_retained.sql(),
                params![cutoff],
            )?;
            let removed_usage_delta_count =
                crate::conn::cached_execute(tx, session_sql().usage.delete_reclaimable.sql(), [])?;
            let removed_attachment_root_count = crate::conn::cached_execute(
                tx,
                crate::attachments::attachment_sql()
                    .manifest_sqlite
                    .delete_deleted_session_roots
                    .sql(),
                [],
            )?;
            Ok(lash_core_execution::store::RetentionReport {
                removed_receipt_count,
                removed_usage_delta_count,
                removed_attachment_root_count,
                retired_effect_scope_count: 0,
            })
        })
        .await
        .map_err(|error| failed_before_any_work(sqlite_error(error)))
}
