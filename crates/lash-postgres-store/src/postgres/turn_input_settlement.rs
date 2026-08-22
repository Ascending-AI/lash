//! Turn-input settlement for the PostgreSQL store.
//!
//! One conditional write serves both settlement regimes (ADR 0069 §5): the
//! claim fields strengthen the predicate when a claim exists, and the terminal
//! state set — derived from `TurnInputState` so it cannot drift — bounds it
//! when one does not.

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
        .bind(&completed.session_id)
        .bind(input_id)
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
        // One predicate, two regimes: the claim fields only strengthen it
        // (ADR 0069 section 5).
        let owns_row = match completed.claim.as_ref() {
            Some(claim) => observed
                .as_ref()
                .is_some_and(|(claim_id, claim_token, _, _)| {
                    claim_id.as_deref() == Some(claim.claim_id.as_str())
                        && claim_token.as_deref() == Some(claim.lease_token.as_str())
                }),
            None => observed.as_ref().is_some_and(|(claim_id, _, _, state)| {
                claim_id.is_none() && unclaimed_turn_input_is_settleable(state)
            }),
        };
        if !owns_row {
            return Err(match completed.claim.as_ref() {
                Some(claim) => StoreError::TurnInputClaimSuperseded {
                    session_id: completed.session_id.clone(),
                    claim_id: claim.claim_id.clone(),
                    row_id: Some(input_id.clone().into_boxed_str()),
                    superseding_claim_id: observed
                        .as_ref()
                        .and_then(|(claim_id, _, _, _)| claim_id.clone())
                        .map(String::into_boxed_str),
                    superseding_session_lease_generation: observed.as_ref().and_then(
                        |(claim_id, _, generation, _)| {
                            claim_id.as_ref().map(|_| Box::new(*generation))
                        },
                    ),
                },
                None => StoreError::UnclaimedTurnInputSettlementSuperseded {
                    session_id: completed.session_id.clone(),
                    input_id: input_id.clone(),
                    observed_state: observed
                        .as_ref()
                        .map(|(_, _, _, state)| state.clone().into_boxed_str()),
                    superseding_claim_id: observed
                        .as_ref()
                        .and_then(|(claim_id, _, _, _)| claim_id.clone())
                        .map(String::into_boxed_str),
                },
            });
        }
    }
    Ok(())
}

/// The lifecycle states an unclaimed settlement can never overwrite.
///
/// A cancelled or already-settled row is terminal: an unclaimed settlement that
/// finds one lost the head CAS (ADR 0069 §5).
pub(crate) const UNCLAIMED_TURN_INPUT_TERMINAL_STATES: [lash_core::TurnInputState; 2] = [
    lash_core::TurnInputState::Completed,
    lash_core::TurnInputState::Cancelled,
];

/// Whether an unclaimed row is still open for settlement.
pub(crate) fn unclaimed_turn_input_is_settleable(state: &str) -> bool {
    !UNCLAIMED_TURN_INPUT_TERMINAL_STATES
        .iter()
        .any(|terminal| terminal.as_str() == state)
}

/// The same terminal set spelled as the body of a SQL `IN (...)` list, so the
/// settlement predicate and its Rust twin above cannot drift from the enum.
pub(crate) fn unclaimed_turn_input_terminal_states_sql() -> String {
    UNCLAIMED_TURN_INPUT_TERMINAL_STATES
        .iter()
        .map(|state| format!("'{}'", state.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}
