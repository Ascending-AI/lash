//! Terminal-session receipt sweep and dependent-root reconciliation (FIG-2502),
//! and the durable owner of deferred effect-scope retirement (ADR 0067).
use crate::*;

/// The sweep's outcome, boxed on the failure side: `MaintenanceFailure`
/// carries the partial report beside the stop, so the `Err` arm is several
/// times the size of the report alone (`clippy::result_large_err`); the
/// factory's trait method, whose signature the trait fixes, unboxes it.
pub(crate) type ReclaimResult = Result<
    lash_core::store::RetentionReport,
    Box<lash_core::MaintenanceFailure<lash_core::store::RetentionReport>>,
>;

pub(crate) async fn reclaim(
    factory: &PostgresSessionStoreFactory,
    bound: lash_core::store::RetentionBound,
) -> ReclaimResult {
    async {
        let mut tx = factory.pool.begin().await.map_err(store_sqlx_error)?;
        // One cross-worker fence for this host-invoked, atomic multi-phase sweep.
        sqlx::query("SELECT pg_advisory_xact_lock(715423, 0)")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        // Phase 0: deferred scope retirement (ADR 0049 / ADR 0067). A
        // session-free runtime-operation scope whose operation recorded its
        // receipt is retired once nothing is live under it, under the same
        // scope lock the receipt-time retirement takes. The receipts of
        // scopes that are still live are kept past the horizon below: they
        // are the proof a later sweep needs.
        let (retired_effect_scope_count, live_scope_receipt_keys) =
            retire_quiescent_operation_scopes(&mut tx)
                .await
                .map_err(store_sqlx_error)?;
        // deleted_sessions permanently protects identity reuse (FIG-754 / FIG-748).
        let removed_receipt_count = sqlx::query(
            "DELETE FROM lash_runtime_turn_commits AS receipt
             WHERE receipt.committed_at_ms < $1
               AND NOT (receipt.turn_id = ANY($2))
               AND EXISTS (SELECT 1 FROM lash_deleted_sessions AS deleted
                           WHERE deleted.session_id = receipt.session_id)",
        )
        .bind(clamp_epoch_ms(bound.committed_before_epoch_ms))
        .bind(&live_scope_receipt_keys)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        // Only terminal usage becomes eligible; live ledgers reconstruct
        // resumed accounting. Anti-join after the receipt-root sweep.
        let removed_usage_delta_count = sqlx::query(
            "DELETE FROM lash_usage_deltas AS usage
             WHERE EXISTS (SELECT 1 FROM lash_deleted_sessions AS deleted
                           WHERE deleted.session_id = usage.session_id)
               AND NOT EXISTS (SELECT 1 FROM lash_runtime_turn_commits AS receipt
                               WHERE receipt.session_id = usage.session_id
                                 AND receipt.turn_id = usage.operation_storage_key)",
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        // Terminal markers replace the positive receipt oracle for deleted
        // owners; graph retention independently protects committed attachments.
        let removed_attachment_root_count =
            sqlx::query(crate::attachments::RECLAIM_DELETED_ATTACHMENT_ROOTS)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected() as usize;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::store::RetentionReport {
            removed_receipt_count,
            removed_usage_delta_count,
            removed_attachment_root_count,
            retired_effect_scope_count,
        })
    }
    .await
    .map_err(|error| Box::new(lash_core::MaintenanceFailure::failed_before_any_work(error)))
}

/// Retire every session-free runtime-operation scope that the facade minted
/// (`is_facade_minted_operation_id`), whose operation has recorded its
/// receipt, and that is quiescent now. Caller-supplied scopes are never the
/// sweep's to retire: a host that names its own operation id may retry it
/// after a lost response and expects the receipt to replay (ADR 0067).
/// Returns the number retired and the receipt keys of the scopes that are
/// still live, which the receipt sweep must keep.
async fn retire_quiescent_operation_scopes(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(usize, Vec<String>), sqlx::Error> {
    let keyed: Vec<String> = sqlx::query_scalar(
        "SELECT scope_id FROM lash_runtime_effect_replay WHERE session_id IS NULL
         UNION
         SELECT scope_id FROM lash_runtime_effect_group WHERE session_id IS NULL",
    )
    .fetch_all(&mut **tx)
    .await?;
    let waited: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT scope_json FROM lash_await_event_waits WHERE session_id IS NULL",
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut scopes: Vec<lash_core::ExecutionScope> = keyed
        .iter()
        .filter_map(|key| lash_core::ExecutionScope::from_journal_key(key))
        .chain(
            waited
                .iter()
                .filter_map(|scope_json| serde_json::from_str(scope_json).ok()),
        )
        .filter(|scope| {
            matches!(
                scope,
                lash_core::ExecutionScope::RuntimeOperation { operation_id }
                    if lash_core::store::is_facade_minted_operation_id(operation_id)
            )
        })
        .collect();
    scopes.sort_by(|left, right| left.id().cmp(right.id()));
    scopes.dedup();
    let mut retired = 0;
    let mut live_receipt_keys = Vec::new();
    for scope in scopes {
        let Ok(receipt_key) = lash_core::store::plugin_operation_receipt_storage_key(&scope) else {
            continue;
        };
        let Ok(identity) = scope.journal_identity() else {
            continue;
        };
        let scope_json = serde_json::to_string(&scope).expect("execution scopes serialize");
        let receipt_recorded: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM lash_runtime_turn_commits WHERE turn_id = $1)",
        )
        .bind(&receipt_key)
        .fetch_one(&mut **tx)
        .await?;
        if !receipt_recorded {
            continue;
        }
        crate::await_event::lock_scope(tx, identity.key()).await?;
        let closure_pinned =
            effect_replay::scope_has_turn_cancel_closure_participant(tx, identity.key()).await?;
        if !closure_pinned
            && effect_replay::scope_is_quiescent(tx, identity.key(), &scope_json).await?
        {
            effect_replay::retire_scope_rows_tx(tx, identity.key(), &scope_json).await?;
            retired += 1;
        } else {
            live_receipt_keys.push(receipt_key);
        }
    }
    Ok((retired, live_receipt_keys))
}
