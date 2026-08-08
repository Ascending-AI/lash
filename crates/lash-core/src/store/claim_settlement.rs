use super::{RuntimeCommit, StoreError};

pub(super) fn validate_claim_settlement(
    commit: &RuntimeCommit,
    originating_queue_claims: &[crate::QueuedWorkCompletion],
    originating_turn_input_claims: &[crate::TurnInputCompletion],
) -> Result<(), StoreError> {
    for originating in originating_queue_claims {
        if !commit.completed_queue_claims.iter().any(|completed| {
            completed.session_id == originating.session_id
                && completed.claim_id == originating.claim_id
        }) {
            return Err(StoreError::UnsettledQueuedWorkClaim {
                session_id: originating.session_id.clone(),
                claim_id: originating.claim_id.clone(),
            });
        }
    }
    for originating in originating_turn_input_claims {
        if !commit.completed_turn_input_claims.iter().any(|completed| {
            completed.session_id == originating.session_id
                && completed.claim_id == originating.claim_id
        }) {
            return Err(StoreError::UnsettledTurnInputClaim {
                session_id: originating.session_id.clone(),
                claim_id: originating.claim_id.clone(),
            });
        }
    }
    for completed in &commit.completed_queue_claims {
        if !originating_queue_claims.iter().any(|originating| {
            originating.session_id == completed.session_id
                && originating.claim_id == completed.claim_id
        }) {
            return Err(StoreError::ForeignQueuedWorkCompletion {
                session_id: completed.session_id.clone(),
                claim_id: completed.claim_id.clone(),
            });
        }
    }
    for completed in &commit.completed_turn_input_claims {
        if !originating_turn_input_claims.iter().any(|originating| {
            originating.session_id == completed.session_id
                && originating.claim_id == completed.claim_id
        }) {
            return Err(StoreError::ForeignTurnInputCompletion {
                session_id: completed.session_id.clone(),
                claim_id: completed.claim_id.clone(),
            });
        }
    }
    if commit.completed_queue_claims.len() != originating_queue_claims.len() {
        return Err(StoreError::ClaimSettlementCountMismatch {
            claim_kind: "queued-work",
            originating_count: originating_queue_claims.len(),
            completed_count: commit.completed_queue_claims.len(),
        });
    }
    if commit.completed_turn_input_claims.len() != originating_turn_input_claims.len() {
        return Err(StoreError::ClaimSettlementCountMismatch {
            claim_kind: "turn-input",
            originating_count: originating_turn_input_claims.len(),
            completed_count: commit.completed_turn_input_claims.len(),
        });
    }
    Ok(())
}
