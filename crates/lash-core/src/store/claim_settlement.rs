use super::{RuntimeCommit, StoreError};

/// Two turn-input settlements name the same work.
///
/// A claimed settlement is identified by its claim id, which already names the
/// rows it fences. An unclaimed settlement has no claim, so it is identified by
/// the rows themselves (ADR 0069 §5).
fn same_turn_input_settlement(
    left: &crate::TurnInputCompletion,
    right: &crate::TurnInputCompletion,
) -> bool {
    left.session_id == right.session_id
        && match (left.claim_id(), right.claim_id()) {
            (Some(left_claim), Some(right_claim)) => left_claim == right_claim,
            (None, None) => left.input_ids == right.input_ids,
            _ => false,
        }
}

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
        if !commit
            .completed_turn_input_claims
            .iter()
            .any(|completed| same_turn_input_settlement(completed, originating))
        {
            return Err(StoreError::UnsettledTurnInputClaim {
                session_id: originating.session_id.clone(),
                claim_id: originating.settlement_identity(),
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
    // The foreign-settlement check is what enforces ADR 0069 §5's ordering
    // rule for unclaimed settlement: an unclaimed completion has no claim id to
    // match on, so it matches only an originating settlement naming exactly the
    // rows this turn accepted. A turn can never settle a row it neither claimed
    // nor accepted.
    for completed in &commit.completed_turn_input_claims {
        if !originating_turn_input_claims
            .iter()
            .any(|originating| same_turn_input_settlement(completed, originating))
        {
            return Err(StoreError::ForeignTurnInputCompletion {
                session_id: completed.session_id.clone(),
                claim_id: completed.settlement_identity(),
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
