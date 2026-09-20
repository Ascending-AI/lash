use super::*;

pub(super) fn warn_process_registry_not_wired(path: &'static str) {
    tracing::warn!(
        store = "sqlite",
        path,
        consequence = "process-owned uncommitted intents are never reclaimed",
        "SQLite attachment GC process-owner liveness is not wired; process-owned intents will be retained indefinitely. Call SqliteSessionStoreFactory::new_with_process_registry(...)."
    );
}

pub(super) async fn delete_session_from_catalog(
    root: &Path,
    session_id: &SessionId,
    policy: SqliteConnectionPolicy,
) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
    let path = root.join(DURABLE_CORE_DB_FILE);
    if !path.exists() {
        return Ok(lash_core::SessionBlobReclaimReport::default());
    }
    let session_id = SessionId::from(session_id.to_string());
    let conn = SqliteConnection::open_with_policy(&path, policy)
        .await
        .map_err(|err| {
            lash_core::MaintenanceFailure::failed_before_any_work(lash_core::StoreError::Backend(
                err.to_string(),
            ))
        })?;
    ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
        .await
        .map_err(|err| lash_core::MaintenanceFailure::failed_before_any_work(sqlite_error(err)))?;
    conn.write_flow(move |tx| {
        let mut report = lash_core::SessionBlobReclaimReport::default();
        let outcome: Result<lash_core::SessionBlobReclaimReport, lash_core::StoreError> = (|| {
            let pending_count = tx
                .query_row(
                    "SELECT COUNT(*) FROM turn_cancel_closure_authorizations WHERE session_id = ?1",
                    params![session_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sqlite_error)?;
            let pending_count = usize::try_from(pending_count).map_err(|_| {
                lash_core::StoreError::StoredDataCorrupt {
                    record_kind: "TurnCancelClosureAuthorization",
                    message: "negative pending closure count".to_string(),
                }
            })?;
            if pending_count != 0 {
                return Err(lash_core::StoreError::TurnCancelClosureLifecyclePinned {
                    session_id: session_id.clone(),
                    pending_count,
                });
            }
            let existed = tx
                .query_row(
                    "SELECT 1 FROM session_meta WHERE session_id = ?1
                     UNION ALL
                     SELECT 1 FROM session_head WHERE session_id = ?1
                     LIMIT 1",
                    params![session_id.as_str()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sqlite_error)?
                .is_some();
            if existed {
                // Permanent identity evidence for every deleted session id,
                // host-facing and runtime-internal alike. The deleted set is
                // also the reclaim frontier for the delete arm below: a
                // process-owned session id that never entered it would leave
                // its tombstoned rows unreachable forever, because the id is
                // just as unbindable as a host-facing one once deleted.
                tx.execute(
                    "INSERT OR IGNORE INTO deleted_sessions
                     (session_id, created_at_ms, last_commit_at_ms, head_revision,
                      relation_kind, parent_session_id)
                     SELECT meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                            COALESCE(head.head_revision, 0), meta.relation_kind,
                            meta.parent_session_id
                     FROM session_meta AS meta
                     LEFT JOIN session_head AS head ON head.session_id = meta.session_id
                     WHERE meta.session_id = ?1",
                    params![session_id.as_str()],
                )
                .map_err(sqlite_error)?;
                tx.execute(
                    "INSERT OR IGNORE INTO deleted_sessions
                     (session_id, created_at_ms, last_commit_at_ms, head_revision,
                      relation_kind, parent_session_id)
                     VALUES (?1, 0, NULL, 0, 'root', NULL)",
                    params![session_id.as_str()],
                )
                .map_err(sqlite_error)?;
            }
            let (leaf_node_id, checkpoint_ref) = tx
                .query_row(
                    "SELECT leaf_node_id, checkpoint_ref FROM session_head WHERE session_id = ?1",
                    params![session_id.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()
                .map_err(sqlite_error)?
                .unwrap_or((None, None));
            let mut candidates = std::collections::BTreeSet::new();
            if let Some(checkpoint_ref) = checkpoint_ref.as_deref() {
                candidates.insert(checkpoint_ref.to_string());
                let mut stmt = tx
                    .prepare(
                        "SELECT blob_ref FROM checkpoint_blob_refs
                         WHERE checkpoint_ref = ?1 ORDER BY blob_ref",
                    )
                    .map_err(sqlite_error)?;
                let rows = stmt
                    .query_map(params![checkpoint_ref], |row| row.get::<_, String>(0))
                    .map_err(sqlite_error)?;
                candidates.extend(rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?);
            }
            for blob_ref in &candidates {
                let exists = tx
                    .query_row(
                        crate::artifact_store::artifact_sql()
                            .blobs_sqlite
                            .select_exists
                            .sql(),
                        params![blob_ref],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(sqlite_error)?;
                if !exists {
                    return Err(stored_data_corrupt(
                        "session blob reference",
                        format!("blob `{blob_ref}` is missing"),
                    ));
                }
            }
            report.enumerated_blob_count = candidates.len();
            tx.execute(
                "DELETE FROM session_head WHERE session_id = ?1",
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            if let Some(leaf_node_id) = leaf_node_id {
                persistence::retire_unreachable_ancestry_conn(tx, &leaf_node_id)?;
            }
            let unreachable_candidates = {
                let mut stmt = tx
                    .prepare(
                        "SELECT g.node_id FROM graph_nodes AS g
                         WHERE g.session_id = ?1 AND g.tombstoned = 0
                           AND NOT EXISTS (
                               SELECT 1 FROM graph_nodes AS child
                               WHERE child.parent_node_id = g.node_id
                                 AND child.tombstoned = 0
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM session_head AS head
                               WHERE head.leaf_node_id = g.node_id
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM node_anchors AS anchor
                               WHERE anchor.node_id = g.node_id
                           )
                         ORDER BY g.generation DESC",
                    )
                    .map_err(sqlite_error)?;
                let rows = stmt
                    .query_map(params![session_id.as_str()], |row| row.get::<_, String>(0))
                    .map_err(sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
            };
            for node_id in unreachable_candidates {
                persistence::retire_unreachable_ancestry_conn(tx, &node_id)?;
            }
            // Delete-time reclaim covers this session's tombstoned rows plus any
            // tombstoned row owned by an already-deleted session. A node can be
            // tombstoned *after* its owner is gone (unpin of a pinned leaf whose
            // session was deleted, or ancestry retired at a fork child's delete),
            // and no session-scoped vacuum could ever reach it: the owning id is
            // permanently unbindable. Live sessions' rows stay resident for their
            // own vacuum, so this is not a catalog-wide sweep.
            tx.execute(
                "DELETE FROM graph_nodes
                 WHERE tombstoned = 1
                   AND (session_id = ?1
                        OR session_id IN (SELECT session_id FROM deleted_sessions))",
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM fork_lineage WHERE session_id = ?1",
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM queued_work_batches WHERE session_id = ?1",
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM wake_redelivery_fences WHERE session_id = ?1",
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            for table in [
                "pending_turn_inputs",
                "turn_cancel_requests",
                // Administration revokes the session's effect authority before
                // entering store deletion. Only then may the pinned closure
                // obligation and its selected-owner identity be retired.
                "turn_cancel_closure_authorizations",
                "turn_cancellation_bindings",
                "session_execution_leases",
                "session_meta",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE session_id = ?1"),
                    params![session_id.as_str()],
                )
                .map_err(sqlite_error)?;
            }
            tx.execute(attachments::RECLAIM_DELETED_ATTACHMENT_ROOTS, [])
                .map_err(sqlite_error)?;
            if let Some(checkpoint_ref) = checkpoint_ref.as_ref() {
                // Sever this root's outgoing projection before any blob delete
                // when the owner transaction removed its final head/anchor.
                // The root bytes may remain as another root's opaque component;
                // its projection no longer has a live root owner in that case.
                tx.execute(
                    "DELETE FROM checkpoint_blob_refs AS edge
                     WHERE edge.checkpoint_ref = ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM session_head AS head
                           WHERE head.checkpoint_ref = edge.checkpoint_ref
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM node_anchors AS anchor
                           WHERE anchor.checkpoint_ref = edge.checkpoint_ref
                       )",
                    params![checkpoint_ref],
                )
                .map_err(sqlite_error)?;
            }
            // Every predicate is an indexed NOT EXISTS over exact edges; no
            // whole-catalog mark/sweep runs in this transaction.
            for blob_ref in candidates {
                let deleted = tx
                    .execute(
                        "DELETE FROM blobs AS candidate
                         WHERE candidate.hash = ?1
                           AND NOT EXISTS (
                               SELECT 1 FROM session_head AS head
                               WHERE head.checkpoint_ref = candidate.hash
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM node_anchors AS anchor
                               WHERE anchor.checkpoint_ref = candidate.hash
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM artifact_refs AS artifact
                               WHERE artifact.blob_ref = candidate.hash
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM checkpoint_blob_refs AS edge
                               WHERE edge.blob_ref = candidate.hash
                                 AND (
                                     EXISTS (
                                         SELECT 1 FROM session_head AS head
                                         WHERE head.checkpoint_ref = edge.checkpoint_ref
                                     )
                                     OR EXISTS (
                                         SELECT 1 FROM node_anchors AS anchor
                                         WHERE anchor.checkpoint_ref = edge.checkpoint_ref
                                     )
                                 )
                           )",
                        params![blob_ref],
                    )
                    .map_err(sqlite_error)?;
                if deleted == 0 {
                    report.retained_blob_count += 1;
                } else {
                    report.deleted_blob_count += deleted;
                }
            }
            tracing::debug!(
                session_id = session_id.as_str(),
                enumerated_blob_count = report.enumerated_blob_count,
                retained_blob_count = report.retained_blob_count,
                deleted_blob_count = report.deleted_blob_count,
                sweep = ?lash_core::MaintenanceReport::sweep(&report),
                "session delete reclaimed owner-scoped blobs"
            );
            Ok(report.clone())
        })(
        );
        Ok(match outcome {
            Ok(value) => TxOutcome::Commit(Ok(value)),
            Err(err) => {
                report.deleted_blob_count = 0;
                TxOutcome::Rollback(Err(lash_core::MaintenanceFailure::failed(err, report)))
            }
        })
    })
    .await
    .map_err(|err| {
        lash_core::MaintenanceFailure::failed_before_any_work(lash_core::StoreError::Backend(
            err.to_string(),
        ))
    })?
}

pub(super) async fn delete_wake_allocation_floors_from_process_registry(
    process_registry_path: &Path,
    target_session_id: &SessionId,
    policy: SqliteConnectionPolicy,
) -> Result<(), String> {
    if !process_registry_path.exists() {
        return Ok(());
    }
    let conn = SqliteConnection::open_with_policy(process_registry_path, policy)
        .await
        .map_err(|err| err.to_string())?;
    ensure_versioned_schema(&conn, SqliteDatabase::ProcessRegistry)
        .await
        .map_err(|err| err.to_string())?;
    let target_session_id = SessionId::from(target_session_id.to_string());
    conn.write_flow(move |tx| {
        let outcome = tx
            .execute(
                "DELETE FROM wake_allocation_floors WHERE target_session_id = ?1",
                params![target_session_id.as_str()],
            )
            .map(|_| ())
            .map_err(sqlite_error);
        Ok(match outcome {
            Ok(()) => TxOutcome::Commit(Ok(())),
            Err(error) => TxOutcome::Rollback(Err(error)),
        })
    })
    .await
    .map_err(|err| err.to_string())?
    .map_err(|err| err.to_string())
}
