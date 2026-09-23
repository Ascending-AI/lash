//! Terminal-session receipt sweep and dependent-root reconciliation (FIG-2502),
//! and the durable owner of deferred effect-scope retirement (ADR 0067).
use crate::session_sql::session_sql;
use crate::*;

/// The sweep's outcome, boxed on the failure side: `MaintenanceFailure`
/// carries the partial report beside the stop, so the `Err` arm is several
/// times the size of the report alone (`clippy::result_large_err`); the
/// factory's trait method, whose signature the trait fixes, unboxes it.
pub(crate) type ReclaimResult = Result<
    lash_core_execution::store::RetentionReport,
    Box<lash_core_execution::MaintenanceFailure<lash_core_execution::store::RetentionReport>>,
>;

pub(crate) async fn reclaim(
    factory: &PostgresSessionStoreFactory,
    bound: lash_core_execution::store::RetentionBound,
) -> ReclaimResult {
    async {
        let mut tx = factory.pool.begin().await.map_err(store_sqlx_error)?;
        // One cross-worker fence for this host-invoked, atomic multi-phase sweep.
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_evidence_retention
                .sql(),
        )
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
            session_sql()
                .turn_commits_postgres
                .delete_retained_except_live
                .sql(),
        )
        .bind(clamp_epoch_ms(bound.committed_before_epoch_ms))
        .bind(&live_scope_receipt_keys)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        // Only terminal usage becomes eligible; live ledgers reconstruct
        // resumed accounting. Anti-join after the receipt-root sweep.
        let removed_usage_delta_count = sqlx::query(session_sql().usage.delete_reclaimable.sql())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected() as usize;
        // Terminal markers replace the positive receipt oracle for deleted
        // owners; graph retention independently protects committed attachments.
        let removed_attachment_root_count = sqlx::query(
            crate::attachments::attachment_sql()
                .manifest_postgres
                .delete_deleted_session_roots
                .sql(),
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        tx.commit().await.map_err(store_sqlx_error)?;
        // Phase 0 deleted journal rows a parked claim in this process may be
        // waiting on; wake them to re-read. Other processes see it on their
        // cross-process poll.
        lash_core_execution::facade_support::effect_replay_driver::EffectJournalNotifiers::announce_journal(
            &effect_replay::journal_identity(&factory.await_event_signing_secret),
        );
        Ok(lash_core_execution::store::RetentionReport {
            removed_receipt_count,
            removed_usage_delta_count,
            removed_attachment_root_count,
            retired_effect_scope_count,
        })
    }
    .await
    .map_err(|error| {
        Box::new(lash_core_execution::MaintenanceFailure::failed_before_any_work(error))
    })
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
        crate::effect_replay::effect_sql()
            .journal
            .select_session_free_scope_ids
            .sql(),
    )
    .fetch_all(&mut **tx)
    .await?;
    let waited: Vec<String> = sqlx::query_scalar(
        crate::await_event::wait_sql()
            .shared
            .select_session_free_scope_json
            .sql(),
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut scopes: Vec<lash_core_execution::ExecutionScope> = keyed
        .iter()
        .filter_map(|key| lash_core_execution::ExecutionScope::from_journal_key(key))
        .chain(
            waited
                .iter()
                .filter_map(|scope_json| serde_json::from_str(scope_json).ok()),
        )
        .filter(|scope| {
            matches!(
                scope,
                lash_core_execution::ExecutionScope::RuntimeOperation { operation_id }
                    if lash_core_execution::store::is_facade_minted_operation_id(operation_id)
            )
        })
        .collect();
    scopes.sort_by(|left, right| left.id().cmp(right.id()));
    scopes.dedup();
    let mut retired = 0;
    let mut live_receipt_keys = Vec::new();
    for scope in scopes {
        let Ok(receipt_key) =
            lash_core_execution::store::plugin_operation_receipt_storage_key(&scope)
        else {
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
        let receipt_recorded: bool =
            sqlx::query_scalar(session_sql().turn_commits.exists_for_operation.sql())
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
