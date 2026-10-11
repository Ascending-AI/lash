//! Garbage collection of the blob store on [`SqliteStore`].
//!
//! The collector is the checkpoint graph's (`durable/graph.rs`): it marks
//! from the complete root catalog and sweeps the rest in one `BEGIN
//! IMMEDIATE` transaction, so a failure rolls every delete back and the
//! partial report is empty *because no work survived*.

use super::*;
use crate::durable::{CollectionLimits, collect_graph_in};

/// The component references of an opaque session checkpoint manifest, which
/// the collector retains with it.
///
/// Transitional (FIG-5890): session manifests become graph nodes whose
/// edges `blob_edges` holds, and the integrator's cutover (FIG-5894) deletes
/// this reader with the `checkpoint_blob_refs` table. A present manifest
/// that does not decode is a hard error, so collection aborts rather than
/// dropping a live checkpoint's components; refs a newer component codec
/// wrote are retained all the same.
pub(crate) fn legacy_manifest_components(
    bytes: &[u8],
    fleet: lash_core_execution::FleetFormat,
) -> Result<Vec<String>, lash_core_store::checkpoint_graph::GraphRefusal> {
    let refuse = |error: StoreError| {
        lash_core_store::checkpoint_graph::GraphRefusal::Backend(lash_durable::StoreFailure {
            kind: lash_durable::StoreFailureKind::Corrupt,
            message: error.to_string(),
        })
    };
    let content = decode_artifact_blob(bytes).map_err(refuse)?;
    let checkpoint = decode_checkpoint_for_fleet(&content, fleet).map_err(refuse)?;
    Ok(crate::catalog::retained_artifact_refs(&checkpoint)
        .into_iter()
        .map(|retained| retained.blob_ref.0)
        .collect())
}

impl SqliteStore {
    /// Mark-and-sweep the blob table, answering in the maintenance outcome
    /// contract (ADR 0067 §4).
    ///
    /// Expired read pins are retired first, in a transaction of their own;
    /// then the whole collection runs in one `BEGIN IMMEDIATE` transaction.
    pub async fn gc_unreachable(&self) -> lash_core_execution::MaintenanceResult<GcReport> {
        let failed = |error: StoreError| {
            lash_core_execution::MaintenanceFailure::failed_before_any_work(error)
        };
        let now = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        self.conn
            .write(move |tx| crate::durable::retire_expired_read_pins_in(tx, now))
            .await
            .map_err(|error| failed(sqlite_error(error)))?;
        self.conn
            .write(move |tx| {
                let fleet = tx.fleet();
                Self::gc_unreachable_in_tx(tx, fleet).map_err(|err| {
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
    /// `fleet` is the store's recorded `F`: a legacy checkpoint manifest
    /// admits the `[N-1, N]` reader window `F` names (FIG-3796).
    pub(crate) fn gc_unreachable_in_tx(
        tx: &Transaction<'_>,
        fleet: lash_core_execution::FleetFormat,
    ) -> Result<GcReport, StoreError> {
        // A host collection is what ends `until_gc`'s hold: release every
        // revision no head, pin or retention window keeps before reading the
        // roots, so the sweep below collects what only they named.
        crate::revisions::release_unretained_conn(tx, true, None)?;
        let legacy = move |bytes: &[u8]| legacy_manifest_components(bytes, fleet);
        let collection = collect_graph_in(tx, CollectionLimits::UNBOUNDED, &legacy)
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
