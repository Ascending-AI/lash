//! Session-graph persistence and garbage collection on [`SqliteStore`].
//!
//! The shared
//! `*_from_conn` helpers are **synchronous** and take a `&rusqlite::Connection`
//! so callers already on the connection thread can reuse them inside a
//! `conn.call` closure — this is the load-bearing change from the prior store,
//! which had them `async`.
//!
//! Read paths go through `self.conn.call(...)`; the graph-mutating and GC paths
//! go through `self.conn.write(...)` so `BEGIN IMMEDIATE` takes the write lock
//! up front, replacing the prior store `BEGIN IMMEDIATE` / `COMMIT` / `ROLLBACK`
//! ceremony.

use super::artifact_store::artifact_namespace_kind;
use super::*;
use crate::catalog::retained_artifact_refs;
use crate::session_sql::session_sql;

/// One GC root class. The variant *is* the label choice: a pointer-table row
/// derives its [`PersistedArtifactKind`] from its own namespace key, the sole
/// owner of the payload-family fact, so a new root class must pick a variant
/// rather than silently inherit a sibling's label (FIG-1949). Child refs a
/// traversal discovers are not roots and keep flowing as
/// [`RetainedArtifactRef`].
enum GcRoot {
    /// A live session checkpoint root; the only root class whose stored ref
    /// graph the sweep traverses.
    CheckpointManifest(BlobRef),
    /// A pointer-table `artifact_refs` row; a leaf whose label its namespace
    /// owns.
    ArtifactRef {
        blob_ref: BlobRef,
        kind: PersistedArtifactKind,
    },
}

impl GcRoot {
    fn into_retained(self) -> RetainedArtifactRef {
        match self {
            Self::CheckpointManifest(blob_ref) => RetainedArtifactRef {
                blob_ref,
                kind: PersistedArtifactKind::CheckpointManifest,
            },
            Self::ArtifactRef { blob_ref, kind } => RetainedArtifactRef { blob_ref, kind },
        }
    }
}

impl SqliteStore {
    /// Mark-and-sweep the blob table, answering in the maintenance outcome
    /// contract (ADR 0067 §4).
    ///
    /// The whole sweep runs in one `BEGIN IMMEDIATE` transaction, so a backend
    /// failure rolls every delete back and the partial report is empty *because
    /// no work survived* — never because the failure was absorbed into a clean
    /// zero report.
    pub async fn gc_unreachable(&self) -> lash_core_execution::MaintenanceResult<GcReport> {
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
            .map_err(|err| {
                lash_core_execution::MaintenanceFailure::failed_before_any_work(sqlite_error(err))
            })
    }

