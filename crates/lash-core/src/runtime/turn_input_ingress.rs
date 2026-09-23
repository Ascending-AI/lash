#[allow(unused_imports)]
use crate::CheckpointKind;
#[allow(unused_imports)]
use crate::SessionId;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TurnId;

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
        let active = TurnInputIngress::active_turn(
            TurnId::from("turn-1"),
            TurnInputCheckpointBoundary::AfterWork,
        );
        let next = TurnInputIngress::next_turn();
        for kind in TurnInputStateKind::ALL.iter().copied() {
            // Each name has at least one scope it admits, and decoding that
            // pair reproduces the name — disagreement stays unrepresentable.
            let ingress = match kind {
                TurnInputStateKind::PendingActive
                | TurnInputStateKind::Accepted
                | TurnInputStateKind::Cancelled
                | TurnInputStateKind::Completed => active.clone(),
                TurnInputStateKind::DeferredNextTurn => next.clone(),
            };
            let state = TurnInputState::from_persisted(kind.as_str(), ingress)
                .expect("scope-legal pair decodes");
            assert_eq!(state.kind(), kind);
            assert_eq!(state.as_str(), kind.as_str());
        }
    }

    #[test]
    fn turn_input_state_decode_rejects_scope_disagreement() {
        let active = TurnInputIngress::active_turn(
            TurnId::from("turn-1"),
            TurnInputCheckpointBoundary::AfterWork,
        );
        let next = TurnInputIngress::next_turn();
        assert_eq!(
            TurnInputState::from_persisted("pending_active", next.clone()),
            None
        );
        assert_eq!(
            TurnInputState::from_persisted("deferred_next_turn", active.clone()),
            None
        );
        assert_eq!(TurnInputState::from_persisted("accepted", next), None);
    }

    #[test]
    fn turn_input_state_wire_values_match_the_persisted_ingress_encoding() {
        assert_eq!(TurnInputStateKind::PendingActive.as_str(), "pending_active");
        assert_eq!(
            TurnInputStateKind::DeferredNextTurn.as_str(),
            "deferred_next_turn"
        );
        assert_eq!(TurnInputStateKind::Accepted.as_str(), "accepted");
        assert_eq!(TurnInputStateKind::Cancelled.as_str(), "cancelled");
        assert_eq!(TurnInputStateKind::Completed.as_str(), "completed");
    }

    #[test]
    fn turn_input_state_terminality_covers_exactly_settled_states() {
        for state in TurnInputStateKind::ALL.iter().copied() {
            assert_eq!(
                state.is_terminal(),
                matches!(
                    state,
                    TurnInputStateKind::Cancelled | TurnInputStateKind::Completed
                ),
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
