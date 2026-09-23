use super::*;

/// Execute the ordered writes the shared settlement plans prescribed
/// (FIG-1065).
///
/// Write order per covered row is the plan's: the consumed wake's redelivery
/// fence first — under the wake source's advisory lock, which serializes
/// queue insertion against consumption — then the row's removal. A crash
/// between the two would replay a wake the session already consumed, so the
/// fence must land first.
pub(super) async fn complete_queued_work_claims_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    plans: &[lash_core::store::claim_plan::QueuedWorkSettlementPlan],
) -> Result<(), StoreError> {
    use lash_core::store::claim_plan::QueuedWorkSettlementWrite;
    let sql = crate::turn_ingress::turn_ingress_sql();
    for plan in plans {
        for write in plan.writes() {
            match write {
                QueuedWorkSettlementWrite::FenceWakeRedelivery { wake, .. } => {
                    raise_wake_redelivery_fence_tx(tx, plan.session_id(), wake).await?;
                }
                QueuedWorkSettlementWrite::SettleClaimedBatch { batch_id } => {
                    let completion = sqlx::query(sql.queued_batches.settle_claimed.sql())
                        .bind(plan.session_id().as_str())
                        .bind(batch_id.as_str())
                        .bind(plan.claim_id())
                        .bind(plan.lease_token())
                        .execute(&mut **tx)
                        .await
                        .map_err(store_sqlx_error)?;
                    // Backstop: `plan_queued_work_settlement_tx` already took
                    // the verdict over this row under `FOR UPDATE` earlier in
                    // this same transaction, so the predicate cannot
                    // legitimately miss. A miss is recorded as evidence and
                    // then fails closed with the same supersession this site
                    // has always returned.
                    lash_core::store_backend_support::require_fenced_write_applied(
                        lash_core::store_backend_support::FencedWrite::QueuedWorkClaimSettlement,
                        crate::POSTGRES_BACKEND,
                        batch_id.as_str(),
                        completion.rows_affected(),
                        || plan.superseded_error(batch_id),
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Execute the ordered settlement steps the shared plans prescribed
/// (FIG-1065). One conditional write serves both regimes (ADR 0069 §5).
pub(crate) async fn complete_turn_input_claims_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    plans: &[lash_core::store::claim_plan::TurnInputSettlementPlan],
) -> Result<(), StoreError> {
    use lash_core::store::claim_plan::TurnInputSettlementRegime;
    let pending_inputs = &crate::turn_ingress::turn_ingress_sql().pending_inputs;
    let unclaimed_settlement_statement = pending_inputs.settle_unclaimed.sql();
    let claimed_settlement_statement = pending_inputs.settle_claimed.sql();
    for plan in plans {
        for step in plan.steps() {
            // One conditional write for both settlement regimes: the claim
            // fields are an optional predicate strengthener, and either way
            // exactly one row must change (ADR 0069 §5).
            let settlement = match (step.regime, plan.claim()) {
                (TurnInputSettlementRegime::Claimed, Some(claim)) => {
                    sqlx::query(claimed_settlement_statement)
                        .bind(plan.session_id().as_str())
                        .bind(step.input_id.as_str())
                        .bind(step.settle_state.as_str())
                        .bind(&claim.claim_id)
                        .bind(&claim.lease_token)
                }
                (TurnInputSettlementRegime::Claimed, None) => {
                    return Err(StoreError::Backend(
                        "claimed turn-input settlement step without a claim".to_string(),
                    ));
                }
                (TurnInputSettlementRegime::Unclaimed, _) => {
                    sqlx::query(unclaimed_settlement_statement)
                        .bind(plan.session_id().as_str())
                        .bind(step.input_id.as_str())
                        .bind(step.settle_state.as_str())
                }
            }
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            // Backstop: `plan_turn_input_settlement_tx` already took the
            // verdict over this row under `FOR UPDATE` earlier in this same
            // transaction, so the predicate cannot legitimately miss. A miss
            // is recorded as evidence and then fails closed with the same
            // supersession this site has always returned.
            lash_core::store_backend_support::require_fenced_write_applied(
                lash_core::store::claim_plan::TurnInputSettlementPlan::fenced_write(step),
                crate::POSTGRES_BACKEND,
                step.input_id.as_str(),
                settlement.rows_affected(),
                || plan.superseded_error(step),
            )?;
        }
    }
    Ok(())
}
