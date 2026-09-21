//! Turn-input settlement for the PostgreSQL store.
//!
//! Settlement authority is decided once, in
//! [`require_settleable_turn_input`](lash_core::store_backend_support::require_settleable_turn_input),
//! over the row this module locks `FOR UPDATE`. One conditional write then
//! serves both settlement regimes (ADR 0069 §5) with that same predicate as
//! its backstop: the claim fields strengthen it when a claim exists, and the
//! terminal state set — derived from `TurnInputState` so it cannot drift —
//! bounds it when one does not.

use crate::*;

pub(crate) async fn ensure_turn_input_completion_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed: &lash_core::TurnInputCompletion,
) -> Result<(), StoreError> {
    for input_id in &completed.input_ids {
        let observed: Option<(Option<String>, Option<String>, i64, String)> = sqlx::query_as(
            "SELECT claim_id, claim_token, claim_session_lease_generation, state
             FROM lash_pending_turn_inputs
             WHERE session_id = $1
               AND input_id = $2
             LIMIT 1
             FOR UPDATE",
        )
        .bind(completed.session_id.as_str())
        .bind(input_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let observed = observed
            .map(|(claim_id, claim_token, generation, state)| {
                Ok((
                    claim_id,
                    claim_token,
                    u64_from_sql(
                        "PendingTurnInput",
                        "claim_session_lease_generation",
                        generation,
                    )?,
                    state,
                ))
            })
            .transpose()?;
        // The shared verdict is the decision. One predicate, two regimes: the
        // claim fields only strengthen it (ADR 0069 section 5).
        lash_core::store_backend_support::require_settleable_turn_input(
            completed,
            input_id,
            observed
                .as_ref()
                .map(|(claim_id, claim_token, generation, state)| {
                    lash_core::store_backend_support::TurnInputSettlementFacts {
                        claim_id: claim_id.as_deref(),
                        claim_token: claim_token.as_deref(),
                        claim_session_lease_generation: *generation,
                        state: state.as_str(),
                    }
                }),
        )?;
    }
    Ok(())
}

/// The terminal state set spelled as the body of a SQL `IN (...)` list, so the
/// settlement predicate and its Rust twin above cannot drift from the enum.
pub(crate) fn unclaimed_turn_input_terminal_states_sql() -> String {
    lash_core::store_backend_support::terminal_turn_input_states_sql()
}
