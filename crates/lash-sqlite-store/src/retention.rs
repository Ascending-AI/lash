//! Terminal-session receipt sweep and dependent-root reconciliation.
use crate::session_sql::session_sql;
use crate::*;
use lash_core_execution::SessionCatalogStore as _;

pub(crate) type ReclaimResult = Result<
    lash_core_execution::store::RetentionReport,
    Box<lash_core_execution::MaintenanceFailure<lash_core_execution::store::RetentionReport>>,
>;

pub(crate) async fn reclaim(
    store: &SqliteStore,
    bound: lash_core_execution::store::RetentionBound,
) -> ReclaimResult {
    let failed_before_any_work = |error: lash_core_execution::StoreError| {
        Box::new(lash_core_execution::MaintenanceFailure::failed_before_any_work(error))
    };
    let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);
    let mut report = store
        .conn
        .write_flow(move |tx| {
            Ok(
                match (|| {
                    let sql = &session_sql().turn_commits;
                    let current: i64 = tx
                        .query_row(sql.change_clock.sql(), [], |row| row.get(0))
                        .map_err(sqlite_error)?;
                    let current =
                        u64::try_from(current).map_err(|_| StoreError::StoredDataCorrupt {
                            record_kind: "TurnChangeClock",
                            message: "negative current sequence".to_owned(),
                        })?;
                    let watermark = bound.turn_watermark.acknowledged_sequence(current)?;
                    let horizon: Option<i64> = tx
                        .query_row(
                            sql.removed_horizon.sql(),
                            params![cutoff, watermark],
                            |row| row.get(0),
                        )
                        .map_err(sqlite_error)?;
                    if let Some(horizon) = horizon {
                        crate::conn::cached_execute(
                            tx,
                            sql.advance_horizon.sql(),
                            params![horizon],
                        )
                        .map_err(sqlite_error)?;
                    }
                    let removed_session_terminal_count = crate::conn::cached_execute(
                        tx,
                        sql.delete_session_terminals.sql(),
                        params![cutoff, watermark],
                    )
                    .map_err(sqlite_error)?;
                    let removed_receipt_count = crate::conn::cached_execute(
                        tx,
                        session_sql().turn_commits_sqlite.delete_retained.sql(),
                        params![cutoff, watermark],
                    )
                    .map_err(sqlite_error)?;

                    Ok(lash_core_execution::store::RetentionReport {
                        removed_receipt_count,
                        removed_session_terminal_count,
                        removed_tool_intent_submission_count: 0,
                        removed_attachment_root_count: 0,
                        retired_effect_scope_count: 0,
                    })
                })() {
                    Ok(report) => TxOutcome::Commit(Ok(report)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                },
            )
        })
        .await
        .map_err(|error| failed_before_any_work(sqlite_error(error)))?
        .map_err(failed_before_any_work)?;
    report.removed_tool_intent_submission_count = reclaim_tool_intent_submissions(store, cutoff)
        .await
        .map_err(|error| {
            Box::new(lash_core_execution::MaintenanceFailure::failed(
                error,
                report.clone(),
            ))
        })?;
    Ok(report)
}

/// The process registry's half of the sweep (FIG-1509): the host
/// tool-intent submission ledger is retained evidence of its owner session.
/// The owner-death proof is read through
/// `lookup_session`: candidates come from the registry's tables, and only
/// owners the catalog reports deleted are fenced. A deletion is permanent,
/// so the proof still holds when a later transaction fences each owner and
/// deletes its rows older than the bound.
async fn reclaim_tool_intent_submissions(
    store: &SqliteStore,
    cutoff: i64,
) -> Result<usize, StoreError> {
    let conn = &store.conn;
    let candidates = conn
        .call(move |conn| {
            let mut stmt = conn.prepare_cached(
                crate::turn_ingress::tool_intent_sql()
                    .sqlite
                    .select_reclaim_candidate_sessions
                    .sql(),
            )?;
            let rows = stmt.query_map(params![cutoff], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(sqlite_error)?;
    let mut deleted_owner_ids = Vec::new();
    for owner_id in candidates {
        let session_id =
            SessionId::parse(&owner_id).map_err(|error| StoreError::StorageFailure {
                backend: SQLITE_BACKEND,
                message: format!(
                    "tool-intent submission names a malformed session owner `{owner_id}`: {error}"
                ),
            })?;
        if store.lookup_session(&session_id).await? == lash_core_execution::SessionLookup::Deleted {
            deleted_owner_ids.push(owner_id);
        }
    }
    let deleted_owner_ids_json =
        serde_json::to_string(&deleted_owner_ids).map_err(|error| StoreError::StorageFailure {
            backend: SQLITE_BACKEND,
            message: format!("encode deleted tool-intent owner ids: {error}"),
        })?;
    // Owners fenced by an earlier sweep may still hold rows that were inside
    // its bound, so the delete runs even when no owner is newly proved dead.
    conn.write(move |tx| {
        let sql = crate::turn_ingress::tool_intent_sql();
        crate::conn::cached_execute(
            tx,
            sql.sqlite.fence_retired_owners.sql(),
            params![deleted_owner_ids_json],
        )?;
        crate::conn::cached_execute(tx, sql.shared.reclaim_retired.sql(), params![cutoff])
    })
    .await
    .map_err(sqlite_error)
}
