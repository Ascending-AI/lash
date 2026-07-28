//! The [`RuntimePersistence`] capability-segment implementations for
//! [`Store`]: [`SessionCommitStore`], [`SessionExecutionLeaseStore`],
//! [`QueuedWorkStore`], [`TurnInputStore`], and [`StoreMaintenance`].
//!
//! This is the tokio-rusqlite port of the prior store's `persistence.rs`. The
//! public surface is byte-for-byte the prior store async trait: identical method
//! names and signatures, so consumers swap backends with a path rename only.
//!
//! The translation rules (see `conn.rs`, `lifecycle.rs`, `blobs.rs`):
//!
//! * Pure reads run through `self.conn.call(move |conn| { ... })`.
//! * Read-then-write paths run through `self.conn.write(move |tx| { ... })`
//!   (`BEGIN IMMEDIATE`, commit on `Ok`, rollback on `Err`) — this is the
//!   cross-process write-lock guard.
//! * Paths that may abandon partially-applied writes (the queued-work claim)
//!   run through `self.conn.write_flow`, deciding commit vs rollback via
//!   [`TxOutcome`].
//! * The shared `*_conn` helpers (`try_load_session_head_meta_from_conn`,
//!   `Self::put_checkpoint_conn`, `Self::load_usage_deltas_conn`,
//!   `Self::load_session_graph_from_conn`, the queued-work helpers, …) are
//!   synchronous and take a `&rusqlite::Connection`, so they are reused from
//!   inside these closures (a `&Transaction` derefs to `&Connection`).
//! * Closures must be `'static` + `Send`: every borrow of `self`/caller data is
//!   cloned into an owned value before being moved in.

use super::*;

const SQLITE_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE: &str = "session_id = ?1
       AND available_at_ms <= ?2
       AND (
            claim_token IS NULL
            OR claim_session_lease_generation <> ?3
       )";

fn sqlite_queued_work_head_candidate_cte(boundary: QueuedWorkClaimBoundary) -> String {
    let delivery_gate = match boundary {
        QueuedWorkClaimBoundary::Idle => "",
        QueuedWorkClaimBoundary::ActiveTurnCheckpoint => {
            "WHERE head_delivery_policy = 'earliest_safe_boundary'"
        }
    };
    format!(
        "queued_work_head_candidate AS (
            SELECT head_enqueue_seq, head_batch_id, head_delivery_policy
            FROM (
                SELECT enqueue_seq AS head_enqueue_seq,
                       batch_id AS head_batch_id,
                       delivery_policy AS head_delivery_policy
                FROM queued_work_batches
                WHERE {SQLITE_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
                ORDER BY enqueue_seq ASC
                LIMIT 1
            ) AS unfiltered_head
            {delivery_gate}
         )"
    )
}

