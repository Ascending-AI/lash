use crate::SessionId;
use crate::TurnId;
use crate::{CheckpointKind, PluginMessage, TurnCause, TurnInput};
pub use lash_core_store::turn_input_vocabulary::*;

/// Generates the checkpoint enumeration a claim can name, from one variant list.
///
/// The generated match is exhaustive, so a new [`CheckpointKind`] variant fails to compile until it
/// is added here, which keeps `CLAIM_CHECKPOINTS` complete for the tests that sweep every
/// checkpoint.
macro_rules! turn_input_claim_checkpoints {
    ($($variant:ident),+ $(,)?) => {
        #[cfg(test)]
        pub(crate) const CLAIM_CHECKPOINTS: &[CheckpointKind] = &[$(CheckpointKind::$variant),+];

        /// Compile-time guard only: the exhaustive match below is what fails on a new variant.
        #[cfg(test)]
        #[allow(dead_code)]
        fn assert_claim_checkpoints_are_exhaustive(checkpoint: CheckpointKind) {
            match checkpoint {
                $(CheckpointKind::$variant => ()),+
            }
        }
    };
}

turn_input_claim_checkpoints!(AfterWork, BeforeCompletion);

/// The turn-input rows one turn is driving, with the authority it will settle
/// them under.
///
/// Exactly two regimes, one authority: a claimed drive settles under its
/// generation-fenced claim, an unclaimed drive settles under the head CAS
/// alone. Everything between acceptance and settlement — materialization,
/// application evidence, the committed message's provenance — is identical, so
/// the runtime carries both through one list rather than two.
#[derive(Clone, Debug)]
pub(crate) enum TurnInputDrive {
    Claimed(TurnInputClaim),
    Unclaimed(UnclaimedTurnInputs),
}

impl TurnInputDrive {
    pub(crate) fn materialize_turn_input(&self) -> TurnInput {
        match self {
            Self::Claimed(claim) => claim.materialize_turn_input(),
            Self::Unclaimed(unclaimed) => {
                lash_core_store::turn_input_vocabulary::materialize_turn_input(&unclaimed.inputs)
            }
        }
    }

    pub(crate) fn inputs(&self) -> &[PendingTurnInput] {
        match self {
            Self::Claimed(claim) => &claim.inputs,
            Self::Unclaimed(unclaimed) => &unclaimed.inputs,
        }
    }

    pub(crate) fn applications(&self) -> &[TurnInputApplication] {
        match self {
            Self::Claimed(claim) => &claim.applications,
            Self::Unclaimed(unclaimed) => &unclaimed.applications,
        }
    }

    pub(crate) fn completion(&self) -> TurnInputCompletion {
        match self {
            Self::Claimed(claim) => claim.completion(),
            Self::Unclaimed(unclaimed) => unclaimed.completion(),
        }
    }

    /// The lease generation this drive is fenced by, or `None` when it holds no
    /// claim. Only a claimed drive can be superseded by a later generation, so
    /// only a claimed drive can have its settlement dropped and retried.
    pub(crate) fn claim_generation(&self) -> Option<(String, u64)> {
        match self {
            Self::Claimed(claim) => Some((claim.claim_id.clone(), claim.session_lease_generation)),
            Self::Unclaimed(_) => None,
        }
    }

    pub(crate) fn as_claim(&self) -> Option<&TurnInputClaim> {
        match self {
            Self::Claimed(claim) => Some(claim),
            Self::Unclaimed(_) => None,
        }
    }

    pub(crate) fn record_initial_turn_application(
        &mut self,
        turn_id: &crate::TurnId,
        committed_message_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        match self {
            Self::Claimed(claim) => {
                claim.record_initial_turn_application(turn_id, committed_message_id);
                Ok(())
            }
            Self::Unclaimed(unclaimed) => {
                unclaimed.record_initial_turn_application(turn_id, committed_message_id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_boundary_wire_values_match_the_persisted_ingress_encoding() {
        for boundary in TurnInputCheckpointBoundary::ALL.iter().copied() {
            assert_eq!(
                serde_json::to_value(boundary).expect("serialize boundary"),
                serde_json::Value::String(boundary.as_wire_str().to_string()),
                "the SQL literal a store filters on must equal the persisted wire value"
            );
        }
    }

    #[test]
    fn turn_input_state_wire_round_trips_every_variant() {
        for state in TurnInputState::ALL.iter().copied() {
            assert_eq!(TurnInputState::from_wire_str(state.as_str()), Some(state));
        }
    }

    #[test]
    fn turn_input_state_wire_values_match_the_persisted_ingress_encoding() {
        assert_eq!(TurnInputState::PendingActive.as_str(), "pending_active");
        assert_eq!(
            TurnInputState::DeferredNextTurn.as_str(),
            "deferred_next_turn"
        );
        assert_eq!(TurnInputState::Accepted.as_str(), "accepted");
        assert_eq!(TurnInputState::Cancelled.as_str(), "cancelled");
        assert_eq!(TurnInputState::Completed.as_str(), "completed");
    }

    #[test]
    fn turn_input_state_terminality_covers_exactly_settled_states() {
        for state in TurnInputState::ALL.iter().copied() {
            assert_eq!(
                state.is_terminal(),
                matches!(state, TurnInputState::Cancelled | TurnInputState::Completed),
                "terminality drifted for {state:?}"
            );
        }
    }

    #[test]
    fn every_checkpoint_admits_at_least_the_default_boundary() {
        for checkpoint in CLAIM_CHECKPOINTS.iter().copied() {
            let admitted = TurnInputCheckpointBoundary::ALL
                .iter()
                .filter(|boundary| boundary.admits(checkpoint))
                .collect::<Vec<_>>();
            assert!(
                admitted.contains(&&TurnInputCheckpointBoundary::default()),
                "an absent min_boundary reads as the default, which must stay admissible at {checkpoint:?}"
            );
            let predicate =
                crate::store_backend_support::admitted_min_boundary_sql("min_boundary", checkpoint);
            assert!(
                !predicate.contains("IN ()"),
                "an empty admitted set must not emit an `IN ()` syntax error at {checkpoint:?}"
            );
        }
        assert!(
            !TurnInputCheckpointBoundary::BeforeCompletion.admits(CheckpointKind::AfterWork),
            "before-completion ingress must be withheld at the after-work checkpoint"
        );
    }

    #[test]
    fn admitted_min_boundary_sql_spells_the_current_checkpoints_exactly() {
        assert_eq!(
            crate::store_backend_support::admitted_min_boundary_sql(
                "min_boundary",
                CheckpointKind::AfterWork
            ),
            "COALESCE(min_boundary, 'after_work') IN ('after_work')"
        );
        assert_eq!(
            crate::store_backend_support::admitted_min_boundary_sql(
                "min_boundary",
                CheckpointKind::BeforeCompletion
            ),
            "COALESCE(min_boundary, 'after_work') IN ('after_work', 'before_completion')"
        );
    }

    #[test]
    fn pending_turn_input_id_mint_preserves_the_fig_886_format() {
        assert_eq!(
            derive_pending_turn_input_id(&SessionId::from("session"), Some("source"), 123, 7),
            "ti:f876d5a24aeb836217de2df548afda96b5194380cfb630d3a4ececf306ec20eb"
        );
        assert_ne!(
            derive_pending_turn_input_id(&SessionId::from("session"), Some("source"), 123, 7),
            derive_pending_turn_input_id(&SessionId::from("session"), Some("source"), 123, 8)
        );
    }
}
