//! Terminal-session receipt sweep and dependent-root reconciliation (FIG-2502).
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
    factory: &PostgresStore,
    bound: lash_core_execution::store::RetentionBound,
) -> ReclaimResult {
    async {
        let mut tx = begin_guarded(&factory.pool, &factory.fence).await?;
        // One cross-worker fence for this host-invoked, atomic multi-phase sweep.
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_evidence_retention
                .sql(),
        )
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let sql = &session_sql().turn_commits;
        let current: i64 = sqlx::query_scalar(sql.lock_change_clock.sql())
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let current = crate::support::u64_from_sql("TurnChangeClock", "current_seq", current)?;
        let watermark = bound.turn_watermark.acknowledged_sequence(current)?;
        let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);
        let horizon: Option<i64> = sqlx::query_scalar(sql.removed_horizon.sql())
            .bind(cutoff)
            .bind(watermark)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if let Some(horizon) = horizon {
            sqlx::query(sql.advance_horizon.sql())
                .bind(horizon)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        let removed_session_terminal_count = sqlx::query(sql.delete_session_terminals.sql())
            .bind(cutoff)
            .bind(watermark)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected() as usize;
        // deleted_sessions permanently protects identity reuse (FIG-754 / FIG-748).
        let mut removed_receipt_count =
            sqlx::query(session_sql().turn_commits_postgres.delete_retained.sql())
                .bind(cutoff)
                .bind(watermark)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected() as usize;
        let tool_sql = lash_store_sql::tool_receipts::ToolReceiptStatements::render(
            lash_store_sql::Dialect::postgres(),
        );
        removed_receipt_count += sqlx::query(tool_sql.reclaim.sql())
            .bind(clamp_epoch_ms(bound.committed_before_epoch_ms))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected() as usize;
        let wait_sql = lash_store_sql::wait_receipts::WaitReceiptStatements::render(
            lash_store_sql::Dialect::postgres(),
        );
        removed_receipt_count += sqlx::query(wait_sql.reclaim.sql())
            .bind(clamp_epoch_ms(bound.committed_before_epoch_ms))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected() as usize;

        // Trigger mutation receipts are durable evidence under the same lever
        // (FIG-4108): ownerless rows by age, session rows once the owner is
        // durably deleted and no outstanding delivery names it.
        let removed_trigger_mutation_receipt_count = sqlx::query(
            crate::trigger_store::trigger_sql()
                .retention_postgres
                .reclaim_mutation_receipts
                .sql(),
        )
        .bind(cutoff)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        // The host tool-intent submission ledger is evidence of its owner
        // session under the same lever (FIG-1509): fence each durably
        // deleted owner with a row past the bound, then delete every fenced
        // owner's rows past it. Submissions hold this sweep's lock shared, so
        // none claims an identity between the fence and the delete.
        let tool_intents = crate::turn_ingress::turn_ingress_sql();
        sqlx::query(
            tool_intents
                .tool_intents_postgres
                .fence_retired_owners
                .sql(),
        )
        .bind(cutoff)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let removed_tool_intent_submission_count =
            sqlx::query(tool_intents.tool_intents.reclaim_retired.sql())
                .bind(cutoff)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected() as usize;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core_execution::store::RetentionReport {
            removed_receipt_count,
            removed_session_terminal_count,
            removed_trigger_mutation_receipt_count,
            removed_tool_intent_submission_count,
            removed_attachment_root_count: 0,
            // Effect scopes are the engine's to retire; this catalog holds
            // no effect journal.
            retired_effect_scope_count: 0,
        })
    }
    .await
    .map_err(|error| {
        Box::new(lash_core_execution::MaintenanceFailure::failed_before_any_work(error))
    })
}
