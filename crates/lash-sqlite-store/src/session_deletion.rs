use super::*;
use crate::session_sql::session_sql;

pub(super) fn warn_process_registry_not_wired(path: &'static str) {
    tracing::warn!(
        store = "sqlite",
        path,
        consequence = "process-owned uncommitted intents are never reclaimed",
        "SQLite attachment GC process-owner liveness is not wired; process-owned intents will be retained indefinitely. Call SqliteSessionStoreFactory::new_with_process_registry(...)."
    );
}

pub(super) async fn delete_session_from_catalog(
    catalog: &DatabaseLocation,
    session_id: &SessionId,
    policy: SqliteConnectionPolicy,
    now_ms: u64,
) -> lash_core_execution::MaintenanceResult<lash_core_execution::SessionBlobReclaimReport> {
    if !catalog.target().exists() {
        return Ok(lash_core_execution::SessionBlobReclaimReport::default());
    }
    let session_id = SessionId::from(session_id.to_string());
    let conn = SqliteConnection::open_with_policy(catalog.target(), policy)
        .await
        .map_err(|err| {
            lash_core_execution::MaintenanceFailure::failed_before_any_work(
                lash_core_execution::StoreError::Backend(err.to_string()),
            )
        })?;
    ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
        .await
        .map_err(|err| {
            lash_core_execution::MaintenanceFailure::failed_before_any_work(sqlite_error(err))
        })?;
    conn.write_flow(move |tx| {
        let mut report = lash_core_execution::SessionBlobReclaimReport::default();
        let outcome: Result<
            lash_core_execution::SessionBlobReclaimReport,
            lash_core_execution::StoreError,
        > = (|| {
            let pending_count = tx
                .query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .closures
                        .count_by_session
                        .sql(),
                    params![session_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sqlite_error)?;
            let pending_count = usize::try_from(pending_count).map_err(|_| {
                lash_core_execution::StoreError::StoredDataCorrupt {
                    record_kind: "TurnCancelClosureAuthorization",
                    message: "negative pending closure count".to_string(),
                }
            })?;
            if pending_count != 0 {
                return Err(
                    lash_core_execution::StoreError::TurnCancelClosureLifecyclePinned {
                        session_id: session_id.clone(),
                        pending_count,
                    },
                );
            }
            let existed = tx
                .query_row(
                    session_sql().meta_sqlite.exists_materialized.sql(),
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
                    session_sql().deleted_sqlite.insert_from_meta.sql(),
                    params![session_id.as_str()],
                )
                .map_err(sqlite_error)?;
                tx.execute(
                    session_sql().deleted_sqlite.insert_root.sql(),
                    params![session_id.as_str()],
                )
                .map_err(sqlite_error)?;
            }
            let (leaf_node_id, checkpoint_ref) = tx
                .query_row(
                    session_sql().head.select_reclaim.sql(),
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
                    .prepare(session_sql().checkpoint_edges.select_components.sql())
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
                session_sql().head.delete_by_session.sql(),
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            if let Some(leaf_node_id) = leaf_node_id {
                persistence::retire_unreachable_ancestry_conn(tx, &leaf_node_id)?;
            }
            let unreachable_candidates = {
                let mut stmt = tx
                    .prepare(session_sql().graph_sqlite.select_unreachable_leaves.sql())
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
                session_sql()
                    .graph_sqlite
                    .delete_tombstoned_reclaimable
                    .sql(),
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                session_sql().lineage.delete_by_session.sql(),
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            let turn_ingress = crate::turn_ingress::turn_ingress_sql();
            let queued_runs = &turn_ingress.queued_runs;
            tx.execute(
                turn_ingress.queued_batches.delete_by_session.sql(),
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                crate::process_registry::sql::process_sql()
                    .fence
                    .delete_by_session
                    .sql(),
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            // A deleted session's parked turn is cancelled, and its feed event
            // outlives the session row: the ledger is the only place the park
            // transition stays durable (FIG-3659).
            let released: Option<(String, i64)> = tx
                .query_row(
                    turn_ingress.turn_parks.delete_by_session_returning.sql(),
                    params![session_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?;
            if let Some((released_turn_id, released_park_id)) = released {
                crate::persistence::turn_park_feed::log_turn_park_closed_conn(
                    tx,
                    &session_id,
                    &released_turn_id,
                    released_park_id,
                    &lash_core_execution::store::ParkEventKind::Cancelled {
                        cause: lash_core_execution::store::ParkCancelCause::SessionDeleted,
                    },
                    crate::clamp_epoch_ms(now_ms),
                )?;
            }
            // Administration revokes the session's effect authority before
            // entering store deletion. Only then may the pinned closure
            // obligation and its selected-owner identity be retired.
            for statement in [
                queued_runs.delete_members.sql(),
                queued_runs.delete_runs.sql(),
                turn_ingress.pending_inputs.delete_by_session.sql(),
                crate::session_ingress::session_ingress_sql()
                    .shared
                    .delete_by_session
                    .sql(),
                crate::session_ingress::session_ingress_sql()
                    .shared
                    .delete_sequence
                    .sql(),
                turn_ingress.cancel_requests.delete_by_session.sql(),
                turn_ingress.closures.delete_by_session.sql(),
                turn_ingress.bindings.delete_by_session.sql(),
                turn_ingress.leases.delete_by_session.sql(),
            ] {
                tx.execute(statement, params![session_id.as_str()])
                    .map_err(sqlite_error)?;
            }
            // The session's logical roots and their input bindings go with
            // it; a `close_session` intent stays as its deletion tombstone.
            crate::session_roots::delete_session_roots_conn(tx, &session_id)?;
            // The session-core rows the family owns, named rather than spelled.
            for statement in [
                session_sql().observer_intents.delete_by_session.sql(),
                session_sql().meta.delete_by_session.sql(),
            ] {
                tx.execute(statement, params![session_id.as_str()])
                    .map_err(sqlite_error)?;
            }
            tx.execute(
                crate::attachments::attachment_sql()
                    .manifest_sqlite
                    .delete_deleted_session_roots
                    .sql(),
                [],
            )
            .map_err(sqlite_error)?;
            if let Some(checkpoint_ref) = checkpoint_ref.as_ref() {
                // Sever this root's outgoing projection before any blob delete
                // when the owner transaction removed its final head/anchor.
                // The root bytes may remain as another root's opaque component;
                // its projection no longer has a live root owner in that case.
                tx.execute(
                    session_sql()
                        .checkpoint_edges
                        .delete_unrooted_for_checkpoint
                        .sql(),
                    params![checkpoint_ref],
                )
                .map_err(sqlite_error)?;
            }
            // Every predicate is an indexed NOT EXISTS over exact edges; no
            // whole-catalog mark/sweep runs in this transaction.
            for blob_ref in candidates {
                let deleted = tx
                    .execute(
                        crate::artifact_store::artifact_sql()
                            .blobs_sqlite
                            .reclaim_session_candidate
                            .sql(),
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
                sweep = ?lash_core_execution::MaintenanceReport::sweep(&report),
                "session delete reclaimed owner-scoped blobs"
            );
            Ok(report.clone())
        })();
        Ok(match outcome {
            Ok(value) => TxOutcome::Commit(Ok(value)),
            Err(err) => {
                report.deleted_blob_count = 0;
                TxOutcome::Rollback(Err(lash_core_execution::MaintenanceFailure::failed(
                    err, report,
                )))
            }
        })
    })
    .await
    .map_err(|err| {
        lash_core_execution::MaintenanceFailure::failed_before_any_work(
            lash_core_execution::StoreError::Backend(err.to_string()),
        )
    })?
}

pub(super) async fn delete_wake_allocation_floors_from_process_registry(
    process_registry: &DatabaseTarget,
    target_session_id: &SessionId,
    policy: SqliteConnectionPolicy,
) -> Result<(), String> {
    if !process_registry.exists() {
        return Ok(());
    }
    let conn = SqliteConnection::open_with_policy(process_registry, policy)
        .await
        .map_err(|err| err.to_string())?;
    ensure_versioned_schema(&conn, SqliteDatabase::ProcessRegistry)
        .await
        .map_err(|err| err.to_string())?;
    let target_session_id = SessionId::from(target_session_id.to_string());
    conn.write_flow(move |tx| {
        let outcome = tx
            .execute(
                crate::process_registry::sql::process_sql()
                    .floor
                    .delete_by_session
                    .sql(),
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
