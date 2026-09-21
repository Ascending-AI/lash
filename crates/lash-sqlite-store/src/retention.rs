//! Terminal-session receipt sweep and dependent-root reconciliation (FIG-2502),
//! and the durable owner of deferred effect-scope retirement (ADR 0067).
use crate::*;

use crate::await_event::wait_sql;
use crate::scope_fence::Schema;
use crate::session_sql::session_sql;

/// The schema the bound effect journal is attached under for a sweep.
const EFFECT_JOURNAL_SCHEMA: Schema = Schema::EffectJournal;

/// The sweep's outcome, boxed on the failure side: `MaintenanceFailure`
/// carries the partial report beside the stop, so the `Err` arm is several
/// times the size of the report alone (`clippy::result_large_err`); the
/// factory's trait method, whose signature the trait fixes, unboxes it.
pub(crate) type ReclaimResult = Result<
    lash_core::store::RetentionReport,
    Box<lash_core::MaintenanceFailure<lash_core::store::RetentionReport>>,
>;

pub(crate) async fn reclaim(
    factory: &SqliteSessionStoreFactory,
    bound: lash_core::store::RetentionBound,
) -> ReclaimResult {
    let failed_before_any_work = |error: lash_core::StoreError| {
        Box::new(lash_core::MaintenanceFailure::failed_before_any_work(error))
    };
    // Attach order is lock order under `BEGIN IMMEDIATE`: the catalog, then
    // the journal, then the registry — the order a journal connection with
    // the registry attached takes its own locks in, so the two never wait on
    // each other in a cycle.
    let store = factory
        .open_catalog_for_maintenance_without_registry("evidence retention")
        .await
        .map_err(failed_before_any_work)?;
    let effect_journal = factory
        .effect_journal_path
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let journal_attached = if let Some(path) = effect_journal {
        let path = path.to_string_lossy().into_owned();
        store
            .conn
            .call(move |connection| {
                connection.execute(
                    // `ATTACH` names a schema rather than qualifying a table,
                    // so the renderer has nothing to say about it; the name is
                    // `Schema::EffectJournal.qualifier()`.
                    "ATTACH DATABASE ?1 AS effect_journal",
                    params![path],
                )
            })
            .await
            .map_err(|error| failed_before_any_work(sqlite_error(error)))?;
        true
    } else {
        false
    };
    let registry_attached = if let Some(path) = factory.process_registry_path.as_deref() {
        lifecycle::attach_process_registry(&store.conn, path, factory.options.connection_policy)
            .await
            .map_err(|error| {
                failed_before_any_work(lash_core::StoreError::Backend(format!(
                    "evidence retention aborted: process registry {} could not be attached: {error}",
                    path.display()
                )))
            })?;
        true
    } else {
        false
    };
    let fences = if registry_attached {
        crate::scope_fence::FenceLocations::attached(EFFECT_JOURNAL_SCHEMA)
    } else {
        crate::scope_fence::FenceLocations::journal_only_at(EFFECT_JOURNAL_SCHEMA)
    };
    let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);
    let now_ms = factory.clock.timestamp_ms();
    store
        .conn
        .write(move |tx| {
            // Phase 0: deferred scope retirement (ADR 0049 / ADR 0067). First
            // the cleanup a process-scope retirement may still owe: rows under
            // a scope the registry file fences are garbage a lost purge left.
            // Then a session-free runtime-operation scope the facade minted,
            // whose operation recorded its receipt, is retired once nothing
            // is live under it, under the same write fence the receipt-time
            // retirement takes. The receipts of scopes that are still live
            // are kept past the horizon below: they are the proof a later
            // sweep needs.
            let (retired_effect_scope_count, live_scope_receipt_keys) = if journal_attached {
                effect_replay::purge_rows_under_fenced_scopes(tx, EFFECT_JOURNAL_SCHEMA, fences)?;
                retire_quiescent_operation_scopes(tx, now_ms)?
            } else {
                (0, Vec::new())
            };
            // Phase 1: terminal evidence roots. deleted_sessions is permanent
            // identity evidence, exempt from retention (FIG-754 / FIG-748).
            // Two named statements, one per filter shape the sweep actually
            // issues. SQLite has no spelling for an empty `NOT IN (...)`, and
            // the exclusion list rides as one JSON array rather than as a
            // per-call placeholder run.
            let removed_receipt_count = if live_scope_receipt_keys.is_empty() {
                tx.execute(
                    session_sql().turn_commits_sqlite.delete_retained.sql(),
                    params![cutoff],
                )?
            } else {
                let live_keys =
                    serde_json::to_string(&live_scope_receipt_keys).map_err(|error| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                            format!("failed to encode live scope receipt keys: {error}"),
                        )))
                    })?;
                tx.execute(
                    session_sql()
                        .turn_commits_sqlite
                        .delete_retained_except_live
                        .sql(),
                    params![cutoff, live_keys],
                )?
            };
            // Phase 2: correlated anti-joins reconcile dependent rows after the
            // receipt sweep, under the same BEGIN IMMEDIATE fence. Live ledgers
            // are never eligible; they rebuild resumed-session accounting.
            let removed_usage_delta_count =
                tx.execute(session_sql().usage.delete_reclaimable.sql(), [])?;
            // The permanent terminal marker proves intent-owner death even after
            // the positive supersession receipt is gone. Retained graph prefixes
            // protect committed attachments independently of receipt retention.
            let removed_attachment_root_count = tx.execute(
                crate::attachments::attachment_sql()
                    .manifest_sqlite
                    .delete_deleted_session_roots
                    .sql(),
                [],
            )?;
            Ok(lash_core::store::RetentionReport {
                removed_receipt_count,
                removed_usage_delta_count,
                removed_attachment_root_count,
                retired_effect_scope_count,
            })
        })
        .await
        .map_err(|error| failed_before_any_work(sqlite_error(error)))
}

