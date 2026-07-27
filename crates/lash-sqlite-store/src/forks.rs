use super::*;

async fn open_factory_catalog(root: &Path) -> Result<SqliteConnection, lash_core::StoreError> {
    std::fs::create_dir_all(root).map_err(|err| lash_core::StoreError::Backend(err.to_string()))?;
    let conn = SqliteConnection::open(&root.join("durable-core.db"))
        .await
        .map_err(|err| lash_core::StoreError::Backend(err.to_string()))?;
    ensure_schema(&conn).await.map_err(sqlite_error)?;
    Ok(conn)
}

pub(super) async fn pin_in_catalog(
    root: &Path,
    node_id: &str,
) -> Result<lash_core::ForkPoint, lash_core::StoreError> {
    let conn = open_factory_catalog(root).await?;
    let node_id = node_id.to_string();
    conn.write_flow(move |tx| {
        let outcome: Result<lash_core::ForkPoint, lash_core::StoreError> = (|| {
            if let Some(checkpoint_ref) = tx
                .query_row(
                    "SELECT checkpoint_ref FROM node_anchors WHERE node_id = ?1",
                    params![node_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(sqlite_error)?
            {
                return Ok(lash_core::ForkPoint {
                    node_id,
                    checkpoint_ref: checkpoint_ref.into(),
                    pinned: true,
                });
            }
            let checkpoint_ref = tx
                .query_row(
                    "SELECT checkpoint_ref FROM session_head
                     WHERE leaf_node_id = ?1 AND checkpoint_ref IS NOT NULL
                     ORDER BY session_id LIMIT 1",
                    params![node_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(sqlite_error)?
                .ok_or_else(|| lash_core::StoreError::ForkPointNotRetained {
                    node_id: node_id.clone(),
                })?;
            let changed = tx
                .execute(
                    "UPDATE graph_nodes
                     SET incoming_refs = incoming_refs + 1
                     WHERE node_id = ?1 AND tombstoned = 0",
                    params![node_id],
                )
                .map_err(sqlite_error)?;
            if changed != 1 {
                return Err(lash_core::StoreError::ForkPointNotRetained {
                    node_id: node_id.clone(),
                });
            }
            tx.execute(
                "INSERT INTO node_anchors (node_id, checkpoint_ref) VALUES (?1, ?2)",
                params![node_id, checkpoint_ref],
            )
            .map_err(sqlite_error)?;
            Ok(lash_core::ForkPoint {
                node_id,
                checkpoint_ref: checkpoint_ref.into(),
                pinned: true,
            })
        })();
        Ok(match outcome {
            Ok(value) => TxOutcome::Commit(Ok(value)),
            Err(err) => TxOutcome::Rollback(Err(err)),
        })
    })
    .await
    .map_err(sqlite_error)?
}

pub(super) async fn unpin_in_catalog(
    root: &Path,
    node_id: &str,
) -> Result<(), lash_core::StoreError> {
    let conn = open_factory_catalog(root).await?;
    let node_id = node_id.to_string();
    conn.write_flow(move |tx| {
        let outcome: Result<(), lash_core::StoreError> = (|| {
            let removed = tx
                .execute(
                    "DELETE FROM node_anchors WHERE node_id = ?1",
                    params![node_id],
                )
                .map_err(sqlite_error)?;
            if removed == 1 {
                persistence::decrement_node_ref_conn(tx, &node_id)?;
            }
            Ok(())
        })();
        Ok(match outcome {
            Ok(value) => TxOutcome::Commit(Ok(value)),
            Err(err) => TxOutcome::Rollback(Err(err)),
        })
    })
    .await
    .map_err(sqlite_error)?
}

pub(super) async fn fork_points_in_catalog(
    root: &Path,
) -> Result<Vec<lash_core::ForkPoint>, lash_core::StoreError> {
    let conn = open_factory_catalog(root).await?;
    conn.call(|conn| {
        let mut stmt = conn.prepare(
            "SELECT retained.node_id, retained.checkpoint_ref,
                    EXISTS(SELECT 1 FROM node_anchors a WHERE a.node_id = retained.node_id)
             FROM (
                 SELECT node_id, checkpoint_ref FROM node_anchors
                 UNION
                 SELECT leaf_node_id, checkpoint_ref FROM session_head
                 WHERE leaf_node_id IS NOT NULL AND checkpoint_ref IS NOT NULL
             ) AS retained
             ORDER BY retained.node_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(lash_core::ForkPoint {
                node_id: row.get(0)?,
                checkpoint_ref: lash_core::BlobRef(row.get(1)?),
                pinned: row.get::<_, i64>(2)? != 0,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    })
    .await
    .map_err(sqlite_error)
}

pub(super) async fn fork_at_in_catalog(
    root: &Path,
    request: &lash_core::ForkSessionRequest,
) -> Result<lash_core::ForkSessionResult, lash_core::StoreError> {
    let conn = open_factory_catalog(root).await?;
    let request = request.clone();
    conn.write_flow(move |tx| {
        let outcome: Result<lash_core::ForkSessionResult, lash_core::StoreError> = (|| {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM session_meta WHERE session_id = ?1
                     UNION ALL
                     SELECT 1 FROM session_head WHERE session_id = ?1
                     LIMIT 1",
                    params![request.session_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sqlite_error)?
                .is_some();
            if exists {
                return Err(lash_core::StoreError::ForkSessionAlreadyExists {
                    session_id: request.session_id.clone(),
                });
            }
            let checkpoint_ref = tx
                .query_row(
                    "SELECT checkpoint_ref FROM node_anchors WHERE node_id = ?1
                     UNION ALL
                     SELECT checkpoint_ref FROM session_head
                     WHERE leaf_node_id = ?1 AND checkpoint_ref IS NOT NULL
                     ORDER BY checkpoint_ref LIMIT 1",
                    params![request.node_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(sqlite_error)?
                .ok_or_else(|| lash_core::StoreError::ForkPointNotRetained {
                    node_id: request.node_id.clone(),
                })?;
            let source_session_id = tx
                .query_row(
                    "SELECT session_id FROM graph_nodes
                     WHERE node_id = ?1 AND tombstoned = 0",
                    params![request.node_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(sqlite_error)?
                .ok_or_else(|| lash_core::StoreError::ForkPointNotRetained {
                    node_id: request.node_id.clone(),
                })?;
            let current_frame_node_id =
                persistence::nearest_frame_node_id_conn(tx, &request.node_id)?.ok_or_else(
                    || lash_core::StoreError::MissingFrameOpenAncestor {
                        leaf_node_id: request.node_id.clone(),
                    },
                )?;
            let graph_node_count = tx
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
                    params![request.node_id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sqlite_error)? as usize;
            let changed = tx
                .execute(
                    "UPDATE graph_nodes SET incoming_refs = incoming_refs + 1
                     WHERE node_id = ?1 AND tombstoned = 0",
                    params![request.node_id],
                )
                .map_err(sqlite_error)?;
            if changed != 1 {
                return Err(lash_core::StoreError::ForkPointNotRetained {
                    node_id: request.node_id.clone(),
                });
            }
            let config = lash_core::PersistedSessionConfig {
                provider_id: request.policy.recorded_provider_id().to_string(),
                model: request.policy.model.clone(),
            };
            let meta = lash_core::store::SessionHeadMeta {
                schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                session_id: request.session_id.clone(),
                head_revision: 0,
                config,
                current_frame_node_id: Some(current_frame_node_id),
                checkpoint_ref: Some(checkpoint_ref.clone().into()),
                leaf_node_id: Some(request.node_id.clone()),
                graph_node_count,
                token_ledger: Vec::new(),
            };
            tx.execute(
                "INSERT INTO session_head
                 (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
                 VALUES (?1, ?2, 0, ?3, ?4)",
                params![
                    request.session_id,
                    encode_json(&meta),
                    request.node_id,
                    checkpoint_ref
                ],
            )
            .map_err(sqlite_error)?;
            let session_meta = lash_core::SessionMeta {
                session_id: request.session_id.clone(),
                session_name: request.session_id.clone(),
                created_at: lash_core::Clock::timestamp_rfc3339(&lash_core::SystemClock),
                model: request.policy.model.id.clone(),
                cwd: std::env::current_dir()
                    .ok()
                    .and_then(|path| path.to_str().map(str::to_string)),
                relation: request.relation,
            };
            tx.execute(
                "INSERT INTO session_meta
                 (session_id, session_name, created_at, model, cwd, relation_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_meta.session_id,
                    session_meta.session_name,
                    session_meta.created_at,
                    session_meta.model,
                    session_meta.cwd,
                    encode_json(&session_meta.relation)
                ],
            )
            .map_err(sqlite_error)?;
            Ok(lash_core::ForkSessionResult {
                session_id: request.session_id,
                node_id: request.node_id,
                source_session_id,
            })
        })();
        Ok(match outcome {
            Ok(value) => TxOutcome::Commit(Ok(value)),
            Err(err) => TxOutcome::Rollback(Err(err)),
        })
    })
    .await
    .map_err(sqlite_error)?
}
