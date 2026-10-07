use super::*;
use crate::session_sql::session_sql;
use lash_core_execution::FleetFormat;

pub(super) async fn delete_session_from_catalog(
    catalog: &DatabaseLocation,
    session_id: &SessionId,
    policy: SqliteConnectionPolicy,
    now_ms: u64,
) -> lash_core_execution::MaintenanceResult<lash_core_execution::SessionBlobReclaimReport> {
    if !catalog.target().exists() {
        return Ok(lash_core_execution::SessionBlobReclaimReport::default());
    }
    let session_id = session_id.clone();
    let conn = SqliteConnection::open_with_policy(catalog.target(), policy)
        .await
        .map_err(|err| {
            lash_core_execution::MaintenanceFailure::failed_before_any_work(
                lash_core_execution::StoreError::Backend(err.to_string()),
            )
        })?;
    conn.install(FleetFormat::writable(), |tx| {
        crate::compat::fence(tx, FleetFormat::writable())
    })
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
            // A closing session's pins are its ended runs': the close cut
            // their turns' final commits short and no activation will ever
            // drain them, so they go with the storage below. Any other pin is
            // a live turn's closure, and refuses the delete.
            let closing = tx
                .query_row(
                    session_sql().meta.select_closing_intent.sql(),
                    params![session_id.as_str()],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()
                .map_err(sqlite_error)?
                .flatten()
                .is_some();
            let pending_count = if closing {
                0
            } else {
                tx.query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .closures
                        .count_by_session
                        .sql(),
                    params![session_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sqlite_error)?
            };
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
                crate::catalog::catalog_reads::record_session_terminal(
                    tx,
                    &session_id,
                    None,
                    crate::clamp_epoch_ms(now_ms),
                )?;
                // Permanent identity evidence for every deleted session id,
                // host-facing and runtime-internal alike. The deleted set is
                // also the reclaim frontier for the delete arm below: a
                // process-owned session id that never entered it would leave
                // its tombstoned rows unreachable forever, because the id is
                // just as unbindable as a host-facing one once deleted.
                crate::conn::cached_execute(
                    tx,
                    session_sql().deleted_sqlite.insert_from_meta.sql(),
                    params![session_id.as_str()],
                )
                .map_err(sqlite_error)?;
                crate::conn::cached_execute(
                    tx,
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
            if let Some(meta) = crate::codec::try_load_session_head_meta_from_conn(
                tx,
                &session_id,
                FleetFormat::current(),
            )? && let Some(frame_node_id) = meta.current_frame_node_id
            {
                let referrer = lash_core_execution::ArtifactReferrer::FrameEnvironment(
                    lash_core_execution::FrameEnvironmentId::new(session_id.clone(), frame_node_id),
                );
                crate::artifact_store::fence_artifact_referrer_tx(tx, &referrer, now_ms)
                    .map_err(sqlite_error)?;
                let cleanup =
                    lash_core_execution::ArtifactCleanup::ended(referrer, Vec::new(), None);
                crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now_ms)?;
            }
            if existed {
                crate::obligation_ledger::arm_cleanup_tx(
                    tx,
                    &lash_core_execution::ArtifactCleanup::Await(
                        lash_core_execution::ReferrerGuard::SessionGraphRetired(session_id.clone()),
                    ),
                    now_ms,
                )?;
            }
            // What the session roots: its head, and every revision it still
            // retains. Its pins and revisions go with it, so each root they
            // held becomes a reclaim candidate here.
            let text_column = |sql: &str| -> Result<Vec<String>, lash_core_execution::StoreError> {
                let mut stmt = tx.prepare(sql).map_err(sqlite_error)?;
                let rows = stmt
                    .query_map(params![session_id.as_str()], |row| row.get::<_, String>(0))
                    .map_err(sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
            };
            let mut roots: std::collections::BTreeSet<String> =
                text_column(session_sql().revisions.select_session_checkpoints.sql())?
                    .into_iter()
                    .collect();
            roots.extend(checkpoint_ref);
            let mut retained_leaves: std::collections::BTreeSet<String> =
                text_column(session_sql().revisions.select_session_leaves.sql())?
                    .into_iter()
                    .collect();
            retained_leaves.extend(leaf_node_id);
            let mut candidates = std::collections::BTreeSet::new();
            for checkpoint_ref in &roots {
                candidates.insert(checkpoint_ref.clone());
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
            for statement in [
                session_sql().head.delete_by_session.sql(),
                session_sql().revisions.delete_by_session.sql(),
                session_sql().pins.delete_by_session.sql(),
            ] {
                crate::conn::cached_execute(tx, statement, params![session_id.as_str()])
                    .map_err(sqlite_error)?;
            }
            for leaf_node_id in &retained_leaves {
                persistence::retire_unreachable_ancestry_conn(tx, leaf_node_id)?;
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
            // tombstoned *after* its owner is gone (ancestry retired at a fork
            // child's delete or collection),
            // and no session-scoped vacuum could ever reach it: the owning id is
            // permanently unbindable. Live sessions' rows stay resident for their
            // own vacuum, so this is not a catalog-wide sweep.
            crate::conn::cached_execute(
                tx,
                session_sql()
                    .graph_sqlite
                    .delete_tombstoned_reclaimable
                    .sql(),
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            crate::conn::cached_execute(
                tx,
                session_sql().lineage.delete_by_session.sql(),
                params![session_id.as_str()],
            )
            .map_err(sqlite_error)?;
            let turn_ingress = crate::turn_ingress::turn_ingress_sql();
            crate::conn::cached_execute(
                tx,
                turn_ingress.queued_batches.delete_by_session.sql(),
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
                turn_ingress.pending_inputs.delete_by_session.sql(),
                turn_ingress.run_specs.delete_session.sql(),
                crate::session_ingress::session_ingress_sql()
                    .delete_sequence
                    .sql(),
                turn_ingress.cancel_requests.delete_by_session.sql(),
                turn_ingress.closures.delete_by_session.sql(),
                turn_ingress.bindings.delete_by_session.sql(),
            ] {
                crate::conn::cached_execute(tx, statement, params![session_id.as_str()])
                    .map_err(sqlite_error)?;
            }
            // The session's logical runs and their input bindings go with
            // it; a `close_session` intent stays as its deletion tombstone.
            crate::session_runs::delete_session_runs_conn(tx, &session_id)?;
            // The session-core rows the family owns, named rather than spelled.
            for statement in [
                session_sql().observer_intents.delete_by_session.sql(),
                session_sql().meta.delete_by_session.sql(),
            ] {
                crate::conn::cached_execute(tx, statement, params![session_id.as_str()])
                    .map_err(sqlite_error)?;
            }
            if !candidates.is_empty() {
                // A superseded root outside this session can still reference
                // a candidate. Apply GC's root rules to every touching edge
                // before deleting any blob; surviving roots retain their edges.
                let candidate_json = crate::codec::encode_json(&candidates)?;
                crate::conn::cached_execute(
                    tx,
                    session_sql()
                        .checkpoint_edges
                        .delete_unrooted_for_candidates
                        .sql(),
                    params![candidate_json],
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