/// Retire every session-free runtime-operation scope in the attached journal
/// that the facade minted (`is_facade_minted_operation_id`), whose operation
/// has recorded its receipt in this catalog, and that is quiescent now.
/// Caller-supplied scopes are never the sweep's to retire: a host that names
/// its own operation id may retry it after a lost response and expects the
/// receipt to replay (ADR 0067). Returns the number retired and the receipt
/// keys of the scopes that are still live, which the receipt sweep must keep.
fn retire_quiescent_operation_scopes(
    tx: &rusqlite::Transaction<'_>,
    now_ms: u64,
) -> rusqlite::Result<(usize, Vec<String>)> {
    let mut scopes: Vec<lash_core::ExecutionScope> = Vec::new();
    {
        let mut keyed = tx.prepare(
            effect_replay::effect_sql(EFFECT_JOURNAL_SCHEMA)
                .journal
                .select_session_free_scope_ids
                .sql(),
        )?;
        for key in keyed.query_map([], |row| row.get::<_, String>(0))? {
            if let Some(scope) = lash_core::ExecutionScope::from_journal_key(&key?) {
                scopes.push(scope);
            }
        }
        let mut waited = tx.prepare(
            wait_sql(EFFECT_JOURNAL_SCHEMA)
                .shared
                .select_session_free_scope_json
                .sql(),
        )?;
        for scope_json in waited.query_map([], |row| row.get::<_, String>(0))? {
            if let Ok(scope) = serde_json::from_str::<lash_core::ExecutionScope>(&scope_json?) {
                scopes.push(scope);
            }
        }
    }
    scopes.sort_by(|left, right| left.id().cmp(right.id()));
    scopes.dedup();
    let mut retired = 0;
    let mut live_receipt_keys = Vec::new();
    for scope in scopes.into_iter().filter(|scope| {
        matches!(
            scope,
            lash_core::ExecutionScope::RuntimeOperation { operation_id }
                if lash_core::store::is_facade_minted_operation_id(operation_id)
        )
    }) {
        let Ok(receipt_key) = lash_core::store::plugin_operation_receipt_storage_key(&scope) else {
            continue;
        };
        let Ok(identity) = scope.journal_identity() else {
            continue;
        };
        #[expect(
            clippy::expect_used,
            reason = "`ExecutionScope` is a derived-`Serialize` enum of strings, so encoding it cannot fail"
        )]
        let scope_json = serde_json::to_string(&scope).expect("execution scopes serialize");
        let receipt_recorded: bool = tx.query_row(
            session_sql().turn_commits.exists_for_operation.sql(),
            params![receipt_key],
            |row| row.get(0),
        )?;
        if !receipt_recorded {
            continue;
        }
        let closure_pinned = effect_replay::scope_has_turn_cancel_closure_participant(
            tx,
            EFFECT_JOURNAL_SCHEMA,
            identity.key(),
        )?;
        if !closure_pinned
            && effect_replay::scope_is_quiescent(
                tx,
                EFFECT_JOURNAL_SCHEMA,
                identity.key(),
                &scope_json,
            )?
        {
            effect_replay::retire_scope_rows(
                tx,
                EFFECT_JOURNAL_SCHEMA,
                identity.key(),
                &scope_json,
                now_ms,
            )?;
            retired += 1;
        } else {
            live_receipt_keys.push(receipt_key);
        }
    }
    Ok((retired, live_receipt_keys))
}

impl SqliteSessionStoreFactory {
    /// Open the existing catalog with the factory connection and registry options
    /// for a host-invoked maintenance operation.
    pub(crate) async fn open_catalog_for_maintenance(
        &self,
        operation: &str,
    ) -> Result<Store, lash_core::StoreError> {
        self.open_catalog_for_maintenance_configured(operation, true)
            .await
    }

    /// Like [`Self::open_catalog_for_maintenance`] with the configured process
    /// registry left for the caller to attach after the files it must lock
    /// ahead of it.
    pub(crate) async fn open_catalog_for_maintenance_without_registry(
        &self,
        operation: &str,
    ) -> Result<Store, lash_core::StoreError> {
        self.open_catalog_for_maintenance_configured(operation, false)
            .await
    }

    async fn open_catalog_for_maintenance_configured(
        &self,
        operation: &str,
        attach_process_registry: bool,
    ) -> Result<Store, lash_core::StoreError> {
        let path = self.catalog_path();
        if !path.exists() {
            return Err(lash_core::StoreError::Backend(format!(
                "maintenance {operation} aborted: durable-core catalog {} does not exist",
                path.display()
            )));
        }
        Store::open_with_options_clock_and_process_registry(
            &path,
            self.options,
            Arc::clone(&self.clock),
            self.process_registry_path
                .as_deref()
                .filter(|_| attach_process_registry),
            self.turn_cancel_closure_owner_binding(),
            #[cfg(feature = "testing")]
            self.fault_injector.clone(),
        )
        .await
        .map_err(|err| {
            lash_core::StoreError::Backend(format!(
                "maintenance {operation} aborted: durable-core catalog {} could not be opened: {err}",
                path.display()
            ))
        })
    }
}
