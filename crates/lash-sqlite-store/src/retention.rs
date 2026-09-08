//! Terminal-session receipt sweep and dependent-root reconciliation (FIG-2502).
use crate::*;

pub(crate) async fn reclaim(
    factory: &SqliteSessionStoreFactory,
    bound: lash_core::store::RetentionBound,
) -> lash_core::MaintenanceResult<lash_core::store::RetentionReport> {
    let store = factory
        .open_catalog_for_maintenance("evidence retention")
        .await
        .map_err(lash_core::MaintenanceFailure::failed_before_any_work)?;
    let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);
    store
        .conn
        .write(move |tx| {
            // Phase 1: terminal evidence roots. deleted_sessions is permanent
            // identity evidence, exempt from retention (FIG-754 / FIG-748).
            let removed_receipt_count = tx.execute(
                "DELETE FROM runtime_turn_commits AS receipt
             WHERE receipt.committed_at_ms < ?1
               AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                           WHERE deleted.session_id = receipt.session_id)",
                params![cutoff],
            )?;
            // Phase 2: correlated anti-joins reconcile dependent rows after the
            // receipt sweep, under the same BEGIN IMMEDIATE fence. Live ledgers
            // are never eligible; they rebuild resumed-session accounting.
            let removed_usage_delta_count = tx.execute(
                "DELETE FROM usage_deltas AS usage
             WHERE EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                           WHERE deleted.session_id = usage.session_id)
               AND NOT EXISTS (SELECT 1 FROM runtime_turn_commits AS receipt
                               WHERE receipt.session_id = usage.session_id
                                 AND receipt.turn_id = usage.operation_storage_key)",
                [],
            )?;
            // The permanent terminal marker proves intent-owner death even after
            // the positive supersession receipt is gone. Retained graph prefixes
            // protect committed attachments independently of receipt retention.
            let removed_attachment_root_count =
                tx.execute(attachments::RECLAIM_DELETED_ATTACHMENT_ROOTS, [])?;
            Ok(lash_core::store::RetentionReport {
                removed_receipt_count,
                removed_usage_delta_count,
                removed_attachment_root_count,
            })
        })
        .await
        .map_err(|error| lash_core::MaintenanceFailure::failed_before_any_work(sqlite_error(error)))
}

impl SqliteSessionStoreFactory {
    /// Open the existing catalog with the factory connection and registry options
    /// for a host-invoked maintenance operation.
    pub(crate) async fn open_catalog_for_maintenance(
        &self,
        operation: &str,
    ) -> Result<Store, lash_core::StoreError> {
        let path = self.catalog_path();
        if !path.exists() {
            return Err(lash_core::StoreError::Backend(format!(
                "maintenance {operation} aborted: durable-core catalog {} does not exist",
                path.display()
            )));
        }
        Store::open_with_options_clock_and_process_registry(
            &path,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry_path.as_deref(),
            #[cfg(feature = "testing")]
            self.fault_injector.clone(),
        )
        .await
        .map_err(|err| {
            lash_core::StoreError::Backend(format!(
                "maintenance {operation} aborted: durable-core catalog {} could not be opened: {err}",
                path.display()
            ))
        })
    }
}
