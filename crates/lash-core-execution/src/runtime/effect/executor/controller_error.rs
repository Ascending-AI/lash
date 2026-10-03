//! Effect-controller error widening that needs `lash-core`'s plugin errors.
//!
//! The error type itself is store vocabulary (`lash_core_store::runtime_error`)
//! and is re-exported at this path.

pub use lash_core_store::runtime_error::RuntimeEffectControllerError;

use crate::PluginError;
#[allow(unused_imports)]
use crate::runtime::{RuntimeError, RuntimeErrorCode};

impl From<PluginError> for RuntimeEffectControllerError {
    /// The code the effect controller carries a plugin error by. Its class
    /// is the error's own ([`PluginError::class`]): the match is exhaustive,
    /// so a new variant does not compile until it names its code.
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::ProviderFailure {
                kind,
                code,
                retryable,
                terminal_reason,
                message,
            } => RuntimeError::new(RuntimeErrorCode::LlmProvider, message)
                .with_cause(crate::RuntimeErrorCause::ProviderFailure {
                    failure_kind: kind,
                    code,
                    retryable,
                    terminal_reason,
                })
                .into(),
            PluginError::Operation(failure) => {
                let mut error = plugin_failure_error(&failure);
                error.cause = Some(crate::RuntimeErrorCause::PluginOperation { failure });
                error
            }
            PluginError::HookFailures { causes } => {
                let mut error = causes
                    .iter()
                    .min_by_key(|cause| cause.failure.class.precedence())
                    .map(|cause| plugin_failure_error(&cause.failure))
                    .unwrap_or_else(|| Self::new(RuntimeErrorCode::Plugin, "plugin hooks failed"));
                error.message = "plugin runtime event hooks failed".into();
                error.cause = Some(crate::RuntimeErrorCause::PluginHooks { causes });
                error
            }
            PluginError::Format(refusal) => refusal.into(),
            PluginError::State(crate::PluginStateError::Unrecorded { plugin }) => {
                let mut error = Self::new(
                    RuntimeErrorCode::Plugin,
                    format!(
                        "plugin `{plugin}` returned state commands outside a recorded callback"
                    ),
                );
                error.cause = Some(crate::RuntimeErrorCause::PluginStateUnrecorded { plugin });
                error
            }
            PluginError::State(crate::PluginStateError::EffectOwnerMismatch) => {
                let mut error = Self::new(
                    RuntimeErrorCode::EffectReplayDivergence,
                    "recorded callback state belongs to another runtime owner",
                );
                error.cause = Some(crate::RuntimeErrorCause::PluginStateEffectOwnerMismatch);
                error
            }
            PluginError::State(crate::PluginStateError::Frontier { plugin, refusal }) => {
                let mut error = Self::new(
                    RuntimeErrorCode::EffectReplayDivergence,
                    format!("plugin `{plugin}` cannot apply a recorded publication: {refusal}"),
                );
                error.cause = Some(crate::RuntimeErrorCause::PluginStateFrontier {
                    refusal: Box::new(lash_core_store::tool_run::NamespaceFrontierRefusal {
                        plugin,
                        refusal,
                    }),
                });
                error
            }
            PluginError::TriggerOperation(err) => {
                let code = if err.is_terminal() {
                    RuntimeErrorCode::Plugin
                } else {
                    RuntimeErrorCode::PluginSessionManager
                };
                Self::new(code, err.to_string())
            }
            PluginError::StoredDataCorrupt {
                record_kind,
                message,
            } => Self::stored_data_corrupt(record_kind, message),
            PluginError::StoreRefusal(err) => err.into_store_error().into(),
            PluginError::StoreUnavailable { fault } => Self::new(fault.code(), fault.to_string()),
            PluginError::Runtime(err) => err.into(),
            PluginError::RuntimeEffectController(err) => err,
            err @ PluginError::SessionHeadOwned { .. } => {
                Self::new(RuntimeErrorCode::SessionHeadOwned, err.to_string())
            }
            PluginError::MissingRecordedProcessConfig { engine_kind } => {
                let mut error = Self::new(
                    RuntimeErrorCode::MissingRecordedProcessConfig,
                    format!("process engine `{engine_kind}` has no recorded configuration"),
                );
                error.cause =
                    Some(crate::RuntimeErrorCause::MissingRecordedProcessConfig { engine_kind });
                error
            }
            err @ PluginError::SessionExecutionLeaseLost { .. } => {
                Self::new(RuntimeErrorCode::SessionExecutionLeaseLost, err.to_string())
            }
            err @ PluginError::ProcessExecutionSuperseded { .. } => {
                Self::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            }
            err @ PluginError::ProcessNotVisible { .. } => {
                Self::new(RuntimeErrorCode::ProcessNotVisible, err.to_string())
            }
            err @ PluginError::NotASessionRuntime { .. } => {
                Self::new(RuntimeErrorCode::NotASessionRuntime, err.to_string())
            }
            err @ PluginError::ProcessAlreadyTerminal { .. } => {
                Self::new(RuntimeErrorCode::ProcessAlreadyTerminal, err.to_string())
            }
            ref err @ PluginError::ParentEnded {
                ref start_key,
                ref parent,
            } => RuntimeError::new(RuntimeErrorCode::ProcessParentEnded, err.to_string())
                .with_cause(crate::RuntimeErrorCause::ProcessParentEnded {
                    start_key: start_key.clone(),
                    parent: Box::new(parent.clone()),
                })
                .into(),
            ref err @ PluginError::StartKeyConflict { ref start_key } => {
                RuntimeError::new(RuntimeErrorCode::ProcessStartKeyConflict, err.to_string())
                    .with_cause(crate::RuntimeErrorCause::ProcessStartKeyConflict {
                        start_key: start_key.clone(),
                    })
                    .into()
            }
            err @ PluginError::TriggerDeliveryBound { .. } => {
                Self::new(RuntimeErrorCode::TriggerDeliveryBound, err.to_string())
            }
            err @ PluginError::TriggerDeliveryRetired { .. } => {
                Self::new(RuntimeErrorCode::TriggerDeliveryRetired, err.to_string())
            }
            err @ PluginError::ProcessCancelConflict { .. } => {
                Self::new(RuntimeErrorCode::ProcessCancelConflict, err.to_string())
            }
            err @ PluginError::ProcessNoLongerRetained { .. } => {
                Self::new(RuntimeErrorCode::ProcessNoLongerRetained, err.to_string())
            }
            err @ (PluginError::UnusableSchema { .. }
            | PluginError::UnusableToolSchema { .. }
            | PluginError::ValueMismatch { .. }) => {
                err.into_turn_failure(RuntimeErrorCode::Plugin).into()
            }
            err @ (PluginError::AppendReceiptRequestedNodeCountCorrupt { .. }
            | PluginError::MonotonicCounterOverflow { .. }) => {
                Self::new(RuntimeErrorCode::RuntimeStoreCorrupt, err.to_string())
            }
            err @ (PluginError::Session(_)
            | PluginError::Registration(_)
            | PluginError::ConfigRegistration(_)
            | PluginError::Invoke(_)
            | PluginError::State(_)
            | PluginError::Declaration(_)
            | PluginError::InvalidTriggerTarget { .. }
            | PluginError::InvalidToolDiscovery { .. }
            | PluginError::InvalidBatchMaximum { .. }
            | PluginError::ResidentToolContractUnavailable { .. }
            | PluginError::ResidentToolDuplicateId { .. }
            | PluginError::ResidentToolDuplicateName { .. }
            | PluginError::ResidentToolRouteUnavailable { .. }
            | PluginError::SessionAlreadyExists { .. }
            | PluginError::SessionInitTooLarge { .. }
            | PluginError::MissingRecordedSessionConfig { .. }
            | PluginError::RecordedSessionConfigConflict { .. }
            | PluginError::AppendOperationIdentityConflict { .. }
            | PluginError::ClockBeforeUnixEpoch { .. }
            | PluginError::ProcessOutputAttachmentUnavailable { .. }
            | PluginError::ProcessUnknown { .. }
            | PluginError::ProcessChangeCursorPruned { .. }
            | PluginError::TriggerSubscriptionChangeCursorPruned { .. }
            | PluginError::ProcessParkFeedCursorCompacted { .. }
            | PluginError::ProcessEventsReleased { .. }
            | PluginError::ProcessCallerDeparted { .. }
            | PluginError::ProcessHandedOver { .. }
            | PluginError::ProcessTerminalOutcomeMismatch { .. }
            | PluginError::ReservedProcessEvent { .. }
            | PluginError::WakeDeliveryIdentityMismatch { .. }
            | PluginError::ProcessWakeDeliveryFormatVersionMismatch { .. }
            | PluginError::ProcessRegistryCursorBackendMismatch { .. }) => {
                Self::new(RuntimeErrorCode::Plugin, err.to_string())
            }
        }
    }
}

