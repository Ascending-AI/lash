use super::*;
use crate::session_sql::session_sql;
use lash_core_store::store::FleetFormat;

/// Delete `session_id` from the deployment's catalog on `conn`, the store's
/// writer connection, so the delete keeps the store's connection policy and
/// hooks.
pub(super) async fn delete_session_from_catalog(
    conn: &SqliteConnection,
    session_id: &SessionId,
    now_ms: u64,
) -> lash_core_store::store::MaintenanceResult<lash_core_store::store::SessionBlobReclaimReport> {
    let session_id = session_id.clone();
    conn.write_flow(move |tx| {
        let mut report = lash_core_store::store::SessionBlobReclaimReport::default();
        let outcome: Result<
            lash_core_store::store::SessionBlobReclaimReport,
            lash_core_store::store::StoreError,
        > = (|| {
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
                // host-facing and runtime-internal alike: the admission fence
                // that refuses a re-bind (ADR 0049).
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
                None,
            )? && let Some(frame_node_id) = meta.current_frame_node_id
            {
                let referrer =
                    lash_core_store::artifact_referrer::ArtifactReferrer::FrameEnvironment(
                        lash_core_store::artifact_referrer::FrameEnvironmentId::new(
                            session_id.clone(),
                            frame_node_id,
                        ),
                    );
                crate::artifact_store::fence_artifact_referrer_tx(tx, &referrer, now_ms)
                    .map_err(sqlite_error)?;
                let cleanup = lash_core_store::artifact_referrer::ArtifactCleanup::ended(
                    referrer,
                    Vec::new(),
                    None,
                );
                crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now_ms)?;
            }
            if existed {
                crate::obligation_ledger::arm_cleanup_tx(
                    tx,
                    &lash_core_store::artifact_referrer::ArtifactCleanup::Await(
                        lash_core_store::artifact_referrer::ReferrerGuard::SessionGraphRetired(
                            session_id.clone(),
                        ),
                    ),
                    now_ms,
                )?;
            }
            // What the session roots: its head, every revision it still
            // retains (the windows its terminals name among them), its turns'
            // transcripts and its admission base. Its pins, revisions, runs and
            // metadata go with it, so each root they held is retired here and
            // held below.
            let text_column =
                |sql: &str| -> Result<Vec<String>, lash_core_store::store::StoreError> {
                    let mut stmt = tx.prepare(sql).map_err(sqlite_error)?;
                    let rows = stmt
                        .query_map(params![session_id.as_str()], |row| row.get::<_, String>(0))
                        .map_err(sqlite_error)?;
                    rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
                };
            let mut roots: std::collections::BTreeSet<String> =
                text_column(session_sql().revisions.select_session_checkpoints.sql())?
                    .into_iter()
                    .chain(text_column(
                        crate::session_runs::session_runs_sql()
                            .runs
                            .select_session_transcripts
                            .sql(),
                    )?)
                    .collect();
            roots.extend(checkpoint_ref);
            roots.extend(
                tx.query_row(
                    session_sql().meta.select_admission_base.sql(),
                    params![session_id.as_str()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(sqlite_error)?
                .flatten(),
            );
            let mut retained_leaves: std::collections::BTreeSet<String> =
                text_column(session_sql().revisions.select_session_leaves.sql())?
                    .into_iter()
                    .collect();
            retained_leaves.extend(leaf_node_id);
            for statement in [
                session_sql().head.delete_by_session.sql(),
                session_sql().revisions.delete_by_session.sql(),
                session_sql().pins.delete_by_session.sql(),
            ] {
                crate::conn::cached_execute(tx, statement, params![session_id.as_str()])
                    .map_err(sqlite_error)?;
            }
            // The session's logical runs and their input bindings go with
            // it. A `close_session` intent stays as its deletion tombstone.
            crate::session_runs::delete_session_runs_conn(tx, &session_id)?;
            // Its engine rows go too, every turn's ledger and every cell's
            // working state, and each root they held is retired with them.
            roots.extend(
                crate::durable::retire_session_rows(tx, &session_id).map_err(sqlite_error)?,
            );
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
            // tombstoned row with no live owner. A node can be tombstoned
            // *after* its owner is gone (ancestry retired at a fork child's
            // delete or collection), and no session-scoped vacuum could ever
            // reach it. Live sessions' rows stay resident for their own vacuum,
            // so this is not a catalog-wide sweep.
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

            // Administration revokes the session's effect authority before
            // entering store deletion. Only then may the pinned closure
            // obligation and its selected-owner identity be retired.
            for statement in [
                turn_ingress.pending_inputs.delete_by_session.sql(),
                turn_ingress.run_specs.delete_session.sql(),
                turn_ingress.forsaken_inputs.delete_by_session.sql(),
                crate::session_ingress::session_ingress_sql()
                    .delete_sequence
                    .sql(),
                turn_ingress.cancel_requests.delete_by_session.sql(),
            ] {
                crate::conn::cached_execute(tx, statement, params![session_id.as_str()])
                    .map_err(sqlite_error)?;
            }
            // The session-core rows the family owns, named rather than spelled.
            for statement in [
                session_sql().observer_intents.delete_by_session.sql(),
                session_sql().meta.delete_by_session.sql(),
            ] {
                crate::conn::cached_execute(tx, statement, params![session_id.as_str()])
                    .map_err(sqlite_error)?;
            }
            // Deleting the session is the explicit retention its prompt
            // snapshots wait for: its roots go, and every text no other
            // root shares.
            crate::durable::release_prompt_snapshots(tx, &session_id, None)
                .map_err(sqlite_error)?;
            // The session's roots are retired: hold them, closures and all,
            // for the retirement steps, which delete what no other root
            // names or reaches in bounded transactions of their own. The
            // session's exclusive heap is never peeled here.
            report.held_root_count = crate::durable::retire_roots(
                tx,
                &roots,
                lash_durable::DurableInstant(crate::clamp_epoch_ms(now_ms)),
            )
            .map_err(sqlite_error)?;
            tracing::debug!(
                session_id = session_id.as_str(),
                held_root_count = report.held_root_count,
                "session delete held its roots for retirement"
            );
            Ok(report.clone())
        })();
        Ok(match outcome {
            Ok(value) => TxOutcome::Commit(Ok(value)),
            Err(err) => {
                report.held_root_count = 0;
                TxOutcome::Rollback(Err(lash_core_store::store::MaintenanceFailure::failed(
                    err, report,
                )))
            }
        })
    })
    .await
    .map_err(|err| {
        lash_core_store::store::MaintenanceFailure::failed_before_any_work(sqlite_error(err))
    })?
}
