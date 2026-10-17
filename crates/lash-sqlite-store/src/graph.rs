//! Garbage collection of the blob store on [`SqliteStore`].
//!
//! Every transaction that retires a root holds its closure for retirement
//! before it commits (`durable/graph.rs`), so a collection is no mark and
//! sweep: it ends `until_gc`'s hold on revisions, runs the retirement steps
//! until nothing is held, and runs the orphan audit, each in transactions
//! of its own. None of them holds the write lock for more than one
//! retirement step or audit page.

use super::*;

impl SqliteStore {
    /// Release what the host's collection ends, retire what is held, then
    /// audit for orphans, answering in the maintenance outcome contract
    /// (ADR 0067 §4).
    ///
    /// The revisions only `until_gc` retained are released and held in one
    /// transaction; retirement steps then delete what every hold reaches and
    /// nothing else does, a bounded step per transaction, and the orphan
    /// audit peels whatever no root names, a page per transaction. The
    /// report counts every blob those steps and peels deleted, and the roots
    /// and blobs stored once the audit ended: with nothing held and no
    /// orphan left in the DAG, every stored blob is one a root reaches.
    pub async fn gc_unreachable(&self) -> lash_core_store::store::MaintenanceResult<GcReport> {
        let mut report = GcReport::default();
        let outcome = self.collect(&mut report).await;
        outcome.map(|()| report.clone()).map_err(|error| {
            if report.deleted_blob_count == 0 {
                lash_core_store::store::MaintenanceFailure::failed_before_any_work(error)
            } else {
                lash_core_store::store::MaintenanceFailure::failed(error, report)
            }
        })
    }

    /// The body of [`Self::gc_unreachable`]: every peel it commits counts
    /// into `report` as it commits.
    async fn collect(&self, report: &mut GcReport) -> Result<(), StoreError> {
        let collection_error = |err: StoreError| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                err.to_string(),
            )))
        };
        // A host collection is what ends `until_gc`'s hold.
        let now = self.retirement_instant();
        self.conn
            .write(move |tx| {
                crate::revisions::release_unretained_conn(tx, true, None, now)
                    .map_err(collection_error)
            })
            .await
            .map_err(sqlite_error)?;
        // A quiet session needs no future commit to finish audit reclamation.
        loop {
            let released = self
                .conn
                .write(|tx| crate::durable::release_material_batch(tx, None))
                .await
                .map_err(sqlite_error)?;
            if released == 0 {
                break;
            }
        }
        // The most nodes one of the collection's retirement steps deletes:
        // the standard node's step.
        let step = lash_durable::domain::pages::RetirementBudget {
            nodes: lash_durable::RetirementSettings::standard().batch_nodes,
        };
        loop {
            let now = self.retirement_instant();
            let progress = self
                .conn
                .write(move |tx| crate::durable::retire_step(tx, step, now))
                .await
                .map_err(sqlite_error)?;
            report.deleted_blob_count +=
                usize::try_from(progress.nodes_deleted).unwrap_or(usize::MAX);
            if progress.holds_advanced == 0 && progress.leases_reaped == 0 {
                break;
            }
        }
        let mut after = String::new();
        loop {
            let from = after.clone();
            let (last, gone) = self
                .conn
                .write(move |tx| crate::durable::audit_page(tx, &from, crate::durable::AUDIT_PAGE))
                .await
                .map_err(sqlite_error)?;
            report.deleted_blob_count += gone;
            match last {
                Some(last) => after = last,
                None => break,
            }
        }
        let (roots, blobs) = self
            .conn
            .read(|tx| crate::durable::graph_counts(tx))
            .await
            .map_err(sqlite_error)?;
        report.root_count = roots;
        report.retained_blob_count = blobs;
        Ok(())
    }
}
