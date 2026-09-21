use super::*;
use crate::session_sql::session_sql;

async fn open_factory_catalog(
    root: &Path,
    policy: SqliteConnectionPolicy,
) -> Result<SqliteConnection, lash_core::StoreError> {
    std::fs::create_dir_all(root).map_err(|err| lash_core::StoreError::Backend(err.to_string()))?;
    let conn = SqliteConnection::open_with_policy(&root.join(DURABLE_CORE_DB_FILE), policy)
        .await
        .map_err(|err| lash_core::StoreError::Backend(err.to_string()))?;
    ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
        .await
        .map_err(sqlite_error)?;
    Ok(conn)
}

fn retained_fork_config_conn(
    conn: &rusqlite::Connection,
    node_id: &str,
) -> Result<lash_core::PersistedSessionConfig, lash_core::StoreError> {
    let frame_node_id =
        persistence::nearest_frame_node_id_conn(conn, node_id)?.ok_or_else(|| {
            lash_core::StoreError::MissingFrameOpenAncestor {
                leaf_node_id: node_id.to_string().into(),
            }
        })?;
    let (parent_node_id, node_json) = conn
        .query_row(
            session_sql().graph_sqlite.select_frame_body.sql(),
            params![frame_node_id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?
        .ok_or_else(|| {
            lash_core::StoreError::Backend(format!(
                "retained frame node `{frame_node_id}` is missing"
            ))
        })?;
    lash_core::SessionNodeRecord::decode_storage_body(
        frame_node_id.clone(),
        parent_node_id,
        &node_json,
    )
    .map_err(|error| {
        lash_core::StoreError::Backend(format!(
            "failed to decode retained frame node `{frame_node_id}`: {error}"
        ))
    })?
    .frame_config()
    .ok_or_else(|| {
        lash_core::StoreError::Backend(format!(
            "retained frame node `{frame_node_id}` has no frame assignment"
        ))
    })
}

pub(super) async fn pin_in_catalog(
    root: &Path,
    node_id: &str,
    policy: SqliteConnectionPolicy,
) -> Result<lash_core::ForkPoint, lash_core::StoreError> {
    let conn = open_factory_catalog(root, policy).await?;
    let node_id = node_id.to_string();
    conn.write_flow(move |tx| {
        let outcome: Result<lash_core::ForkPoint, lash_core::StoreError> = (|| {
            if let Some((checkpoint_ref, source_session_id)) = tx
                .query_row(
                    session_sql().anchors.select_by_node.sql(),
                    params![node_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?
            {
                let config = retained_fork_config_conn(tx, &node_id)?;
                return Ok(lash_core::ForkPoint {
                    node_id: node_id.into(),
                    checkpoint_ref: checkpoint_ref.into(),
                    source_session_id: SessionId::from(source_session_id),
                    config,
                    pinned: true,
                });
            }
            let retained = tx
                .query_row(
                    session_sql().head.select_retained_by_leaf.sql(),
                    params![node_id],
                    |row| {
                        Ok((
                            SessionId::from(row.get::<_, String>(0)?),
                            row.get::<_, String>(1)?,
                        ))
                    },
                )
                .optional()
                .map_err(sqlite_error)?;
            let (source_session_id, checkpoint_ref) =
                retained.ok_or_else(|| lash_core::StoreError::ForkPointNotRetained {
                    node_id: node_id.clone().into(),
                })?;
            let live = tx
                .query_row(
                    session_sql().graph_sqlite.exists_live.sql(),
                    params![node_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sqlite_error)?
                .is_some();
            if !live {
                return Err(lash_core::StoreError::ForkPointNotRetained {
                    node_id: node_id.clone().into(),
                });
            }
            tx.execute(
                session_sql().anchors.insert.sql(),
                params![node_id, checkpoint_ref, source_session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            let config = retained_fork_config_conn(tx, &node_id)?;
            Ok(lash_core::ForkPoint {
                node_id: node_id.into(),
                checkpoint_ref: checkpoint_ref.into(),
                source_session_id,
                config,
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
    policy: SqliteConnectionPolicy,
) -> Result<(), lash_core::StoreError> {
    let conn = open_factory_catalog(root, policy).await?;
    let node_id = node_id.to_string();
    conn.write_flow(move |tx| {
        let outcome: Result<(), lash_core::StoreError> = (|| {
            let removed = tx
                .execute(session_sql().anchors.delete_by_node.sql(), params![node_id])
                .map_err(sqlite_error)?;
            if removed == 1 {
                persistence::retire_unreachable_ancestry_conn(tx, &node_id)?;
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
    policy: SqliteConnectionPolicy,
) -> Result<Vec<lash_core::ForkPoint>, lash_core::StoreError> {
    let conn = open_factory_catalog(root, policy).await?;
    conn.call(|conn| {
        let tx = conn.transaction()?;
        let outcome: Result<Vec<lash_core::ForkPoint>, lash_core::StoreError> = (|| {
            let mut stmt = tx
                .prepare(session_sql().head.select_fork_points.sql())
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)? != 0,
                    ))
                })
                .map_err(sqlite_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sqlite_error)?;
            rows.into_iter()
                .map(|(node_id, checkpoint_ref, source_session_id, pinned)| {
                    Ok(lash_core::ForkPoint {
                        config: retained_fork_config_conn(&tx, &node_id)?,
                        node_id: node_id.into(),
                        checkpoint_ref: lash_core::BlobRef(checkpoint_ref),
                        source_session_id: SessionId::from(source_session_id),
                        pinned,
                    })
                })
                .collect()
        })();
        match outcome {
            Ok(points) => {
                tx.commit()?;
                Ok(Ok(points))
            }
            Err(error) => Ok(Err(error)),
        }
    })
    .await
    .map_err(sqlite_error)?
}

pub(super) async fn fork_at_in_catalog(
    root: &Path,
    request: &lash_core::ForkSessionRequest,
    created_at_ms: u64,
    policy: SqliteConnectionPolicy,
) -> Result<lash_core::ForkSessionReceipt, lash_core::StoreError> {
    let conn = open_factory_catalog(root, policy).await?;
    let request = request.clone();
    conn.write_flow(move |tx| {
        let outcome: Result<lash_core::ForkSessionReceipt, lash_core::StoreError> = (|| {
            // Keep the fork fences in the shared order: exists -> deleted ->
            // retained -> live -> frame.
            let exists = tx
                .query_row(
                    session_sql().meta_sqlite.exists_materialized.sql(),
                    params![request.session_id.as_str()],
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
            let deleted = tx
                .query_row(
                    session_sql().deleted_sqlite.exists.sql(),
                    params![request.session_id.as_str()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sqlite_error)?
                .is_some();
            if deleted {
                return Err(lash_core::StoreError::SessionDeleted {
                    session_id: request.session_id.clone(),
                });
            }
            let retained = tx
                .query_row(
                    session_sql().head.select_retained_checkpoint.sql(),
                    params![request.node_id.as_str()],
                    |row| Ok((SessionId::from(row.get::<_, String>(0)?), row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?;
            let (source_session_id, checkpoint_ref) =
                retained.ok_or_else(|| lash_core::StoreError::ForkPointNotRetained {
                    node_id: request.node_id.clone(),
                })?;
            // The relation records which session the host branched from, while
            // the retained point records which session originally wrote the
            // node. Those identities legitimately differ after a rewind.
            let live = tx
                .query_row(
                    session_sql().graph_sqlite.exists_live.sql(),
                    params![request.node_id.as_str()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sqlite_error)?
                .is_some();
            if !live {
                return Err(lash_core::StoreError::ForkPointNotRetained {
                    node_id: request.node_id.clone(),
                });
            }
            let node_facts = tx
                .query_row(
                    session_sql().graph_sqlite.select_owner_generation.sql(),
                    params![request.node_id.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                        ))
                    },
                )
                .optional()
                .map_err(sqlite_error)?;
            let (_owning_session_id, fork_generation) = node_facts
                .ok_or_else(|| lash_core::StoreError::ForkPointNotRetained {
                    node_id: request.node_id.clone(),
                })?;
            let current_frame_node_id = persistence::nearest_frame_node_id_conn(tx, &request.node_id)?
                .ok_or_else(|| lash_core::StoreError::MissingFrameOpenAncestor {
                    leaf_node_id: request.node_id.clone(),
                })?;
            let fork_generation = u64::try_from(fork_generation).map_err(|_| {
                stored_data_corrupt(
                    "SessionGraph node",
                    format!("negative generation {fork_generation}"),
                )
            })?;
            let mut edge_path = Vec::new();
            let mut current_node_id = request.node_id.clone();
            let mut expected_generation = fork_generation;
            loop {
                let facts = tx
                    .query_row(
                        session_sql().graph_sqlite.select_edge.sql(),
                        params![current_node_id.as_str()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(sqlite_error)?
                    .ok_or_else(|| {
                        stored_data_corrupt(
                            "SessionGraph",
                            format!(
                                "retained fork path is missing or tombstoned at `{current_node_id}`"
                            ),
                        )
                    })?;
                let generation = u64::try_from(facts.3).map_err(|_| {
                    stored_data_corrupt(
                        "SessionGraph node",
                        format!("negative generation {}", facts.3),
                    )
                })?;
                if generation != expected_generation {
                    return Err(stored_data_corrupt(
                        "SessionGraph",
                        format!(
                            "parent generation {generation} does not match expected {expected_generation}"
                        ),
                    ));
                }
                let parent_node_id = facts.1.clone();
                edge_path.push(lash_core::store::ForkNodeFacts {
                    node_id: facts.0.into(),
                    parent_node_id: facts.1.map(lash_core::NodeId::from),
                    owning_session_id: SessionId::from(facts.2),
                    generation,
                });
                if expected_generation == 0 {
                    break;
                }
                current_node_id = parent_node_id.ok_or_else(|| {
                    stored_data_corrupt(
                        "SessionGraph",
                        "retained fork path ended before generation zero",
                    )
                })?.into();
                expected_generation -= 1;
            }
            edge_path.reverse();
            let fork_plan =
                lash_core::store::ForkPlan::derive(&request.session_id, edge_path)?;
            let config = lash_core::PersistedSessionConfig::from(&request.policy);
            let meta = lash_core::store::SessionHeadMeta::assemble(
                &request.session_id,
                lash_core::store::SessionHeadPayload {
                    schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                    session_id: request.session_id.clone(),
                    config,
                    current_frame_node_id: Some({
                        #[expect(
                            clippy::expect_used,
                            reason = "the target is a transparent newtype over `String`, so decoding a JSON string into it cannot fail"
                        )]
                        let node_id = serde_json::from_value(serde_json::Value::String(
                            current_frame_node_id,
                        ))
                        .expect("a persisted frame node id is a transparent string");
                        node_id
                    }),
                },
                0,
                Some(checkpoint_ref.clone().into()),
                Some(request.node_id.clone()),
            )?;
            tx.execute(
                session_sql().head.insert_fork.sql(),
                params![
                    request.session_id.as_str(),
                    encode_json(&meta.payload())?,
                    request.node_id.as_str(),
                    checkpoint_ref
                ],
            )
            .map_err(sqlite_error)?;
            {
                let mut stmt = tx
                    .prepare(
                        session_sql().lineage.insert.sql(),
                    )
                    .map_err(sqlite_error)?;
                for ancestor in fork_plan.ancestors() {
                    stmt.execute(params![
                        fork_plan.session_id(),
                        ancestor.ancestor_session_id.as_str(),
                        ancestor.fork_node_id.as_str(),
                        i64::try_from(ancestor.fork_generation).map_err(|_| {
                            lash_core::StoreError::Backend(
                                "fork generation does not fit SQLite INTEGER".to_string(),
                            )
                        })?,
                    ])
                    .map_err(sqlite_error)?;
                }
            }
            let session_meta = lash_core::SessionMeta {
                session_id: request.session_id.clone(),
                relation: request.relation,
                pending_observer_intents: request.pending_observer_intents,
            };
            crate::session_meta::write_session_meta(
                tx,
                &session_meta,
                crate::session_meta::SessionMetaWrite::Insert,
                created_at_ms,
            )?;
            Ok(lash_core::ForkSessionReceipt {
                session_id: request.session_id,
                node_id: request.node_id,
                source_session_id,
                observed_processes: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn factory_catalog_connection_uses_requested_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = SqliteConnectionPolicy {
            busy_timeout: std::time::Duration::from_millis(321),
            synchronous: SqliteSynchronous::Full,
            wal_autocheckpoint_pages: 17,
            cache_size: -4096,
        };
        let conn = open_factory_catalog(dir.path(), policy)
            .await
            .expect("open factory catalog with connection policy");

        let pragmas = conn
            .call(|connection| {
                Ok((
                    connection.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))?,
                    connection.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))?,
                    connection
                        .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get::<_, i64>(0))?,
                    connection.query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))?,
                    connection
                        .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?,
                ))
            })
            .await
            .expect("read factory catalog connection policy");

        assert_eq!(pragmas, (321, 2, 17, -4096, "wal".to_string()));
    }
}
