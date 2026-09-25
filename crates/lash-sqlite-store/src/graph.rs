//! Session-graph persistence and garbage collection on [`Store`].
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
use crate::session_sql::session_sql;
use crate::session_store_factory::retained_artifact_refs;
use lash_sansio::SessionId;

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

impl Store {
    pub(crate) fn load_session_graph_from_conn(
        conn: &Connection,
        session_id: &SessionId,
        leaf_node_id: Option<String>,
    ) -> Result<lash_core_execution::SessionGraph, StoreError> {
        Self::load_readable_graph_from_conn(conn, session_id, leaf_node_id, false)
    }

    pub(crate) fn load_active_path_session_graph_from_conn(
        conn: &Connection,
        session_id: &SessionId,
        leaf_node_id: Option<String>,
    ) -> Result<lash_core_execution::SessionGraph, StoreError> {
        let Some(leaf_node_id) = leaf_node_id else {
            return Ok(lash_core_execution::SessionGraph::default());
        };
        Self::load_readable_graph_from_conn(conn, session_id, Some(leaf_node_id), true)
    }

    fn load_readable_graph_from_conn(
        conn: &Connection,
        session_id: &SessionId,
        leaf_node_id: Option<String>,
        active_path_only: bool,
    ) -> Result<lash_core_execution::SessionGraph, StoreError> {
        let leaf_generation = match leaf_node_id.as_deref() {
            Some(leaf_node_id) => {
                let row = conn
                    .query_row(
                        session_sql().graph_sqlite.select_leaf_state.sql(),
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
        // One statement per filter shape, chosen exhaustively: a single
        // statement carrying `?2 IS NULL OR generation <= ?2` cannot use an
        // index for either shape, and this read is the whole session graph.
        let (statement, bound): (_, Vec<rusqlite::types::Value>) = match leaf_generation {
            None => (
                session_sql().graph_sqlite.select_readable.sql(),
                vec![rusqlite::types::Value::Text(
                    session_id.as_str().to_string(),
                )],
            ),
            Some(generation) => (
                session_sql()
                    .graph_sqlite
                    .select_readable_to_generation
                    .sql(),
                vec![
                    rusqlite::types::Value::Text(session_id.as_str().to_string()),
                    rusqlite::types::Value::Integer(generation),
                ],
            ),
        };
        let mut stmt = conn.prepare(statement).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bound), |row| {
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
            let node = lash_core_execution::SessionNodeRecord::decode_storage_body(
                node_id.clone(),
                parent_node_id,
                &node_json,
            )
            .map_err(|error| stored_data_corrupt("SessionGraph node", error))?;
            if matches!(
                node.payload,
                lash_core_execution::SessionNodePayload::FrameOpen { .. }
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
        lash_core_execution::SessionGraph::from_nodes(
            nodes,
            leaf_node_id.map(lash_core_execution::NodeId::from),
        )
        .map_err(|error| stored_data_corrupt("SessionGraph", error))
    }

    pub async fn load_session_graph(
        &self,
    ) -> Result<lash_core_execution::SessionGraph, StoreError> {
        let session_id = self.selected_session_id()?;
        self.conn
            .call(move |conn| {
                let leaf_node_id = conn
                    .query_row(
                        session_sql().head.select_leaf_node_id.sql(),
                        params![session_id.as_str()],
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

    /// Mark-and-sweep the blob table, answering in the maintenance outcome
    /// contract (ADR 0067 §4).
    ///
    /// The whole sweep runs in one `BEGIN IMMEDIATE` transaction, so a backend
    /// failure rolls every delete back and the partial report is empty *because
    /// no work survived* — never because the failure was absorbed into a clean
    /// zero report.
    pub async fn gc_unreachable(&self) -> lash_core_execution::MaintenanceResult<GcReport> {
        self.conn
            .write(|tx| {
                Self::gc_unreachable_in_tx(tx).map_err(|err| {
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

    /// Synchronous body of [`Store::gc_unreachable`], run on the connection thread
    /// inside the `BEGIN IMMEDIATE` transaction so the mark/sweep is atomic and
    /// holds the write lock for its duration.
    pub(crate) fn gc_unreachable_in_tx(tx: &Transaction<'_>) -> Result<GcReport, StoreError> {
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
            let checkpoint = decode_checkpoint(&content)?;
            // GC interprets only the root's ref graph, never component bodies.
            // Retain refs even when a newer writer used an unknown component
            // codec so an older binary cannot turn incompatibility into loss.
            stack.extend(retained_artifact_refs(&checkpoint));
        }
        // Match PostgreSQL's strict ordering even though SQLite's component
        // side is not FK-enforced: every dead root loses its complete outgoing
        // edge set before any hash-ordered blob delete can reach a component.
        tx.execute(session_sql().checkpoint_edges.delete_unrooted.sql(), [])
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
            tx.execute(
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
                    conn.execute(
                        "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
                         VALUES (?1, ?2, ?3)",
                        params![namespace, artifact_ref, blob_ref],
                    )?;
                }
                let roots = Store::artifact_ref_roots(conn).map_err(sqlite_conversion_error)?;
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
                conn.execute(
                    "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
                     VALUES ('foreign_namespace', 'ref-a', 'blob-a')",
                    [],
                )?;
                assert!(Store::artifact_ref_roots(conn).is_err());
                Ok(())
            })
            .await
            .expect("unknown namespace fails closed");
    }

    #[tokio::test]
    async fn healthy_non_empty_whole_graph_validates_without_resident_leaf() {
        let store = crate::test_support::memory_store()
            .await
            .expect("open healthy whole-graph store");
        let session_id = "healthy-leafless-whole-graph";
        let mut state = lash_core_execution::RuntimeSessionState {
            session_id: SessionId::from(session_id.to_string()),
            ..lash_core_execution::RuntimeSessionState::new(
                lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
            )
        };
        state.ensure_agent_frame_initialized();
        state
            .session_graph
            .append_plugin("healthy-whole-graph", serde_json::json!({"second": true}));
        store
            .admit_and_bind_session(&lash_core_execution::SessionBinding::root(session_id))
            .await
            .expect("bind healthy whole-graph session");
        store
            .commit_runtime_state(
                lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
            )
            .await
            .expect("seed healthy whole-graph session");

        let session_id = SessionId::from(session_id.to_string());
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
