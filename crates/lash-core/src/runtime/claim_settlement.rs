use std::collections::{BTreeSet, HashMap};

pub(super) struct ClaimSettlement<C> {
    pub(super) completions: Vec<C>,
    /// The session-lease generation each claim was taken under, by claim id.
    generations: HashMap<String, u64>,
    #[cfg(test)]
    originating_override: Option<Vec<C>>,
}

impl<C> ClaimSettlement<C> {
    pub(super) fn new(completions: Vec<C>, generations: HashMap<String, u64>) -> Self {
        Self {
            completions,
            generations,
            #[cfg(test)]
            originating_override: None,
        }
    }

    pub(super) fn originating(&self) -> &[C] {
        #[cfg(test)]
        if let Some(originating) = &self.originating_override {
            return originating;
        }
        &self.completions
    }

    /// Whether `claim_id` was taken under a session-lease generation older
    /// than `current`: it was restored from an earlier execution, not claimed
    /// by this one.
    fn is_restored(&self, claim_id: &str, current: Option<u64>) -> bool {
        current.is_some_and(|current| {
            self.generations
                .get(claim_id)
                .is_some_and(|generation| *generation < current)
        })
    }

    #[cfg(test)]
    pub(super) fn divergent_for_test(
        originating: Vec<C>,
        completions: Vec<C>,
        generations: HashMap<String, u64>,
    ) -> Self {
        Self {
            completions,
            generations,
            originating_override: Some(originating),
        }
    }
}

pub(super) struct TurnClaimSettlement {
    pub(super) queued: ClaimSettlement<crate::QueuedWorkCompletion>,
    pub(super) turn_inputs: ClaimSettlement<crate::TurnInputCompletion>,
    /// Turn input a cancelled turn withheld from its terminal checkpoint. It
    /// is released for the cancellation's undelivered disposition, never
    /// completed (FIG-3531).
    pub(super) undelivered_turn_inputs: Vec<crate::TurnInputClaim>,
    /// The claims of the journaled initial drive set (ADR 0069 §6). They cede
    /// on supersession whatever generation the turn commits under: a first
    /// execution holds them under its own generation, and a redrive restores
    /// them from the journal.
    journaled_drive_claims: BTreeSet<String>,
}

impl TurnClaimSettlement {
    pub(super) fn new(
        queued: Vec<crate::QueuedWorkCompletion>,
        turn_inputs: Vec<crate::TurnInputCompletion>,
        queue_generations: HashMap<String, u64>,
        input_generations: HashMap<String, u64>,
    ) -> Self {
        Self {
            queued: ClaimSettlement::new(queued, queue_generations),
            turn_inputs: ClaimSettlement::new(turn_inputs, input_generations),
            undelivered_turn_inputs: Vec::new(),
            journaled_drive_claims: BTreeSet::new(),
        }
    }

    pub(super) fn with_undelivered_turn_inputs(
        mut self,
        undelivered: Vec<crate::TurnInputClaim>,
    ) -> Self {
        self.undelivered_turn_inputs = undelivered;
        self
    }

    pub(super) fn with_journaled_drive_claims(mut self, claims: BTreeSet<String>) -> Self {
        self.journaled_drive_claims = claims;
        self
    }