    /// Collect the checkpoint-manifest roots that must survive GC.
    ///
    /// The session head's current `checkpoint_ref` is the live checkpoint; its
    /// manifest blob (and, transitively, the tool/plugin/execution snapshot
    /// blobs it references) is reachable and must be kept. Synchronous: runs
    /// inside the GC `conn.write` closure on the connection thread.
    fn live_checkpoint_roots(conn: &Connection) -> Result<Vec<GcRoot>, StoreError> {
        let mut roots = Vec::new();
        let mut stmt = conn
            .prepare(session_sql().head.select_checkpoint_roots.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        for row in rows {
            roots.push(GcRoot::CheckpointManifest(BlobRef(
                row.map_err(sqlite_error)?,
            )));
        }
        Ok(roots)
    }

    /// Collect the pointer-table roots that must survive GC.
    ///
    /// Each row's label is derived from its own namespace key — the sole owner
    /// of the payload-family fact — via [`artifact_namespace_kind`], so a
    /// pointer row can never inherit the module label (FIG-1949).
    /// Synchronous: runs inside the GC `conn.write` closure.
    fn artifact_ref_roots(conn: &Connection) -> Result<Vec<GcRoot>, StoreError> {
        let mut roots = Vec::new();
        let mut stmt = conn
            .prepare(
                crate::artifact_store::artifact_sql()
                    .refs
                    .select_gc_roots
                    .sql(),
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?;
        for row in rows {
            let (namespace, blob_ref) = row.map_err(sqlite_error)?;
            roots.push(GcRoot::ArtifactRef {
                blob_ref: BlobRef(blob_ref),
                kind: artifact_namespace_kind(&namespace)?,
            });
        }
        Ok(roots)
    }

    /// Synchronous body of [`SqliteStore::gc_unreachable`], run on the connection thread
    /// inside the `BEGIN IMMEDIATE` transaction so the mark/sweep is atomic and
    /// holds the write lock for its duration.
    /// `fleet` is the store's recorded `F`: a live checkpoint manifest admits
    /// the `[N-1, N]` reader window `F` names (FIG-3796).
    pub(crate) fn gc_unreachable_in_tx(
        tx: &Transaction<'_>,
        fleet: lash_core_execution::FleetFormat,
    ) -> Result<GcReport, StoreError> {
        let mut roots = Self::live_checkpoint_roots(tx)?;
        roots.extend(Self::artifact_ref_roots(tx)?);
        let root_count = roots.len();
        let mut retained = std::collections::BTreeMap::<String, PersistedArtifactKind>::new();
        let mut stack: Vec<RetainedArtifactRef> =
            roots.into_iter().map(GcRoot::into_retained).collect();
        while let Some(current) = stack.pop() {
            if retained
                .insert(current.blob_ref.0.clone(), current.kind)
                .is_some()
            {
                continue;
            }
            if current.kind != PersistedArtifactKind::CheckpointManifest {
                continue;
            }
            // A rooted checkpoint manifest is *live*. If we cannot read or
            // decode it we must not silently drop the keyed component blobs it
            // points at — doing so would delete blobs
            // that belong to a live checkpoint. Skip a manifest that simply
            // isn't present (it may have been collected on a prior run), but
            // treat a present-yet-undecodable manifest as a hard error so GC
            // aborts rather than deleting live data.
            let bytes: Option<Vec<u8>> = tx
                .query_row(
                    crate::artifact_store::artifact_sql()
                        .blobs
                        .select_content
                        .sql(),
                    params![current.blob_ref.as_str()],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(sqlite_error)?;
            let Some(bytes) = bytes else {
                continue;
            };
            let content = decode_artifact_blob(&bytes)?;
            let checkpoint = decode_checkpoint_for_fleet(&content, fleet)?;
            // GC interprets only the root's ref graph, never component bodies.
            // Retain refs even when a newer writer used an unknown component
            // codec so an older binary cannot turn incompatibility into loss.
            stack.extend(retained_artifact_refs(&checkpoint));
        }
        // Match PostgreSQL's strict ordering even though SQLite's component
        // side is not FK-enforced: every dead root loses its complete outgoing
        // edge set before any hash-ordered blob delete can reach a component.
        crate::conn::cached_execute(tx, session_sql().checkpoint_edges.delete_unrooted.sql(), [])
            .map_err(sqlite_error)?;
        let all_hashes = {
            let mut stmt = tx
                .prepare(
                    crate::artifact_store::artifact_sql()
                        .blobs
                        .select_all_hashes
                        .sql(),
                )
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
        };
        let mut deleted_blob_count = 0usize;
        for hash in &all_hashes {
            if retained.contains_key(hash) {
                continue;
            }
            crate::conn::cached_execute(
                tx,
                crate::artifact_store::artifact_sql()
                    .blobs
                    .delete_by_hash
                    .sql(),
                params![hash],
            )
            .map_err(sqlite_error)?;
            deleted_blob_count += 1;
        }
        Ok(GcReport {
            root_count,
            retained_blob_count: retained.len(),
            deleted_blob_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_store::{MODULE_ARTIFACT_NAMESPACE, PROCESS_ENV_NAMESPACE};

    /// A pointer-table row's retained label comes from its own namespace key:
    /// a non-manifest namespace cannot inherit the module label (FIG-1949).
    #[tokio::test]
    async fn pointer_table_roots_derive_labels_from_their_namespace() {
        let store = crate::test_support::memory_store()
            .await
            .expect("open store");
        store
            .conn
            .call(|conn| {
                for (namespace, artifact_ref, blob_ref) in [
                    (MODULE_ARTIFACT_NAMESPACE, "mod-a", "blob-mod"),
                    (PROCESS_ENV_NAMESPACE, "env-a", "blob-env"),
                ] {
                    crate::conn::cached_execute(
                        conn,
                        "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
                         VALUES (?1, ?2, ?3)",
                        params![namespace, artifact_ref, blob_ref],
                    )?;
                }
                let roots =
                    SqliteStore::artifact_ref_roots(conn).map_err(sqlite_conversion_error)?;
                let kinds: std::collections::BTreeMap<String, PersistedArtifactKind> = roots
                    .into_iter()
                    .map(|root| match root {
                        GcRoot::ArtifactRef { blob_ref, kind } => (blob_ref.0, kind),
                        GcRoot::CheckpointManifest(_) => {
                            unreachable!("pointer collection yields no manifest root")
                        }
                    })
                    .collect();
                assert_eq!(kinds["blob-mod"], PersistedArtifactKind::LashlangModule);
                assert_eq!(
                    kinds["blob-env"],
                    PersistedArtifactKind::ProcessExecutionEnv
                );
                Ok(())
            })
            .await
            .expect("namespace-derived pointer labels");
    }

    /// A namespace nobody mapped fails the sweep rather than being labelled
    /// with a sibling namespace's kind.
    #[tokio::test]
    async fn pointer_table_root_with_unknown_namespace_fails_closed() {
        let store = crate::test_support::memory_store()
            .await
            .expect("open store");
        store
            .conn
            .call(|conn| {
                crate::conn::cached_execute(
                    conn,
                    "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
                     VALUES ('foreign_namespace', 'ref-a', 'blob-a')",
                    [],
                )?;
                assert!(SqliteStore::artifact_ref_roots(conn).is_err());
                Ok(())
            })
            .await
            .expect("unknown namespace fails closed");
    }
}
