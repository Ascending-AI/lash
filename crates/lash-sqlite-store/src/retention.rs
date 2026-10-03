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
                    let mut removed_receipt_count = crate::conn::cached_execute(
                        tx,
                        session_sql().turn_commits_sqlite.delete_retained.sql(),
                        params![cutoff, watermark],
                    )
                    .map_err(sqlite_error)?;
                    let tool_sql = lash_store_sql::tool_receipts::ToolReceiptStatements::render(
                        crate::schema_layout::Schema::Main.dialect(),
                    );
                    removed_receipt_count +=
                        crate::conn::cached_execute(tx, tool_sql.reclaim.sql(), params![cutoff])
                            .map_err(sqlite_error)?;
                    let wait_sql = lash_store_sql::wait_receipts::WaitReceiptStatements::render(
                        crate::schema_layout::Schema::Main.dialect(),
                    );
                    removed_receipt_count +=
                        crate::conn::cached_execute(tx, wait_sql.reclaim.sql(), params![cutoff])
                            .map_err(sqlite_error)?;

                    Ok(lash_core_execution::store::RetentionReport {
                        removed_receipt_count,
                        removed_session_terminal_count,
                        removed_trigger_mutation_receipt_count: 0,
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
    if let Some(trigger_store) = store.trigger_store.as_ref() {
        report.removed_trigger_mutation_receipt_count =
            reclaim_trigger_mutation_receipts(store, trigger_store, cutoff)
                .await
                .map_err(|error| {
                    Box::new(lash_core_execution::MaintenanceFailure::failed(
                        error,
                        report.clone(),
                    ))
                })?;
    }
    if let Some(process_registry) = store.process_registry.as_ref() {
        report.removed_tool_intent_submission_count =
            reclaim_tool_intent_submissions(store, process_registry, cutoff)
                .await
                .map_err(|error| {
                    Box::new(lash_core_execution::MaintenanceFailure::failed(
                        error,
                        report.clone(),
                    ))
                })?;
    }
    Ok(report)
}

/// The process registry's half of the sweep (FIG-1509): the host
/// tool-intent submission ledger is retained evidence of its owner session.
/// Like the trigger arm, the owner-death proof crosses databases at the Rust
/// boundary: candidates come from the registry, and only owners the durable
/// core reports deleted are fenced. A deletion is permanent, so the proof
/// still holds when the registry's own transaction fences each owner and
/// deletes its rows older than the bound.
async fn reclaim_tool_intent_submissions(
    store: &SqliteStore,
    process_registry: &DatabaseTarget,
    cutoff: i64,
) -> Result<usize, StoreError> {
    if !process_registry.exists() {
        return Ok(0);
    }
    let conn =
        SqliteConnection::open_with_policy(process_registry, store.options.connection_policy)
            .await
            .map_err(sqlite_async_error)?;
    conn.install(
        SqliteDatabase::ProcessRegistry,
        lash_core_execution::FleetFormat::writable(),
        |tx| {
            crate::compat::fence(
                tx,
                SqliteDatabase::ProcessRegistry,
                lash_core_execution::FleetFormat::writable(),
            )
        },
    )
    .await
    .map_err(sqlite_error)?;
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

/// The trigger database's half of the sweep (FIG-4108): mutation receipts are
/// durable evidence, reclaimed by the same bound as every other kind, so the
/// low-level per-kind primitive is gone. The trigger database is its own
/// file in the store set, so — like the process-registry arm of
/// `delete_session` — its writes go through a connection of their own that
/// installs and fences `SqliteDatabase::Triggers` rather than an `ATTACH`ed
/// name.
async fn reclaim_trigger_mutation_receipts(
    store: &SqliteStore,
    trigger_store: &DatabaseTarget,
    cutoff: i64,
) -> Result<usize, StoreError> {
    if !trigger_store.exists() {
        return Ok(0);
    }
    let conn = SqliteConnection::open_with_policy(trigger_store, store.options.connection_policy)
        .await
        .map_err(sqlite_async_error)?;
    conn.install(
        SqliteDatabase::Triggers,
        lash_core_execution::FleetFormat::writable(),
        |tx| {
            crate::compat::fence(
                tx,
                SqliteDatabase::Triggers,
                lash_core_execution::FleetFormat::writable(),
            )
        },
    )
    .await
    .map_err(sqlite_error)?;
    // Enumerate the session owners with a receipt older than the bound, then
    // keep only the durably deleted ones: `deleted_sessions` lives in the
    // durable core, so the proof crosses databases at the Rust boundary the
    // reconcile driver already uses (`lookup_session` on each candidate).
    let session_owners = conn
        .call(move |conn| {
            let mut stmt = conn.prepare_cached(
                crate::triggers::trigger_sql()
                    .retention_sqlite
                    .select_receipt_session_owners
                    .sql(),
            )?;
            let rows = stmt.query_map(params![cutoff], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(sqlite_error)?;
    let mut deleted_owner_ids = Vec::new();
    for owner_id in session_owners {
        let session_id =
            SessionId::parse(&owner_id).map_err(|error| StoreError::StorageFailure {
                backend: SQLITE_BACKEND,
                message: format!(
                    "trigger mutation receipt names a malformed session owner `{owner_id}`: {error}"
                ),
            })?;
        if store.lookup_session(&session_id).await? == lash_core_execution::SessionLookup::Deleted {
            deleted_owner_ids.push(owner_id);
        }
    }
    // Ownerless (host/platform) receipts are swept even when no session
    // owner was durably deleted, so the write transaction always runs.
    let deleted_owner_ids_json =
        serde_json::to_string(&deleted_owner_ids).map_err(|error| StoreError::StorageFailure {
            backend: SQLITE_BACKEND,
            message: format!("encode deleted trigger owner ids: {error}"),
        })?;
    sweep_trigger_mutation_receipts(&conn, cutoff, deleted_owner_ids_json).await
}

/// Delete every sweep-eligible receipt in one fenced write transaction: the
/// bound's ownerless rows and the deleted owners' rows, with the outstanding-
/// delivery guard re-proved inside the transaction.
async fn sweep_trigger_mutation_receipts(
    conn: &SqliteConnection,
    cutoff: i64,
    deleted_owner_ids_json: String,
) -> Result<usize, StoreError> {
    conn.write(move |tx| {
        crate::conn::cached_execute(
            tx,
            crate::triggers::trigger_sql()
                .retention_sqlite
                .reclaim_mutation_receipts
                .sql(),
            params![cutoff, deleted_owner_ids_json],
        )
    })
    .await
    .map_err(sqlite_error)
}
