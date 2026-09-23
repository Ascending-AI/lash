use crate::{InputId, LeaseOwnerIdentity, TurnId};

/// Ownership and predecessor identity are distinct states. An interrupted
/// predecessor is never live, even when abandon restores its token.
#[derive(Clone, Default)]
pub(super) struct ClaimHold {
    pub(super) fencing_token: u64,
    state: HoldState,
}

#[derive(Clone)]
enum HoldState {
    Unheld {
        // Abandon metadata is not live ownership. The predecessor identity is
        // all-or-none, mirroring the SQL backends' all-or-none constraint on
        // the claim id/token pair.
        prior_claim_id: Option<String>,
        prior_token: Option<String>,
    },
    Held {
        claim_id: String,
        token: String,
        owner: LeaseOwnerIdentity,
        generation: u64,
        /// The aborted direct turn this claim is bound to, and the input its
        /// receipt names (FIG-3589): the claim no longer lapses with its
        /// generation, and only that turn's redrive re-takes it.
        bound_to: Option<(TurnId, InputId)>,
    },
}

impl Default for HoldState {
    fn default() -> Self {
        Self::Unheld {
            prior_claim_id: None,
            prior_token: None,
        }
    }
}

impl ClaimHold {
    pub(super) fn with_fencing_token(fencing_token: u64) -> Self {
        Self {
            fencing_token,
            state: HoldState::default(),
        }
    }

    pub(super) fn id(&self) -> Option<String> {
        match &self.state {
            HoldState::Unheld { prior_claim_id, .. } => prior_claim_id.clone(),
            HoldState::Held { claim_id, .. } => Some(claim_id.clone()),
        }
    }

    pub(super) fn token(&self) -> Option<String> {
        match &self.state {
            HoldState::Unheld { prior_token, .. } => prior_token.clone(),
            HoldState::Held { token, .. } => Some(token.clone()),
        }
    }

    pub(super) fn owner(&self) -> Option<LeaseOwnerIdentity> {
        match &self.state {
            HoldState::Held { owner, .. } => Some(owner.clone()),
            HoldState::Unheld { .. } => None,
        }
    }

    pub(super) fn generation(&self) -> Option<u64> {
        match self.state {
            HoldState::Held { generation, .. } => Some(generation),
            HoldState::Unheld { .. } => None,
        }
    }

    pub(super) fn live_under(&self, generation: Option<u64>) -> bool {
        self.generation()
            .is_some_and(|held| held != 0 && Some(held) == generation)
    }

    /// Whether a claim under `generation` may take this row: never while
    /// that generation holds it, and never while an aborted turn is bound to
    /// it (FIG-3589).
    pub(super) fn claimable_by(&self, generation: u64) -> bool {
        !self.live_under(Some(generation)) && self.bound_turn().is_none()
    }

    /// The aborted direct turn this claim is bound to, if any (FIG-3589).
    pub(super) fn bound_turn(&self) -> Option<&TurnId> {
        self.binding().map(|(turn_id, _)| turn_id)
    }

    /// The aborted direct turn this claim is bound to and the input its
    /// receipt names, if any (FIG-3589).
    pub(super) fn binding(&self) -> Option<&(TurnId, InputId)> {
        match &self.state {
            HoldState::Held { bound_to, .. } => bound_to.as_ref(),
            HoldState::Unheld { .. } => None,
        }
    }

    /// Bind this hold to the aborted turn `turn_id`, whose receipt names
    /// `receipt_input_id`, when it is still the claim `claim_id`/`token`; any
    /// other hold is left alone.
    pub(super) fn bind(
        &mut self,
        claim_id: &str,
        token: &str,
        turn_id: &TurnId,
        receipt_input_id: &InputId,
    ) {
        if let HoldState::Held {
            claim_id: held_id,
            token: held_token,
            bound_to,
            ..
        } = &mut self.state
            && held_id == claim_id
            && held_token == token
        {
            *bound_to = Some((turn_id.clone(), receipt_input_id.clone()));
        }
    }

    pub(super) fn owned_by(&self, claim_id: &str, token: &str) -> bool {
        match &self.state {
            HoldState::Held {
                claim_id: held_id,
                token: held_token,
                ..
            }
            | HoldState::Unheld {
                prior_claim_id: Some(held_id),
                prior_token: Some(held_token),
            } => held_id == claim_id && held_token == token,
            HoldState::Unheld { .. } => false,
        }
    }

    pub(super) fn acquire(
        &mut self,
        claim_id: String,
        token: String,
        owner: LeaseOwnerIdentity,
        generation: u64,
        fencing_token: u64,
    ) {
        self.fencing_token = fencing_token;
        self.state = HoldState::Held {
            claim_id,
            token,
            owner,
            generation,
            bound_to: None,
        };
    }

    pub(super) fn release(&mut self) {
        self.state = HoldState::default();
    }

    pub(super) fn restore(&mut self, claim_id: Option<String>, token: Option<String>) {
        self.state = HoldState::Unheld {
            prior_claim_id: claim_id,
            prior_token: token,
        };
    }

    // Public diagnostics preserve the historical interrupted-pair spelling.
    pub(super) fn diagnostic_generation(&self) -> Option<u64> {
        match &self.state {
            HoldState::Held { generation, .. } => Some(*generation),
            HoldState::Unheld { prior_token, .. } => prior_token.as_ref().map(|_| 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_claim_preserves_identity_without_becoming_live() {
        let mut hold = ClaimHold::default();
        hold.acquire(
            "first".into(),
            "token".into(),
            LeaseOwnerIdentity::opaque("owner", "incarnation"),
            1,
            1,
        );
        hold.restore(Some("predecessor".into()), Some("prior-token".into()));
        assert_eq!(hold.id().as_deref(), Some("predecessor"));
        assert!(hold.claimable_by(0));
        assert!(hold.claimable_by(1));
        assert!(!hold.live_under(Some(0)));
        assert!(hold.owned_by("predecessor", "prior-token"));
        assert_eq!(hold.fencing_token, 1);
        hold.acquire(
            "second".into(),
            "next-token".into(),
            LeaseOwnerIdentity::opaque("owner", "incarnation"),
            2,
            2,
        );
        assert_eq!(hold.id().as_deref(), Some("second"));
        assert!(hold.owned_by("second", "next-token"));
        assert!(!hold.claimable_by(2));
        hold.release();
        assert_eq!(hold.id(), None);
        assert_eq!(hold.fencing_token, 2);
    }
    #[test]
    fn abandon_preserves_paired_predecessor_identity_without_live_ownership() {
        for (id, token) in [(None, None), (Some("prior"), Some("token"))] {
            let mut hold = ClaimHold::with_fencing_token(9);
            hold.restore(id.map(str::to_string), token.map(str::to_string));
            assert_eq!(hold.id().as_deref(), id);
            assert_eq!(hold.token().as_deref(), token);
            assert_eq!(hold.diagnostic_generation(), token.map(|_| 0));
            assert_eq!(hold.owner(), None);
            assert!(!hold.live_under(Some(0)));
            assert!(hold.claimable_by(1));
            assert_eq!(hold.fencing_token, 9);
        }
    }
}
