//! Turn-input settlement for the PostgreSQL store.
//!
//! Settlement authority is decided once, in
//! [`require_settleable_turn_input`](lash_core::store_backend_support::require_settleable_turn_input),
//! over the row this module locks `FOR UPDATE` — now reached through the
//! shared [`plan_turn_input_settlement`](lash_core::store::claim_plan::plan_turn_input_settlement)
//! planner (FIG-1065). One conditional
//! write then serves both settlement regimes (ADR 0069 §5) with that same
//! predicate as its backstop: the claim fields strengthen it when a claim
//! exists, and the terminal state set — derived from `TurnInputState` so it
//! cannot drift — bounds it when one does not.

use crate::*;

/// Observe every covered row under `FOR UPDATE` and return the shared
/// settlement plan for this completion (FIG-1065). The plan's ordered steps
/// execute later, in
/// [`complete_turn_input_claims_tx`](crate::runtime_persistence::complete_turn_input_claims_tx).
pub(crate) async fn plan_turn_input_settlement_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed: &lash_core::TurnInputCompletion,
) -> Result<lash_core::store::claim_plan::TurnInputSettlementPlan, StoreError> {
    let mut rows = Vec::with_capacity(completed.input_ids.len());
    for input_id in &completed.input_ids {
        let observed: Option<(Option<String>, Option<String>, i64, String)> = sqlx::query_as(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs_postgres
                .settlement_facts
                .sql(),
        )
        .bind(completed.session_id.as_str())
        .bind(input_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let facts = observed
            .map(|(claim_id, claim_token, generation, state)| {
                Ok(lash_core::store::claim_plan::TurnInputSettlementRowFacts {
                    claim_id,
                    claim_token,
                    claim_session_lease_generation: u64_from_sql(
                        "PendingTurnInput",
                        "claim_session_lease_generation",
                        generation,
                    )?,
                    state,
                })
            })
            .transpose()?;
        rows.push(lash_core::store::claim_plan::TurnInputSettlementRow {
            input_id: input_id.clone(),
            facts,
        });
    }
    // The shared planner takes the verdict. One predicate, two regimes: the
    // claim fields only strengthen it (ADR 0069 section 5).
    lash_core::store::claim_plan::plan_turn_input_settlement(completed, rows).into_result()
}