fn sqlite_queued_work_claim_candidates_sql(boundary: QueuedWorkClaimBoundary) -> String {
    let head_candidate = sqlite_queued_work_head_candidate_cte(boundary);
    format!(
        "WITH {head_candidate}
         SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation
         FROM queued_work_batches
         CROSS JOIN queued_work_head_candidate
         WHERE {SQLITE_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
         ORDER BY enqueue_seq ASC
         LIMIT ?4"
    )
}

pub(crate) fn derived_node_refcount_conn(
    conn: &Connection,
    node_id: &str,
) -> Result<i64, StoreError> {
    derived_node_refcount_for_head_move_conn(conn, node_id, None, None)
}

fn derived_node_refcount_for_head_move_conn(
    conn: &Connection,
    node_id: &str,
    removed_head_root: Option<&str>,
    added_head_root: Option<&str>,
) -> Result<i64, StoreError> {
    let stored: i64 = conn
        .query_row(
            "SELECT
            (SELECT COUNT(*) FROM graph_nodes
             WHERE parent_node_id = ?1 AND tombstoned = 0)
          + (SELECT COUNT(*) FROM session_head WHERE leaf_node_id = ?1)
          + (SELECT COUNT(*) FROM node_anchors WHERE node_id = ?1)",
            params![node_id],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    Ok(stored - i64::from(removed_head_root == Some(node_id))
        + i64::from(added_head_root == Some(node_id)))
}

/// Remove one counted reference and reclaim a zero-count ancestry prefix.
///
/// A cached count that is too high only leaks storage and a later scrub can
/// repair it. A count that is too low can delete a prefix still shared by
/// another head; `vacuum` then makes that loss unrecoverable. Therefore every
/// destructive zero transition is re-derived from indexed edge/root rows in
/// this same transaction before the node is tombstoned. A head move applies
/// its pending old-root removal and new-root addition to that query, yielding
/// the transaction's post-commit root set before the CAS row is published.
pub(crate) fn decrement_node_ref_conn(
    conn: &Connection,
    first_node_id: &str,
) -> Result<(), StoreError> {
    decrement_node_ref_for_head_move_conn(conn, first_node_id, None, None)
}

fn decrement_node_ref_for_head_move_conn(
    conn: &Connection,
    first_node_id: &str,
    removed_head_root: Option<&str>,
    added_head_root: Option<&str>,
) -> Result<(), StoreError> {
    let mut node_id = first_node_id.to_string();
    loop {
        let row = conn
            .query_row(
                "UPDATE graph_nodes
                 SET incoming_refs = incoming_refs - 1
                 WHERE node_id = ?1 AND tombstoned = 0 AND incoming_refs > 0
                 RETURNING incoming_refs, parent_node_id",
                params![node_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some((cached, parent_node_id)) = row else {
            let derived = derived_node_refcount_for_head_move_conn(
                conn,
                &node_id,
                removed_head_root,
                added_head_root,
            )?;
            return Err(StoreError::NodeRefcountDrift {
                node_id,
                cached: 0,
                derived,
            });
        };
        if cached > 0 {
            return Ok(());
        }
        let derived = derived_node_refcount_for_head_move_conn(
            conn,
            &node_id,
            removed_head_root,
            added_head_root,
        )?;
        if derived != 0 {
            return Err(StoreError::NodeRefcountDrift {
                node_id,
                cached,
                derived,
            });
        }
        conn.execute(
            "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
            params![node_id],
        )
        .map_err(sqlite_error)?;
        let Some(parent_node_id) = parent_node_id else {
            return Ok(());
        };
        node_id = parent_node_id;
    }
}

pub(crate) fn nearest_frame_node_id_conn(
    conn: &Connection,
    leaf_node_id: &str,
) -> Result<Option<String>, StoreError> {
    conn.query_row(
        "WITH RECURSIVE ancestry(node_id, parent_node_id, node_json, depth) AS (
            SELECT node_id, parent_node_id, node_json, 0
            FROM graph_nodes
            WHERE node_id = ?1 AND tombstoned = 0
          UNION ALL
            SELECT parent.node_id, parent.parent_node_id, parent.node_json, ancestry.depth + 1
            FROM graph_nodes AS parent
            JOIN ancestry ON parent.node_id = ancestry.parent_node_id
            WHERE parent.tombstoned = 0
        )
        SELECT node_id FROM ancestry
        WHERE json_extract(node_json, '$.kind') = 'frame_open'
        ORDER BY depth ASC
        LIMIT 1",
        params![leaf_node_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(sqlite_error)
}

#[async_trait::async_trait]
impl SessionCommitStore for Store {
    async fn load_session(
        &self,
        scope: SessionReadScope,
    ) -> Result<Option<PersistedSessionRead>, StoreError> {
        let Some(session_id) = self.resolve_session_id_for_read().await? else {
            return Ok(None);
        };
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let outcome: Result<Option<PersistedSessionRead>, StoreError> = (|| {
                    let Some(meta) = try_load_session_head_meta_from_conn(&tx, &session_id)? else {
                        return Ok(None);
                    };
                    let leaf_node_id = match &scope {
                        SessionReadScope::FullGraph => meta.leaf_node_id.clone(),
                        SessionReadScope::ActivePath { leaf_node_id } => {
                            leaf_node_id.clone().or_else(|| meta.leaf_node_id.clone())
                        }
                    };
                    let mut graph = match scope {
                        SessionReadScope::FullGraph => Self::load_session_graph_from_conn(
                            &tx,
                            &session_id,
                            meta.leaf_node_id.clone(),
                        )?,
                        SessionReadScope::ActivePath { .. } => {
                            Self::load_active_path_session_graph_from_conn(
                                &tx,
                                &session_id,
                                leaf_node_id.clone(),
                            )
                            .map_err(sqlite_error)?
                        }
                    };
                    if !graph.nodes.is_empty() {
                        graph.set_leaf_node_id(leaf_node_id);
                    }
                    let checkpoint = meta
                        .checkpoint_ref
                        .as_ref()
                        .map(|blob_ref| Self::get_checkpoint_conn(&tx, blob_ref))
                        .transpose()?
                        .flatten();
                    Ok(Some(PersistedSessionRead {
                        session_id: meta.session_id,
                        head_revision: meta.head_revision,
                        config: meta.config,
                        current_frame_node_id: meta.current_frame_node_id,
                        graph,
                        checkpoint_ref: meta.checkpoint_ref,
                        checkpoint,
                        token_ledger: merge_token_ledger_entries(Self::load_usage_deltas_conn(
                            &tx,
                            &session_id,
                        )?),
                    }))
                })(
                );
                tx.commit()?;
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, StoreError> {
        let session_id = self.selected_session_id()?;
        let node_id = node_id.to_string();
        let row: Option<(String, Option<String>, String)> = self
            .conn
            .call(move |conn| {
                conn.query_row(
                    "WITH RECURSIVE ancestry(node_id, parent_node_id) AS (
                         SELECT node.node_id, node.parent_node_id
                         FROM graph_nodes node
                         JOIN session_head head ON head.leaf_node_id = node.node_id
                         WHERE head.session_id = ?2 AND node.tombstoned = 0
                         UNION ALL
                         SELECT parent.node_id, parent.parent_node_id
                         FROM graph_nodes parent
                         JOIN ancestry child ON parent.node_id = child.parent_node_id
                         WHERE parent.tombstoned = 0
                     )
                     SELECT node_id, parent_node_id, node_json FROM graph_nodes
                     WHERE node_id = ?1 AND tombstoned = 0
                       AND (
                           session_id = ?2
                           OR EXISTS (
                               SELECT 1 FROM ancestry WHERE ancestry.node_id = graph_nodes.node_id
                           )
                       )",
                    params![node_id, session_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
            })
            .await
            .map_err(sqlite_error)?;
        row.map(|(node_id, parent_node_id, node_json)| {
            lash_core::SessionNodeRecord::decode_storage_body(node_id, parent_node_id, &node_json)
                .map_err(|err| StoreError::Backend(format!("failed to decode graph node: {err}")))
        })
        .transpose()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError> {
        commit.validate_budget()?;
        commit.validate_operation_session()?;
        let commit_incarnation_id = commit
            .durable_incarnation_id("SQLite runtime commit")?
            .clone();
        self.bind_session(&commit.session_id)?;
        let realization_digest = lash_core::store::graph_realization_digest(&commit.graph);
        let realized_node_timestamps = commit
            .graph
            .appended_nodes()
            .map(|node| lash_core::store::RealizedNodeTimestamp {
                node_id: node.node_id.clone(),
                timestamp: node.timestamp.clone(),
            })
            .collect::<Vec<_>>();
        let blob_profile = self.options.blob_profile;
        let now = self.clock.timestamp_ms();
        let created_at = self.clock.timestamp_rfc3339();
        let enqueue_nonce_start = self.commit_count.fetch_add(
            commit.enqueued_queue_batches.len() as u64,
            AtomicOrdering::Relaxed,
        );
        let result = self
            .conn
            .write_flow(move |tx| {
                let outcome: Result<RuntimeCommitResult, StoreError> = (|| {
                    let existing =
                        try_load_session_head_meta_from_conn(tx, &commit.session_id)?;
                    if let Some(bound_session_id) =
                        existing.as_ref().map(|meta| meta.session_id.as_str())
                        && bound_session_id != commit.session_id
                    {
                        return Err(StoreError::SessionBindingMismatch {
                            bound_session_id: bound_session_id.to_string(),
                            attempted_session_id: commit.session_id.clone(),
                        });
                    }
                    tx.execute(
                        "INSERT OR IGNORE INTO session_meta
                         (session_id, incarnation_id, session_name, created_at, model, cwd, relation_json)
                         VALUES (?1, ?2, ?1, ?3, ?4, NULL, ?5)",
                        params![
                            commit.session_id,
                            commit_incarnation_id.as_str(),
                            created_at,
                            commit.config.model.id,
                            serde_json::to_string(&lash_core::SessionRelation::Root)
                                .map_err(|error| StoreError::Backend(error.to_string()))?,
                        ],
                    )
                    .map_err(sqlite_error)?;
                    let durable_incarnation_id = tx
                        .query_row(
                            "SELECT incarnation_id FROM session_meta WHERE session_id = ?1",
                            params![commit.session_id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .map_err(sqlite_error)?
                        .map(lash_core::IncarnationId::decode_from_store)
                        .ok_or_else(|| {
                            StoreError::Backend(format!(
                                "session `{}` has no durable session metadata",
                                commit.session_id
                            ))
                        })?;
                    commit.validate_node_derivation(&durable_incarnation_id)?;
                    if let Some(completed) = &commit.turn_commit {
                        let operation_key = completed.operation.storage_key()?;
                        let prior: Option<(String, String)> = tx
                            .query_row(
                                "SELECT turn_commit_hash, result_json FROM runtime_turn_commits
                                 WHERE session_id = ?1 AND turn_id = ?2",
                                params![completed.session_id, operation_key],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some((turn_commit_hash, result_json)) = prior {
                            if turn_commit_hash == completed.turn_commit_hash {
                                let result: RuntimeCommitResult =
                                    serde_json::from_str(&result_json).map_err(|err| {
                                        StoreError::Backend(format!(
                                            "failed to decode runtime turn commit result: {err}"
                                        ))
                                    })?;
                                if let Some(completion) =
                                    commit.release_session_execution_lease.as_ref()
                                {
                                    release_session_execution_lease_conn(tx, completion)?;
                                }
                                return Ok(result);
                            }
                            return Err(StoreError::RuntimeTurnCommitConflict {
                                session_id: completed.session_id.clone(),
                                turn_id: completed.operation.storage_key()?,
                            });
                        }
                    }
                    let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
                    let expected_revision = commit.expected_head_revision;
                    if expected_revision != actual_revision {
                        return Err(StoreError::HeadRevisionConflict {
                            expected: commit.expected_head_revision,
                            actual: actual_revision,
                        });
                    }
                    commit.validate_append_node_ids_unique()?;
                    commit.graph.validate_append_topology()?;
                    if let GraphCommitDelta::Append { nodes, .. } = &commit.graph {
                        for node in nodes {
                            let occupied = tx
                                .query_row(
                                    "SELECT 1 FROM graph_nodes WHERE node_id = ?1 LIMIT 1",
                                    params![node.node_id],
                                    |_| Ok(()),
                                )
                                .optional()
                                .map_err(sqlite_error)?
                                .is_some();
                            if occupied {
                                return Err(StoreError::NodeIdCollision {
                                    node_id: node.node_id.clone(),
                                });
                            }
                        }
                    }
                    if let Some(leaf_node_id) = commit.graph.leaf_node_id() {
                        let appended = matches!(
                            &commit.graph,
                            GraphCommitDelta::Append { nodes, .. }
                                if nodes.iter().any(|node| &node.node_id == leaf_node_id)
                        );
                        let live = tx
                            .query_row(
                                "SELECT 1 FROM graph_nodes
                                 WHERE node_id = ?1 AND tombstoned = 0
                                 LIMIT 1",
                                params![leaf_node_id],
                                |_| Ok(()),
                            )
                            .optional()
                            .map_err(sqlite_error)?
                            .is_some();
                        if !appended && !live {
                            return Err(StoreError::InvalidGraphLeaf {
                                leaf_node_id: Some(leaf_node_id.clone()),
                            });
                        }
                    } else {
                        let appends_nodes = matches!(
                            &commit.graph,
                            GraphCommitDelta::Append { nodes, .. } if !nodes.is_empty()
                        );
                        let has_live_nodes = tx
                            .query_row(
                                "SELECT 1 FROM graph_nodes
                                 WHERE session_id = ?1 AND tombstoned = 0
                                 LIMIT 1",
                                params![commit.session_id],
                                |_| Ok(()),
                            )
                            .optional()
                            .map_err(sqlite_error)?
                            .is_some();
                        if appends_nodes || has_live_nodes {
                            return Err(StoreError::InvalidGraphLeaf { leaf_node_id: None });
                        }
                    }
                    for completed in &commit.completed_queue_claims {
                        if completed.session_id != commit.session_id {
                            return Err(StoreError::QueuedWorkClaimSuperseded {
                                session_id: completed.session_id.clone(),
                                claim_id: completed.claim_id.clone(),
                            });
                        }
                        ensure_queued_work_completion_conn(tx, completed)?;
                    }
                    for completed in &commit.completed_turn_input_claims {
                        if completed.session_id != commit.session_id {
                            return Err(StoreError::TurnInputClaimSuperseded {
                                session_id: completed.session_id.clone(),
                                claim_id: completed.claim_id.clone(),
                            });
                        }
                        let owned_rows: usize = tx
                            .query_row(
                                "SELECT COUNT(*)
                                 FROM pending_turn_inputs
                                 WHERE session_id = ?1
                                   AND claim_id = ?2
                                   AND claim_token = ?3",
                                params![
                                    completed.session_id,
                                    completed.claim_id,
                                    completed.lease_token
                                ],
                                |row| row.get::<_, i64>(0),
                            )
                            .map_err(sqlite_error)? as usize;
                        ensure_turn_input_completion_owns_all_inputs(completed, owned_rows)?;
                    }

                    let stored_checkpoint =
                        Self::put_checkpoint_conn(tx, &commit.checkpoint, blob_profile)
                            .map_err(sqlite_error)?;

                    if !commit.usage_deltas.is_empty() {
                        let mut stmt = tx
                            .prepare(
                                "INSERT INTO usage_deltas (
                                    session_id, source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens
                                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            )
                            .map_err(sqlite_error)?;
                        for entry in &commit.usage_deltas {
                            stmt.execute(params![
                                commit.session_id,
                                entry.source,
                                entry.model,
                                entry.usage.input_tokens,
                                entry.usage.output_tokens,
                                entry.usage.cache_read_input_tokens,
                                entry.usage.cache_write_input_tokens,
                                entry.usage.reasoning_output_tokens,
                            ])
                            .map_err(sqlite_error)?;
                        }
                    }

                    let old_leaf_node_id = existing
                        .as_ref()
                        .and_then(|head| head.leaf_node_id.clone());
                    match &commit.graph {
                        GraphCommitDelta::Unchanged { leaf_node_id }
                            if leaf_node_id != &old_leaf_node_id =>
                        {
                            return Err(StoreError::InvalidGraphLeaf {
                                leaf_node_id: leaf_node_id.clone(),
                            });
                        }
                        GraphCommitDelta::Append { nodes, .. }
                            if !nodes.is_empty()
                                && nodes.first().and_then(|node| node.parent_node_id.as_ref())
                                != old_leaf_node_id.as_ref() =>
                        {
                            return Err(StoreError::InvalidGraphParent {
                                node_id: nodes
                                    .first()
                                    .map(|node| node.node_id.clone())
                                    .unwrap_or_default(),
                                expected: old_leaf_node_id.clone(),
                                actual: nodes
                                    .first()
                                    .and_then(|node| node.parent_node_id.clone()),
                            });
                        }
                        GraphCommitDelta::Append {
                            nodes,
                            leaf_node_id,
                        } if nodes.is_empty() && leaf_node_id != &old_leaf_node_id => {
                            return Err(StoreError::InvalidGraphLeaf {
                                leaf_node_id: leaf_node_id.clone(),
                            });
                        }
                        _ => {}
                    }
                    let leaf_node_id = match &commit.graph {
                        GraphCommitDelta::Unchanged { leaf_node_id } => leaf_node_id.clone(),
                        GraphCommitDelta::Append {
                            nodes,
                            leaf_node_id,
                        } => {
                            for node in nodes {
                                let node_json = node.encode_storage_body().map_err(|err| {
                                    StoreError::Backend(format!(
                                        "failed to encode graph node body: {err}"
                                    ))
                                })?;
                                tx.execute(
                                    "INSERT INTO graph_nodes
                                     (session_id, node_id, parent_node_id, node_json, incoming_refs)
                                     VALUES (?1, ?2, ?3, ?4, 0)",
                                    params![
                                        commit.session_id,
                                        node.node_id,
                                        node.parent_node_id,
                                        node_json
                                    ],
                                )
                                .map_err(sqlite_error)?;
                                if let Some(parent_node_id) = &node.parent_node_id {
                                    let changed = tx
                                        .execute(
                                            "UPDATE graph_nodes
                                             SET incoming_refs = incoming_refs + 1
                                             WHERE node_id = ?1 AND tombstoned = 0",
                                            params![parent_node_id],
                                        )
                                        .map_err(sqlite_error)?;
                                    if changed != 1 {
                                        return Err(StoreError::InvalidGraphParent {
                                            node_id: node.node_id.clone(),
                                            expected: node.parent_node_id.clone(),
                                            actual: None,
                                        });
                                    }
                                }
                            }
                            leaf_node_id.clone()
                        }
                    };
                    if old_leaf_node_id != leaf_node_id {
                        if let Some(new_leaf_node_id) = &leaf_node_id {
                            let changed = tx
                                .execute(
                                    "UPDATE graph_nodes
                                     SET incoming_refs = incoming_refs + 1
                                     WHERE node_id = ?1 AND tombstoned = 0",
                                    params![new_leaf_node_id],
                                )
                                .map_err(sqlite_error)?;
                            if changed != 1 {
                                return Err(StoreError::InvalidGraphLeaf {
                                    leaf_node_id: leaf_node_id.clone(),
                                });
                            }
                        }
                        if let Some(old_leaf_node_id) = &old_leaf_node_id {
                            decrement_node_ref_for_head_move_conn(
                                tx,
                                old_leaf_node_id,
                                Some(old_leaf_node_id.as_str()),
                                leaf_node_id.as_deref(),
                            )?;
                        }
                    }
                    let derived_frame_node_id = match leaf_node_id.as_deref() {
                        Some(leaf_node_id) => Some(
                            nearest_frame_node_id_conn(tx, leaf_node_id)?
                                .ok_or_else(|| StoreError::MissingFrameOpenAncestor {
                                    leaf_node_id: leaf_node_id.to_string(),
                                })?,
                        ),
                        None => None,
                    };
                    if commit.current_frame_node_id != derived_frame_node_id {
                        return Err(StoreError::Backend(format!(
                            "current_frame_node_id {:?} does not match nearest FrameOpen ancestor {:?}",
                            commit.current_frame_node_id, derived_frame_node_id
                        )));
                    }
                    let graph_node_count: usize = tx
                        .query_row(
                            "WITH RECURSIVE ancestry(node_id, parent_node_id) AS (
                                 SELECT node_id, parent_node_id FROM graph_nodes
                                 WHERE node_id = ?1 AND tombstoned = 0
                                 UNION ALL
                                 SELECT parent.node_id, parent.parent_node_id
                                 FROM graph_nodes parent
                                 JOIN ancestry ON parent.node_id = ancestry.parent_node_id
                                 WHERE parent.tombstoned = 0
                             )
                             SELECT COUNT(*) FROM ancestry",
                            params![leaf_node_id],
                            |row| row.get::<_, i64>(0),
                        )
                        .map_err(sqlite_error)? as usize;
                    let next_revision = actual_revision + 1;
                    let meta = SessionHeadMeta {
                        schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                        session_id: commit.session_id.clone(),
                        head_revision: next_revision,
                        config: commit.config.clone(),
                        current_frame_node_id: derived_frame_node_id,
                        checkpoint_ref: Some(stored_checkpoint.checkpoint_ref.clone()),
                        leaf_node_id,
                        graph_node_count,
                        token_ledger: Vec::new(),
                    };
                    tx.execute(
                        "INSERT OR REPLACE INTO session_head
                         (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            meta.session_id,
                            encode_json(&meta),
                            meta.head_revision as i64,
                            meta.leaf_node_id,
                            meta.checkpoint_ref.as_ref().map(BlobRef::as_str),
                        ],
                    )
                    .map_err(sqlite_error)?;
                    for completed in &commit.completed_queue_claims {
                        for batch_id in &completed.batch_ids {
                            tx.execute(
                                "DELETE FROM queued_work_batches
                                 WHERE session_id = ?1
                                   AND batch_id = ?2
                                   AND claim_id = ?3
                                   AND claim_token = ?4",
                                params![
                                    completed.session_id,
                                    batch_id,
                                    completed.claim_id,
                                    completed.lease_token
                                ],
                            )
                            .map_err(sqlite_error)?;
                        }
                    }
                    for completed in &commit.completed_turn_input_claims {
                        for input_id in &completed.input_ids {
                            tx.execute(
                                "UPDATE pending_turn_inputs
                                 SET state = ?5,
                                     claim_id = NULL,
                                     claim_owner_id = NULL,
                                     claim_owner_incarnation_id = NULL,
                                     claim_owner_liveness_json = NULL,
                                     claim_token = NULL,
                                     claim_session_lease_generation = 0
                                 WHERE session_id = ?1
                                   AND input_id = ?2
                                   AND claim_id = ?3
                                   AND claim_token = ?4",
                                params![
                                    completed.session_id,
                                    input_id,
                                    completed.claim_id,
                                    completed.lease_token,
                                    lash_core::TurnInputState::Completed.as_str(),
                                ],
                            )
                            .map_err(sqlite_error)?;
                        }
                    }
                    if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_deref() {
                        let input_ids = {
                            let mut stmt = tx
                                .prepare(
                                    "SELECT input_id, ingress_json
                                     FROM pending_turn_inputs
                                     WHERE session_id = ?1 AND state = ?2",
                                )
                                .map_err(sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![
                                        commit.session_id,
                                        lash_core::TurnInputState::PendingActive.as_str()
                                    ],
                                    |row| {
                                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                                    },
                                )
                                .map_err(sqlite_error)?;
                            let mut input_ids = Vec::new();
                            for row in rows {
                                let (input_id, ingress_json) = row.map_err(sqlite_error)?;
                                let ingress = decode_turn_input_ingress(ingress_json)?;
                                if ingress
                                    .active_turn_id()
                                    .is_some_and(|active| active == turn_id)
                                {
                                    input_ids.push(input_id);
                                }
                            }
                            input_ids
                        };
                        let next_turn_ingress = encode_json(&lash_core::TurnInputIngress::NextTurn);
                        let mut stmt = tx
                            .prepare(
                                "UPDATE pending_turn_inputs
                                 SET state = ?3,
                                     ingress_json = ?4,
                                     claim_id = NULL,
                                     claim_owner_id = NULL,
                                     claim_owner_incarnation_id = NULL,
                                     claim_owner_liveness_json = NULL,
                                     claim_token = NULL,
                                     claim_session_lease_generation = 0
                                 WHERE session_id = ?1 AND input_id = ?2",
                            )
                            .map_err(sqlite_error)?;
                        for input_id in input_ids {
                            stmt.execute(params![
                                commit.session_id,
                                input_id,
                                lash_core::TurnInputState::DeferredNextTurn.as_str(),
                                next_turn_ingress
                            ])
                            .map_err(sqlite_error)?;
                        }
                    }
                    if !commit.committed_attachment_ids.is_empty() {
                        let now = now as i64;
                        let mut stmt = tx
                            .prepare(
                                "UPDATE attachment_manifest
                                 SET committed_at_ms = COALESCE(committed_at_ms, ?1)
                                 WHERE attachment_id = ?2 AND session_id = ?3",
                            )
                            .map_err(sqlite_error)?;
                        for id in &commit.committed_attachment_ids {
                            stmt.execute(params![now, id.as_str(), commit.session_id])
                                .map_err(sqlite_error)?;
                        }
                    }
                    if let Some(turn_commit) = &commit.turn_commit
                        && let Some(turn_id) = turn_commit.operation.turn_id()
                    {
                        tx.execute(
                            "UPDATE attachment_manifest
                                 SET committed_at_ms = COALESCE(committed_at_ms, ?1)
                                 WHERE session_id = ?2
                                   AND owner_kind = 'turn'
                                   AND owner_id = ?3
                                   AND committed_at_ms IS NULL",
                            params![now as i64, commit.session_id, turn_id],
                        )
                        .map_err(sqlite_error)?;
                    }
                    let mut enqueued_queue_batches = Vec::new();
                    for (index, batch) in commit.enqueued_queue_batches.iter().enumerate() {
                        if batch.session_id != commit.session_id {
                            return Err(StoreError::SessionBindingMismatch {
                                bound_session_id: commit.session_id.clone(),
                                attempted_session_id: batch.session_id.clone(),
                            });
                        }
                        enqueued_queue_batches.push(enqueue_queued_work_conn(
                            tx,
                            batch,
                            now,
                            enqueue_nonce_start.saturating_add(index as u64),
                        )?);
                    }
                    let result = RuntimeCommitResult {
                        head_revision: next_revision,
                        checkpoint_ref: stored_checkpoint.checkpoint_ref,
                        manifest: stored_checkpoint.manifest,
                        realization_digest: realization_digest.clone(),
                        realized_node_timestamps: realized_node_timestamps.clone(),
                        enqueued_queue_batches,
                        turn_input_applications: commit.turn_input_applications(),
                    };
                    if let Some(completed) = &commit.turn_commit {
                        let operation_key = completed.operation.storage_key()?;
                        tx.execute(
                            "INSERT INTO runtime_turn_commits (
                                session_id, turn_id, turn_commit_hash, result_json, committed_at_ms
                             )
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                completed.session_id,
                                operation_key,
                                completed.turn_commit_hash,
                                encode_json(&result),
                                now as i64
                            ],
                        )
                        .map_err(sqlite_error)?;
                    }
                    if let Some(completion) = commit.release_session_execution_lease.as_ref() {
                        release_session_execution_lease_conn(tx, completion)?;
                    }
                    Ok(result)
                })();
                // Roll back on a `StoreError` so a failure after the first
                // write (e.g. a head-revision conflict surfaced mid-commit, or a
                // backend write error) does not leave the partial transaction
                // committed, while still carrying the typed error to the caller.
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)??;
        self.maybe_auto_gc().await;
        Ok(result)
    }

    async fn ensure_session_incarnation(
        &self,
        session_id: &str,
        policy: &lash_core::SessionPolicy,
    ) -> Result<lash_core::IncarnationId, StoreError> {
        self.bind_session(session_id)?;
        let session_id = session_id.to_string();
        let candidate_value = lash_core::IncarnationId::mint_for_store()
            .as_str()
            .to_string();
        let created_at = self.clock.timestamp_rfc3339();
        let model = policy.model.id.clone();
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|path| path.to_str().map(str::to_string));
        let relation_json = serde_json::to_string(&lash_core::SessionRelation::Root)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let durable = self
            .conn
            .write(move |tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO session_meta
                     (session_id, incarnation_id, session_name, created_at, model, cwd, relation_json)
                     VALUES (?1, ?2, ?1, ?3, ?4, ?5, ?6)",
                    params![
                        session_id,
                        candidate_value,
                        created_at,
                        model,
                        cwd,
                        relation_json,
                    ],
                )?;
                tx.query_row(
                    "SELECT incarnation_id FROM session_meta WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, String>(0),
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(lash_core::IncarnationId::decode_from_store(durable))
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        Store::save_session_meta(self, meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        Ok(Store::load_session_meta(self).await)
    }
}

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for Store {
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let session_id = session_id.to_string();
        let owner = owner.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<SessionExecutionLeaseClaimOutcome, StoreError> = (|| {
                    let current = load_session_execution_lease_row_conn(tx, &session_id)?;
                    if current.as_ref().is_some_and(|lease| {
                        lease.lease_token.is_some() && lease.expires_at_ms > now
                    }) {
                        let current = current.expect("checked current lease is present");
                        if current
                            .owner
                            .as_ref()
                            .is_some_and(|current_owner| current_owner.same_incarnation(&owner))
                        {
                            let expires_at = now.saturating_add(lease_ttl_ms);
                            tx.execute(
                                "UPDATE session_execution_leases
                                 SET lease_expires_at_ms = ?2
                                 WHERE session_id = ?1",
                                params![session_id, expires_at as i64],
                            )
                            .map_err(sqlite_error)?;
                            return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                                SessionExecutionLease {
                                    session_id,
                                    owner,
                                    lease_token: current.lease_token.expect("live lease token set"),
                                    fencing_token: current.fencing_token,
                                    claimed_at_epoch_ms: current.claimed_at_ms,
                                    expires_at_epoch_ms: expires_at,
                                },
                            ));
                        }
                        return Ok(SessionExecutionLeaseClaimOutcome::Busy {
                            holder: row_to_session_execution_lease(&session_id, current)?,
                        });
                    }
                    Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                        acquire_session_execution_lease_conn(
                            tx,
                            &session_id,
                            &owner,
                            current.as_ref().map_or(0, |lease| lease.fencing_token),
                            now,
                            lease_ttl_ms,
                        )?,
                    ))
                })(
                );
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn reclaim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        observed_holder: &SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let session_id = session_id.to_string();
        let owner = owner.clone();
        let observed_holder = observed_holder.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<SessionExecutionLeaseClaimOutcome, StoreError> = (|| {
                    let current = load_session_execution_lease_row_conn(tx, &session_id)?;
                    let Some(current) = current else {
                        return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                            acquire_session_execution_lease_conn(
                                tx,
                                &session_id,
                                &owner,
                                0,
                                now,
                                lease_ttl_ms,
                            )?,
                        ));
                    };
                    if current.lease_token.is_none() || current.expires_at_ms <= now {
                        return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                            acquire_session_execution_lease_conn(
                                tx,
                                &session_id,
                                &owner,
                                current.fencing_token,
                                now,
                                lease_ttl_ms,
                            )?,
                        ));
                    }
                    let holder = row_to_session_execution_lease(&session_id, current)?;
                    if observed_holder.session_id == session_id
                        && holder.owner.same_incarnation(&observed_holder.owner)
                        && holder.lease_token == observed_holder.lease_token
                        && holder.fencing_token == observed_holder.fencing_token
                        && holder.owner.is_definitely_dead_for_claimant(&owner)
                    {
                        let fencing_token = holder.fencing_token.saturating_add(1);
                        let lease_token = format!(
                            "{}:{}:{}:{now}:{fencing_token}",
                            session_id, owner.owner_id, owner.incarnation_id
                        );
                        let expires_at = now.saturating_add(lease_ttl_ms);
                        let liveness_json = encode_liveness(&owner.liveness)?;
                        let changed = tx
                            .execute(
                                "UPDATE session_execution_leases
                                 SET lease_owner_id = ?1,
                                     lease_owner_incarnation_id = ?2,
                                     lease_owner_liveness_json = ?3,
                                     lease_token = ?4,
                                     lease_fencing_token = ?5,
                                     lease_claimed_at_ms = ?6,
                                     lease_expires_at_ms = ?7
                                 WHERE session_id = ?8
                                   AND lease_owner_id = ?9
                                   AND lease_owner_incarnation_id = ?10
                                   AND lease_token = ?11
                                   AND lease_fencing_token = ?12",
                                params![
                                    owner.owner_id,
                                    owner.incarnation_id,
                                    liveness_json,
                                    lease_token,
                                    fencing_token as i64,
                                    now as i64,
                                    expires_at as i64,
                                    session_id,
                                    observed_holder.owner.owner_id,
                                    observed_holder.owner.incarnation_id,
                                    observed_holder.lease_token,
                                    observed_holder.fencing_token as i64,
                                ],
                            )
                            .map_err(sqlite_error)?;
                        if changed == 1 {
                            return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                                SessionExecutionLease {
                                    session_id,
                                    owner,
                                    lease_token,
                                    fencing_token,
                                    claimed_at_epoch_ms: now,
                                    expires_at_epoch_ms: expires_at,
                                },
                            ));
                        }
                        let current = load_session_execution_lease_row_conn(tx, &session_id)?;
                        if current.as_ref().is_some_and(|lease| {
                            lease.lease_token.is_some() && lease.expires_at_ms > now
                        }) {
                            let current = current.expect("checked current lease is present");
                            return Ok(SessionExecutionLeaseClaimOutcome::Busy {
                                holder: row_to_session_execution_lease(&session_id, current)?,
                            });
                        }
                        let previous_fencing_token =
                            current.as_ref().map_or(0, |lease| lease.fencing_token);
                        return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                            acquire_session_execution_lease_conn(
                                tx,
                                &session_id,
                                &owner,
                                previous_fencing_token,
                                now,
                                lease_ttl_ms,
                            )?,
                        ));
                    }
                    Ok(SessionExecutionLeaseClaimOutcome::Busy { holder })
                })(
                );
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        let fence = fence.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<SessionExecutionLease, StoreError> = (|| {
                    let current = load_session_execution_lease_row_conn(tx, &fence.session_id)?;
                    let Some(current) = current else {
                        return Err(StoreError::SessionExecutionLeaseExpired {
                            session_id: fence.session_id.clone(),
                        });
                    };
                    if !current
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner.same_incarnation(&fence.owner))
                        || current.lease_token.as_deref() != Some(fence.lease_token.as_str())
                        || current.fencing_token != fence.fencing_token
                        || current.expires_at_ms <= now
                    {
                        return Err(StoreError::SessionExecutionLeaseExpired {
                            session_id: fence.session_id.clone(),
                        });
                    }
                    let expires_at = now.saturating_add(lease_ttl_ms);
                    tx.execute(
                        "UPDATE session_execution_leases
                         SET lease_expires_at_ms = ?5
                         WHERE session_id = ?1
                           AND lease_owner_id = ?2
                           AND lease_owner_incarnation_id = ?3
                           AND lease_token = ?4
                           AND lease_fencing_token = ?6",
                        params![
                            fence.session_id,
                            fence.owner.owner_id,
                            fence.owner.incarnation_id,
                            fence.lease_token,
                            expires_at as i64,
                            fence.fencing_token as i64
                        ],
                    )
                    .map_err(sqlite_error)?;
                    Ok(SessionExecutionLease {
                        session_id: fence.session_id,
                        owner: fence.owner,
                        lease_token: fence.lease_token,
                        fencing_token: fence.fencing_token,
                        claimed_at_epoch_ms: current.claimed_at_ms,
                        expires_at_epoch_ms: expires_at,
                    })
                })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseCompletion,
    ) -> Result<(), StoreError> {
        let completion = completion.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = release_session_execution_lease_conn(tx, &completion);
                match outcome {
                    Ok(()) => Ok(TxOutcome::Commit(Ok(()))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }
}

#[async_trait::async_trait]
impl QueuedWorkStore for Store {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        let nonce = self.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = enqueue_queued_work_conn(tx, &batch, now, nonce);
                // Roll back the partially-inserted batch/items on a
                // `StoreError` while still returning the typed error.
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        let session_id = session_id.to_string();
        let session_execution_lease = session_execution_lease.clone();
        let owner = owner.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<TxOutcome<Option<QueuedWorkClaim>>, StoreError> = (|| {
                    ensure_session_execution_lease_conn(
                        tx,
                        &session_id,
                        &session_execution_lease,
                        now,
                    )?;
                    // The fence is validated live, so its fencing token is the
                    // currently-live session-lease generation; claims pin it and
                    // are claimable only across a different generation (ADR 0029).
                    let generation = session_execution_lease.fencing_token;
                    let candidate_rows = {
                        let mut stmt = tx
                            .prepare(&sqlite_queued_work_claim_candidates_sql(
                                QueuedWorkClaimBoundary::Idle,
                            ))
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(
                                params![
                                    session_id,
                                    now as i64,
                                    generation as i64,
                                    claim_scan_limit(1)
                                ],
                                queued_batch_row_from_sql,
                            )
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    let candidate_rows = candidate_rows
                        .into_iter()
                        .filter(|row| {
                            row.claim_token.is_none()
                                || row.claim_session_lease_generation != generation
                        })
                        .collect::<Vec<_>>();
                    let candidate_batches = candidate_rows
                        .iter()
                        .map(|row| queued_work_batch_from_conn(tx, row.clone()))
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    let candidates = candidate_rows
                        .iter()
                        .zip(candidate_batches.iter())
                        .map(|(row, batch)| {
                            Ok(ClaimCandidate {
                                enqueue_seq: row.enqueue_seq,
                                claim_fencing_token: row.claim_fencing_token,
                                work_class: batch.work_class().ok_or_else(|| {
                                    StoreError::Backend(format!(
                                        "queued-work batch `{}` has mixed or empty payload classes",
                                        batch.batch_id
                                    ))
                                })?,
                                delivery_policy: decode_delivery_policy(
                                    row.delivery_policy.clone(),
                                )?,
                                slot_policy: decode_slot_policy(row.slot_policy.clone())?,
                                merge_key: decode_merge_key(row.merge_key_json.clone())?,
                            })
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    let selected_len = select_leading_session_command(&candidates);
                    if selected_len == 0 {
                        return Ok(TxOutcome::Commit(None));
                    }
                    let mut selected = candidate_rows;
                    selected.truncate(selected_len);
                    let mut selected_batches = candidate_batches;
                    selected_batches.truncate(selected_len);
                    let lease = QueuedWorkClaimLease::derive(
                        &candidates[0],
                        &session_id,
                        &owner,
                        now,
                        generation,
                    );
                    let liveness_json = encode_liveness(&owner.liveness)?;
                    for row in &selected {
                        let claimed = tx
                            .execute(
                                "UPDATE queued_work_batches
                                 SET claim_id = ?3,
                                     claim_owner_id = ?4,
                                     claim_owner_incarnation_id = ?5,
                                     claim_owner_liveness_json = ?6,
                                     claim_token = ?7,
                                     claim_fencing_token = claim_fencing_token + 1,
                                     claim_session_lease_generation = ?8
                                 WHERE session_id = ?1
                                   AND batch_id = ?2
                                   AND (
                                        claim_token IS NULL
                                        OR claim_session_lease_generation <> ?8
                                   )",
                                params![
                                    session_id,
                                    row.batch_id,
                                    lease.claim_id,
                                    owner.owner_id.as_str(),
                                    owner.incarnation_id.as_str(),
                                    liveness_json.as_str(),
                                    lease.lease_token,
                                    lease.session_lease_generation as i64,
                                ],
                            )
                            .map_err(sqlite_error)?;
                        if claimed == 0 {
                            return Ok(TxOutcome::Rollback(None));
                        }
                    }
                    Ok(TxOutcome::Commit(Some(QueuedWorkClaim {
                        session_id: session_id.clone(),
                        claim_id: lease.claim_id,
                        owner: owner.clone(),
                        lease_token: lease.lease_token,
                        fencing_token: lease.fencing_token,
                        session_lease_generation: lease.session_lease_generation,
                        batches: selected_batches,
                    })))
                })(
                );
                match outcome {
                    Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
                    Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        max_batches: usize,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        if max_batches == 0 {
            return Ok(None);
        }
        let session_id = session_id.to_string();
        let session_execution_lease = session_execution_lease.clone();
        let owner = owner.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<TxOutcome<Option<QueuedWorkClaim>>, StoreError> = (|| {
                    ensure_session_execution_lease_conn(
                        tx,
                        &session_id,
                        &session_execution_lease,
                        now,
                    )?;
                    let generation = session_execution_lease.fencing_token;
                    let candidate_rows = {
                        let mut stmt = tx
                            .prepare(&sqlite_queued_work_claim_candidates_sql(boundary))
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(
                                params![
                                    session_id,
                                    now as i64,
                                    generation as i64,
                                    claim_scan_limit(max_batches)
                                ],
                                queued_batch_row_from_sql,
                            )
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    let candidate_rows = candidate_rows
                        .into_iter()
                        .filter(|row| {
                            row.claim_token.is_none()
                                || row.claim_session_lease_generation != generation
                        })
                        .collect::<Vec<_>>();
                    let candidate_batches = queued_work_batches_from_conn(tx, &candidate_rows)?;
                    let candidates = candidate_rows
                        .iter()
                        .zip(candidate_batches.iter())
                        .map(|(row, batch)| {
                            Ok(ClaimCandidate {
                                enqueue_seq: row.enqueue_seq,
                                claim_fencing_token: row.claim_fencing_token,
                                work_class: batch.work_class().ok_or_else(|| {
                                    StoreError::Backend(format!(
                                        "queued-work batch `{}` has mixed or empty payload classes",
                                        batch.batch_id
                                    ))
                                })?,
                                delivery_policy: decode_delivery_policy(
                                    row.delivery_policy.clone(),
                                )?,
                                slot_policy: decode_slot_policy(row.slot_policy.clone())?,
                                merge_key: decode_merge_key(row.merge_key_json.clone())?,
                            })
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    let selected_len =
                        select_turn_work_claim_prefix(&candidates, boundary, max_batches);
                    if selected_len == 0 {
                        return Ok(TxOutcome::Commit(None));
                    }
                    let mut selected = candidate_rows;
                    selected.truncate(selected_len);
                    let mut selected_batches = candidate_batches;
                    selected_batches.truncate(selected_len);
                    let lease = QueuedWorkClaimLease::derive(
                        &candidates[0],
                        &session_id,
                        &owner,
                        now,
                        generation,
                    );
                    let liveness_json = encode_liveness(&owner.liveness)?;
                    for row in &selected {
                        // Under `BEGIN IMMEDIATE` this connection already holds
                        // the write lock, but the row could still have been
                        // claimed by an earlier committed writer (its
                        // `claim_token` set and not yet expired). The `WHERE`
                        // clause filters those out, so a 0-row update means we
                        // lost the race for this batch: treat the whole claim as
                        // not-won rather than returning a claim that doesn't
                        // actually own the row.
                        let claimed = tx
                            .execute(
                                "UPDATE queued_work_batches
                                 SET claim_id = ?3,
                                     claim_owner_id = ?4,
                                     claim_owner_incarnation_id = ?5,
                                     claim_owner_liveness_json = ?6,
                                     claim_token = ?7,
                                     claim_fencing_token = claim_fencing_token + 1,
                                     claim_session_lease_generation = ?8
                                 WHERE session_id = ?1
                                   AND batch_id = ?2
                                   AND (
                                        claim_token IS NULL
                                        OR claim_session_lease_generation <> ?8
                                   )",
                                params![
                                    session_id,
                                    row.batch_id,
                                    lease.claim_id,
                                    owner.owner_id.as_str(),
                                    owner.incarnation_id.as_str(),
                                    liveness_json.as_str(),
                                    lease.lease_token,
                                    lease.session_lease_generation as i64,
                                ],
                            )
                            .map_err(sqlite_error)?;
                        if claimed == 0 {
                            // Lost the race for this batch. Roll back any sibling
                            // rows we already claimed in this transaction so we
                            // never return a half-owned claim.
                            return Ok(TxOutcome::Rollback(None));
                        }
                    }
                    Ok(TxOutcome::Commit(Some(QueuedWorkClaim {
                        session_id: session_id.clone(),
                        claim_id: lease.claim_id,
                        owner: owner.clone(),
                        lease_token: lease.lease_token,
                        fencing_token: lease.fencing_token,
                        session_lease_generation: lease.session_lease_generation,
                        batches: selected_batches,
                    })))
                })(
                );
                // Lower a `StoreError` into the rollback arm so the closure body
                // can keep using `?` while still propagating the error to the
                // caller. Encode it as a `Result` carried out of the flow.
                match outcome {
                    Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
                    Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        max_batches: usize,
    ) -> Result<(Option<lash_core::TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        #[cfg(test)]
        self.checkpoint_probe_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = self.clock.timestamp_ms();
        if !checkpoint_work_pending_sqlite(
            &self.conn,
            now,
            session_id,
            session_execution_lease.fencing_token,
            turn_id,
            checkpoint,
            max_inputs,
            max_batches,
        )
        .await?
        {
            return Ok((None, None));
        }

        #[cfg(test)]
        self.checkpoint_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session_id = session_id.to_string();
        let session_execution_lease = session_execution_lease.clone();
        let owner = owner.clone();
        let turn_id = turn_id.to_string();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<
                    TxOutcome<(Option<lash_core::TurnInputClaim>, Option<QueuedWorkClaim>)>,
                    StoreError,
                > = (|| {
                    ensure_session_execution_lease_conn(
                        tx,
                        &session_id,
                        &session_execution_lease,
                        now,
                    )?;
                    let input = claim_pending_turn_inputs_sqlite_conn(
                        tx,
                        now,
                        &session_id,
                        &session_execution_lease,
                        &owner,
                        max_inputs,
                        lash_core::TurnInputClaimMode::ActiveTurn {
                            turn_id,
                            checkpoint,
                        },
                    )?;
                    let input = match input {
                        TxOutcome::Commit(input) => input,
                        TxOutcome::Rollback(input) => {
                            return Ok(TxOutcome::Rollback((input, None)));
                        }
                    };
                    let queued = claim_ready_queued_work_sqlite_conn(
                        tx,
                        now,
                        &session_id,
                        &session_execution_lease,
                        &owner,
                        QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                        max_batches,
                    )?;
                    match queued {
                        TxOutcome::Commit(queued) => Ok(TxOutcome::Commit((input, queued))),
                        TxOutcome::Rollback(queued) => Ok(TxOutcome::Rollback((None, queued))),
                    }
                })();
                match outcome {
                    Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
                    Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[String],
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        if batch_ids.is_empty() {
            return Ok(None);
        }
        let session_id = session_id.to_string();
        let fence = session_execution_lease.clone();
        let owner = owner.clone();
        let batch_ids = batch_ids.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Option<QueuedWorkClaim>, StoreError> = (|| {
                    ensure_session_execution_lease_conn(tx, &session_id, &fence, now)?;
                    let generation = fence.fencing_token;
                    let mut rows = Vec::new();
                    let mut batches = Vec::new();
                    for batch_id in &batch_ids {
                        let row = tx
                            .query_row(
                                "SELECT enqueue_seq, batch_id, session_id, source_key,
                                        delivery_policy, slot_policy, merge_key_json,
                                        available_at_ms, enqueued_at_ms, claim_fencing_token,
                                        claim_owner_id, claim_owner_incarnation_id,
                                        claim_owner_liveness_json, claim_token,
                                        claim_session_lease_generation
                                 FROM queued_work_batches
                                 WHERE session_id = ?1 AND batch_id = ?2
                                   AND available_at_ms <= ?3
                                   AND (claim_token IS NULL
                                        OR claim_session_lease_generation <> ?4)",
                                params![session_id, batch_id, now as i64, generation as i64],
                                queued_batch_row_from_sql,
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        let Some(row) = row else {
                            return Ok(None);
                        };
                        let batch = queued_work_batch_from_conn(tx, row.clone())?;
                        if batch.work_class() != Some(lash_core::runtime::QueuedWorkClass::TurnWork)
                        {
                            return Ok(None);
                        }
                        rows.push(row);
                        batches.push(batch);
                    }
                    let candidates = rows
                        .iter()
                        .map(|row| {
                            Ok(ClaimCandidate {
                                enqueue_seq: row.enqueue_seq,
                                claim_fencing_token: row.claim_fencing_token,
                                work_class: lash_core::runtime::QueuedWorkClass::TurnWork,
                                delivery_policy: decode_delivery_policy(
                                    row.delivery_policy.clone(),
                                )?,
                                slot_policy: decode_slot_policy(row.slot_policy.clone())?,
                                merge_key: decode_merge_key(row.merge_key_json.clone())?,
                            })
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    if select_turn_work_claim_prefix(&candidates, boundary, candidates.len())
                        != candidates.len()
                    {
                        return Ok(None);
                    }
                    let lease = QueuedWorkClaimLease::derive(
                        &candidates[0],
                        &session_id,
                        &owner,
                        now,
                        generation,
                    );
                    let owner_liveness_json = encode_liveness(&owner.liveness)?;
                    for row in &rows {
                        let changed = tx
                            .execute(
                                "UPDATE queued_work_batches
                                 SET claim_id = ?3, claim_owner_id = ?4,
                                     claim_owner_incarnation_id = ?5,
                                     claim_owner_liveness_json = ?6, claim_token = ?7,
                                     claim_fencing_token = claim_fencing_token + 1,
                                     claim_session_lease_generation = ?8
                                 WHERE session_id = ?1 AND batch_id = ?2
                                   AND (claim_token IS NULL
                                        OR claim_session_lease_generation <> ?8)",
                                params![
                                    session_id,
                                    row.batch_id,
                                    lease.claim_id,
                                    owner.owner_id,
                                    owner.incarnation_id,
                                    owner_liveness_json,
                                    lease.lease_token,
                                    generation as i64,
                                ],
                            )
                            .map_err(sqlite_error)?;
                        if changed != 1 {
                            return Ok(None);
                        }
                    }
                    Ok(Some(QueuedWorkClaim {
                        session_id,
                        claim_id: lease.claim_id,
                        owner,
                        lease_token: lease.lease_token,
                        fencing_token: lease.fencing_token,
                        session_lease_generation: lease.session_lease_generation,
                        batches,
                    }))
                })();
                match outcome {
                    Ok(Some(value)) => Ok(TxOutcome::Commit(Ok(Some(value)))),
                    Ok(None) => Ok(TxOutcome::Rollback(Ok(None))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        let session_id = claim.session_id.clone();
        let claim_id = claim.claim_id.clone();
        let lease_token = claim.lease_token.clone();
        self.conn
            .write(move |tx| {
                tx.execute(
                    "UPDATE queued_work_batches
                     SET claim_id = NULL,
                         claim_owner_id = NULL,
                         claim_owner_incarnation_id = NULL,
                         claim_owner_liveness_json = NULL,
                         claim_token = NULL,
                         claim_session_lease_generation = 0
                     WHERE session_id = ?1 AND claim_id = ?2 AND claim_token = ?3",
                    params![session_id, claim_id, lease_token],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[QueuedWorkClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        let mut sql = "UPDATE queued_work_batches
             SET claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_owner_liveness_json = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE (session_id, claim_id, claim_token) IN ("
            .to_string();
        let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(claims.len() * 3);
        for (index, claim) in claims.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            sql.push_str("(?, ?, ?)");
            values.push(claim.session_id.clone().into());
            values.push(claim.claim_id.clone().into());
            values.push(claim.lease_token.clone().into());
        }
        sql.push(')');
        self.conn
            .write(move |tx| tx.execute(&sql, rusqlite::params_from_iter(values.iter())))
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        let session_id = session_id.to_string();
        let batch_id = batch_id.to_string();
        let now = self.clock.timestamp_ms() as i64;
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Option<QueuedWorkBatch>, StoreError> = (|| {
                    let row = tx
                        .query_row(
                            "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                                    slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                                    claim_owner_liveness_json, claim_token, claim_session_lease_generation
                             FROM queued_work_batches
                             WHERE session_id = ?1
                               AND batch_id = ?2
                               AND (claim_token IS NULL OR NOT EXISTS (
                                        SELECT 1 FROM session_execution_leases sel
                                        WHERE sel.session_id = ?1
                                          AND sel.lease_token IS NOT NULL
                                          AND sel.lease_expires_at_ms > ?3
                                          AND sel.lease_fencing_token
                                              = queued_work_batches.claim_session_lease_generation
                                   ))",
                            params![session_id, batch_id, now],
                            queued_batch_row_from_sql,
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    let Some(row) = row else {
                        return Ok(None);
                    };
                    let batch = queued_work_batch_from_conn(tx, row)?;
                    tx.execute(
                        "DELETE FROM queued_work_batches
                         WHERE session_id = ?1
                           AND batch_id = ?2
                           AND (claim_token IS NULL OR NOT EXISTS (
                                SELECT 1 FROM session_execution_leases sel
                                WHERE sel.session_id = ?1
                                  AND sel.lease_token IS NOT NULL
                                  AND sel.lease_expires_at_ms > ?3
                                  AND sel.lease_fencing_token
                                      = queued_work_batches.claim_session_lease_generation
                           ))",
                        params![session_id, batch_id, now],
                    )
                    .map_err(sqlite_error)?;
                    Ok(Some(batch))
                })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_queued_work(&self, session_id: &str) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |conn| {
                let outcome: Result<Vec<QueuedWorkBatch>, StoreError> = (|| {
                    let rows = {
                        let mut stmt = conn
                            .prepare(
                                "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                                        slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                                        claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                                 FROM queued_work_batches
                                 WHERE session_id = ?1
                                 ORDER BY enqueue_seq ASC",
                            )
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(params![session_id], queued_batch_row_from_sql)
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    rows.into_iter()
                        .map(|row| queued_work_batch_from_conn(conn, row))
                        .collect()
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let session_id = session_id.to_string();
        let now = self.clock.timestamp_ms();
        self.conn
            .call(move |conn| {
                let outcome: Result<Vec<QueuedWorkBatch>, StoreError> = (|| {
                    let rows = {
                        let mut stmt = conn
                            .prepare(
                                "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                                        slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                                        claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                                 FROM queued_work_batches
                                 WHERE session_id = ?1
                                   AND (claim_token IS NULL OR NOT EXISTS (
                                        SELECT 1 FROM session_execution_leases sel
                                        WHERE sel.session_id = ?1
                                          AND sel.lease_token IS NOT NULL
                                          AND sel.lease_expires_at_ms > ?2
                                          AND sel.lease_fencing_token
                                              = queued_work_batches.claim_session_lease_generation
                                   ))
                                 ORDER BY enqueue_seq ASC",
                            )
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(
                                params![session_id, now as i64],
                                queued_batch_row_from_sql,
                            )
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    rows.into_iter()
                        .map(|row| queued_work_batch_from_conn(conn, row))
                        .collect()
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }
}

#[async_trait::async_trait]
impl TurnInputStore for Store {
    async fn enqueue_pending_turn_input(
        &self,
        draft: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, StoreError> {
        let nonce = self.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core::PendingTurnInput, StoreError> = (|| {
                    if let Some(source_key) = draft.source_key.as_deref() {
                        let existing_id: Option<String> = tx
                            .query_row(
                                "SELECT input_id
                                 FROM pending_turn_inputs
                                 WHERE session_id = ?1 AND source_key = ?2",
                                params![draft.session_id, source_key],
                                |row| row.get(0),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some(input_id) = existing_id {
                            let existing = load_pending_turn_input_by_id_conn(
                                tx,
                                &draft.session_id,
                                &input_id,
                            )?
                            .ok_or_else(|| {
                                StoreError::Backend(
                                    "pending turn input source row disappeared".to_string(),
                                )
                            })?;
                            if !draft.submitted_content_matches(&existing).map_err(|err| {
                                StoreError::Backend(format!(
                                    "failed to compare pending turn input submission: {err}"
                                ))
                            })? {
                                return Err(StoreError::PendingTurnInputSourceKeyConflict {
                                    session_id: draft.session_id.clone(),
                                    source_key: source_key.to_string(),
                                    existing_input_id: existing.input_id.clone(),
                                });
                            }
                            return Ok(existing);
                        }
                    }
                    let input_id = draft.input_id.clone().unwrap_or_else(|| {
                        derive_pending_turn_input_id(
                            &draft.session_id,
                            draft.source_key.as_deref(),
                            now,
                            nonce,
                        )
                    });
                    let state = match draft.ingress {
                        lash_core::TurnInputIngress::ActiveTurn { .. } => {
                            lash_core::TurnInputState::PendingActive
                        }
                        lash_core::TurnInputIngress::NextTurn => {
                            lash_core::TurnInputState::DeferredNextTurn
                        }
                    };
                    tx.execute(
                        "INSERT INTO pending_turn_inputs (
                            input_id, session_id, source_key, ingress_json, state,
                            input_json, enqueued_at_ms
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            input_id,
                            draft.session_id,
                            draft.source_key.as_deref(),
                            encode_json(&draft.ingress),
                            state.as_str(),
                            encode_json(&draft.input),
                            now as i64,
                        ],
                    )
                    .map_err(sqlite_error)?;
                    load_pending_turn_input_by_id_conn(tx, &draft.session_id, &input_id)?
                        .ok_or_else(|| {
                            StoreError::Backend("pending turn input insert disappeared".to_string())
                        })
                })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::PendingTurnInput>, StoreError> {
        let session_id = session_id.to_string();
        let now = self.clock.timestamp_ms();
        self.conn
            .call(move |conn| {
                let outcome: Result<Vec<lash_core::PendingTurnInput>, StoreError> = (|| {
                    let rows = {
                        let mut stmt = conn
                            .prepare(
                                "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                                        claim_owner_id, claim_owner_incarnation_id,
                                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                                 FROM pending_turn_inputs
                                 WHERE session_id = ?1
                                   AND state IN (?2, ?3)
                                   AND (claim_token IS NULL OR NOT EXISTS (
                                        SELECT 1 FROM session_execution_leases sel
                                        WHERE sel.session_id = ?1
                                          AND sel.lease_token IS NOT NULL
                                          AND sel.lease_expires_at_ms > ?4
                                          AND sel.lease_fencing_token
                                              = pending_turn_inputs.claim_session_lease_generation
                                   ))
                                 ORDER BY enqueue_seq ASC",
                            )
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(
                                params![
                                    session_id,
                                    lash_core::TurnInputState::PendingActive.as_str(),
                                    lash_core::TurnInputState::DeferredNextTurn.as_str(),
                                    now as i64
                                ],
                                pending_turn_input_row_from_sql,
                            )
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    rows.into_iter().map(pending_turn_input_from_row).collect()
                })(
                );
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::TurnInputApplication>, StoreError> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |conn| {
                let outcome = (|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT turn_id, result_json
                             FROM runtime_turn_commits
                             WHERE session_id = ?1",
                        )
                        .map_err(sqlite_error)?;
                    let rows = stmt
                        .query_map(params![session_id], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(sqlite_error)?;
                    let mut commits = Vec::new();
                    for row in rows {
                        let (turn_id, result_json) = row.map_err(sqlite_error)?;
                        let result: RuntimeCommitResult = serde_json::from_str(&result_json)
                            .map_err(|err| {
                                StoreError::Backend(format!(
                                    "failed to decode runtime turn commit result: {err}"
                                ))
                            })?;
                        commits.push((
                            result.head_revision,
                            turn_id,
                            result.turn_input_applications,
                        ));
                    }
                    commits.sort_by(|left, right| {
                        (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str()))
                    });
                    Ok(commits
                        .into_iter()
                        .flat_map(|(_, _, applications)| applications)
                        .collect())
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelResult>, StoreError> {
        let session_id = session_id.to_string();
        let targets = targets.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Vec<lash_core::PendingTurnInputCancelResult>, StoreError> =
                    (|| {
                        let mut results = Vec::with_capacity(targets.len());
                        for target in targets {
                            let outcome = match load_pending_turn_input_row_by_target_conn(
                                tx,
                                &session_id,
                                &target,
                            )? {
                                Some(row) => cancel_pending_turn_input_row_conn(tx, row, now)?,
                                None => lash_core::PendingTurnInputCancelOutcome::NotFound,
                            };
                            results
                                .push(lash_core::PendingTurnInputCancelResult { target, outcome });
                        }
                        Ok(results)
                    })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let session_id = session_id.to_string();
        let anchor = anchor.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core::PendingTurnInputSuffixCancelOutcome, StoreError> =
                    (|| {
                        let Some(anchor_row) =
                            load_pending_turn_input_row_by_target_conn(tx, &session_id, &anchor)?
                        else {
                            return Ok(
                                lash_core::PendingTurnInputSuffixCancelOutcome::AnchorNotFound {
                                    anchor,
                                },
                            );
                        };
                        let rows = {
                            let mut stmt = tx
                                .prepare(
                                    "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                                            state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                                            claim_owner_id, claim_owner_incarnation_id,
                                            claim_owner_liveness_json, claim_token, claim_session_lease_generation
                                     FROM pending_turn_inputs
                                     WHERE session_id = ?1 AND enqueue_seq >= ?2
                                     ORDER BY enqueue_seq ASC",
                                )
                                .map_err(sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![session_id, anchor_row.enqueue_seq as i64],
                                    pending_turn_input_row_from_sql,
                                )
                                .map_err(sqlite_error)?;
                            rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                        };
                        let mut outcomes = Vec::with_capacity(rows.len());
                        for row in rows {
                            outcomes.push(cancel_pending_turn_input_row_conn(tx, row, now)?);
                        }
                        Ok(lash_core::PendingTurnInputSuffixCancelOutcome::Outcomes {
                            anchor,
                            outcomes,
                        })
                    })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        claim_pending_turn_inputs_sqlite(
            &self.conn,
            self.clock.timestamp_ms(),
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.to_string(),
                checkpoint,
            },
        )
        .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        claim_pending_turn_inputs_sqlite(
            &self.conn,
            self.clock.timestamp_ms(),
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::NextTurn,
        )
        .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::TurnInputClaim,
    ) -> Result<(), StoreError> {
        let session_id = claim.session_id.clone();
        let claim_id = claim.claim_id.clone();
        let lease_token = claim.lease_token.clone();
        let restored_state = match claim.mode {
            lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        self.conn
            .write(move |tx| {
                tx.execute(
                    "UPDATE pending_turn_inputs
                     SET state = CASE
                             WHEN state = ?4 THEN ?5
                             ELSE state
                         END,
                         claim_id = NULL,
                         claim_owner_id = NULL,
                         claim_owner_incarnation_id = NULL,
                         claim_owner_liveness_json = NULL,
                         claim_token = NULL,
                         claim_session_lease_generation = 0
                     WHERE session_id = ?1 AND claim_id = ?2 AND claim_token = ?3",
                    params![
                        session_id,
                        claim_id,
                        lease_token,
                        lash_core::TurnInputState::Accepted.as_str(),
                        restored_state.as_str(),
                    ],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn abandon_turn_input_claims(
        &self,
        claims: &[lash_core::TurnInputClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        let mut sql = "UPDATE pending_turn_inputs
             SET state = CASE
                     WHEN state = 'accepted' THEN 'pending_active'
                     ELSE state
                 END,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_owner_liveness_json = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE (session_id, claim_id, claim_token) IN ("
            .to_string();
        let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(claims.len() * 3);
        for (index, claim) in claims.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            sql.push_str("(?, ?, ?)");
            values.push(claim.session_id.clone().into());
            values.push(claim.claim_id.clone().into());
            values.push(claim.lease_token.clone().into());
        }
        sql.push(')');
        self.conn
            .write(move |tx| tx.execute(&sql, rusqlite::params_from_iter(values.iter())))
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl StoreMaintenance for Store {
    async fn tombstone_nodes(&self, ids: &[String]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let session_id = self.session_id.get().cloned();
        let ids = ids.to_vec();
        self.conn
            .write(move |tx| {
                for id in &ids {
                    let row = tx
                        .query_row(
                            "SELECT incoming_refs, parent_node_id
                             FROM graph_nodes
                             WHERE node_id = ?1 AND tombstoned = 0
                               AND (?2 IS NULL OR session_id = ?2)",
                            params![id, session_id],
                            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                        )
                        .optional()?;
                    let Some((cached, parent_node_id)) = row else {
                        continue;
                    };
                    let derived = derived_node_refcount_conn(tx, id)
                        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
                    if cached != derived {
                        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                            StoreError::NodeRefcountDrift {
                                node_id: id.clone(),
                                cached,
                                derived,
                            },
                        )));
                    }
                    if derived != 0 {
                        continue;
                    }
                    tx.execute(
                        "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
                        params![id],
                    )?;
                    if let Some(parent_node_id) = parent_node_id {
                        decrement_node_ref_conn(tx, &parent_node_id).map_err(|err| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(err))
                        })?;
                    }
                }
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn vacuum(&self) -> Result<VacuumReport, StoreError> {
        let session_id = self.session_id.get().cloned();
        let (removed_node_count, removed_pending_turn_input_tombstone_count) = self
            .conn
            .write(move |tx| {
                let removed_node_count = if let Some(session_id) = session_id.as_deref() {
                    tx.execute(
                        "DELETE FROM graph_nodes
                         WHERE session_id = ?1 AND tombstoned = 1",
                        params![session_id],
                    )?
                } else {
                    tx.execute("DELETE FROM graph_nodes WHERE tombstoned = 1", [])?
                };
                let removed_pending_turn_input_tombstone_count =
                    if let Some(session_id) = session_id.as_deref() {
                        tx.execute(
                            "DELETE FROM pending_turn_inputs
                             WHERE session_id = ?1 AND state IN (?2, ?3)",
                            params![
                                session_id,
                                lash_core::TurnInputState::Cancelled.as_str(),
                                lash_core::TurnInputState::Completed.as_str()
                            ],
                        )?
                    } else {
                        tx.execute(
                            "DELETE FROM pending_turn_inputs
                             WHERE state IN (?1, ?2)",
                            params![
                                lash_core::TurnInputState::Cancelled.as_str(),
                                lash_core::TurnInputState::Completed.as_str()
                            ],
                        )?
                    };
                Ok((
                    removed_node_count,
                    removed_pending_turn_input_tombstone_count,
                ))
            })
            .await
            .map_err(sqlite_error)?;
        Ok(VacuumReport {
            removed_node_count,
            removed_pending_turn_input_tombstone_count,
        })
    }

    async fn gc_unreachable(&self) -> Result<GcReport, StoreError> {
        Ok(Store::gc_unreachable(self).await)
    }

    async fn verify_node_refcounts(&self) -> Result<NodeRefcountVerification, StoreError> {
        self.conn
            .write(|conn| {
                let outcome: Result<NodeRefcountVerification, StoreError> = (|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT node_id, incoming_refs
                             FROM graph_nodes
                             WHERE tombstoned = 0
                             ORDER BY node_id",
                        )
                        .map_err(sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })
                        .map_err(sqlite_error)?;
                    let mut checked_node_count = 0;
                    for row in rows {
                        let (node_id, cached) = row.map_err(sqlite_error)?;
                        let derived = derived_node_refcount_conn(conn, &node_id)?;
                        if cached != derived {
                            return Err(StoreError::NodeRefcountDrift {
                                node_id,
                                cached,
                                derived,
                            });
                        }
                        checked_node_count += 1;
                    }
                    Ok(NodeRefcountVerification { checked_node_count })
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }
}

fn derive_pending_turn_input_id(
    session_id: &str,
    source_key: Option<&str>,
    now_epoch_ms: u64,
    nonce: u64,
) -> String {
    format!(
        "ti:{:x}",
        Sha256::digest(format!("{session_id}:{source_key:?}:{now_epoch_ms}:{nonce}").as_bytes())
    )
}

fn cancel_pending_turn_input_row_conn(
    conn: &Connection,
    row: PendingTurnInputRow,
    now_epoch_ms: u64,
) -> Result<lash_core::PendingTurnInputCancelOutcome, StoreError> {
    let mut input = pending_turn_input_from_row(row.clone())?;
    match input.state {
        lash_core::TurnInputState::Cancelled => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCancelled(input),
        ),
        lash_core::TurnInputState::Completed => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCompleted(input),
        ),
        lash_core::TurnInputState::Accepted => {
            Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                claim: pending_turn_input_claim_diagnostics_from_row(&row, input.state),
                input,
            })
        }
        lash_core::TurnInputState::PendingActive | lash_core::TurnInputState::DeferredNextTurn => {
            // A claim is live only while the session-execution-lease generation it
            // pins still holds the session lease (ADR 0029).
            let live_claim = row.claim_token.is_some()
                && load_session_execution_lease_row_conn(conn, &row.session_id)?.is_some_and(
                    |lease| {
                        lease.lease_token.is_some()
                            && lease.expires_at_ms > now_epoch_ms
                            && lease.fencing_token == row.claim_session_lease_generation
                    },
                );
            if live_claim {
                return Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                    claim: pending_turn_input_claim_diagnostics_from_row(&row, input.state),
                    input,
                });
            }
            conn.execute(
                "UPDATE pending_turn_inputs
                 SET state = ?3,
                     claim_id = NULL,
                     claim_owner_id = NULL,
                     claim_owner_incarnation_id = NULL,
                     claim_owner_liveness_json = NULL,
                     claim_token = NULL,
                     claim_session_lease_generation = 0
                 WHERE session_id = ?1 AND input_id = ?2",
                params![
                    row.session_id,
                    row.input_id,
                    lash_core::TurnInputState::Cancelled.as_str(),
                ],
            )
            .map_err(sqlite_error)?;
            input.state = lash_core::TurnInputState::Cancelled;
            Ok(lash_core::PendingTurnInputCancelOutcome::Cancelled(input))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn checkpoint_work_pending_sqlite(
    conn: &SqliteConnection,
    now: u64,
    session_id: &str,
    generation: u64,
    turn_id: &str,
    checkpoint: lash_core::CheckpointKind,
    max_inputs: usize,
    max_batches: usize,
) -> Result<bool, StoreError> {
    if max_inputs == 0 && max_batches == 0 {
        return Ok(false);
    }
    let session_id = session_id.to_string();
    let turn_id = turn_id.to_string();
    conn.call(move |conn| {
        let outcome: Result<bool, StoreError> = (|| {
            let head_candidate = sqlite_queued_work_head_candidate_cte(
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            );
            let sql = format!(
                "WITH {head_candidate}
                 SELECT (
                    ?7 > 0 AND EXISTS (
                        SELECT 1
                        FROM pending_turn_inputs
                        WHERE session_id = ?1
                          AND state = ?4
                          AND (claim_token IS NULL OR claim_session_lease_generation <> ?3)
                          AND json_extract(ingress_json, '$.scope') = 'active_turn'
                          AND json_extract(ingress_json, '$.turn_id') = ?5
                          AND (
                              ?6 = 'before_completion'
                              OR COALESCE(
                                  json_extract(ingress_json, '$.min_boundary'),
                                  'after_work'
                              ) = 'after_work'
                          )
                        LIMIT 1
                    )
                ) OR (
                    ?8 > 0 AND EXISTS (
                        SELECT 1
                        FROM queued_work_head_candidate AS head
                        JOIN queued_work_items AS item
                          ON item.batch_id = head.head_batch_id
                        WHERE json_extract(item.payload_json, '$.type') <> 'session_command'
                        LIMIT 1
                    )
                )"
            );
            let pending: i64 = conn
                .query_row(
                    &sql,
                    params![
                        session_id,
                        now as i64,
                        generation as i64,
                        lash_core::TurnInputState::PendingActive.as_str(),
                        turn_id,
                        match checkpoint {
                            lash_core::CheckpointKind::AfterWork => "after_work",
                            lash_core::CheckpointKind::BeforeCompletion => "before_completion",
                        },
                        max_inputs as i64,
                        max_batches as i64,
                    ],
                    |row| row.get(0),
                )
                .map_err(sqlite_error)?;
            Ok(pending != 0)
        })();
        Ok(outcome)
    })
    .await
    .map_err(sqlite_error)?
}

#[allow(clippy::too_many_arguments)]
fn claim_ready_queued_work_sqlite_conn(
    tx: &Connection,
    now: u64,
    session_id: &str,
    session_execution_lease: &SessionExecutionLeaseFence,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    max_batches: usize,
) -> Result<TxOutcome<Option<QueuedWorkClaim>>, StoreError> {
    if max_batches == 0 {
        return Ok(TxOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let candidate_rows = {
        let mut stmt = tx
            .prepare(&sqlite_queued_work_claim_candidates_sql(boundary))
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id,
                    now as i64,
                    generation as i64,
                    claim_scan_limit(max_batches)
                ],
                queued_batch_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let candidate_rows = candidate_rows
        .into_iter()
        .filter(|row| row.claim_token.is_none() || row.claim_session_lease_generation != generation)
        .collect::<Vec<_>>();
    let candidate_batches = candidate_rows
        .iter()
        .map(|row| queued_work_batch_from_conn(tx, row.clone()))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let candidates = candidate_rows
        .iter()
        .zip(candidate_batches.iter())
        .map(|(row, batch)| {
            Ok(ClaimCandidate {
                enqueue_seq: row.enqueue_seq,
                claim_fencing_token: row.claim_fencing_token,
                work_class: batch.work_class().ok_or_else(|| {
                    StoreError::Backend(format!(
                        "queued-work batch `{}` has mixed or empty payload classes",
                        batch.batch_id
                    ))
                })?,
                delivery_policy: decode_delivery_policy(row.delivery_policy.clone())?,
                slot_policy: decode_slot_policy(row.slot_policy.clone())?,
                merge_key: decode_merge_key(row.merge_key_json.clone())?,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let selected_len = select_turn_work_claim_prefix(&candidates, boundary, max_batches);
    if selected_len == 0 {
        return Ok(TxOutcome::Commit(None));
    }
    let mut selected = candidate_rows;
    selected.truncate(selected_len);
    let mut selected_batches = candidate_batches;
    selected_batches.truncate(selected_len);
    let lease = QueuedWorkClaimLease::derive(&candidates[0], session_id, owner, now, generation);
    let liveness_json = encode_liveness(&owner.liveness)?;
    for row in &selected {
        let claimed = tx
            .execute(
                "UPDATE queued_work_batches
                 SET claim_id = ?3,
                     claim_owner_id = ?4,
                     claim_owner_incarnation_id = ?5,
                     claim_owner_liveness_json = ?6,
                     claim_token = ?7,
                     claim_fencing_token = claim_fencing_token + 1,
                     claim_session_lease_generation = ?8
                 WHERE session_id = ?1
                   AND batch_id = ?2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?8
                   )",
                params![
                    session_id,
                    row.batch_id,
                    lease.claim_id,
                    owner.owner_id.as_str(),
                    owner.incarnation_id.as_str(),
                    liveness_json.as_str(),
                    lease.lease_token,
                    lease.session_lease_generation as i64,
                ],
            )
            .map_err(sqlite_error)?;
        if claimed == 0 {
            return Ok(TxOutcome::Rollback(None));
        }
    }
    Ok(TxOutcome::Commit(Some(QueuedWorkClaim {
        session_id: session_id.to_string(),
        claim_id: lease.claim_id,
        owner: owner.clone(),
        lease_token: lease.lease_token,
        fencing_token: lease.fencing_token,
        session_lease_generation: lease.session_lease_generation,
        batches: selected_batches,
    })))
}

#[allow(clippy::too_many_arguments)]
fn claim_pending_turn_inputs_sqlite_conn(
    tx: &Connection,
    now: u64,
    session_id: &str,
    session_execution_lease: &SessionExecutionLeaseFence,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<TxOutcome<Option<lash_core::TurnInputClaim>>, StoreError> {
    if max_inputs == 0 {
        return Ok(TxOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let wanted_state = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
            lash_core::TurnInputState::PendingActive
        }
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let candidate_rows = {
        let mut sql = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM pending_turn_inputs
                 WHERE session_id = ? AND state = ?
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?
                   )"
        .to_string();
        let mut values: Vec<rusqlite::types::Value> = vec![
            session_id.to_string().into(),
            wanted_state.as_str().to_string().into(),
            (generation as i64).into(),
        ];
        if let lash_core::TurnInputClaimMode::ActiveTurn {
            turn_id,
            checkpoint,
        } = &mode
        {
            sql.push_str(
                " AND json_extract(ingress_json, '$.scope') = 'active_turn'
                  AND json_extract(ingress_json, '$.turn_id') = ?",
            );
            values.push(turn_id.clone().into());
            if *checkpoint == lash_core::CheckpointKind::AfterWork {
                sql.push_str(
                    " AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work') = 'after_work'",
                );
            }
        }
        sql.push_str(" ORDER BY enqueue_seq ASC LIMIT ?");
        values.push(i64::try_from(max_inputs).unwrap_or(i64::MAX).into());
        let mut stmt = tx.prepare(&sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(values.iter()),
                pending_turn_input_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let selected = candidate_rows
        .into_iter()
        .take(max_inputs)
        .map(|row| Ok((row.clone(), pending_turn_input_from_row(row)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let Some((head, _)) = selected.first() else {
        return Ok(TxOutcome::Commit(None));
    };
    let lease = TurnInputClaimLease::derive(head, session_id, owner, now, generation);
    let liveness_json = encode_liveness(&owner.liveness)?;
    let state_after_claim = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => lash_core::TurnInputState::Accepted,
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut inputs = Vec::new();
    for (row, mut input) in selected {
        let claimed = tx
            .execute(
                "UPDATE pending_turn_inputs
                 SET state = ?3,
                     claim_id = ?4,
                     claim_owner_id = ?5,
                     claim_owner_incarnation_id = ?6,
                     claim_owner_liveness_json = ?7,
                     claim_token = ?8,
                     claim_fencing_token = claim_fencing_token + 1,
                     claim_session_lease_generation = ?9
                 WHERE session_id = ?1
                   AND input_id = ?2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?9
                   )",
                params![
                    session_id,
                    row.input_id,
                    state_after_claim.as_str(),
                    lease.claim_id,
                    owner.owner_id.as_str(),
                    owner.incarnation_id.as_str(),
                    liveness_json.as_str(),
                    lease.lease_token,
                    lease.session_lease_generation as i64,
                ],
            )
            .map_err(sqlite_error)?;
        if claimed == 0 {
            return Ok(TxOutcome::Rollback(None));
        }
        input.state = state_after_claim;
        inputs.push(input);
    }
    Ok(TxOutcome::Commit(Some(lash_core::TurnInputClaim {
        session_id: session_id.to_string(),
        claim_id: lease.claim_id,
        owner: owner.clone(),
        lease_token: lease.lease_token,
        fencing_token: lease.fencing_token,
        session_lease_generation: lease.session_lease_generation,
        mode,
        inputs,
        applications: Vec::new(),
    })))
}

async fn claim_pending_turn_inputs_sqlite(
    conn: &SqliteConnection,
    now: u64,
    session_id: &str,
    session_execution_lease: &SessionExecutionLeaseFence,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
    if max_inputs == 0 {
        return Ok(None);
    }
    let session_id = session_id.to_string();
    let session_execution_lease = session_execution_lease.clone();
    let owner = owner.clone();
    conn.write_flow(move |tx| {
        let outcome: Result<TxOutcome<Option<lash_core::TurnInputClaim>>, StoreError> = (|| {
            ensure_session_execution_lease_conn(
                tx,
                &session_id,
                &session_execution_lease,
                now,
            )?;
            let generation = session_execution_lease.fencing_token;
            let wanted_state = match &mode {
                lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
                    lash_core::TurnInputState::PendingActive
                }
                lash_core::TurnInputClaimMode::NextTurn => {
                    lash_core::TurnInputState::DeferredNextTurn
                }
            };
            let candidate_rows = {
                let mut sql =
                    "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                            state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                            claim_owner_id, claim_owner_incarnation_id,
                            claim_owner_liveness_json, claim_token, claim_session_lease_generation
                     FROM pending_turn_inputs
                     WHERE session_id = ? AND state = ?
                       AND (
                            claim_token IS NULL
                            OR claim_session_lease_generation <> ?
                       )"
                    .to_string();
                let mut values: Vec<rusqlite::types::Value> = vec![
                    session_id.clone().into(),
                    wanted_state.as_str().to_string().into(),
                    (generation as i64).into(),
                ];
                if let lash_core::TurnInputClaimMode::ActiveTurn {
                    turn_id,
                    checkpoint,
                } = &mode
                {
                    sql.push_str(
                        " AND json_extract(ingress_json, '$.scope') = 'active_turn'
                          AND json_extract(ingress_json, '$.turn_id') = ?",
                    );
                    values.push(turn_id.clone().into());
                    if *checkpoint == lash_core::CheckpointKind::AfterWork {
                        sql.push_str(
                            " AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work') = 'after_work'",
                        );
                    }
                }
                sql.push_str(" ORDER BY enqueue_seq ASC LIMIT ?");
                values.push(i64::try_from(max_inputs).unwrap_or(i64::MAX).into());
                let mut stmt = tx
                    .prepare(&sql)
                    .map_err(sqlite_error)?;
                let rows = stmt
                    .query_map(
                        rusqlite::params_from_iter(values.iter()),
                        pending_turn_input_row_from_sql,
                    )
                    .map_err(sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
            };
            let selected = candidate_rows
                .into_iter()
                .take(max_inputs)
                .map(|row| Ok((row.clone(), pending_turn_input_from_row(row)?)))
                .collect::<Result<Vec<_>, StoreError>>()?;
            let Some((head, _)) = selected.first() else {
                return Ok(TxOutcome::Commit(None));
            };
            let lease = TurnInputClaimLease::derive(head, &session_id, &owner, now, generation);
            let liveness_json = encode_liveness(&owner.liveness)?;
            let state_after_claim = match &mode {
                lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
                    lash_core::TurnInputState::Accepted
                }
                lash_core::TurnInputClaimMode::NextTurn => {
                    lash_core::TurnInputState::DeferredNextTurn
                }
            };
            let mut inputs = Vec::new();
            for (row, mut input) in selected {
                let claimed = tx
                    .execute(
                        "UPDATE pending_turn_inputs
                         SET state = ?3,
                             claim_id = ?4,
                             claim_owner_id = ?5,
                             claim_owner_incarnation_id = ?6,
                             claim_owner_liveness_json = ?7,
                             claim_token = ?8,
                             claim_fencing_token = claim_fencing_token + 1,
                             claim_session_lease_generation = ?9
                         WHERE session_id = ?1
                           AND input_id = ?2
                           AND (
                                claim_token IS NULL
                                OR claim_session_lease_generation <> ?9
                           )",
                        params![
                            session_id,
                            row.input_id,
                            state_after_claim.as_str(),
                            lease.claim_id,
                            owner.owner_id.as_str(),
                            owner.incarnation_id.as_str(),
                            liveness_json.as_str(),
                            lease.lease_token,
                            lease.session_lease_generation as i64,
                        ],
                    )
                    .map_err(sqlite_error)?;
                if claimed == 0 {
                    return Ok(TxOutcome::Rollback(None));
                }
                input.state = state_after_claim;
                inputs.push(input);
            }
            Ok(TxOutcome::Commit(Some(lash_core::TurnInputClaim {
                session_id: session_id.clone(),
                claim_id: lease.claim_id,
                owner: owner.clone(),
                lease_token: lease.lease_token,
                fencing_token: lease.fencing_token,
                session_lease_generation: lease.session_lease_generation,
                mode,
                inputs,
                applications: Vec::new(),
            })))
        })(
        );
        match outcome {
            Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
            Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
            Err(err) => Ok(TxOutcome::Rollback(Err(err))),
        }
    })
    .await
    .map_err(sqlite_error)?
}

struct SessionExecutionLeaseRow {
    owner: Option<LeaseOwnerIdentity>,
    lease_token: Option<String>,
    fencing_token: u64,
    claimed_at_ms: u64,
    expires_at_ms: u64,
}

fn load_session_execution_lease_row_conn(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<SessionExecutionLeaseRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT lease_owner_id, lease_token, lease_fencing_token,
                    lease_claimed_at_ms, lease_expires_at_ms,
                    lease_owner_incarnation_id, lease_owner_liveness_json
             FROM session_execution_leases
             WHERE session_id = ?1",
            params![session_id],
            |row| {
                let owner_id: Option<String> = row.get(0)?;
                let incarnation_id: Option<String> = row.get(5)?;
                let liveness_json: Option<String> = row.get(6)?;
                Ok(SessionExecutionLeaseRow {
                    owner: lease_owner_from_columns(owner_id, incarnation_id, liveness_json),
                    lease_token: row.get(1)?,
                    fencing_token: row.get::<_, i64>(2)? as u64,
                    claimed_at_ms: row.get::<_, i64>(3)? as u64,
                    expires_at_ms: row.get::<_, i64>(4)? as u64,
                })
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    Ok(row)
}

fn lease_owner_from_columns(
    owner_id: Option<String>,
    incarnation_id: Option<String>,
    liveness_json: Option<String>,
) -> Option<LeaseOwnerIdentity> {
    owner_id.map(|owner_id| LeaseOwnerIdentity {
        incarnation_id: incarnation_id.unwrap_or_else(|| owner_id.clone()),
        owner_id,
        liveness: liveness_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or(LeaseOwnerLiveness::Opaque),
    })
}

fn encode_liveness(liveness: &LeaseOwnerLiveness) -> Result<String, StoreError> {
    serde_json::to_string(liveness)
        .map_err(|err| StoreError::Backend(format!("failed to encode lease liveness: {err}")))
}

fn row_to_session_execution_lease(
    session_id: &str,
    row: SessionExecutionLeaseRow,
) -> Result<SessionExecutionLease, StoreError> {
    Ok(SessionExecutionLease {
        session_id: session_id.to_string(),
        owner: row
            .owner
            .ok_or_else(|| StoreError::Backend("live session lease missing owner".to_string()))?,
        lease_token: row.lease_token.ok_or_else(|| {
            StoreError::Backend("live session lease missing lease token".to_string())
        })?,
        fencing_token: row.fencing_token,
        claimed_at_epoch_ms: row.claimed_at_ms,
        expires_at_epoch_ms: row.expires_at_ms,
    })
}

fn acquire_session_execution_lease_conn(
    conn: &Connection,
    session_id: &str,
    owner: &LeaseOwnerIdentity,
    previous_fencing_token: u64,
    now: u64,
    lease_ttl_ms: u64,
) -> Result<SessionExecutionLease, StoreError> {
    let fencing_token = previous_fencing_token.saturating_add(1);
    let lease_token = format!(
        "{}:{}:{}:{now}:{fencing_token}",
        session_id, owner.owner_id, owner.incarnation_id
    );
    let expires_at = now.saturating_add(lease_ttl_ms);
    let liveness_json = encode_liveness(&owner.liveness)?;
    conn.execute(
        "INSERT INTO session_execution_leases (
            session_id, lease_owner_id, lease_owner_incarnation_id, lease_owner_liveness_json,
            lease_token, lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(session_id) DO UPDATE SET
            lease_owner_id = excluded.lease_owner_id,
            lease_owner_incarnation_id = excluded.lease_owner_incarnation_id,
            lease_owner_liveness_json = excluded.lease_owner_liveness_json,
            lease_token = excluded.lease_token,
            lease_fencing_token = excluded.lease_fencing_token,
            lease_claimed_at_ms = excluded.lease_claimed_at_ms,
            lease_expires_at_ms = excluded.lease_expires_at_ms",
        params![
            session_id,
            owner.owner_id,
            owner.incarnation_id,
            liveness_json,
            lease_token,
            fencing_token as i64,
            now as i64,
            expires_at as i64
        ],
    )
    .map_err(sqlite_error)?;
    Ok(SessionExecutionLease {
        session_id: session_id.to_string(),
        owner: owner.clone(),
        lease_token,
        fencing_token,
        claimed_at_epoch_ms: now,
        expires_at_epoch_ms: expires_at,
    })
}

fn ensure_session_execution_lease_conn(
    conn: &Connection,
    session_id: &str,
    fence: &SessionExecutionLeaseFence,
    now: u64,
) -> Result<(), StoreError> {
    if fence.session_id != session_id {
        return Err(StoreError::SessionExecutionLeaseExpired {
            session_id: session_id.to_string(),
        });
    }
    let current = load_session_execution_lease_row_conn(conn, session_id)?;
    let Some(current) = current else {
        return Err(StoreError::SessionExecutionLeaseExpired {
            session_id: session_id.to_string(),
        });
    };
    if current
        .owner
        .as_ref()
        .is_some_and(|owner| owner.same_incarnation(&fence.owner))
        && current.lease_token.as_deref() == Some(fence.lease_token.as_str())
        && current.fencing_token == fence.fencing_token
        && current.expires_at_ms > now
    {
        Ok(())
    } else {
        Err(StoreError::SessionExecutionLeaseExpired {
            session_id: session_id.to_string(),
        })
    }
}

fn release_session_execution_lease_conn(
    conn: &Connection,
    completion: &SessionExecutionLeaseCompletion,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE session_execution_leases
         SET lease_owner_id = NULL,
             lease_owner_incarnation_id = NULL,
             lease_owner_liveness_json = NULL,
             lease_token = NULL,
             lease_claimed_at_ms = 0,
             lease_expires_at_ms = 0
         WHERE session_id = ?1
           AND lease_owner_id = ?2
           AND lease_owner_incarnation_id = ?3
           AND lease_token = ?4
           AND lease_fencing_token = ?5",
        params![
            completion.session_id,
            completion.owner.owner_id,
            completion.owner.incarnation_id,
            completion.lease_token,
            completion.fencing_token as i64
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}
