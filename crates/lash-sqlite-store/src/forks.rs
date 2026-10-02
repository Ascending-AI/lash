use super::*;
use crate::session_sql::session_sql;

#[expect(
    clippy::disallowed_methods,
    reason = "the sqlite store factory ensures the host-supplied store root exists before opening (FIG-2971)"
)]
async fn open_factory_catalog(
    catalog: &DatabaseLocation,
    policy: SqliteConnectionPolicy,
) -> Result<SqliteConnection, lash_core_execution::StoreError> {
    if let Some(root) = catalog.target().file_path().and_then(Path::parent) {
        std::fs::create_dir_all(root)
            .map_err(|err| lash_core_execution::StoreError::Backend(err.to_string()))?;
    }
    let conn = SqliteConnection::open_with_policy(catalog.target(), policy)
        .await
        .map_err(|err| lash_core_execution::StoreError::Backend(err.to_string()))?;
    ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
        .await
        .map_err(sqlite_error)?;
    Ok(conn)
}

pub(super) async fn fork_at_in_catalog(
    catalog: &DatabaseLocation,
    request: &lash_core_execution::ForkSessionRequest,
    created_at_ms: u64,
    policy: SqliteConnectionPolicy,
    blob_profile: BuiltinBlobProfile,
) -> Result<lash_core_execution::ForkSessionReceipt, lash_core_execution::StoreError> {
    let conn = open_factory_catalog(catalog, policy).await?;
    let request = request.clone();
    conn.write_flow(move |tx| {
        let outcome: Result<lash_core_execution::ForkSessionReceipt, lash_core_execution::StoreError> = (|| {
            // The catalog carries the fleet-format row the ADR has every
            // writer consult; this transaction writes durable head/meta rows,
            // so it reads the deployment's generation rather than a build
            // constant.
            let fleet_format = crate::compat::read_recorded(
                tx,
                lash_core_execution::FleetFormat::writable(),
            )
            .map_err(sqlite_error)?;
            // Keep the fork fences in the shared order: exists -> deleted ->
            // retained revision -> live leaf -> frame.
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
                return Err(lash_core_execution::StoreError::ForkSessionAlreadyExists {
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
                return Err(lash_core_execution::StoreError::SessionDeleted {
                    session_id: request.session_id.clone(),
                });
            }
            // The revision row is the retained point. No other revision is
            // ever forked in its place.
            let source_session_id = request.source_session_id.clone();
            let pruned = || lash_core_execution::StoreError::ForkTargetPruned {
                session_id: source_session_id.clone(),
                target: lash_core_execution::Target::Revision(request.head_revision),
            };
            let sql_revision = i64::try_from(request.head_revision).map_err(|_| pruned())?;
            let retained = tx
                .query_row(
                    session_sql().revisions.select.sql(),
                    params![source_session_id.as_str(), sql_revision],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()
                .map_err(sqlite_error)?;
            let Some((leaf_node_id, mut checkpoint_ref)) = retained else {
                persistence::ensure_session_not_deleted_conn(tx, &source_session_id)?;
                let source_exists = tx
                    .query_row(
                        session_sql().meta_sqlite.exists_materialized.sql(),
                        params![source_session_id.as_str()],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(sqlite_error)?
                    .is_some();
                return Err(if source_exists {
                    pruned()
                } else {
                    lash_core_execution::StoreError::SessionNotFound {
                        session_id: source_session_id.clone(),
                    }
                });
            };
            let mut current_frame_node_id = None;
            let mut fork_plan = None;
            if let Some(leaf_node_id) = leaf_node_id.as_deref() {
                // Retirement never tombstones a retained revision's leaf, so
                // a dead one is damage, not a collected point.
                let node_facts = tx
                    .query_row(
                        session_sql().graph_sqlite.select_owner_generation.sql(),
                        params![leaf_node_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()
                    .map_err(sqlite_error)?;
                let (_owning_session_id, fork_generation) = node_facts.ok_or_else(|| {
                    stored_data_corrupt(
                        "SessionRevision",
                        format!(
                            "revision {} of session `{source_session_id}` retains leaf \
                             `{leaf_node_id}`, which is missing or tombstoned",
                            request.head_revision
                        ),
                    )
                })?;
                let frame = persistence::nearest_frame_node_id_conn(tx, leaf_node_id)?
                    .ok_or_else(|| lash_core_execution::StoreError::MissingFrameOpenAncestor {
                        leaf_node_id: leaf_node_id.to_string().into(),
                    })?;
                let frame_node_id = lash_core_execution::FrameNodeId::new(frame)
                    .map_err(|error| stored_data_corrupt("fork frame", error))?;
                let source_frame = lash_core_execution::ArtifactReferrer::FrameEnvironment(
                    lash_core_execution::FrameEnvironmentId::new(source_session_id.clone(), frame_node_id.clone()),
                );
                let fork_frame = lash_core_execution::ArtifactReferrer::FrameEnvironment(
                    lash_core_execution::FrameEnvironmentId::new(request.session_id.clone(), frame_node_id.clone()),
                );
                let source_frame_ended = crate::artifact_store::artifact_fenced_tx(tx, &source_frame)
                    .map_err(sqlite_error)?;
                if source_frame_ended {
                    if let Some(retained_ref) = checkpoint_ref.clone() {
                        let mut checkpoint = SqliteStore::get_checkpoint_conn(
                            tx, &BlobRef(retained_ref), fleet_format,
                        )?.ok_or_else(|| stored_data_corrupt("fork checkpoint", "the retained checkpoint is missing"))?;
                        checkpoint.components.retain(|key, _| {
                            key != lash_core_execution::store::EXECUTION_STATE_CHECKPOINT_COMPONENT
                                && !matches!(
                                    lash_core_execution::plugin::CheckpointComponentKey::parse(key),
                                    lash_core_execution::plugin::CheckpointComponentKey::ExecutionLeaf(_)
                                )
                        });
                        checkpoint_ref = Some(
                            SqliteStore::put_checkpoint_conn(tx, &checkpoint, blob_profile, fleet_format)?
                                .checkpoint_ref.as_str().to_owned(),
                        );
                    }
                } else {
                    crate::conn::cached_execute(tx,
                        crate::artifact_store::artifact_sql().edges.copy_referrer_edges.sql(),
                        params![source_frame.kind().as_str(), source_frame.canonical_id(),
                            fork_frame.kind().as_str(), fork_frame.canonical_id()],
                    ).map_err(sqlite_error)?;
                }
                let fork_generation = u64::try_from(fork_generation).map_err(|_| {
                    stored_data_corrupt(
                        "SessionGraph node",
                        format!("negative generation {fork_generation}"),
                    )
                })?;
                let mut edge_path = Vec::new();
                let mut current_node_id = lash_core_execution::NodeId::from(leaf_node_id);
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
                    edge_path.push(lash_core_execution::store::ForkNodeFacts {
                        node_id: facts.0.into(),
                        parent_node_id: facts.1.map(lash_core_execution::NodeId::from),
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
                fork_plan = Some(lash_core_execution::store::ForkPlan::derive(
                    &request.session_id,
                    edge_path,
                )?);
                current_frame_node_id = Some(frame_node_id);
            }
            if let Some(checkpoint_ref) = checkpoint_ref.as_deref() {
                let exists = tx
                    .query_row(
                        crate::artifact_store::artifact_sql().blobs_sqlite.select_exists.sql(),
                        params![checkpoint_ref],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(sqlite_error)?;
                if !exists {
                    return Err(lash_core_execution::StoreError::CheckpointRootMissing {
                        blob_ref: BlobRef(checkpoint_ref.to_owned()),
                    });
                }
            }
            let config = request.config.clone();
            // The fork's head republishes its fork point's plugin config, so
            // the namespaces are admitted like any other publication
            // (FIG-4746).
            tx.admit_plugin_writers(
                &lash_core_execution::store::plugin_writers::PluginPublication::of_session_config(&config),
            )
            .map_err(sqlite_error)?;
            let meta = lash_core_execution::store::SessionHeadMeta::assemble(
                &request.session_id,
                lash_core_execution::store::SessionHeadPayload {
                    schema_version: fleet_format.writer_version(
                        lash_core_execution::surface_format!(
                            lash_core_execution::store::SESSION_HEAD_META_SCHEMA_VERSION
                        ),
                    ),
                    session_id: request.session_id.clone(),
                    config,
                    published_by_drive: false,
                },
                0,
                checkpoint_ref.clone().map(Into::into),
                leaf_node_id.clone().map(Into::into),
                current_frame_node_id,
            )?;
            let head_json = encode_json(&meta.payload())?;
            crate::conn::cached_execute(tx,
                session_sql().head_sqlite.insert_fork.sql(),
                params![
                    request.session_id.as_str(),
                    head_json,
                    leaf_node_id.as_deref(),
                    checkpoint_ref.as_deref()
                ],
            )
            .map_err(sqlite_error)?;
            // The fork's own head is its first retained revision.
            crate::revisions::record_revision_conn(
                tx,
                &request.session_id,
                0,
                leaf_node_id.as_deref(),
                checkpoint_ref.as_deref(),
                &head_json,
            )?;
            if let Some(fork_plan) = &fork_plan {
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
                            lash_core_execution::StoreError::Backend(
                                "fork generation does not fit SQLite INTEGER".to_string(),
                            )
                        })?,
                    ])
                    .map_err(sqlite_error)?;
                }
            }
            let session_meta = lash_core_execution::SessionMeta {
                owning_process_id: None,
                session_id: request.session_id.clone(),
                relation: request.relation,
                pending_observer_intents: request.pending_observer_intents,
            };
            crate::session_meta::write_session_meta(
                tx,
                &session_meta,

                created_at_ms,
                fleet_format,
            )?;
            Ok(lash_core_execution::ForkSessionReceipt {
                session_id: request.session_id,
                source_session_id,
                head_revision: request.head_revision,
                leaf_node_id: leaf_node_id.map(Into::into),
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
            read_connections: std::num::NonZeroUsize::new(4).expect("four is nonzero"),
            busy_timeout: std::time::Duration::from_millis(321),
            synchronous: SqliteSynchronous::Full,
            wal_autocheckpoint_pages: 17,
            cache_size: -4096,
        };
        let conn = open_factory_catalog(
            &crate::location::DatabaseLocation::standalone_file(
                &dir.path().join(crate::DURABLE_CORE_DB_FILE),
            ),
            policy,
        )
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