fn plugin_failure_error(
    failure: &crate::plugin::PluginOperationFailure,
) -> RuntimeEffectControllerError {
    if failure.code.namespace().as_str() == "lash" {
        RuntimeEffectControllerError::new(
            RuntimeErrorCode::from_wire_code(failure.code.spelling()),
            failure.message.clone(),
        )
    } else {
        let cause = match failure.class {
            crate::plugin::PluginFailureClass::Terminal => crate::TurnFailureCause::Outcome,
            crate::plugin::PluginFailureClass::Parked => crate::TurnFailureCause::Parked,
            crate::plugin::PluginFailureClass::Retryable
            | crate::plugin::PluginFailureClass::Redrivable => crate::TurnFailureCause::LiveFault,
        };
        RuntimeEffectControllerError::foreign(
            failure.code.namespaced(),
            cause,
            failure.message.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_ended_refusal_stays_typed_and_terminal_through_effect_controller() {
        let error = PluginError::ParentEnded {
            start_key: Some(crate::StartKey::for_host("late-child")),
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
                    terminal_label: crate::RetiredProcessStatus::Completed,
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
                message: "negative shift_epoch".to_string(),
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
        for (store_error, code) in [
            (
                crate::StoreError::StorageFailure {
                    backend: "sqlite",
                    message: "database is locked".to_string(),
                },
                crate::RuntimeErrorCode::RuntimeStore,
            ),
            (
                crate::StoreError::Contended,
                crate::RuntimeErrorCode::StoreCommitContended,
            ),
        ] {
            let controller_error = RuntimeEffectControllerError::from(store_error);
            let runtime_error = controller_error.into_runtime_error();
            assert_eq!(runtime_error.code, code);
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
            crate::RuntimeErrorCode::EffectReplayDivergence,
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
    #[test]
    fn retained_material_refusal_keeps_its_cause_through_plugin_and_host_errors() {
        use lash_core_store::tool_run::{
            MaterialLocation, MaterialOwner, MaterialPayload, MaterialRefusal, MaterialRole,
        };
        let owner = MaterialOwner::Run {
            opener: lash_core_store::effect_opener::EffectOpener::turn("s", "r"),
        };
        let reference = MaterialPayload::new(
            owner,
            MaterialRole::AttemptOutput,
            None,
            "recorded output".into(),
        )
        .reference(MaterialLocation::JournalLocal)
        .unwrap();
        for refusal in [
            MaterialRefusal::Missing {
                reference: Box::new(reference.clone()),
            },
            MaterialRefusal::Retired {
                reference: Box::new(reference),
            },
        ] {
            let controller = RuntimeEffectControllerError::from(refusal.clone());
            assert!(controller.journaled);
            let plugin = PluginError::RuntimeEffectController(controller);
            let host = RuntimeEffectControllerError::from(plugin).into_runtime_error();
            assert_eq!(host.code, RuntimeErrorCode::RetainedResultRefused);
            assert!(host.is_terminal());
            assert!(!host.is_retryable());
            let wire = serde_json::to_vec(&host).unwrap();
            let reopened: RuntimeError = serde_json::from_slice(&wire).unwrap();
            assert!(
                matches!(reopened.cause, Some(crate::RuntimeErrorCause::MaterialRefused { refusal: found }) if *found == refusal)
            );
        }
    }
}
