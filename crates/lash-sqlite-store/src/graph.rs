//! Garbage collection of the blob store on [`SqliteStore`].
//!
//! The collector is the checkpoint graph's (`durable/graph.rs`): it marks
//! from the complete root catalog and sweeps the rest in one `BEGIN
//! IMMEDIATE` transaction, so a failure rolls every delete back and the
//! partial report is empty *because no work survived*.

use super::*;
use crate::durable::{CollectionLimits, collect_graph_in};

impl SqliteStore {
    /// Mark-and-sweep the blob table, answering in the maintenance outcome
    /// contract (ADR 0067 §4).
    ///
    /// The whole collection runs in one `BEGIN IMMEDIATE` transaction.
    pub async fn gc_unreachable(&self) -> lash_core_execution::MaintenanceResult<GcReport> {
        let failed = |error: StoreError| {
            lash_core_execution::MaintenanceFailure::failed_before_any_work(error)
        };
        self.conn
            .write(move |tx| {
                Self::gc_unreachable_in_tx(tx).map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                        err.to_string(),
                    )))
                })
            })
            .await
            .map_err(|error| failed(sqlite_error(error)))
    }

    /// Synchronous body of [`SqliteStore::gc_unreachable`], run on the
    /// connection thread inside the `BEGIN IMMEDIATE` transaction so the
    /// mark/sweep is atomic and holds the write lock for its duration.
    pub(crate) fn gc_unreachable_in_tx(tx: &Transaction<'_>) -> Result<GcReport, StoreError> {
        // A host collection is what ends `until_gc`'s hold: release every
        // revision no head, pin or retention window keeps before reading the
        // roots, so the sweep below collects what only they named.
        crate::revisions::release_unretained_conn(tx, true, None)?;
        let collection = collect_graph_in(tx, CollectionLimits::UNBOUNDED)
            .map_err(sqlite_error)?
            .map_err(|refusal| StoreError::Backend(refusal.to_string()))?;
        let as_usize = |count: u64| usize::try_from(count).unwrap_or(usize::MAX);
        Ok(GcReport {
            root_count: as_usize(collection.roots.values().sum()),
            retained_blob_count: as_usize(collection.marked),
            deleted_blob_count: as_usize(collection.swept_blobs),
        })
    }
}