    /// Whether `error` supersedes a claim whose loss cedes the turn.
    ///
    /// A claim the turn restored from an earlier execution (its generation
    /// predates `current`, the generation the turn commits under) or a
    /// journaled drive claim carries authority the turn already spent: the
    /// journal holds the words it answered them with. A resumed queued run
    /// retakes its own assigned rows under the committing generation before
    /// the replay and settles them under those claims, so supersession here
    /// proves another driver took the rows through the claim CAS: committing
    /// the turn would answer them a second time. The turn cedes and commits
    /// nothing (FIG-3552).
    ///
    /// A claim taken under `current` cannot be superseded while the turn holds
    /// the lane; if it is, the error stands as it is.
    pub(super) fn cedes(&self, error: &crate::StoreError, current: Option<u64>) -> bool {
        match error {
            crate::StoreError::TurnInputClaimSuperseded { claim_id, .. } => {
                self.journaled_drive_claims.contains(claim_id.as_str())
                    || self.turn_inputs.is_restored(claim_id, current)
            }
            crate::StoreError::QueuedWorkClaimSuperseded { claim_id, .. } => {
                self.queued.is_restored(claim_id, current)
            }
            _ => false,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(
        originating_queue: Vec<crate::QueuedWorkCompletion>,
        originating_inputs: Vec<crate::TurnInputCompletion>,
        completed_queue: Vec<crate::QueuedWorkCompletion>,
        completed_inputs: Vec<crate::TurnInputCompletion>,
        queue_generations: HashMap<String, u64>,
        input_generations: HashMap<String, u64>,
    ) -> Self {
        Self {
            queued: ClaimSettlement::divergent_for_test(
                originating_queue,
                completed_queue,
                queue_generations,
            ),
            turn_inputs: ClaimSettlement::divergent_for_test(
                originating_inputs,
                completed_inputs,
                input_generations,
            ),
            undelivered_turn_inputs: Vec::new(),
            journaled_drive_claims: BTreeSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;

    fn queued_superseded(claim_id: &str) -> crate::StoreError {
        crate::StoreError::QueuedWorkClaimSuperseded {
            session_id: SessionId::from("fig3552"),
            claim_id: claim_id.to_string(),
            row_id: Some("fig3552-row".into()),
            superseding_claim_id: Some("successor-claim".into()),
            superseding_session_lease_generation: Some(Box::new(3)),
        }
    }

    fn turn_input_superseded(claim_id: &str) -> crate::StoreError {
        crate::StoreError::TurnInputClaimSuperseded {
            session_id: SessionId::from("fig3552"),
            claim_id: claim_id.to_string(),
            row_id: Some("fig3552-row".into()),
            superseding_claim_id: Some("successor-claim".into()),
            superseding_session_lease_generation: Some(Box::new(3)),
        }
    }

    /// One queued-work and one turn-input claim of each age: `restored-*`
    /// under generation 1 and `live-*` under the commit's generation 4.
    fn settlement() -> TurnClaimSettlement {
        let generations = |kind: &str| {
            [(format!("restored-{kind}"), 1), (format!("live-{kind}"), 4)]
                .into_iter()
                .collect()
        };
        TurnClaimSettlement::new(
            Vec::new(),
            Vec::new(),
            generations("queued"),
            generations("input"),
        )
    }

    #[test]
    fn a_superseded_restored_claim_cedes_for_both_row_kinds() {
        let settlement = settlement();
        assert!(settlement.cedes(&queued_superseded("restored-queued"), Some(4)));
        assert!(settlement.cedes(&turn_input_superseded("restored-input"), Some(4)));
    }

    #[test]
    fn a_superseded_claim_of_the_committing_generation_does_not_cede() {
        let settlement = settlement();
        assert!(!settlement.cedes(&queued_superseded("live-queued"), Some(4)));
        assert!(!settlement.cedes(&turn_input_superseded("live-input"), Some(4)));
    }

    #[test]
    fn without_a_committing_generation_no_claim_counts_as_restored() {
        let settlement = settlement();
        assert!(!settlement.cedes(&queued_superseded("restored-queued"), None));
        assert!(!settlement.cedes(&turn_input_superseded("restored-input"), None));
    }

    #[test]
    fn claim_ids_are_matched_within_their_own_row_kind() {
        let settlement = settlement();
        assert!(!settlement.cedes(&queued_superseded("restored-input"), Some(4)));
        assert!(!settlement.cedes(&turn_input_superseded("restored-queued"), Some(4)));
        assert!(!settlement.cedes(&turn_input_superseded("unknown"), Some(4)));
    }

    #[test]
    fn a_journaled_drive_claim_cedes_under_any_generation() {
        let settlement = settlement()
            .with_journaled_drive_claims(std::iter::once("live-input".to_string()).collect());
        assert!(settlement.cedes(&turn_input_superseded("live-input"), Some(4)));
        assert!(settlement.cedes(&turn_input_superseded("live-input"), None));
    }

    #[test]
    fn other_store_errors_never_cede() {
        let settlement = settlement();
        let error = crate::StoreError::HeadRevisionConflict {
            expected: 1,
            actual: 2,
        };
        assert!(!settlement.cedes(&error, Some(4)));
    }
}
