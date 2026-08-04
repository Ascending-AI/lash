pub(super) fn drop_queue_settlement_row(
    claims: &mut Vec<crate::QueuedWorkCompletion>,
    claim_id: &str,
    row_id: &str,
) -> bool {
    let mut removed = false;
    for claim in claims.iter_mut().filter(|claim| claim.claim_id == claim_id) {
        let prior = claim.batch_ids.len();
        claim.batch_ids.retain(|batch_id| batch_id != row_id);
        removed |= claim.batch_ids.len() != prior;
    }
    claims.retain(|claim| !claim.batch_ids.is_empty());
    removed
}

pub(super) fn drop_turn_input_settlement_row(
    claims: &mut Vec<crate::TurnInputCompletion>,
    claim_id: &str,
    row_id: &str,
) -> bool {
    let mut removed = false;
    for claim in claims.iter_mut().filter(|claim| claim.claim_id == claim_id) {
        let prior = claim.input_ids.len();
        claim.input_ids.retain(|input_id| input_id != row_id);
        claim
            .applications
            .retain(|application| application.input_id != row_id);
        removed |= claim.input_ids.len() != prior;
    }
    claims.retain(|claim| !claim.input_ids.is_empty());
    removed
}
