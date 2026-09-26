//! Effect-controller error widening that needs `lash-core`'s plugin errors.
//!
//! The error type itself is store vocabulary (`lash_core_store::runtime_error`)
//! and is re-exported at this path.

pub use lash_core_store::runtime_error::RuntimeEffectControllerError;

use crate::PluginError;
#[allow(unused_imports)]
use crate::runtime::{RuntimeError, RuntimeErrorCode};

impl From<PluginError> for RuntimeEffectControllerError {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::Runtime(err) => err.into(),
            PluginError::RuntimeEffectController(err) => err,
            err @ PluginError::ProcessNotVisible { .. } => {
                Self::new(RuntimeErrorCode::ProcessNotVisible, err.to_string())
            }
            err @ PluginError::ProcessAlreadyTerminal { .. } => {
                Self::new(RuntimeErrorCode::ProcessAlreadyTerminal, err.to_string())
            }
            err @ PluginError::ParentEnded { .. } => {
                Self::new(RuntimeErrorCode::ProcessParentEnded, err.to_string())
            }
            err @ PluginError::ProcessCancelConflict { .. } => {
                Self::new(RuntimeErrorCode::ProcessCancelConflict, err.to_string())
            }
            err @ PluginError::ProcessNoLongerRetained { .. } => {
                Self::new(RuntimeErrorCode::ProcessNoLongerRetained, err.to_string())
            }
            err => Self::new(RuntimeErrorCode::Plugin, err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_ended_refusal_stays_typed_and_terminal_through_effect_controller() {
        let error = PluginError::ParentEnded {
            start_key: Some(crate::StartKey::for_host(
                crate::StartKeyOwner::HOST,
                "late-child",
            )),
            parent: crate::ScopeId::process(crate::process_id_for_test("ended-parent")),
        };
        assert!(error.is_terminal());
        assert!(!error.is_retryable());
        let runtime = RuntimeEffectControllerError::from(error).into_runtime_error();
        assert_eq!(runtime.code.as_str(), "process_parent_ended");
        assert!(runtime.is_terminal());
        assert!(!runtime.is_retryable());
    }

    #[test]
    fn cancellation_conflict_preserves_the_standing_request_at_the_effect_boundary() {
        let existing =
            crate::CancelRequest::new(crate::CancelOrigin::OperatorRequested, "actor:first", 11);
        let requested =
            crate::CancelRequest::new(crate::CancelOrigin::ModelRequested, "actor:second", 22);
        assert!(!existing.same_cancellation_as(&requested));
        let error = PluginError::ProcessCancelConflict {
            process_id: crate::process_id_for_test("conflict"),
            existing: Box::new(existing),
            requested: Box::new(requested),
        };
        assert!(error.is_terminal());
        assert!(!error.is_retryable());
        let runtime = RuntimeEffectControllerError::from(error).into_runtime_error();
        assert_eq!(runtime.code.as_str(), "process_cancel_conflict");
        assert!(runtime.is_terminal());
        assert!(!runtime.is_retryable());
        assert!(runtime.message.contains("OperatorRequested"));
        assert!(runtime.message.contains("actor:first"));
    }

    #[test]
    fn process_target_discriminators_survive_the_effect_controller_boundary() {
        for (error, expected) in [
            (
                PluginError::ProcessNotVisible {
                    process_id: crate::process_id_for_test("missing"),
                },
                RuntimeErrorCode::ProcessNotVisible,
            ),
            (
                PluginError::ProcessAlreadyTerminal {
                    process_id: crate::process_id_for_test("done"),
                    status: crate::ProcessStatus::Completed,
                },
                RuntimeErrorCode::ProcessAlreadyTerminal,
            ),
            (
                PluginError::ProcessNoLongerRetained {
                    terminal_label: "completed".to_string(),
                    pruned_at_ms: 42,
                },
                RuntimeErrorCode::ProcessNoLongerRetained,
            ),
        ] {
            assert_eq!(RuntimeEffectControllerError::from(error).code, expected);
        }
    }

    #[test]
    fn permanent_store_integrity_errors_are_terminal_and_non_retryable() {
        for store_error in [
            crate::StoreError::StoredDataCorrupt {
                record_kind: "RuntimeEffectReplay",
                message: "negative lease_expires_at_ms".to_string(),
            },
            crate::StoreError::MonotonicCounterOverflow {
                counter: "effect_replay_fence",
                current: i64::MAX as u64,
            },
        ] {
            let controller_error = RuntimeEffectControllerError::from(store_error);
            let runtime_error = controller_error.into_runtime_error();
            assert_eq!(
                runtime_error.code,
                crate::RuntimeErrorCode::RuntimeStoreCorrupt
            );
            assert!(!runtime_error.is_retryable());
            assert!(runtime_error.is_terminal());
        }
    }

    #[test]
    fn transient_store_failures_stay_retryable_and_non_terminal() {
        for store_error in [
            crate::StoreError::StorageFailure {
                backend: "sqlite",
                message: "database is locked".to_string(),
            },
            crate::StoreError::Contended,
        ] {
            let controller_error = RuntimeEffectControllerError::from(store_error);
            let runtime_error = controller_error.into_runtime_error();
            assert_eq!(runtime_error.code, crate::RuntimeErrorCode::RuntimeStore);
            assert!(runtime_error.is_retryable());
            assert!(!runtime_error.is_terminal());
        }
    }

    #[test]
    fn foreign_constructor_preserves_extension_code_as_foreign() {
        let runtime_error = RuntimeEffectControllerError::foreign(
            "plugin_defined_abort",
            crate::TurnFailureCause::Outcome,
            "extension refused the effect",
        )
        .into_runtime_error();

        assert_eq!(runtime_error.code.as_str(), "plugin_defined_abort");
        assert!(!runtime_error.is_retryable());
        // The host chose an outcome, so the code is terminal (FIG-3575).
        assert!(runtime_error.is_terminal());
    }

    #[test]
    fn replay_mismatch_summary_survives_runtime_error_conversion() {
        let summary = crate::RuntimeEffectReplayMismatchReport {
            divergent_path_count: 2,
            first_divergent_paths: vec![
                "command.duration_ms".to_string(),
                "invocation.replay_key".to_string(),
            ],
            effect_kind: None,
        };
        let runtime_error = RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::SqliteEffectReplayHashConflict,
            "recorded runtime effect diverged at command.duration_ms",
        )
        .with_summary(summary.clone())
        .into_runtime_error();

        assert!(runtime_error.code.is_replay_mismatch());
        assert_eq!(runtime_error.summary.as_deref(), Some(&summary));
        assert!(runtime_error.to_string().contains("command.duration_ms"));
    }

    #[test]
    fn replay_mismatch_summary_survives_controller_error_conversion() {
        let summary = crate::RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["invocation.scope.turn_index".to_string()],
            effect_kind: None,
        };
        let mut runtime_error = RuntimeError::new(
            crate::RuntimeErrorCode::EffectReplayDivergence,
            "replacement reconstructed a different turn index",
        );
        runtime_error.summary = Some(Box::new(summary.clone()));

        let controller_error = RuntimeEffectControllerError::from(runtime_error);

        assert_eq!(
            controller_error.code,
            crate::RuntimeErrorCode::EffectReplayDivergence
        );
        assert_eq!(
            controller_error.summary.map(|summary| *summary),
            Some(summary)
        );
    }
}
