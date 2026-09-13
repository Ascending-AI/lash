use crate::LeaseOwnerIdentity;
use crate::SessionId;
use crate::store::queued_work::ClaimIdDialect;

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

    pub(super) fn claimable_by(&self, generation: u64) -> bool {
        !self.live_under(Some(generation))
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

pub(super) trait InMemoryClaimRow {
    fn claim(&self) -> &ClaimHold;
    fn claim_mut(&mut self) -> &mut ClaimHold;
}

pub(super) struct InMemoryClaimMint<'a> {
    pub selected_indices: &'a [usize],
    pub enqueue_seq: u64,
    pub dialect: ClaimIdDialect,
    pub fencing_label: &'static str,
    pub session_id: &'a SessionId,
    pub owner: &'a LeaseOwnerIdentity,
    pub generation: u64,
    pub now: u64,
}

pub(super) struct MintedInMemoryClaim {
    pub claim_id: String,
    pub lease_token: String,
    pub fencing_token: u64,
    pub abandon_restore_claim_id: Option<String>,
    pub abandon_restore_claim_token: Option<String>,
}

pub(super) fn mint_in_memory_claim<R: InMemoryClaimRow>(
    rows: &mut [R],
    mint: InMemoryClaimMint<'_>,
) -> Result<MintedInMemoryClaim, crate::store::StoreError> {
    let next_fencing_tokens = mint
        .selected_indices
        .iter()
        .map(|&index| {
            crate::StoreError::checked_monotonic_increment(
                mint.fencing_label,
                rows[index].claim().fencing_token,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let first = rows[mint.selected_indices[0]].claim();
    let abandon_restore_claim_id = first.id();
    let abandon_restore_claim_token = first.token();
    let fencing_token = next_fencing_tokens[0];
    let claim_id =
        crate::store::queued_work::derive_claim_id(mint.dialect, mint.enqueue_seq, fencing_token);
    let lease_token = crate::store::queued_work::derive_claim_lease_token(
        mint.session_id,
        mint.owner,
        &claim_id,
        mint.now,
    );
    for (&index, next_fencing_token) in mint.selected_indices.iter().zip(&next_fencing_tokens) {
        rows[index].claim_mut().acquire(
            claim_id.clone(),
            lease_token.clone(),
            mint.owner.clone(),
            mint.generation,
            *next_fencing_token,
        );
    }
    Ok(MintedInMemoryClaim {
        claim_id,
        lease_token,
        fencing_token,
        abandon_restore_claim_id,
        abandon_restore_claim_token,
    })
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
