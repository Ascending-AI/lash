//! Session-graph persistence and garbage collection on [`Store`].
//!
//! Ported from the prior store. The public async surface
//! (`append_session_graph_nodes`, `load_session_graph`, `gc_unreachable`) keeps
//! the exact prior store signatures. The shared
//! `*_from_conn` helpers are **synchronous** and take a `&rusqlite::Connection`
//! so `lifecycle::load_picker_info` (and any future caller already on the
//! connection thread) can reuse them inside a `conn.call` closure — this is the
//! load-bearing change from the prior store, which had them `async`.
//!
//! Read paths go through `self.conn.call(...)`; the graph-mutating and GC paths
//! go through `self.conn.write(...)` so `BEGIN IMMEDIATE` takes the write lock
//! up front, replacing the prior store `BEGIN IMMEDIATE` / `COMMIT` / `ROLLBACK`
//! ceremony.

use super::*;

impl Store {
    pub(crate) fn load_session_graph_from_conn(
        conn: &Connection,
        leaf_node_id: Option<String>,
    ) -> lash_core::SessionGraph {
        // Tombstoned rows are physically still present until `vacuum()` is
        // called; the runtime view should never see them.
        let mut stmt = match conn
            .prepare("SELECT node_json FROM graph_nodes WHERE tombstoned = 0 ORDER BY seq ASC")
        {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::warn!(error = %err, "failed to prepare graph load statement");
                return lash_core::SessionGraph::from_nodes(Vec::new(), leaf_node_id);
            }
        };
        let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "failed to query graph rows");
                return lash_core::SessionGraph::from_nodes(Vec::new(), leaf_node_id);
            }
        };
        let nodes = rows
            .filter_map(Result::ok)
            .filter_map(|node_json| {
                serde_json::from_str::<lash_core::SessionNodeRecord>(&node_json).ok()
            })
            .collect();
        lash_core::SessionGraph::from_nodes(nodes, leaf_node_id)
    }

    pub(crate) fn load_active_path_session_graph_from_conn(
        conn: &Connection,
        leaf_node_id: Option<String>,
    ) -> rusqlite::Result<lash_core::SessionGraph> {
        let Some(leaf_node_id) = leaf_node_id else {
            return Ok(lash_core::SessionGraph::default());
        };
        let mut stmt = conn.prepare(
            "WITH RECURSIVE active(node_id, node_json, parent_node_id, depth) AS (
                SELECT
                    node_id,
                    node_json,
                    json_extract(node_json, '$.parent_node_id'),
                    0
                FROM graph_nodes
                WHERE node_id = ?1 AND tombstoned = 0
              UNION ALL
                SELECT
                    g.node_id,
                    g.node_json,
                    json_extract(g.node_json, '$.parent_node_id'),
                    active.depth + 1
                FROM graph_nodes g
                JOIN active ON g.node_id = active.parent_node_id
                WHERE g.tombstoned = 0
            )
            SELECT node_json FROM active ORDER BY depth DESC",
        )?;
        let rows = stmt.query_map(params![leaf_node_id.as_str()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut nodes = Vec::new();
        for row in rows {
            let node_json = row?;
            if let Ok(node) = serde_json::from_str::<lash_core::SessionNodeRecord>(&node_json) {
                nodes.push(node);
            }
        }
        Ok(lash_core::SessionGraph::from_nodes(
            nodes,
            Some(leaf_node_id),
        ))
    }

    pub(crate) async fn maybe_auto_gc(&self) {
        let Some(interval) = self.options.gc_policy.auto_run_every_commits else {
            return;
        };
        let commits = self.commit_count.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        if interval != 0 && commits.is_multiple_of(interval) {
            let _ = self.gc_unreachable().await;
        }
    }

    pub async fn append_session_graph_nodes(&self, nodes: &[lash_core::SessionNodeRecord]) {
        if nodes.is_empty() {
            return;
        }
        let nodes = nodes.to_vec();
        let result = self
            .conn
            .write(move |tx| {
                let mut stmt =
                    tx.prepare("INSERT INTO graph_nodes (node_id, node_json) VALUES (?1, ?2)")?;
                for node in &nodes {
                    let node_json = encode_json(node);
                    stmt.execute(params![node.node_id, node_json])?;
                }
                Ok(())
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "failed to append session graph nodes");
        }
    }

    pub async fn load_session_graph(&self) -> lash_core::SessionGraph {
        self.conn
            .call(|conn| Ok(Self::load_session_graph_from_conn(conn, None)))
            .await
            .unwrap_or_else(|_| lash_core::SessionGraph::from_nodes(Vec::new(), None))
    }

    pub async fn gc_unreachable(&self) -> GcReport {
        match self.try_gc_unreachable().await {
            Ok(report) => report,
            Err(err) => {
                // GC is best-effort space reclamation. A backend failure must
                // never panic inside the commit and brick the store; log and
                // leave every blob in place (the conservative outcome).
                tracing::warn!(error = %err, "gc_unreachable failed; retaining all blobs");
                GcReport::default()
            }
        }
    }

    /// Collect the checkpoint-manifest roots that must survive GC.
    ///
    /// The session head's current `checkpoint_ref` is the live checkpoint; its
    /// manifest blob (and, transitively, the tool/plugin/execution snapshot
    /// blobs it references) is reachable and must be kept. Synchronous: runs
    /// inside the GC `conn.write` closure on the connection thread.
    fn live_checkpoint_roots(conn: &Connection) -> Result<Vec<RetainedArtifactRef>, StoreError> {
        let mut roots = Vec::new();
        if let Some(checkpoint_ref) = load_session_head_meta_from_conn(conn)
            .as_ref()
            .and_then(|meta| meta.checkpoint_ref.as_ref())
            .cloned()
        {
            roots.push(RetainedArtifactRef {
                blob_ref: checkpoint_ref,
                kind: PersistedArtifactKind::CheckpointManifest,
            });
        }
        Ok(roots)
    }

    async fn try_gc_unreachable(&self) -> Result<GcReport, StoreError> {
        self.conn
            .write(|tx| {
                Self::gc_unreachable_in_tx(tx).map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                        err.to_string(),
                    )))
                })
            })
            .await
            .map_err(sqlite_error)
    }

    /// Synchronous body of [`try_gc_unreachable`], run on the connection thread
    /// inside the `BEGIN IMMEDIATE` transaction so the mark/sweep is atomic and
    /// holds the write lock for its duration.
    fn gc_unreachable_in_tx(tx: &Transaction<'_>) -> Result<GcReport, StoreError> {
        let mut roots = Self::live_checkpoint_roots(tx)?;
        {
            let mut stmt = tx
                .prepare("SELECT blob_ref FROM artifact_refs ORDER BY namespace, artifact_ref")
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite_error)?;
            for row in rows {
                roots.push(RetainedArtifactRef {
                    blob_ref: BlobRef(row.map_err(sqlite_error)?),
                    kind: PersistedArtifactKind::LashlangModule,
                });
            }
        }
        let root_count = roots.len();
        let mut retained = std::collections::BTreeMap::<String, PersistedArtifactKind>::new();
        let mut stack = roots;
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
            // decode it we must not silently drop the child blobs it points at
            // (tool/plugin/execution snapshots) — doing so would delete blobs
            // that belong to a live checkpoint. Skip a manifest that simply
            // isn't present (it may have been collected on a prior run), but
            // treat a present-yet-undecodable manifest as a hard error so GC
            // aborts rather than deleting live data.
            let bytes: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT content FROM blobs WHERE hash = ?1",
                    params![current.blob_ref.as_str()],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(sqlite_error)?;
            let Some(bytes) = bytes else {
                continue;
            };
            let content = decode_artifact_blob(&bytes).unwrap_or(bytes);
            let checkpoint = decode_checkpoint(&content)?;
            stack.extend(retained_artifact_refs(&checkpoint));
        }
        let all_hashes = {
            let mut stmt = tx
                .prepare("SELECT hash FROM blobs ORDER BY hash ASC")
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
            tx.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])
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
