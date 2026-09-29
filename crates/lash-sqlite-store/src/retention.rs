//! Terminal-session receipt sweep and dependent-root reconciliation.
use crate::session_sql::session_sql;
use crate::*;

pub(crate) type ReclaimResult = Result<
    lash_core_execution::store::RetentionReport,
    Box<lash_core_execution::MaintenanceFailure<lash_core_execution::store::RetentionReport>>,
>;

pub(crate) async fn reclaim(
    factory: &SqliteSessionStoreFactory,
    bound: lash_core_execution::store::RetentionBound,
) -> ReclaimResult {
    let failed_before_any_work = |error: lash_core_execution::StoreError| {
        Box::new(lash_core_execution::MaintenanceFailure::failed_before_any_work(error))
    };
    let store = factory
        .open_catalog_for_maintenance("evidence retention")
        .await
        .map_err(failed_before_any_work)?;
    let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);
    store
        .conn
        .write(move |tx| {
            let removed_receipt_count = crate::conn::cached_execute(
                tx,
                session_sql().turn_commits_sqlite.delete_retained.sql(),
                params![cutoff],
            )?;
            let removed_stopped_partial_count = crate::conn::cached_execute(
                tx,
                crate::capture::retention_delete_sql(),
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
                removed_stopped_partial_count,
                removed_usage_delta_count,
                removed_attachment_root_count,
                retired_effect_scope_count: 0,
            })
        })
        .await
        .map_err(|error| failed_before_any_work(sqlite_error(error)))
}

impl SqliteSessionStoreFactory {
    pub(crate) async fn open_catalog_for_maintenance(
        &self,
        operation: &str,
    ) -> Result<Store, lash_core_execution::StoreError> {
        self.open_catalog_for_maintenance_configured(operation)
            .await
    }

    async fn open_catalog_for_maintenance_configured(
        &self,
        operation: &str,
    ) -> Result<Store, lash_core_execution::StoreError> {
        let catalog = self.core.target();
        if !catalog.exists() {
            return Err(lash_core_execution::StoreError::Backend(format!(
                "maintenance {operation} aborted: durable-core catalog {catalog} does not exist"
            )));
        }
        Store::open_at(
            &self.core,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry.as_ref(),
            self.turn_cancel_closure_owner_binding(),
            lash_core_execution::FleetFormat::writable_range(),
            #[cfg(feature = "testing")]
            self.fault_injector.clone(),
        )
        .await
        .map_err(|err| {
            lash_core_execution::StoreError::Backend(format!(
                "maintenance {operation} aborted: durable-core catalog {catalog} could not be opened: {err}"
            ))
        })
    }
}
