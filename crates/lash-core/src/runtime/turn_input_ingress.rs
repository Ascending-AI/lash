#[allow(unused_imports)]
use crate::CheckpointKind;
#[allow(unused_imports)]
use crate::SessionId;
pub use lash_core_store::turn_input_vocabulary::*;

/// Generates the checkpoint enumeration an admission can name, from one variant list.
///
/// The generated match is exhaustive, so a new [`CheckpointKind`] variant fails to compile until it
/// is added here, which keeps `ADMISSION_CHECKPOINTS` complete for the tests that sweep every
/// checkpoint.
macro_rules! turn_input_admission_checkpoints {
    ($($variant:ident),+ $(,)?) => {
        #[cfg(test)]
        pub(crate) const ADMISSION_CHECKPOINTS: &[CheckpointKind] = &[$(CheckpointKind::$variant),+];

        /// Compile-time guard only: the exhaustive match below is what fails on a new variant.
        #[cfg(test)]
        #[allow(dead_code)]
        fn assert_admission_checkpoints_are_exhaustive(checkpoint: CheckpointKind) {
            match checkpoint {
                $(CheckpointKind::$variant => ()),+
            }
        }
    };
}

turn_input_admission_checkpoints!(AfterWork, BeforeCompletion);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TurnId;

    #[test]
    fn turn_input_state_decode_rejects_scope_disagreement() {
        let active = TurnInputIngress::active_turn(
            TurnId::from("turn-1"),
            TurnInputCheckpointBoundary::AfterWork,
        );
        let next = TurnInputIngress::next_turn();
        assert!(TurnInputState::from_persisted("pending_active", next.clone(), None).is_err());
        assert!(
            TurnInputState::from_persisted("deferred_next_turn", active.clone(), None).is_err()
        );
        assert!(TurnInputState::from_persisted("accepted", next, None).is_err());
    }

    #[test]
    fn every_checkpoint_admits_at_least_the_default_boundary() {
        for checkpoint in ADMISSION_CHECKPOINTS.iter().copied() {
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
