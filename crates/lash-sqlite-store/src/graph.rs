//! Session-graph persistence and garbage collection on [`Store`].
//!
//! The shared
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
        session_id: &str,
        leaf_node_id: Option<String>,
    ) -> Result<lash_core::SessionGraph, StoreError> {
        Self::load_readable_graph_from_conn(conn, session_id, leaf_node_id, false)
    }

    pub(crate) fn load_active_path_session_graph_from_conn(
        conn: &Connection,
        session_id: &str,
        leaf_node_id: Option<String>,
    ) -> Result<lash_core::SessionGraph, StoreError> {
        let Some(leaf_node_id) = leaf_node_id else {
            return Ok(lash_core::SessionGraph::default());
        };
        Self::load_readable_graph_from_conn(conn, session_id, Some(leaf_node_id), true)
    }

    fn load_readable_graph_from_conn(
        conn: &Connection,
        session_id: &str,
        leaf_node_id: Option<String>,
        active_path_only: bool,
    ) -> Result<lash_core::SessionGraph, StoreError> {
        let leaf_generation = match leaf_node_id.as_deref() {
            Some(leaf_node_id) => {
                let row = conn
                    .query_row(
                        "SELECT generation, tombstoned FROM graph_nodes WHERE node_id = ?1",
                        params![leaf_node_id],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()
                    .map_err(sqlite_error)?;
                let Some((generation, 0)) = row else {
                    return Err(stored_data_corrupt(
                        "SessionGraph",
                        format!("leaf `{leaf_node_id}` is missing or tombstoned"),
                    ));
                };
                active_path_only.then_some(generation)
            }
            None => None,
        };
        let mut stmt = conn
            .prepare(
                "SELECT g.node_id, g.parent_node_id, g.node_json,
                        g.generation, g.frame_node_id
                 FROM graph_nodes AS g
                 WHERE g.tombstoned = 0
                   AND (?2 IS NULL OR g.generation <= ?2)
                   AND (
                       g.session_id = ?1
                       OR EXISTS (
                           SELECT 1 FROM fork_lineage AS lineage
                           WHERE lineage.session_id = ?1
                             AND lineage.ancestor_session_id = g.session_id
                             AND g.generation <= lineage.fork_generation
                       )
                   )
                 ORDER BY g.generation ASC",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![session_id, leaf_generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(sqlite_error)?;
        let mut nodes = Vec::new();
        let mut prior_node_id: Option<String> = None;
        let mut expected_generation = 0_i64;
        let mut expected_frame_node_id: Option<String> = None;
        for row in rows {
            let (node_id, parent_node_id, node_json, generation, frame_node_id) =
                row.map_err(sqlite_error)?;
            if generation != expected_generation || parent_node_id != prior_node_id {
                return Err(stored_data_corrupt(
                    "SessionGraph",
                    format!(
                        "generation/parent gap at `{node_id}`: generation {generation}, expected {expected_generation}"
                    ),
                ));
            }
            let node = lash_core::SessionNodeRecord::decode_storage_body(
                node_id.clone(),
                parent_node_id,
                &node_json,
            )
            .map_err(|error| stored_data_corrupt("SessionGraph node", error))?;
            if matches!(
                node.payload,
                lash_core::SessionNodePayload::FrameOpen { .. }
            ) {
                expected_frame_node_id = Some(node_id.clone());
            }
            if expected_frame_node_id.as_deref() != Some(frame_node_id.as_str()) {
                return Err(stored_data_corrupt(
                    "SessionGraph",
                    format!("frame pointer mismatch at `{node_id}`"),
                ));
            }
            prior_node_id = Some(node_id);
            expected_generation = expected_generation
                .checked_add(1)
                .ok_or_else(|| stored_data_corrupt("SessionGraph", "generation overflow"))?;
            nodes.push(node);
        }
        if let Some(leaf_node_id) = leaf_node_id.as_deref()
            && prior_node_id.as_deref() != Some(leaf_node_id)
        {
            return Err(stored_data_corrupt(
                "SessionGraph",
                format!("readable path does not end at leaf `{leaf_node_id}`"),
            ));
        }
        lash_core::SessionGraph::from_nodes(nodes, leaf_node_id)
            .map_err(|error| stored_data_corrupt("SessionGraph", error))
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

    pub async fn load_session_graph(&self) -> Result<lash_core::SessionGraph, StoreError> {
        let session_id = self.selected_session_id()?;
        self.conn
            .call(move |conn| {
                let leaf_node_id = conn
                    .query_row(
                        "SELECT leaf_node_id FROM session_head WHERE session_id = ?1",
                        params![session_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten();
                Self::load_session_graph_from_conn(conn, &session_id, leaf_node_id)
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
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
        let mut stmt = conn
            .prepare(
                "SELECT checkpoint_ref FROM session_head WHERE checkpoint_ref IS NOT NULL
                 UNION
                 SELECT checkpoint_ref FROM node_anchors",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        for row in rows {
            roots.push(RetainedArtifactRef {
                blob_ref: BlobRef(row.map_err(sqlite_error)?),
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
    pub(crate) fn gc_unreachable_in_tx(tx: &Transaction<'_>) -> Result<GcReport, StoreError> {
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
            // decode it we must not silently drop the keyed component blobs it
            // points at — doing so would delete blobs
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
            let content = decode_artifact_blob(&bytes)?.unwrap_or(bytes);
            let checkpoint = decode_checkpoint(&content)?;
            // GC interprets only the root's ref graph, never component bodies.
            // Retain refs even when a newer writer used an unknown component
            // codec so an older binary cannot turn incompatibility into loss.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn healthy_non_empty_whole_graph_validates_without_resident_leaf() {
        let store = Store::memory()
            .await
            .expect("open healthy whole-graph store");
        let session_id = "healthy-leafless-whole-graph";
        let mut state = lash_core::RuntimeSessionState {
            session_id: session_id.to_string(),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();
        state
            .session_graph
            .append_plugin("healthy-whole-graph", serde_json::json!({"second": true}));
        store
            .admit_and_bind_session(&lash_core::SessionBinding::root(session_id, &state.policy))
            .await
            .expect("bind healthy whole-graph session");
        store
            .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
                &state,
                &[],
            ))
            .await
            .expect("seed healthy whole-graph session");

        let session_id = session_id.to_string();
        let graph = store
            .conn
            .call(move |conn| {
                Store::load_session_graph_from_conn(conn, &session_id, None)
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
            .expect("healthy leafless whole graph loads");
        assert!(graph.nodes.len() >= 2);
        assert!(graph.leaf_node_id.is_none());
    }
}
